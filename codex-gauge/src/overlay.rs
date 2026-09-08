//! Floating semi-transparent overlay window drawn with GDI.
//! Borderless, always-on-top, not shown in taskbar.

use std::sync::Arc;

use windows::core::*;
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::quota::Quota;
use crate::tray;

pub const WM_APP_REFRESH: u32 = WM_APP + 1;
pub const WM_APP_QUIT: u32 = WM_APP + 2;

pub struct Overlay {
    pub hwnd: HWND,
}

pub fn register_window_class(hinstance: HINSTANCE) -> Result<()> {
    let class_name = w!("CodexGaugeOverlay");
    let wc = WNDCLASSW {
        lpfnWndProc: Some(overlay_wndproc),
        hInstance: hinstance,
        lpszClassName: class_name,
        hCursor: unsafe {
            LoadCursorW(None, IDC_ARROW).unwrap_or_default()
        },
        ..Default::default()
    };
    unsafe {
        RegisterClassW(&wc);
    }
    Ok(())
}

/// The latest quota the overlay should draw. Mutated from the message loop thread.
static QUOTA: std::sync::OnceLock<Arc<std::sync::Mutex<Quota>>> = std::sync::OnceLock::new();

/// The embedded app icon (resource ID 1), loaded once and drawn in the offline
/// state so the widget is recognisable while it is still connecting.
///
/// `HICON` is a raw pointer and not `Send + Sync`, but this icon is created by
/// the UI thread, drawn read-only via GDI, and never mutated or freed, so
/// sharing it behind a `static` is sound.
struct AppIcon(HICON);
// SAFETY: the icon is created once, accessed read-only for drawing, and lives
// for the whole process; it is never modified, freed, or ownership-transferred.
unsafe impl Send for AppIcon {}
unsafe impl Sync for AppIcon {}
static APP_ICON: std::sync::OnceLock<AppIcon> = std::sync::OnceLock::new();

pub fn set_quota(q: Quota) {
    if let Some(m) = QUOTA.get() {
        *m.lock().unwrap() = q;
    }
}

/// Create the overlay window at the top-right of the primary screen.
pub fn create(hinstance: HINSTANCE) -> Result<Overlay> {
    let _ = QUOTA.set(Arc::new(std::sync::Mutex::new(Quota::disconnected())));

    // Determine primary screen working area to place the widget.
    let sm = unsafe { GetSystemMetrics(SM_CXSCREEN) };
    let w = 236i32;
    let h = 122i32;
    let x = sm - w - 24;
    let y = 24;

    let ex_style = WS_EX_LAYERED | WS_EX_TOOLWINDOW | WS_EX_TOPMOST;
    let style = WS_POPUP;

    let hwnd = unsafe {
        CreateWindowExW(
            ex_style,
            w!("CodexGaugeOverlay"),
            w!("CodexGauge"),
            style,
            x,
            y,
            w,
            h,
            HWND::default(),
            HMENU::default(),
            hinstance,
            None,
        )
    }?;

    // Semi-transparency for the whole window.
    unsafe {
        let _ = SetLayeredWindowAttributes(hwnd, COLORREF(0), 235, LWA_ALPHA);
    }

    // Rounded corners (12px) for a softer, more modern floating look.
    let rgn = unsafe { CreateRoundRectRgn(0, 0, w + 1, h + 1, 24, 24) };
    if !rgn.is_invalid() {
        unsafe {
            let _ = SetWindowRgn(hwnd, rgn, TRUE);
        }
    }

    // Load the embedded app icon once so the offline state can draw it. The
    // window procedure has no hinstance, so cache it here at creation time.
    let _ = APP_ICON.get_or_init(|| {
        AppIcon(unsafe { LoadIconW(hinstance, PCWSTR(1 as *const u16)) }.unwrap_or_default())
    });

    Ok(Overlay { hwnd })
}

