//! Tray context menu: a compact, rounded card drawn in its own popup window.
//!
//! `TrackPopupMenu` cannot round its corners — it is a plain GDI menu window —
//! so the menu is now a `WS_POPUP` window created here, with the card, rows,
//! icons and separators painted by `WM_PAINT`. The popup runs its own modal
//! message loop (the same technique `TrackPopupMenu` used), so the hidden
//! tray window still blocks for the lifetime of the menu and returns the
//! chosen command synchronously, keeping `tray.rs` / `app.rs` dispatch intact.
//!
//! Corners: Windows 11 rounds the window through
//! `DWMWA_WINDOW_CORNER_PREFERENCE`, which also keeps the system drop shadow.
//! Older builds fall back to a rounded window region plus `CS_DROPSHADOW`.
//!
//! The menu is a plain data structure ([`MenuItem`]) so its order, separators
//! and the disabled version row stay unit-testable without a GUI. Colours come
//! from the gpui-kit semantic tokens for the persisted light/dark appearance,
//! never from hardcoded literals, so icons/text stay readable in both themes.

use std::mem::size_of;
use std::sync::Once;

use windows::core::PCWSTR;
use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Dwm::{
    DwmExtendFrameIntoClientArea, DwmSetWindowAttribute, DWMWA_BORDER_COLOR, DWMWA_COLOR_NONE,
    DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_ROUND, DWM_WINDOW_CORNER_PREFERENCE,
};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, CreateFontW, CreatePen, CreateRoundRectRgn, CreateSolidBrush, DeleteObject,
    DrawTextW, Ellipse, EndPaint, FillRect, GetMonitorInfoW, GetStockObject, InvalidateRect,
    LineTo, MonitorFromPoint, MoveToEx, Rectangle, RoundRect, SelectObject, SetBkMode,
    SetTextColor, SetWindowRgn, UpdateWindow, CLEARTYPE_QUALITY, CLIP_DEFAULT_PRECIS,
    DEFAULT_CHARSET, DT_CALCRECT, DT_LEFT, DT_NOPREFIX, DT_RIGHT, DT_SINGLELINE, DT_VCENTER, HDC,
    HFONT, MONITORINFO, MONITOR_DEFAULTTONEAREST, NULL_BRUSH, OUT_DEFAULT_PRECIS, PAINTSTRUCT,
    PS_SOLID, TRANSPARENT,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Controls::{MARGINS, WM_MOUSELEAVE};
use windows::Win32::UI::HiDpi::{GetDpiForMonitor, GetDpiForWindow, MDT_EFFECTIVE_DPI};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    ReleaseCapture, SetCapture, SetFocus, TrackMouseEvent, TME_LEAVE, TRACKMOUSEEVENT, VK_DOWN,
    VK_ESCAPE, VK_RETURN, VK_UP,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetClientRect, GetCursorPos,
    GetMessageW, GetWindowLongPtrW, IsWindow, LoadCursorW, PostQuitMessage, RegisterClassExW,
    SetForegroundWindow, SetWindowLongPtrW, ShowWindow, TranslateMessage, CS_DROPSHADOW,
    CS_HREDRAW, CS_VREDRAW, GWLP_USERDATA, IDC_ARROW, MSG, SW_SHOW, WA_ACTIVE, WA_CLICKACTIVE,
    WA_INACTIVE, WM_ACTIVATE, WM_CAPTURECHANGED, WM_ERASEBKGND, WM_KEYDOWN, WM_LBUTTONDOWN,
    WM_LBUTTONUP, WM_MOUSEMOVE, WM_PAINT, WM_RBUTTONUP, WNDCLASSEXW, WS_EX_TOOLWINDOW,
    WS_EX_TOPMOST, WS_POPUP,
};

// Command ids. The original numbers are kept so `tray.rs` / `app.rs` dispatch
// is unchanged; ID_CLOSE_ALL wires the reference's 关闭所有贴图 row to the
// existing "关闭全部钉图" action.
pub(crate) const ID_REGION: u32 = 1;
pub(crate) const ID_FULL: u32 = 2;
pub(crate) const ID_COPY: u32 = 3;
pub(crate) const ID_SETTINGS: u32 = 4;
pub(crate) const ID_CLOSE_ALL: u32 = 5;
pub(crate) const ID_EXIT: u32 = 9;

