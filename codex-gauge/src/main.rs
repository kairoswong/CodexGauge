//! CodexGauge — floating OS-tray widget showing realtime Codex quota.
//! Native Win32 (Rust). Auto-connects to `codex app-server --stdio`.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod appserver;
mod overlay;
mod quota;
mod tray;

use windows::core::*;
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::LibraryLoader::*;
use windows::Win32::System::Threading::*;
use windows::Win32::UI::WindowsAndMessaging::*;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::OnceLock;

use crate::quota::Quota;

const REFRESH_MS: u32 = 300_000; // 5 min
const STARTUP_DELAY_MS: u32 = 100;
const RETRY_MS: u32 = 2_000; // fast retry after a failed refresh
const TIMER_REFRESH: usize = 1;
const TIMER_STARTUP: usize = 2;
const TIMER_WAKE_REFRESH: usize = 3;
const TIMER_RETRY: usize = 4;
/// Worker-thread signal: a refresh failed, so schedule a fast retry.
const WM_APP_RETRY: u32 = WM_APP + 3;
/// Cap on consecutive fast retries before falling back to the slow 5-min
/// cadence, so a long-absent codex process isn't constantly respawned.
const MAX_CONSECUTIVE_RETRIES: u32 = 4;

/// Overlay HWND as raw usize (HWND is not Send), read from the bus window proc.
static OVERLAY_HWND: OnceLock<usize> = OnceLock::new();

/// Skips a refresh tick while a previous fetch is still running, so a slow
/// network can't pile up overlapping `codex app-server` subprocesses.
static REFRESH_IN_FLIGHT: AtomicBool = AtomicBool::new(false);

fn main() -> Result<()> {
    let _instance_mutex = unsafe { CreateMutexW(None, false, w!("Local\\CodexGauge"))? };
    if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
        return Ok(());
    }

    let hinstance: HINSTANCE = unsafe { GetModuleHandleW(None) }?.into();

    overlay::register_window_class(hinstance)?;

    // A hidden message-only window owns the tray and timers.
    let bus = create_bus_window(hinstance)?;

    tray::add(bus, hinstance)?;

    // Overlay, hidden until shown below.
    let overlay = match overlay::create(hinstance) {
        Ok(o) => o,
        Err(e) => {
            eprintln!("overlay create failed: {e}");
            return Err(e);
        }
    };

    // Expose the overlay HWND to the bus proc for tray callbacks.
    let _ = OVERLAY_HWND.set(overlay.hwnd.0 as usize);

    unsafe { let _ = ShowWindow(overlay.hwnd, SW_SHOWNOACTIVATE); };
    unsafe { let _ = UpdateWindow(overlay.hwnd); };

    unsafe { SetTimer(bus, TIMER_STARTUP, STARTUP_DELAY_MS, None) }; // refresh shortly after startup
    unsafe { SetTimer(bus, TIMER_REFRESH, REFRESH_MS, None) }; // periodic

    // Consecutive failed refreshes; reset on success to bound fast retries.
    let mut consecutive_failures: u32 = 0;

    let mut msg = MSG::default();
    loop {
        let ret = unsafe { GetMessageW(&mut msg, None, 0, 0) };
        if ret.0 <= 0 {
            break;
        }
        match msg.message {
            overlay::WM_APP_REFRESH => {
                refresh_async(bus, overlay.hwnd);
            }
            overlay::WM_APP_QUIT => {
                tray::remove(bus);
                unsafe { PostQuitMessage(0) };
            }
            WM_APP_RETRY => {
                // Worker reported a failed/successful refresh. Reconnect fast
                // on failure so the widget isn't stuck "Connecting..." for the
                // whole 5-minute cadence when the service becomes available.
                // Cap consecutive retries to avoid respawning a codex that is
                // genuinely absent; the slow 5-min timer takes over after that.
                if msg.wParam.0 == 0 {
                    consecutive_failures += 1;
                    if consecutive_failures <= MAX_CONSECUTIVE_RETRIES {
                        unsafe { let _ = SetTimer(bus, TIMER_RETRY, RETRY_MS, None); };
                    }
                } else {
                    consecutive_failures = 0;
                    unsafe { let _ = KillTimer(bus, TIMER_RETRY); };
                }
            }
            WM_TIMER => {
                if msg.wParam.0 == TIMER_STARTUP as usize {
                    unsafe { let _ = KillTimer(bus, TIMER_STARTUP); };
                    refresh_async(bus, overlay.hwnd);
                } else if msg.wParam.0 == TIMER_REFRESH as usize {
                    refresh_async(bus, overlay.hwnd);
                } else if msg.wParam.0 == TIMER_WAKE_REFRESH as usize {
                    unsafe { let _ = KillTimer(bus, TIMER_WAKE_REFRESH); };
                    refresh_async(bus, overlay.hwnd);
                } else if msg.wParam.0 == TIMER_RETRY as usize {
                    unsafe { let _ = KillTimer(bus, TIMER_RETRY); };
                    refresh_async(bus, overlay.hwnd);
                }
            }
            WM_POWERBROADCAST => {
                // Resume from sleep: the 5-min timer may not have fired while
                // asleep, so kick a delayed refresh (lets the network settle).
                if msg.wParam.0 == PBT_APMRESUMEAUTOMATIC as usize
                    || msg.wParam.0 == PBT_APMRESUMESUSPEND as usize
                {
                    unsafe {
                        let _ = KillTimer(bus, TIMER_WAKE_REFRESH);
                        let _ = SetTimer(bus, TIMER_WAKE_REFRESH, 2000, None);
                    }
                }
            }
            WM_COMMAND => {
                let cmd = msg.wParam.0 as u32 & 0xffff;
                if cmd == tray::CMD_REFRESH {
                    refresh_async(bus, overlay.hwnd);
                } else if cmd == tray::CMD_EXIT {
                    tray::remove(bus);
                    unsafe { PostQuitMessage(0) };
                }
            }
            _ => {
                unsafe {
                    let _ = TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                }
            }
        }
    }

    tray::remove(bus);
    Ok(())
}

