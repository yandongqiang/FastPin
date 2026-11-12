//! Owner-drawn tray context menu, styled after the reference card.
//!
//! `TrackPopupMenu` still owns the modal loop (keyboard navigation, screen
//! clamping, click-outside dismiss, `TPM_RETURNCMD`), so every existing tray
//! command keeps its behaviour and the `Shell_NotifyIconW` add/delete logic is
//! untouched. Only the rendering changes: each row is an `MFT_OWNERDRAW` item
//! measured by `WM_MEASUREITEM` and painted by `WM_DRAWITEM`, both routed here
//! from the hidden window's `hidden_proc`.
//!
//! The menu is a plain data structure ([`MenuItem`]) so its order, separators
//! and the disabled version row can be unit-tested without a GUI. Colours come
//! from the gpui-kit semantic tokens for the persisted light/dark appearance,
//! never from hardcoded literals, so icons/text stay readable in both themes.

use std::cell::RefCell;

use windows::core::PCWSTR;
use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, POINT, RECT};
use windows::Win32::Graphics::Gdi::{
    CreateFontW, CreatePen, CreateSolidBrush, DeleteObject, DrawTextW, Ellipse, FillRect,
    GetStockObject, LineTo, MoveToEx, Polyline, Rectangle, SelectObject, SetBkMode, SetTextColor,
    CLEARTYPE_QUALITY, CLIP_DEFAULT_PRECIS, DEFAULT_CHARSET, DT_CALCRECT, DT_LEFT, DT_NOPREFIX,
    DT_RIGHT, DT_SINGLELINE, DT_VCENTER, HBRUSH, HDC, HFONT, NULL_BRUSH, OUT_DEFAULT_PRECIS,
    PS_SOLID, TRANSPARENT,
};
use windows::Win32::UI::Controls::{
    DRAWITEMSTRUCT, MEASUREITEMSTRUCT, ODS_CHECKED, ODS_DISABLED, ODS_GRAYED, ODS_SELECTED,
    ODT_MENU,
};
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::Win32::UI::WindowsAndMessaging::{
    CreatePopupMenu, DestroyMenu, GetCursorPos, InsertMenuItemW, SetForegroundWindow, SetMenuInfo,
    TrackPopupMenu, MENU_ITEM_STATE, MENUINFO, MENUITEMINFOW, MFS_DISABLED, MFT_OWNERDRAW,
    MIIM_DATA, MIIM_FTYPE, MIIM_ID, MIIM_STATE, MIM_BACKGROUND, TPM_LEFTALIGN, TPM_RETURNCMD,
    TPM_RIGHTBUTTON,
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

// Layout in logical (96-DPI) pixels, scaled by the monitor DPI in `track`.
const CARD_W: f32 = 290.0;
const ROW_H: f32 = 46.0;
const SEP_H: f32 = 13.0;
const VERSION_H: f32 = 40.0;
const PAD_X: f32 = 20.0;
const ICON_BOX: f32 = 18.0;
const ICON_GAP: f32 = 14.0;
const TEXT_GAP: f32 = 16.0;
const SEP_INSET: f32 = 16.0;
const FONT_PX: f32 = 15.0;

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
    Check,
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
    use gpui::base::ColorTokens;
    let tokens = if crate::config::get().dark() {
        ColorTokens::dark()
    } else {
        ColorTokens::light()
    };
    MenuColors {
        bg: colorref(tokens.surface),
        hover: colorref(tokens.secondary),
        text: colorref(tokens.foreground),
        shortcut: colorref(tokens.muted_foreground),
        disabled: colorref(tokens.muted_foreground),
        separator: colorref(tokens.border),
    }
}

// ---- live menu state ------------------------------------------------------

struct ActiveMenu {
    items: Vec<MenuItem>,
    colors: MenuColors,
    scale: f32,
    font: HFONT,
    bg_brush: HBRUSH,
}

thread_local! {
    static ACTIVE: RefCell<Option<ActiveMenu>> = const { RefCell::new(None) };
}