extern "system" fn overlay_wndproc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_NCHITTEST => {
            // Report the entire client area as the title bar so the user can
            // drag the borderless window by holding anywhere on it.
            LRESULT(HTCAPTION as isize)
        }
        WM_PAINT => {
            paint(hwnd);
            LRESULT(0)
        }
        WM_ERASEBKGND => LRESULT(1),
        WM_COMMAND => {
            // Tray context-menu items are delivered to the overlay (the menu
            // owner). Forward them to the main message loop for handling.
            let cmd = wparam.0 as u32 & 0xffff;
            if cmd == tray::CMD_REFRESH {
                unsafe { let _ = PostMessageW(hwnd, WM_APP_REFRESH, WPARAM(0), LPARAM(0)); };
            } else if cmd == tray::CMD_EXIT {
                unsafe { let _ = PostMessageW(hwnd, WM_APP_QUIT, WPARAM(0), LPARAM(0)); };
            }
            LRESULT(0)
        }
        WM_DESTROY => {
            unsafe { PostQuitMessage(0) };
            LRESULT(0)
        }
        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}

fn paint(hwnd: HWND) {
    let mut ps = PAINTSTRUCT::default();
    let hdc = unsafe { BeginPaint(hwnd, &mut ps) };
    if hdc.is_invalid() {
        return;
    }

    let (w, h) = {
        let mut r = RECT::default();
        unsafe { let _ = GetClientRect(hwnd, &mut r); };
        (r.right - r.left, r.bottom - r.top)
    };

    let quota = QUOTA.get().map(|m| m.lock().unwrap().clone()).unwrap_or_default();

    // ---- Background: deep charcoal, slightly rounded feel via window region.
    let bg = rgb(24, 24, 30);
    unsafe { SetBkColor(hdc, bg) };
    unsafe {
        SelectObject(hdc, GetStockObject(DC_BRUSH));
    }
    unsafe { SetDCBrushColor(hdc, bg) };
    unsafe { let _ = Rectangle(hdc, 0, 0, w, h); };

    // Two fonts for visual hierarchy: a small one for labels/secondary text,
    // and a larger one for the primary "used %" figures.
    let small_font = create_font(-12, 400);
    let big_font = create_font(-13, 550); // slightly bolder, primary figures
    let small_old = unsafe { SelectObject(hdc, small_font) };
    unsafe { SetBkMode(hdc, TRANSPARENT) };

    let left = 14i32;
    let right = w - 14;

    if quota.connected {
        // Two stacked rows: [5h] [========================] [12%]
        //                   [1w] [========================] [45%]
        let row_top = 12i32;
        let row_gap = 34i32;
        let wins: Vec<&crate::quota::Window> = quota
            .primary
            .iter()
            .chain(quota.secondary.iter())
            .collect();

        for (i, win) in wins.iter().enumerate() {
            let used = win.used_percent.clamp(0.0, 100.0);
            let label = win.label();
            let used_txt = format!("{}%", used.round() as i32);
            let y = row_top + i as i32 * row_gap;

            // Label (small, muted) anchored left.
            unsafe { SetTextColor(hdc, rgb(150, 152, 162)) };
            draw_text(hdc, left, y, &label);

            // Primary figure (big, bright, follows status color) anchored right.
            let (r, g, b) = color_for(used);
            unsafe { SelectObject(hdc, big_font) };
            unsafe { SetTextColor(hdc, rgb(r, g, b)) };
            let w_used = text_width(hdc, &used_txt);
            unsafe { let _ = TextOutW(hdc, right - w_used, y - 2, &to_wide(&used_txt)); };
            unsafe { SelectObject(hdc, small_font) };

            // Progress bar (below the text row): full-width track.
            let bar_y = y + 20;
            let bar_h = 6i32;
            let track = rgb(57, 59, 70);
            unsafe {
                SetDCBrushColor(hdc, track);
                let _ = Rectangle(hdc, left, bar_y, right, bar_y + bar_h);
            }
            let fill_w = ((right - left) as f64 * (used / 100.0)).round() as i32;
            if fill_w > 0 {
                unsafe {
                    SetDCBrushColor(hdc, rgb(r, g, b));
                    let _ = Rectangle(hdc, left, bar_y, left + fill_w, bar_y + bar_h);
                }
            }
        }

        // Thin divider separating the figures from the reset line.
        let div_y = 92i32;
        unsafe { SetDCBrushColor(hdc, rgb(43, 45, 54)) };
        unsafe { let _ = Rectangle(hdc, left, div_y, right, div_y + 1); };

        // Reset line (small, faint) anchored left under the divider.
        unsafe { SetTextColor(hdc, rgb(136, 139, 150)) };
        let second = match quota.next_reset() {
            Some(win) => format_reset(win.resets_at),
            None => "Connected".to_string(),
        };
        draw_text(hdc, left, 98, &second);

        // Brand mark in the bottom-right corner: "Codex".
        let brand = "Codex";
        let bw = text_width(hdc, brand);
        unsafe { SetTextColor(hdc, rgb(120, 124, 138)) };
        draw_text(hdc, right - bw, 98, brand);
    } else {
        // Not connected: show the app icon and a larger terse centered line so
        // the widget reads clearly while it is still connecting.
        let icon = APP_ICON.get().map(|a| a.0).unwrap_or(HICON::default());
        if !icon.is_invalid() {
            let icon_size = 32i32;
            let ix = (w - icon_size) / 2;
            let iy = 32;
            unsafe {
                let _ = DrawIconEx(
                    hdc,
                    ix,
                    iy,
                    icon,
                    icon_size,
                    icon_size,
                    0,
                    HBRUSH::default(),
                    DI_NORMAL,
                );
            }
        }
        let offline_font = create_font(-16, 550);
        unsafe { SetTextColor(hdc, rgb(170, 172, 182)) };
        draw_centered(hdc, w, 68, "Connecting...", offline_font);
        unsafe { let _ = DeleteObject(offline_font); };
    }

    unsafe { SelectObject(hdc, small_old) };
    unsafe { let _ = DeleteObject(big_font); };
    unsafe { let _ = DeleteObject(small_font); };

    unsafe { let _ = EndPaint(hwnd, &ps); };
}

