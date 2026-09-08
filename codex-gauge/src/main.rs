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
use windows::Win32::UI::WindowsAndMessaging::*;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::OnceLock;

use crate::quota::Quota;

const REFRESH_MS: u32 = 300_000; // 5 minutes
const TIMER_REFRESH: usize = 1;
const TIMER_STARTUP: usize = 2;

/// Overlay HWND (stored as a raw usize because HWND is not Send).
/// The tray callback and startup refresh need it from the bus window proc.
static OVERLAY_HWND: OnceLock<usize> = OnceLock::new();

/// Guards against overlapping quota fetches. When network is slow/down, a
/// fetch can stall beyond the 5-minute window; without this lock every timer
/// tick would spawn a fresh codex subprocess concurrently (multiple children,
/// UI redrawing each time one returns). Skips the new tick if one is running.
static REFRESH_IN_FLIGHT: AtomicBool = AtomicBool::new(false);

fn main() -> Result<()> {
    let hinstance: HINSTANCE = unsafe { GetModuleHandleW(None) }?.into();

    // Register the overlay window class.
    overlay::register_window_class(hinstance)?;

    // Create a hidden message-only window as the tray/message dispatcher owner.
    let bus = create_bus_window(hinstance)?;

    // Create tray.
    tray::add(bus, hinstance)?;

    // Create the overlay (hidden by default).
    let overlay = match overlay::create(hinstance) {
        Ok(o) => o,
        Err(e) => {
            eprintln!("overlay create failed: {e}");
            return Err(e);
        }
    };

    // Make the overlay handle available to the bus window proc (tray callbacks
    // arrive there via SendMessage, not through the GetMessage queue).
    let _ = OVERLAY_HWND.set(overlay.hwnd.0 as usize);

    // Show overlay on startup.
    unsafe { let _ = ShowWindow(overlay.hwnd, SW_SHOWNOACTIVATE); };
    unsafe { let _ = UpdateWindow(overlay.hwnd); };

    // Kick off an immediate refresh shortly after startup.
    unsafe { SetTimer(bus, TIMER_STARTUP, 500, None) };
    // Periodic refresh.
    unsafe { SetTimer(bus, TIMER_REFRESH, REFRESH_MS, None) };

    let mut msg = MSG::default();
    loop {
        let ret = unsafe { GetMessageW(&mut msg, None, 0, 0) };
        if ret.0 <= 0 {
            break;
        }
        match msg.message {
            overlay::WM_APP_REFRESH => {
                let hwnd = msg.hwnd;
                refresh_async(hwnd);
            }
            overlay::WM_APP_QUIT => {
                tray::remove(bus);
                unsafe { PostQuitMessage(0) };
            }
            WM_TIMER => {
                if msg.wParam.0 == TIMER_STARTUP as usize {
                    unsafe { let _ = KillTimer(bus, TIMER_STARTUP); };
                    refresh_async(overlay.hwnd);
                } else if msg.wParam.0 == TIMER_REFRESH as usize {
                    refresh_async(overlay.hwnd);
                }
            }
            WM_COMMAND => {
                let cmd = msg.wParam.0 as u32 & 0xffff;
                if cmd == tray::CMD_REFRESH {
                    refresh_async(overlay.hwnd);
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

/// A hidden window that owns the tray and timers; overlay paints itself via its own HWND.
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
        // Tray callbacks are delivered via SendMessage straight into this
        // window proc (they never enter the GetMessage queue).
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
            // Use the overlay (a visible window) as the menu owner so the
            // popup reliably gets the foreground and actually shows up.
            tray::show_menu(overlay);
        }
        _ => {}
    }
}

fn refresh_async(overlay: HWND) {
    // Skip if a previous fetch is still in flight (e.g. network stalled).
    // This prevents spawning a new `codex app-server` subprocess on every
    // timer tick during an outage or slow link — the main cause of the
    // "keeps refreshing / connecting forever" symptom after reconnect.
    if REFRESH_IN_FLIGHT.swap(true, Ordering::AcqRel) {
        return;
    }

    // HWND is not Send; carry it as a raw integer across the thread boundary.
    let overlay_bits = overlay.0 as usize;
    // Do the potentially-slow fetch off the UI thread, then repaint.
    std::thread::spawn(move || {
        let result = appserver::fetch_quota_async();
        let quota = match result {
            Ok(q) => q,
            Err(_) => Quota::disconnected(),
        };
        overlay::set_quota(quota);
        let hwnd = HWND(overlay_bits as *mut std::ffi::c_void);
        unsafe {
            let _ = RedrawWindow(hwnd, None, None, RDW_INVALIDATE | RDW_UPDATENOW);
        }
        // Clear the in-flight flag so the next timer tick can fetch again.
        REFRESH_IN_FLIGHT.store(false, Ordering::Release);
    });
}
