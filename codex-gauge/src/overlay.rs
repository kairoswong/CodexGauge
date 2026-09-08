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

/// Latest quota the overlay draws (mutated from the message-loop thread).
static QUOTA: std::sync::OnceLock<Arc<std::sync::Mutex<Quota>>> = std::sync::OnceLock::new();

/// GDI fonts, created once and cached so `WM_PAINT` (which fires often while
/// dragging) doesn't create and destroy an `HFONT` every time. Like the icon,
/// they are read-only and live for the whole process.
struct AppFont(HFONT);
unsafe impl Send for AppFont {}
unsafe impl Sync for AppFont {}

/// Small (-12, normal) for labels / reset line / brand mark.
static FONT_SMALL: std::sync::OnceLock<AppFont> = std::sync::OnceLock::new();
/// Big (-13, semibold) for the primary "used %" figures.
static FONT_BIG: std::sync::OnceLock<AppFont> = std::sync::OnceLock::new();

/// Cached font for (height, weight), created on first use and never freed.
fn cached_font(slot: &'static std::sync::OnceLock<AppFont>, height: i32, weight: i32) -> HFONT {
    slot.get_or_init(|| AppFont(create_font(height, weight))).0
}

pub fn set_quota(q: Quota) {
    if let Some(m) = QUOTA.get() {
        *m.lock().unwrap() = q;
    }
}

/// Create the overlay window at the bottom-right of the primary screen.
pub fn create(hinstance: HINSTANCE) -> Result<Overlay> {
    let _ = QUOTA.set(Arc::new(std::sync::Mutex::new(Quota::disconnected())));

    // Determine primary screen working area to place the widget.
    let screen_w = unsafe { GetSystemMetrics(SM_CXSCREEN) };
    let screen_h = unsafe { GetSystemMetrics(SM_CYSCREEN) };
    let w = 236i32;
    let h = 122i32;
    let x = screen_w - w - 50;
    let y = screen_h - h - 50;

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

    // Rounded corners (12px radius) for a softer floating look.
    let rgn = unsafe { CreateRoundRectRgn(0, 0, w + 1, h + 1, 24, 24) };
    if !rgn.is_invalid() {
        unsafe {
            let _ = SetWindowRgn(hwnd, rgn, TRUE);
        }
    }

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
            // Treat the whole client area as the title bar so the window can be
            // dragged by holding anywhere on it.
            LRESULT(HTCAPTION as isize)
        }
        WM_PAINT => {
            paint(hwnd);
            LRESULT(0)
        }
        WM_ERASEBKGND => LRESULT(1),
        WM_COMMAND => {
            // Tray menu items are delivered to the overlay (the menu owner);
            // forward them to the main message loop.
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

    // ---- Background: deep charcoal, slightly rounded via window region.
    let bg = rgb(24, 24, 30);
    unsafe { SetBkColor(hdc, bg) };
    unsafe {
        SelectObject(hdc, GetStockObject(DC_BRUSH));
    }
    unsafe { SetDCBrushColor(hdc, bg) };
    unsafe { let _ = Rectangle(hdc, 0, 0, w, h); };

    // Cached fonts (see above); small for labels, big (bolder) for the figures.
    let small_font = cached_font(&FONT_SMALL, -12, 400);
    let big_font = cached_font(&FONT_BIG, -13, 550);
    let small_old = unsafe { SelectObject(hdc, small_font) };
    unsafe { SetBkMode(hdc, TRANSPARENT) };

    let left = 14i32;
    let right = w - 14;

    // Keep the quota layout visible while the first app-server request is in
    // flight. Placeholder rows make the initial state look like an empty
    // gauge instead of a separate connection screen.
    let rows: Vec<(String, f64)> = vec![
        (
            quota.primary.as_ref().map(|win| win.label()).unwrap_or_else(|| "5h".to_string()),
            quota.primary.as_ref().map(|win| win.used_percent).unwrap_or(0.0),
        ),
        (
            quota.secondary.as_ref().map(|win| win.label()).unwrap_or_else(|| "1w".to_string()),
            quota.secondary.as_ref().map(|win| win.used_percent).unwrap_or(0.0),
        ),
    ];

    // Two stacked rows: [5h] [========================] [12%]
    //                   [1w] [========================] [45%]
    let row_top = 12i32;
    let row_gap = 34i32;

    for (i, (label, raw_used)) in rows.iter().enumerate() {
        let used = raw_used.clamp(0.0, 100.0);
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
    let second = if quota.connected {
        quota
            .next_reset()
            .map(|win| format_reset(win.resets_at))
            .unwrap_or_else(|| "Reset unavailable".to_string())
    } else {
        "Connecting...".to_string()
    };
    draw_text(hdc, left, 98, &second);

    // Brand mark in the bottom-right corner: "Codex".
    let brand = "Codex";
    let bw = text_width(hdc, brand);
    unsafe { SetTextColor(hdc, rgb(120, 124, 138)) };
    draw_text(hdc, right - bw, 98, brand);

    unsafe { SelectObject(hdc, small_old) };

    unsafe { let _ = EndPaint(hwnd, &ps); };
}

/// Create a Segoe UI font at the given (negative) pixel height and weight.
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
