//! Talks to the real Codex CLI by spawning `codex app-server --stdio`
//! and driving a JSON-RPC exchange. The child process handles all auth;
//! this binary never touches credentials.

use std::io::{BufRead, BufReader, Write};
use std::os::windows::process::CommandExt;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::time::Duration;

use serde_json::{json, Value};

use crate::quota::{Quota, Window};

/// Error type for quota fetch failures.
#[derive(Debug)]
pub enum FetchError {
    Spawn(String),
    Io(String),
    Rpc(String),
}

impl std::fmt::Display for FetchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FetchError::Spawn(e) => write!(f, "failed to start codex: {e}"),
            FetchError::Io(e) => write!(f, "codex I/O error: {e}"),
            FetchError::Rpc(e) => write!(f, "codex RPC error: {e}"),
        }
    }
}

struct Server {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    next_id: i64,
}

/// Always kill the spawned codex subprocess, even on early-return error paths
/// (timeouts, I/O errors). Prevents orphaned `codex app-server` processes from
/// piling up when the network is down/slow and fetches keep failing.
impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Server {
    /// Kill the child process once we're done, best-effort.
    fn shutdown(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }

    fn start() -> Result<Self, FetchError> {
        // On Windows, npm installs codex as `codex.cmd`, but CreateProcess only
        // auto-appends `.exe`, so `Command::new("codex")` can't resolve it.
        // Try the common spellings in order and keep the first that spawns.
        #[cfg(windows)]
        const CANDIDATES: [&str; 3] = ["codex.cmd", "codex.exe", "codex"];
        #[cfg(not(windows))]
        const CANDIDATES: [&str; 1] = ["codex"];

        let mut last_err: Option<std::io::Error> = None;
        let mut spawned = None;
        for name in CANDIDATES {
            // CREATE_NO_WINDOW: spawning codex.cmd goes through cmd.exe, which
            // would otherwise flash a console window on this GUI-subsystem app.
            match Command::new(name)
                .arg("app-server")
                .arg("--stdio")
                .env("CODEX_APP_SERVER_LOG_MODE", "off")
                .creation_flags(0x0800_0000) // CREATE_NO_WINDOW
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn()
            {
                Ok(c) => {
                    spawned = Some(c);
                    break;
                }
                Err(e) => last_err = Some(e),
            }
        }
        let mut child = spawned.ok_or_else(|| {
            FetchError::Spawn(
                last_err
                    .map(|e| e.to_string())
                    .unwrap_or_else(|| "codex not found".to_string()),
            )
        })?;

        let stdin = child.stdin.take().ok_or_else(|| {
            FetchError::Spawn("no stdin on codex process".to_string())
        })?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| FetchError::Spawn("no stdout on codex process".to_string()))?;

        Ok(Server {
            child,
            stdin,
            stdout: BufReader::new(stdout),
            next_id: 1,
        })
    }

    fn write(&mut self, v: &Value) -> Result<(), FetchError> {
        let mut buf = serde_json::to_vec(v).map_err(|e| FetchError::Io(e.to_string()))?;
        buf.push(b'\n');
        self.stdin
            .write_all(&buf)
            .and_then(|_| self.stdin.flush())
            .map_err(|e| FetchError::Io(e.to_string()))
    }

    /// Read lines until we find a message whose `id` matches, returning its `result`.
    fn read_for(&mut self, want_id: i64, timeout: Duration) -> Result<Value, FetchError> {
        let deadline = std::time::Instant::now() + timeout;
        loop {
            if std::time::Instant::now() >= deadline {
                return Err(FetchError::Rpc("timeout waiting for codex".to_string()));
            }
            let mut line = String::new();
            match self
                .stdout
                .read_line(&mut line)
                .map_err(|e| FetchError::Io(e.to_string()))
            {
                Ok(0) => return Err(FetchError::Rpc("codex closed stdout".to_string())),
                Ok(_) => {}
                Err(e) => return Err(e),
            }
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            let val: Value =
                serde_json::from_str(trimmed).map_err(|e| FetchError::Rpc(e.to_string()))?;
            // Some messages have no id (notifications); skip them.
            if val["id"].as_i64() == Some(want_id) {
                if let Some(err) = val.get("error") {
                    return Err(FetchError::Rpc(format!("{err}")));
                }
                return val.get("result").cloned().ok_or_else(|| {
                    FetchError::Rpc("response missing result".to_string())
                });
            }
        }
    }

    /// Full handshake + quota read.
    fn fetch_quota(mut self) -> Result<Quota, FetchError> {
        // initialize
        let init_id = self.next_id;
        self.next_id += 1;
        self.write(&json!({
            "method": "initialize",
            "id": init_id,
            "params": {
                "protocolVersion": 1,
                "capabilities": {},
                "clientInfo": { "name": "codex-gauge", "version": "0.1.0" }
            }
        }))?;
        self.read_for(init_id, Duration::from_secs(10))?;

        // account/rateLimits/read
        let read_id = self.next_id;
        self.next_id += 1;
        self.write(&json!({
            "method": "account/rateLimits/read",
            "id": read_id,
            "params": {}
        }))?;
        let result = self.read_for(read_id, Duration::from_secs(10))?;

        self.shutdown();
        Ok(map_result(&result))
    }
}

/// Map one bucket JSON object into a `Window`.
fn map_window(node: &Value) -> Option<Window> {
    let used = node.get("usedPercent").and_then(|v| v.as_f64())?;
    Some(Window {
        used_percent: used.clamp(0.0, 100.0),
        resets_at: node.get("resetsAt").and_then(|v| v.as_i64()).unwrap_or(0),
        window_mins: node
            .get("windowDurationMins")
            .and_then(|v| v.as_i64())
            .unwrap_or(0),
    })
}

/// Map the `account/rateLimits/read` result into a normalized `Quota`.
/// Reads both windows: `primary` (5h) and `secondary` (weekly).
pub fn map_result(result: &Value) -> Quota {
    let mut q = Quota::disconnected();

    let root = result
        .pointer("/rateLimitsByLimitId/codex")
        .or_else(|| result.get("rateLimits"));

    if let Some(root) = root {
        q.primary = root.get("primary").and_then(map_window);
        q.secondary = root.get("secondary").and_then(map_window);
        q.connected = q.primary.is_some() || q.secondary.is_some();
    }

    q
}

/// Spawn codex and return the fetched quota. Returns a disconnected quota
/// (or propagates the error) so the UI can show status.
pub fn fetch_quota_async() -> Result<Quota, FetchError> {
    let server = Server::start()?;
    server.fetch_quota()
}