/// Left-hand line icons. `TOOLBAR_ICONS` / `assets.rs` are SVG paths consumed
/// by GPUI, not GDI bitmaps, so the menu draws its own outline icons instead.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MenuIcon {
    None,
    Scissors,
    Monitor,
    Copy,
    Close,
    Gear,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MenuKind {
    Action,
    Separator,
    Disabled,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MenuItem {
    pub id: u32,
    pub kind: MenuKind,
    pub icon: MenuIcon,
    pub label: String,
    pub shortcut: String,
}

fn action(id: u32, icon: MenuIcon, label: &str, shortcut: &str) -> MenuItem {
    MenuItem {
        id,
        kind: MenuKind::Action,
        icon,
        label: label.into(),
        shortcut: shortcut.into(),
    }
}

fn separator() -> MenuItem {
    MenuItem {
        id: 0,
        kind: MenuKind::Separator,
        icon: MenuIcon::None,
        label: String::new(),
        shortcut: String::new(),
    }
}

fn version_row() -> MenuItem {
    MenuItem {
        id: 0,
        kind: MenuKind::Disabled,
        icon: MenuIcon::None,
        label: format!("FastPin v{}", env!("CARGO_PKG_VERSION")),
        shortcut: String::new(),
    }
}

/// Builds the menu spec from the live hotkey strings and pin count. Pure: no
/// Win32 / GPUI calls, so order, grouping and the disabled row are testable.
pub fn build(region: &str, fullscreen: &str, copy_only: &str, pin_count: usize) -> Vec<MenuItem> {
    vec![
        // Group 1: capture + pin management. FastPin has three capture modes
        // (no single 截屏 action), so all three keep their existing labels and
        // click behaviour, each with its own registered shortcut.
        action(ID_REGION, MenuIcon::Scissors, "区域截图", region),
        action(ID_FULL, MenuIcon::Monitor, "全屏截图", fullscreen),
        action(ID_COPY, MenuIcon::Copy, "仅复制", copy_only),
        action(ID_CLOSE_ALL, MenuIcon::Close, "关闭所有贴图", &pin_count.to_string()),
        separator(),
        action(ID_SETTINGS, MenuIcon::Gear, "设置", ""),
        separator(),
        action(ID_EXIT, MenuIcon::Close, "退出程序", ""),
        separator(),
        version_row(),
    ]
}

// ---- compact metrics -------------------------------------------------------
//
// Logical (96-DPI) pixels: 12px text, 32px rows, 12px side padding, 9px
// separator inset, 14px icons. `Metrics` is pure so the values can be
// unit-tested; [`scaled`] applies the monitor DPI at paint and hit-test time
// exactly once, the way the old owner-draw menu did.

#[derive(Clone, Copy, Debug)]
struct Metrics {
    font: f32,
    card_w: f32,
    row_h: f32,
    sep_h: f32,
    version_h: f32,
    pad: f32,
    pad_y: f32,
    icon_box: f32,
    icon_gap: f32,
    text_gap: f32,
    sep_inset: f32,
    radius: f32,
}

impl Metrics {
    /// The theme body size is 16px; the menu runs four steps below it so the
    /// card stays dense. The clamp keeps it in an 11-13px band: smaller than
    /// 11px clips the CJK glyphs, larger than 13px stops being a menu.
    fn from_font(theme_font: f32) -> Self {
        Self {
            font: (theme_font - 4.0).clamp(11.0, 13.0),
            card_w: 240.0,
            row_h: 32.0,
            sep_h: 8.0,
            version_h: 28.0,
            pad: 12.0,
            pad_y: 4.0,
            icon_box: 14.0,
            icon_gap: 8.0,
            text_gap: 8.0,
            sep_inset: 9.0,
            radius: 10.0,
        }
    }

    fn from_theme() -> Self {
        Self::from_font(crate::overlay::theme::font_size().into())
    }

    fn kind_height(&self, kind: MenuKind) -> f32 {
        match kind {
            MenuKind::Action => self.row_h,
            MenuKind::Separator => self.sep_h,
            MenuKind::Disabled => self.version_h,
        }
    }

    /// Full card height: every row plus the top/bottom inner padding.
    fn height(&self, items: &[MenuItem]) -> f32 {
        self.pad_y * 2.0 + items.iter().map(|it| self.kind_height(it.kind)).sum::<f32>()
    }
}

/// Rounds one logical length to physical pixels for the current monitor.
fn scaled(value: f32, scale: f32) -> i32 {
    (value * scale).round() as i32
}

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

// ---- semantic colours -----------------------------------------------------

#[derive(Clone, Copy)]
struct MenuColors {
    bg: u32,
    hover: u32,
    text: u32,
    shortcut: u32,
    disabled: u32,
    separator: u32,
}

/// Pack an `Hsla` into a GDI `COLORREF` (`0x00BBGGRR`). GDI has no alpha, so
/// alpha-bearing tokens are simply read at their RGB value.
fn colorref(c: gpui::Hsla) -> u32 {
    let r = gpui::Rgba::from(c);
    let byte = |v: f32| (v.clamp(0.0, 1.0) * 255.0).round() as u32;
    byte(r.r) | (byte(r.g) << 8) | (byte(r.b) << 16)
}

fn menu_colors() -> MenuColors {
    use crate::overlay::theme;
    MenuColors {
        bg: colorref(theme::surface_solid()),
        hover: colorref(theme::elevated_surface()),
        text: colorref(theme::primary_fg()),
        shortcut: colorref(theme::secondary_fg()),
        disabled: colorref(theme::disabled_fg()),
        separator: colorref(theme::border()),
    }
}

// ---- live popup -----------------------------------------------------------

/// Everything the popup needs while it is on screen. Stored behind
/// `GWLP_USERDATA` so the window procedure can reach it without a thread-local
/// borrow that a re-entrant message would panic on.
struct PopupState {
    items: Vec<MenuItem>,
    colors: MenuColors,
    metrics: Metrics,
    scale: f32,
    font: HFONT,
    hover: Option<usize>,
    chosen: Option<u32>,
    activated: bool,
    leave_tracked: bool,
    closing: bool,
}

const POPUP_CLASS: &str = "FastPinTrayPopup";

fn register_class() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| unsafe {
        let name = wide(POPUP_CLASS);
        let wc = WNDCLASSEXW {
            cbSize: size_of::<WNDCLASSEXW>() as u32,
            // DROPSHADOW is the fallback outline on Windows 10; Windows 11
            // rounds the window through DWM and keeps the system shadow.
            style: CS_HREDRAW | CS_VREDRAW | CS_DROPSHADOW,
            lpfnWndProc: Some(popup_proc),
            hInstance: GetModuleHandleW(None).ok().map(Into::into).unwrap_or_default(),
            hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
            lpszClassName: PCWSTR(name.as_ptr()),
            ..Default::default()
        };
        let _ = RegisterClassExW(&wc);
    });
}