/// Create a Segoe UI font at the given pixel height (negative = character height)
/// and weight.
fn create_font(height: i32, weight: i32) -> HFONT {
    unsafe {
        CreateFontW(
            height, 0, 0, 0, weight, 0, 0, 0, 1, // DEFAULT_CHARSET
            0, 0, 5, // CLEARTYPE_QUALITY
            0, w!("Segoe UI"),
        )
    }
}

fn to_wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

/// Draw a string at (x, y) with the currently selected font/color.
fn draw_text(hdc: HDC, x: i32, y: i32, s: &str) {
    unsafe { let _ = TextOutW(hdc, x, y, &to_wide(s)); };
}

/// Measure the on-screen width (px) of `s` with the current font. Does not draw.
fn text_width(hdc: HDC, s: &str) -> i32 {
    let wide = to_wide(s);
    unsafe {
        let mut sz = SIZE::default();
        let _ = GetTextExtentPoint32W(hdc, &wide, &mut sz);
        sz.cx
    }
}

/// Draw text horizontally centered across the window width.
fn draw_centered(hdc: HDC, w: i32, y: i32, s: &str, font: HFONT) {
    unsafe { SelectObject(hdc, font) };
    let wide = to_wide(s);
    let width = unsafe {
        let mut sz = SIZE::default();
        let _ = GetTextExtentPoint32W(hdc, &wide, &mut sz);
        sz.cx
    };
    unsafe { let _ = TextOutW(hdc, (w - width) / 2, y, &wide); };
}

/// Build a GDI COLORREF from r/g/b (0-255).
fn rgb(r: u8, g: u8, b: u8) -> COLORREF {
    COLORREF((r as u32) | ((g as u32) << 8) | ((b as u32) << 16))
}

fn color_for(used: f64) -> (u8, u8, u8) {
    if used < 60.0 {
        (76, 175, 80) // green
    } else if used < 85.0 {
        (255, 193, 7) // amber
    } else {
        (229, 57, 53) // red
    }
}

fn format_reset(epoch: i64) -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let secs = (epoch - now).max(0);
    if secs <= 0 {
        return "resetting soon".to_string();
    }
    let mins = secs / 60;
    let h = mins / 60;
    let m = mins % 60;
    if h > 0 {
        format!("resets in {h}h {m}m")
    } else {
        format!("resets in {m}m")
    }
}