fn make_font(scale: f32) -> HFONT {
    let height = -((FONT_PX * scale).round() as i32);
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

/// Shows the owner-drawn card menu at the cursor and returns the chosen
/// command id (None when dismissed). The `TrackPopupMenu` flags, owner window
/// and `TPM_RETURNCMD` are byte-for-byte the previous native menu's.
pub(crate) fn track(hwnd: HWND, items: Vec<MenuItem>) -> Option<u32> {
    unsafe {
        let menu = CreatePopupMenu().ok()?;
        let colors = menu_colors();
        let scale = {
            let dpi = GetDpiForWindow(hwnd);
            if dpi == 0 { 1.0 } else { dpi as f32 / 96.0 }
        };
        let font = make_font(scale);
        let bg_brush = CreateSolidBrush(COLORREF(colors.bg));

        ACTIVE.with(|cell| {
            *cell.borrow_mut() = Some(ActiveMenu {
                items,
                colors,
                scale,
                font,
                bg_brush,
            });
        });

        // Cover the menu window's own margin with the card colour (owner-draw
        // items only own their rows). The brush must live until DestroyMenu.
        let info = MENUINFO {
            cbSize: std::mem::size_of::<MENUINFO>() as u32,
            fMask: MIM_BACKGROUND,
            hbrBack: bg_brush,
            ..Default::default()
        };
        let _ = SetMenuInfo(menu, &info);

        ACTIVE.with(|cell| {
            let guard = cell.borrow();
            let active = guard.as_ref().unwrap();
            for (index, it) in active.items.iter().enumerate() {
                let mii = MENUITEMINFOW {
                    cbSize: std::mem::size_of::<MENUITEMINFOW>() as u32,
                    fMask: MIIM_FTYPE | MIIM_ID | MIIM_DATA | MIIM_STATE,
                    fType: MFT_OWNERDRAW,
                    // Separators and the version row are display-only: disabled
                    // so they cannot be hovered, selected or clicked.
                    fState: if it.kind == MenuKind::Action {
                        MENU_ITEM_STATE(0)
                    } else {
                        MFS_DISABLED
                    },
                    wID: it.id,
                    dwItemData: index,
                    ..Default::default()
                };
                let _ = InsertMenuItemW(menu, index as u32, true, &mii);
            }
        });

        let mut pt = POINT::default();
        let _ = GetCursorPos(&mut pt);
        let _ = SetForegroundWindow(hwnd);
        let cmd = TrackPopupMenu(
            menu,
            TPM_LEFTALIGN | TPM_RIGHTBUTTON | TPM_RETURNCMD,
            pt.x,
            pt.y,
            Some(0),
            hwnd,
            None,
        );
        let _ = DestroyMenu(menu);

        ACTIVE.with(|cell| {
            if let Some(active) = cell.borrow_mut().take() {
                let _ = DeleteObject(active.font.into());
                let _ = DeleteObject(active.bg_brush.into());
            }
        });

        let cmd = cmd.0 as u32;
        (cmd != 0).then_some(cmd)
    }
}

// ---- owner-draw message handlers ------------------------------------------

pub(crate) fn on_measure_item(lparam: LPARAM) -> LRESULT {
    unsafe {
        let m = &mut *(lparam.0 as *mut MEASUREITEMSTRUCT);
        if m.CtlType.0 != ODT_MENU.0 {
            return LRESULT(0);
        }
        let handled = ACTIVE.with(|cell| {
            let guard = cell.borrow();
            let Some(active) = guard.as_ref() else {
                return false;
            };
            let Some(it) = active.items.get(m.itemData) else {
                return false;
            };
            m.itemWidth = (CARD_W * active.scale).round() as u32;
            m.itemHeight = ((match it.kind {
                MenuKind::Action => ROW_H,
                MenuKind::Separator => SEP_H,
                MenuKind::Disabled => VERSION_H,
            }) * active.scale)
                .round() as u32;
            true
        });
        LRESULT(handled as isize)
    }
}

pub(crate) fn on_draw_item(lparam: LPARAM) -> LRESULT {
    unsafe {
        let d = &*(lparam.0 as *const DRAWITEMSTRUCT);
        if d.CtlType.0 != ODT_MENU.0 {
            return LRESULT(0);
        }
        let handled = ACTIVE.with(|cell| {
            let guard = cell.borrow();
            let Some(active) = guard.as_ref() else {
                return false;
            };
            let Some(it) = active.items.get(d.itemData) else {
                return false;
            };
            draw_item(d, it, active);
            true
        });
        LRESULT(handled as isize)
    }
}

unsafe fn draw_item(d: &DRAWITEMSTRUCT, it: &MenuItem, active: &ActiveMenu) {
    let hdc = d.hDC;
    let rc = d.rcItem;
    let s = active.scale;
    let c = &active.colors;
    let state = d.itemState.0;

    let selected = state & ODS_SELECTED.0 != 0;
    let disabled = it.kind == MenuKind::Disabled || state & (ODS_DISABLED.0 | ODS_GRAYED.0) != 0;
    let checked = state & ODS_CHECKED.0 != 0;

    // Background / hover wash.
    let bg = if selected && !disabled { c.hover } else { c.bg };
    let brush = CreateSolidBrush(COLORREF(bg));
    let _ = FillRect(hdc, &rc, brush);
    let _ = DeleteObject(brush.into());

    if it.kind == MenuKind::Separator {
        let inset = (SEP_INSET * s).round() as i32;
        let y = (rc.top + rc.bottom) / 2;
        let pen = CreatePen(PS_SOLID, 1, COLORREF(c.separator));
        let old_pen = SelectObject(hdc, pen.into());
        let old_brush = SelectObject(hdc, GetStockObject(NULL_BRUSH));
        let _ = MoveToEx(hdc, rc.left + inset, y, None);
        let _ = LineTo(hdc, rc.right - inset, y);
        SelectObject(hdc, old_brush);
        SelectObject(hdc, old_pen);
        let _ = DeleteObject(pen.into());
        return;
    }

    let old_font = SelectObject(hdc, active.font.into());
    let _ = SetBkMode(hdc, TRANSPARENT);
    let _ = SetTextColor(hdc, COLORREF(if disabled { c.disabled } else { c.text }));

    let pad = (PAD_X * s).round() as i32;
    let icon_box = (ICON_BOX * s).round() as i32;
    let icon_gap = (ICON_GAP * s).round() as i32;
    let text_gap = (TEXT_GAP * s).round() as i32;

    if !disabled {
        let ix = rc.left + pad;
        let iy = rc.top + ((rc.bottom - rc.top) - icon_box) / 2;
        let b = IconBox { l: ix, t: iy, sz: icon_box };
        let icon = if checked { MenuIcon::Check } else { it.icon };
        if icon != MenuIcon::None {
            draw_icon(hdc, icon, &b, COLORREF(c.text));
        }
    }

    let label_left = rc.left + pad + icon_box + icon_gap;

    // Measure the right-aligned column so a long label never overlaps it.
    let mut sw = 0i32;
    if !it.shortcut.is_empty() {
        let mut probe: Vec<u16> = it.shortcut.encode_utf16().collect();
        let mut calc = RECT::default();
        let _ = DrawTextW(hdc, &mut probe, &mut calc, DT_SINGLELINE | DT_NOPREFIX | DT_CALCRECT);
        sw = calc.right - calc.left;
    }
    let label_right = rc.right - pad - sw - if sw > 0 { text_gap } else { 0 };

    if !it.label.is_empty() {
        let mut text: Vec<u16> = it.label.encode_utf16().collect();
        let mut r = RECT { left: label_left, top: rc.top, right: label_right, bottom: rc.bottom };
        let _ = DrawTextW(hdc, &mut text, &mut r, DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX);
    }

    if !it.shortcut.is_empty() {
        let _ = SetTextColor(hdc, COLORREF(c.shortcut));
        let mut text: Vec<u16> = it.shortcut.encode_utf16().collect();
        let mut r = RECT {
            left: rc.right - pad - sw,
            top: rc.top,
            right: rc.right - pad,
            bottom: rc.bottom,
        };
        let _ = DrawTextW(hdc, &mut text, &mut r, DT_RIGHT | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX);
    }

    SelectObject(hdc, old_font);
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
    unsafe fn poly(&self, hdc: HDC, pts: &[(f32, f32)]) {
        let v: Vec<POINT> = pts
            .iter()
            .map(|(x, y)| POINT { x: self.x(*x), y: self.y(*y) })
            .collect();
        let _ = Polyline(hdc, &v);
    }
    unsafe fn circle(&self, hdc: HDC, cx: f32, cy: f32, r: f32) {
        let _ = Ellipse(hdc, self.x(cx - r), self.y(cy - r), self.x(cx + r), self.y(cy + r));
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
        MenuIcon::Check => {
            b.poly(hdc, &[(4.0, 12.0), (9.5, 17.5), (20.0, 5.0)]);
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
        assert_eq!(items.len(), 10, "4 actions + sep + settings + sep + exit + sep + version");

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
}