fn make_font(font_px: f32) -> HFONT {
    let height = -(font_px.round() as i32);
    let face: Vec<u16> = "Microsoft YaHei UI\0".encode_utf16().collect();
    unsafe {
        CreateFontW(
            height,
            0,
            0,
            0,
            400,
            0,
            0,
            0,
            DEFAULT_CHARSET,
            OUT_DEFAULT_PRECIS,
            CLIP_DEFAULT_PRECIS,
            CLEARTYPE_QUALITY,
            0,
            PCWSTR(face.as_ptr()),
        )
    }
}

/// Monitor DPI under the cursor, so a tray on a scaled display still draws a
/// card of the size the reference shows. Falls back to the tray window's DPI
/// when `GetDpiForMonitor` is unavailable.
fn dpi_scale(hwnd: HWND, pt: POINT) -> f32 {
    unsafe {
        let monitor = MonitorFromPoint(pt, MONITOR_DEFAULTTONEAREST);
        let mut dx = 96u32;
        let mut dy = 96u32;
        if GetDpiForMonitor(monitor, MDT_EFFECTIVE_DPI, &mut dx, &mut dy).is_ok() && dx > 0 {
            return dx as f32 / 96.0;
        }
        let dpi = GetDpiForWindow(hwnd);
        if dpi == 0 { 1.0 } else { dpi as f32 / 96.0 }
    }
}

