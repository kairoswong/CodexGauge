//! Embeds the application icon (and a minimal version manifest) into the
//! Windows exe so it shows a proper icon in Explorer and in the tray.

fn main() {
    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    if target_os != "windows" {
        return;
    }

    let mut res = winres::WindowsResource::new();
    // Icon is embedded as the standard application icon (resource ID 1).
    // Path is relative to the crate root where build.rs runs.
    res.set_icon("../assets/CodexGauge.ico");
    // Branding/version info so Explorer shows a proper description.
    res.set("FileDescription", "CodexGauge");
    res.set("ProductName", "CodexGauge");
    res.set("OriginalFilename", "codex-gauge.exe");
    res.set("LegalCopyright", "MIT License");
    if let Err(e) = res.compile() {
        eprintln!("failed to embed icon resource: {e}");
        std::process::exit(1);
    }
}