/// Hidden window that owns the tray and timers.
fn create_bus_window(hinstance: HINSTANCE) -> Result<HWND> {
    let class_name = w!("CodexGaugeBus");
    let wc = WNDCLASSW {
        lpfnWndProc: Some(bus_wndproc),
        hInstance: hinstance,
        lpszClassName: class_name,
        ..Default::default()
    };
    unsafe { RegisterClassW(&wc) };
    let hwnd = unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE(0),
            class_name,
            w!("CodexGaugeBus"),
            WS_OVERLAPPEDWINDOW,
            0,
            0,
            0,
            0,
            HWND_MESSAGE,
            HMENU::default(),
            hinstance,
            None,
        )
    }?;
    Ok(hwnd)
}

extern "system" fn bus_wndproc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if msg == tray::WM_TRAY {
        // Tray callbacks arrive via SendMessage directly in this proc.
        let overlay = HWND(*OVERLAY_HWND.get().unwrap_or(&0) as *mut std::ffi::c_void);
        handle_tray(lparam, overlay);
        return LRESULT(0);
    }
    match msg {
        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}

fn handle_tray(lparam: LPARAM, overlay: HWND) {
    let low = (lparam.0 as u32) & 0xffff;
    match low {
        WM_LBUTTONUP => {
            // Click toggles overlay visibility.
            if unsafe { IsWindowVisible(overlay) }.as_bool() {
                unsafe { let _ = ShowWindow(overlay, SW_HIDE); };
            } else {
                unsafe { let _ = ShowWindow(overlay, SW_SHOWNOACTIVATE); };
                unsafe { let _ = UpdateWindow(overlay); };
            }
            unsafe { let _ = RedrawWindow(overlay, None, None, RDW_INVALIDATE | RDW_UPDATENOW); };
        }
        WM_RBUTTONUP => {
            // A visible owner window lets the popup reliably take the foreground.
            tray::show_menu(overlay);
        }
        _ => {}
    }
}

fn refresh_async(bus: HWND, overlay: HWND) {
    // Skip if a previous fetch is still in flight.
    if REFRESH_IN_FLIGHT.swap(true, Ordering::AcqRel) {
        return;
    }

    // HWND is not Send; carry them as raw integers across the thread boundary.
    let bus_bits = bus.0 as usize;
    let overlay_bits = overlay.0 as usize;
    // Fetch off the UI thread, then repaint.
    std::thread::spawn(move || {
        let result = appserver::fetch_quota_async();
        let quota = match &result {
            Ok(q) => q.clone(),
            Err(_) => Quota::disconnected(),
        };
        overlay::set_quota(quota);
        let hwnd = HWND(overlay_bits as *mut std::ffi::c_void);
        unsafe {
            let _ = RedrawWindow(hwnd, None, None, RDW_INVALIDATE | RDW_UPDATENOW);
        }
        // Tell the main thread whether to schedule a fast retry. 0 = failed
        // (reconnect soon), 1 = succeeded (no outstanding retry needed).
        let ok = if result.is_ok() { WPARAM(1) } else { WPARAM(0) };
        unsafe {
            // Cached bus HWND; fall back to posting to the overlay if unset.
            let target = if bus_bits != 0 {
                HWND(bus_bits as *mut std::ffi::c_void)
            } else {
                hwnd
            };
            let _ = PostMessageW(target, WM_APP_RETRY, ok, LPARAM(0));
        }
        // Clear the in-flight flag so the next tick can fetch again.
        REFRESH_IN_FLIGHT.store(false, Ordering::Release);
    });
}