/// Cursor position clamped into the monitor work area, flipping left/up when
/// the card would run past the right/bottom edge (the tray lives there).
fn place(pt: POINT, w: i32, h: i32) -> (i32, i32) {
    unsafe {
        let monitor = MonitorFromPoint(pt, MONITOR_DEFAULTTONEAREST);
        let mut info = MONITORINFO {
            cbSize: size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        if GetMonitorInfoW(monitor, &mut info).as_bool() {
            let work = info.rcWork;
            let mut x = if pt.x + w > work.right { work.right - w } else { pt.x };
            let mut y = if pt.y + h > work.bottom { pt.y - h } else { pt.y };
            x = x.max(work.left);
            y = y.max(work.top);
            return (x, y);
        }
    }
    (pt.x, pt.y)
}

/// Real rounded corners: DWM on Windows 11, a rounded region elsewhere.
unsafe fn apply_rounding(hwnd: HWND, w: i32, h: i32, scale: f32) {
    let preference: DWM_WINDOW_CORNER_PREFERENCE = DWMWCP_ROUND;
    let rounded = DwmSetWindowAttribute(
        hwnd,
        DWMWA_WINDOW_CORNER_PREFERENCE,
        &preference as *const _ as _,
        size_of::<DWM_WINDOW_CORNER_PREFERENCE>() as u32,
    )
    .is_ok();

    if rounded {
        // A one-pixel extended frame is what asks DWM for the system drop
        // shadow (and lets it clip the card's rounded corners); the hairline
        // visible in the reference is painted by us instead.
        let margins = MARGINS {
            cxLeftWidth: 1,
            cxRightWidth: 1,
            cyTopHeight: 1,
            cyBottomHeight: 1,
        };
        let _ = DwmExtendFrameIntoClientArea(hwnd, &margins);
        let none = DWMWA_COLOR_NONE;
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_BORDER_COLOR,
            &none as *const _ as _,
            size_of::<u32>() as u32,
        );
        return;
    }

    let radius = (10.0 * scale).round() as i32;
    let region = CreateRoundRectRgn(0, 0, w + 1, h + 1, radius * 2, radius * 2);
    if !region.is_invalid() && SetWindowRgn(hwnd, Some(region), true) == 0 {
        let _ = DeleteObject(region.into());
    }
}

