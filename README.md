# CodexGauge

A floating Codex quota widget for Windows 10/11 x64. It's a pure native **Win32 (Rust)** build — a single tiny `codex-gauge.exe` (~**266 KB**) with **no runtime or dependencies**. It runs on any 64-bit Windows out of the box.

The window stays above other windows and stays out of the taskbar. Click the tray gauge to show or hide it; right-click for **Refresh** and **Exit**.

## Motivation

When you're in the middle of a Codex session, it's all too easy to lose track of how much quota is left. Usage only shows up deep inside the CLI, so you tend to find out it's gone right when you need it most. I wanted a lightweight, always-visible reminder of real Codex usage that refreshed on its own — no heavy service, no browser tab left open.

CodexGauge is that reminder: a tiny, always-on-top widget that quietly shows your current `5h` and weekly usage, refreshes itself every few minutes, and shifts colour as you near the limit, so you always know how much runway you have left.

## Live data connection

CodexGauge reads **real** Codex quota through the official **app-server JSON-RPC interface** — no separate HTTP service, no API key, no token, and no scraping of private files.

On each refresh it spawns the Codex CLI as a child process:

```powershell
codex app-server --stdio
```

and exchanges two newline-delimited JSON-RPC messages: `initialize`, then `account/rateLimits/read`. It maps the returned percentage window (the `codex` metered bucket) onto the widget model:

- `used_percent` ← `rateLimits.primary.usedPercent`
- `resets_at` ← `rateLimits.primary.resetsAt` (Unix epoch seconds)

The app never sends an `Authorization`/`Bearer`/`Cookie` header and never reads account credentials — the Codex CLI handles authentication itself, out of process.

## Build

You'll need the **Rust toolchain (MSVC)** — `rustc`/`cargo` with the MSVC target.

```powershell
cd codex-gauge
cargo build --release
```

The binary lands at `codex-gauge\target\release\codex-gauge.exe` (~**266 KB**), and has **no .NET or other runtime dependency**.

## Features

- Borderless, rounded-corner, semi-transparent, always-on-top overlay at the top-right of the primary screen, drawn entirely with GDI:
  - Deep `#18181E` background with 12px rounded corners, semi-transparent (alpha 235).
  - Two stacked rows, one per rate-limit window — `5h` (primary) and `1w` (weekly):
    - Muted small label on the left, bold integer percentage (`12%`) right-aligned.
    - A color-coded progress bar fills the row; green <60%, amber <85%, red ≥85%.
    - Both the bar **and** the percentage digit take the status color, so heavy usage reads at a glance.
  - A divider separates the figures from a footer line: a reset countdown (e.g. `resets in 1h 23m`) on the left, and a subtle `Codex` brand mark in the bottom-right corner.
  - When it can't reach the server, it collapses to a single centered `Connecting...` line (with the app icon).
  - Drag the overlay by holding anywhere on it.
- System tray icon (the CodexGauge icon, embedded in the exe):
  - **Left-click** toggles the overlay show/hide.
  - **Right-click** opens a menu with **Refresh** and **Exit**.
  - Tooltip reads `Codex Usage`.
- Auto-refresh every 5 minutes, plus a refresh shortly after startup. A global in-flight guard skips a tick if a previous fetch is still running, so a slow or dropped network can't pile up overlapping codex subprocesses, and failed fetches always dispose the child.
- Reads live quota via `codex app-server --stdio` (JSON-RPC); the Codex CLI handles authentication out of process, so the app never touches credentials.

## Behavior

- A startup refresh, a single 5-minute timer, and manual refresh drive data updates. An in-flight guard skips a periodic tick when a previous fetch is still running, keeping at most one codex subprocess alive at a time.
- The default data source spawns `codex app-server --stdio` per request and always disposes of the child afterwards — even on timeout or I/O failure (via `Drop`) — so no orphan processes accumulate.
- On a failed fetch the widget shows a terse `Connecting...` state and retries on the next timer tick.
- Tray **Exit** quits the process and removes the tray icon; closing via the tray is the way to quit (the overlay has no close button).

## Project layout

```
codex-gauge\
  build.rs        # embeds assets\CodexGauge.ico + version info via winres
  src\main.rs     # entry point, message loop, tray dispatch, timers
  src\overlay.rs  # floating GDI overlay window (paint, drag)
  src\tray.rs     # system tray icon + context menu
  src\quota.rs    # quota data model
  src\appserver.rs# spawns `codex app-server --stdio` and parses quota
assets\
  CodexGauge.ico  # shared application icon
```

## License

Released under the MIT License — see the [LICENSE](LICENSE) file.
