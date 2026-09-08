//! System tray icon + context menu (refresh / exit).

use windows::core::*;
use windows::Win32::Foundation::*;
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::Win32::UI::Shell::*;

pub const WM_TRAY: u32 = WM_APP + 10;
pub const CMD_REFRESH: u32 = 1001;
pub const CMD_EXIT: u32 = 1002;

pub fn add(hwnd: HWND, hinstance: HINSTANCE) -> Result<()> {
    let mut nd = NOTIFYICONDATAW {
        cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: hwnd,
        uID: 1,
        uFlags: NIF_ICON | NIF_MESSAGE | NIF_TIP,
        uCallbackMessage: WM_TRAY,
        ..Default::default()
    };

    // Load the embedded app icon (resource ID 1); fall back to the generic
    // application icon if missing.
    let icon = unsafe { LoadIconW(hinstance, PCWSTR(1 as *const u16)) }.unwrap_or_default();
    nd.hIcon = if !icon.is_invalid() {
        icon
    } else {
        unsafe { LoadIconW(None, IDI_APPLICATION) }.unwrap_or_default()
    };

    // Tooltip
    let tip: Vec<u16> = "Codex Usage".encode_utf16().chain(Some(0)).collect();
    let mut i = 0;
    for &u in tip.iter() {
        if i >= nd.szTip.len() - 1 {
            break;
        }
        nd.szTip[i] = u;
        i += 1;
    }

    unsafe {
        let _ = Shell_NotifyIconW(NIM_ADD, &nd);
    }
    Ok(())
}

pub fn remove(hwnd: HWND) {
    let nd = NOTIFYICONDATAW {
        cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: hwnd,
        uID: 1,
        ..Default::default()
    };
    unsafe {
        let _ = Shell_NotifyIconW(NIM_DELETE, &nd);
    }
}

/// Build and show the right-click context menu. `hwnd` should be visible (the
/// overlay) so the popup reliably takes the foreground.
pub fn show_menu(hwnd: HWND) {
    let hmenu = match unsafe { CreatePopupMenu() } {
        Ok(m) => m,
        Err(_) => return,
    };
    unsafe {
        let _ = AppendMenuW(hmenu, MF_STRING, CMD_REFRESH as usize, w!("Refresh"));
        let _ = AppendMenuW(hmenu, MF_STRING, CMD_EXIT as usize, w!("Exit"));
    }
    let mut pt = POINT::default();
    unsafe { let _ = GetCursorPos(&mut pt); };
    unsafe {
        let _ = SetForegroundWindow(hwnd);
        // No TPM_RETURNCMD: the click is delivered to the owner (overlay) as a
        // WM_COMMAND, which overlay_wndproc forwards to the main message loop.
        let _ = TrackPopupMenu(
            hmenu,
            TPM_LEFTALIGN | TPM_RIGHTBUTTON,
            pt.x,
            pt.y,
            0,
            hwnd,
            None,
        );
        let _ = PostMessageW(hwnd, WM_NULL, WPARAM(0), LPARAM(0));
        let _ = DestroyMenu(hmenu);
    }
}