/// Shows the compact card at the cursor and returns the chosen command id
/// (None when dismissed). The popup runs its own message loop, so this still
/// returns synchronously, exactly like the previous `TrackPopupMenu` call.
pub(crate) fn track(hwnd: HWND, items: Vec<MenuItem>) -> Option<u32> {
    if items.is_empty() {
        return None;
    }
    register_class();

    let metrics = Metrics::from_theme();
    let colors = menu_colors();
    let mut pt = POINT::default();
    unsafe {
        let _ = GetCursorPos(&mut pt);
    }
    let scale = dpi_scale(hwnd, pt);
    let font = make_font(metrics.font * scale);

    let w = scaled(metrics.card_w, scale);
    let h = scaled(metrics.height(&items), scale);
    let (x, y) = place(pt, w, h);

    let class = wide(POPUP_CLASS);
    let title = wide("");
    let popup = unsafe {
        CreateWindowExW(
            WS_EX_TOOLWINDOW | WS_EX_TOPMOST,
            PCWSTR(class.as_ptr()),
            PCWSTR(title.as_ptr()),
            WS_POPUP,
            x,
            y,
            w,
            h,
            None,
            None,
            GetModuleHandleW(None).ok().map(Into::into),
            None,
        )
    };
    let Ok(popup) = popup else {
        unsafe {
            let _ = DeleteObject(font.into());
        }
        return None;
    };

    let state = Box::into_raw(Box::new(PopupState {
        items,
        colors,
        metrics,
        scale,
        font,
        hover: None,
        chosen: None,
        activated: false,
        leave_tracked: false,
        closing: false,
    }));

    unsafe {
        SetWindowLongPtrW(popup, GWLP_USERDATA, state as isize);
        apply_rounding(popup, w, h, scale);
        let _ = ShowWindow(popup, SW_SHOW);
        let _ = SetForegroundWindow(popup);
        let _ = SetFocus(Some(popup));
        // Capture guarantees the first click anywhere reaches us even when the
        // foreground lock refuses `SetForegroundWindow` from a tray click.
        let _ = SetCapture(popup);
        let _ = UpdateWindow(popup);
    }

    let mut msg = MSG::default();
    loop {
        let ret = unsafe { GetMessageW(&mut msg, None, 0, 0) };
        if ret.0 == 0 {
            // Preserve a quit request for the application's own loop.
            unsafe { PostQuitMessage(msg.wParam.0 as i32) };
            break;
        }
        if ret.0 == -1 {
            break;
        }
        unsafe {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        if !unsafe { IsWindow(Some(popup)).as_bool() } {
            break;
        }
    }

    unsafe {
        let _ = ReleaseCapture();
        if IsWindow(Some(popup)).as_bool() {
            let _ = DestroyWindow(popup);
        }
        let state = Box::from_raw(state);
        let chosen = state.chosen;
        let _ = DeleteObject(state.font.into());
        chosen
    }
}

// ---- window procedure -----------------------------------------------------

unsafe extern "system" fn popup_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut PopupState;
    let Some(state) = ptr.as_mut() else {
        return DefWindowProcW(hwnd, msg, wparam, lparam);
    };

    match msg {
        WM_PAINT => {
            paint(hwnd, state);
            LRESULT(0)
        }
        WM_ERASEBKGND => LRESULT(1),
        WM_MOUSEMOVE => {
            track_leave(hwnd, state);
            let (x, y) = mouse_xy(lparam);
            let hit = state.hit_test(x, y);
            if hit != state.hover {
                state.hover = hit;
                let _ = InvalidateRect(Some(hwnd), None, false);
            }
            LRESULT(0)
        }
        WM_MOUSELEAVE => {
            state.leave_tracked = false;
            if state.hover.take().is_some() {
                let _ = InvalidateRect(Some(hwnd), None, false);
            }
            LRESULT(0)
        }
        WM_LBUTTONUP => {
            let (x, y) = mouse_xy(lparam);
            if let Some(index) = state.hit_test(x, y) {
                state.chosen = Some(state.items[index].id);
            }
            close(hwnd, state);
            LRESULT(0)
        }
        WM_LBUTTONDOWN | WM_RBUTTONUP => {
            // A click outside the card (capture still delivers it) dismisses;
            // a press on a separator or the disabled version row does nothing.
            let (_, y) = mouse_xy(lparam);
            if msg == WM_RBUTTONUP || !state.inside(y) {
                close(hwnd, state);
            }
            LRESULT(0)
        }
        WM_ACTIVATE => {
            match wparam.0 as u32 & 0xFFFF {
                // Ignore the initial inactive notification Windows sends while
                // the window is being shown; only a real deactivation closes.
                WA_INACTIVE => {
                    if state.activated {
                        close(hwnd, state);
                    }
                }
                WA_ACTIVE | WA_CLICKACTIVE => state.activated = true,
                _ => {}
            }
            LRESULT(0)
        }
        WM_CAPTURECHANGED => {
            // Another window took the mouse (clicked elsewhere): dismiss.
            let gained = HWND(lparam.0 as *mut core::ffi::c_void);
            if state.activated && gained != hwnd {
                close(hwnd, state);
            }
            LRESULT(0)
        }
        WM_KEYDOWN => {
            match wparam.0 as u32 {
                v if v == VK_ESCAPE.0 as u32 => close(hwnd, state),
                v if v == VK_DOWN.0 as u32 => {
                    state.hover = state.next_action(state.hover, 1);
                    let _ = InvalidateRect(Some(hwnd), None, false);
                }
                v if v == VK_UP.0 as u32 => {
                    state.hover = state.next_action(state.hover, -1);
                    let _ = InvalidateRect(Some(hwnd), None, false);
                }
                v if v == VK_RETURN.0 as u32 => {
                    if let Some(index) = state.hover {
                        state.chosen = Some(state.items[index].id);
                        close(hwnd, state);
                    }
                }
                _ => {}
            }
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

unsafe fn close(hwnd: HWND, state: &mut PopupState) {
    if state.closing {
        return;
    }
    state.closing = true;
    let _ = ReleaseCapture();
    let _ = DestroyWindow(hwnd);
}

fn mouse_xy(lparam: LPARAM) -> (i32, i32) {
    let x = (lparam.0 & 0xFFFF) as u16 as i16 as i32;
    let y = ((lparam.0 >> 16) & 0xFFFF) as u16 as i16 as i32;
    (x, y)
}

fn track_leave(hwnd: HWND, state: &mut PopupState) {
    if state.leave_tracked {
        return;
    }
    let mut tme = TRACKMOUSEEVENT {
        cbSize: size_of::<TRACKMOUSEEVENT>() as u32,
        dwFlags: TME_LEAVE,
        hwndTrack: hwnd,
        dwHoverTime: 0,
    };
    if unsafe { TrackMouseEvent(&mut tme) }.is_ok() {
        state.leave_tracked = true;
    }
}

impl PopupState {
    /// Row under the cursor, only for actionable rows. Padding, separators and
    /// the disabled version row are not clickable.
    fn hit_test(&self, _x: i32, y: i32) -> Option<usize> {
        let mut top = scaled(self.metrics.pad_y, self.scale);
        for (index, item) in self.items.iter().enumerate() {
            let height = scaled(self.metrics.kind_height(item.kind), self.scale);
            if y >= top && y < top + height {
                return (item.kind == MenuKind::Action).then_some(index);
            }
            top += height;
        }
        None
    }

    /// True anywhere inside the card's painted rows (padding included), so a
    /// press on a separator or the disabled version row is not a dismissal.
    fn inside(&self, y: i32) -> bool {
        let pad = scaled(self.metrics.pad_y, self.scale);
        y >= pad && y < scaled(self.metrics.height(&self.items), self.scale) - pad
    }

    /// Next actionable row in `direction`, wrapping — for Up/Down.
    fn next_action(&self, from: Option<usize>, direction: i32) -> Option<usize> {
        let actions: Vec<usize> = self
            .items
            .iter()
            .enumerate()
            .filter(|(_, item)| item.kind == MenuKind::Action)
            .map(|(index, _)| index)
            .collect();
        if actions.is_empty() {
            return None;
        }
        let current = from.and_then(|index| actions.iter().position(|a| *a == index));
        let next = match (current, direction.is_positive()) {
            (None, true) => 0,
            (None, false) => actions.len() - 1,
            (Some(pos), true) => (pos + 1) % actions.len(),
            (Some(pos), false) => (pos + actions.len() - 1) % actions.len(),
        };
        Some(actions[next])
    }
}

// ---- painting -------------------------------------------------------------

unsafe fn paint(hwnd: HWND, state: &PopupState) {
    let mut ps = PAINTSTRUCT::default();
    let hdc = BeginPaint(hwnd, &mut ps);
    let mut rc = RECT::default();
    let _ = GetClientRect(hwnd, &mut rc);
    let s = state.scale;
    let m = state.metrics;
    let c = state.colors;

    // Card fill. Corners are clipped by DWM or the window region, so a plain
    // fill is enough; the hairline border is drawn on top.
    let brush = CreateSolidBrush(COLORREF(c.bg));
    let _ = FillRect(hdc, &rc, brush);
    let _ = DeleteObject(brush.into());

    let radius = scaled(m.radius, s);
    let border = CreatePen(PS_SOLID, 1, COLORREF(c.separator));
    let old_pen = SelectObject(hdc, border.into());
    let old_brush = SelectObject(hdc, GetStockObject(NULL_BRUSH));
    // RECT's right/bottom are exclusive, so stop one pixel short or the
    // right/bottom hairline falls outside the client area.
    let _ = RoundRect(hdc, 0, 0, rc.right - 1, rc.bottom - 1, radius * 2, radius * 2);
    SelectObject(hdc, old_brush);
    SelectObject(hdc, old_pen);
    let _ = DeleteObject(border.into());

    let old_font = SelectObject(hdc, state.font.into());
    let _ = SetBkMode(hdc, TRANSPARENT);

    let pad = scaled(m.pad, s);
    let icon_box = scaled(m.icon_box, s);
    let icon_gap = scaled(m.icon_gap, s);
    let text_gap = scaled(m.text_gap, s);

    let mut top = scaled(m.pad_y, s);
    for (index, item) in state.items.iter().enumerate() {
        let height = scaled(m.kind_height(item.kind), s);
        let row = RECT {
            left: 0,
            top,
            right: rc.right,
            bottom: top + height,
        };

        if item.kind == MenuKind::Separator {
            let inset = scaled(m.sep_inset, s);
            let y = (row.top + row.bottom) / 2;
            let pen = CreatePen(PS_SOLID, 1, COLORREF(c.separator));
            let old_pen = SelectObject(hdc, pen.into());
            let old_brush = SelectObject(hdc, GetStockObject(NULL_BRUSH));
            let _ = MoveToEx(hdc, inset, y, None);
            let _ = LineTo(hdc, rc.right - inset, y);
            SelectObject(hdc, old_brush);
            SelectObject(hdc, old_pen);
            let _ = DeleteObject(pen.into());
            top += height;
            continue;
        }

        let disabled = item.kind == MenuKind::Disabled;
        if state.hover == Some(index) && !disabled {
            let hover = CreateSolidBrush(COLORREF(c.hover));
            let inset = scaled(6.0, s);
            let mut r = row;
            r.left += inset;
            r.right -= inset;
            let _ = FillRect(hdc, &r, hover);
            let _ = DeleteObject(hover.into());
        }

        let _ = SetTextColor(hdc, COLORREF(if disabled { c.disabled } else { c.text }));

        let icon_left = pad;
        if !disabled {
            let iy = row.top + (height - icon_box) / 2;
            let b = IconBox {
                l: icon_left,
                t: iy,
                sz: icon_box,
            };
            if item.icon != MenuIcon::None {
                draw_icon(hdc, item.icon, &b, COLORREF(c.text));
            }
        }

        let label_left = pad + icon_box + icon_gap;

        // Measure the right-aligned column so a long label never overlaps it.
        let mut shortcut_w = 0i32;
        if !item.shortcut.is_empty() {
            let mut probe: Vec<u16> = item.shortcut.encode_utf16().collect();
            let mut calc = RECT::default();
            let _ = DrawTextW(
                hdc,
                &mut probe,
                &mut calc,
                DT_SINGLELINE | DT_NOPREFIX | DT_CALCRECT,
            );
            shortcut_w = calc.right - calc.left;
        }
        let label_right = rc.right - pad - shortcut_w - if shortcut_w > 0 { text_gap } else { 0 };

        if !item.label.is_empty() {
            let mut text: Vec<u16> = item.label.encode_utf16().collect();
            let mut r = RECT {
                left: label_left,
                top: row.top,
                right: label_right,
                bottom: row.bottom,
            };
            let _ = DrawTextW(
                hdc,
                &mut text,
                &mut r,
                DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX,
            );
        }

        if !item.shortcut.is_empty() {
            let _ = SetTextColor(hdc, COLORREF(c.shortcut));
            let mut text: Vec<u16> = item.shortcut.encode_utf16().collect();
            let mut r = RECT {
                left: rc.right - pad - shortcut_w,
                top: row.top,
                right: rc.right - pad,
                bottom: row.bottom,
            };
            let _ = DrawTextW(
                hdc,
                &mut text,
                &mut r,
                DT_RIGHT | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX,
            );
        }

        top += height;
    }

    SelectObject(hdc, old_font);
    let _ = EndPaint(hwnd, &ps);
}

// ---- outline icons --------------------------------------------------------

struct IconBox {
    l: i32,
    t: i32,
    sz: i32,
}

impl IconBox {
    fn x(&self, v: f32) -> i32 {
        self.l + (v * self.sz as f32 / 24.0).round() as i32
    }
    fn y(&self, v: f32) -> i32 {
        self.t + (v * self.sz as f32 / 24.0).round() as i32
    }
    unsafe fn line(&self, hdc: HDC, x1: f32, y1: f32, x2: f32, y2: f32) {
        let _ = MoveToEx(hdc, self.x(x1), self.y(y1), None);
        let _ = LineTo(hdc, self.x(x2), self.y(y2));
    }
    unsafe fn circle(&self, hdc: HDC, cx: f32, cy: f32, r: f32) {
        let _ = Ellipse(
            hdc,
            self.x(cx - r),
            self.y(cy - r),
            self.x(cx + r),
            self.y(cy + r),
        );
    }
    unsafe fn rect(&self, hdc: HDC, x1: f32, y1: f32, x2: f32, y2: f32) {
        let _ = Rectangle(hdc, self.x(x1), self.y(y1), self.x(x2), self.y(y2));
    }
}

unsafe fn draw_icon(hdc: HDC, icon: MenuIcon, b: &IconBox, ink: COLORREF) {
    let pen = CreatePen(PS_SOLID, (b.sz / 12).max(1), ink);
    let old_pen = SelectObject(hdc, pen.into());
    let old_brush = SelectObject(hdc, GetStockObject(NULL_BRUSH));

    match icon {
        MenuIcon::None => {}
        MenuIcon::Scissors => {
            b.line(hdc, 6.0, 17.0, 18.0, 4.0);
            b.line(hdc, 18.0, 17.0, 6.0, 4.0);
            b.circle(hdc, 6.0, 18.5, 2.6);
            b.circle(hdc, 18.0, 18.5, 2.6);
        }
        MenuIcon::Monitor => {
            b.rect(hdc, 3.0, 4.0, 21.0, 16.0);
            b.line(hdc, 12.0, 16.0, 12.0, 20.0);
            b.line(hdc, 8.0, 20.0, 16.0, 20.0);
        }
        MenuIcon::Copy => {
            b.rect(hdc, 3.0, 7.0, 15.0, 21.0);
            b.rect(hdc, 9.0, 3.0, 21.0, 17.0);
        }
        MenuIcon::Close => {
            b.line(hdc, 5.0, 5.0, 19.0, 19.0);
            b.line(hdc, 19.0, 5.0, 5.0, 19.0);
        }
        MenuIcon::Gear => {
            b.circle(hdc, 12.0, 12.0, 3.0);
            b.circle(hdc, 12.0, 12.0, 7.6);
            for k in 0..8 {
                let a = (k as f32) * std::f32::consts::FRAC_PI_4;
                let (sin, cos) = a.sin_cos();
                b.line(
                    hdc,
                    12.0 + 7.6 * cos,
                    12.0 + 7.6 * sin,
                    12.0 + 10.0 * cos,
                    12.0 + 10.0 * sin,
                );
            }
        }
    }
    SelectObject(hdc, old_brush);
    SelectObject(hdc, old_pen);
    let _ = DeleteObject(pen.into());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tray_menu_spec_keeps_action_order_separators_and_disabled_version() {
        let items = build("Ctrl+Alt+A", "PrtSc", "Ctrl+Alt+W", 0);
        assert_eq!(
            items.len(),
            10,
            "4 actions + sep + settings + sep + exit + sep + version"
        );

        let ids: Vec<u32> = items.iter().map(|i| i.id).collect();
        assert_eq!(
            ids,
            vec![ID_REGION, ID_FULL, ID_COPY, ID_CLOSE_ALL, 0, ID_SETTINGS, 0, ID_EXIT, 0, 0]
        );

        // Group 1 is four working actions; three separators split the groups.
        assert!(items[..4].iter().all(|i| i.kind == MenuKind::Action));
        assert_eq!(items[4].kind, MenuKind::Separator);
        assert_eq!(items[5].kind, MenuKind::Action);
        assert_eq!(items[6].kind, MenuKind::Separator);
        assert_eq!(items[7].kind, MenuKind::Action);
        assert_eq!(items[8].kind, MenuKind::Separator);

        // Separators carry no label / shortcut / icon.
        assert!(items
            .iter()
            .filter(|i| i.kind == MenuKind::Separator)
            .all(|i| i.label.is_empty() && i.shortcut.is_empty() && i.icon == MenuIcon::None));

        // Disabled version row: FastPin branding + crate version, not CitePix.
        let v = &items[9];
        assert_eq!(v.kind, MenuKind::Disabled);
        assert_eq!(v.label, format!("FastPin v{}", env!("CARGO_PKG_VERSION")));
        assert!(!v.label.contains("CitePix"));
        assert_eq!(v.icon, MenuIcon::None);
        assert!(v.shortcut.is_empty());

        // Shortcuts follow the registered hotkeys, never a hardcoded Alt+A.
        assert_eq!(items[0].shortcut, "Ctrl+Alt+A");
        assert_eq!(items[1].shortcut, "PrtSc");
        assert_eq!(items[2].shortcut, "Ctrl+Alt+W");
        // The close-all row shows the live pin count on the right.
        assert_eq!(items[3].shortcut, "0");
        assert_eq!(build("A", "B", "C", 7)[3].shortcut, "7");

        // Every action row has a line icon; the version row has none.
        assert!(items[..4].iter().all(|i| i.icon != MenuIcon::None));
        assert_eq!(items[5].icon, MenuIcon::Gear);
        assert_eq!(items[7].icon, MenuIcon::Close);
    }

    /// The card is dense: 12px text, 32px rows, 12px side padding and 9px
    /// separator insets at 100% DPI. Every length must follow the monitor
    /// scale, so a 150% display gets the same proportions.
    #[test]
    fn tray_menu_metrics_are_compact_and_scale_with_dpi() {
        let m = Metrics::from_font(16.0);
        assert_eq!(m.font, 12.0, "theme 16px minus 4");
        assert_eq!(m.row_h, 32.0);
        assert_eq!(m.sep_h, 8.0);
        assert_eq!(m.version_h, 28.0);
        assert_eq!(m.icon_box, 14.0);
        assert_eq!(m.pad, 12.0);
        assert_eq!(m.pad_y, 4.0);
        assert_eq!(m.icon_gap, 8.0);
        assert_eq!(m.text_gap, 8.0);
        assert_eq!(m.sep_inset, 9.0);
        assert_eq!(m.card_w, 240.0);
        assert_eq!(m.radius, 10.0, "the rounded-corner radius is unchanged");

        // Upper bounds the user asked for, so a future tweak cannot drift back
        // to the previous, roomier card.
        assert!(m.font <= 13.0);
        assert!(m.row_h <= 32.0);
        assert!(m.card_w <= 240.0);

        // Larger or smaller body fonts stay inside the 11-13px band.
        assert_eq!(Metrics::from_font(22.0).font, 13.0);
        assert_eq!(Metrics::from_font(11.0).font, 11.0);

        let items = build("a", "b", "c", 0);
        assert_eq!(
            m.height(&items),
            32.0 * 6.0 + 8.0 * 3.0 + 28.0 + 8.0,
            "six action rows, three separators, version row, 4px inner padding"
        );

        // One multiplication by the DPI scale, rounded to physical pixels.
        assert_eq!(scaled(m.font, 1.5), 18);
        assert_eq!(scaled(m.row_h, 1.5), 48);
        assert_eq!(scaled(m.pad, 1.25), 15);
        assert_eq!(scaled(m.sep_inset, 2.0), 18);
        assert_eq!(scaled(m.card_w, 1.0), 240);
    }
}
