//! Overlay chrome tokens, projected from the gpui-component theme.
//!
//! The free functions below keep their historical names and signatures so the
//! toolbar, settings window and pin editor do not have to know where the
//! colours come from. They read a process-wide snapshot that
//! [`apply_theme`] fills from `gpui_component::Theme`; before the first apply
//! the snapshot falls back to the gpui-component light theme.
//!
//! Everything is a snapshot rather than a `cx` lookup because most of these
//! helpers are called from pure element builders that never see the app.

use std::sync::RwLock;

use gpui::{hsla, rgb, Hsla, Pixels};
use gpui_kit::component::{Theme, ThemeColor, ThemeMode};

/// Colours the chrome needs, already resolved from the theme's colour tokens.
///
/// `Copy` so readers can take it out of the lock and drop the guard before
/// building elements (no lock is held while rendering).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Palette {
    pub surface: Hsla,
    pub secondary: Hsla,
    pub muted: Hsla,
    pub foreground: Hsla,
    pub muted_foreground: Hsla,
    pub primary: Hsla,
    pub primary_foreground: Hsla,
    pub selection: Hsla,
    pub border: Hsla,
    pub destructive: Hsla,
    pub input: Hsla,
    /// The component theme's `primary_hover` / `primary_active`.
    pub accent_hover: Hsla,
    pub accent_pressed: Hsla,
    pub danger_hover: Hsla,
    /// Text/icon colour painted on a `primary` fill.
    pub on_accent: Hsla,
    /// Text painted on a `destructive` fill.
    pub on_danger: Hsla,
    /// Darkest available ink, for shadows.
    pub shadow: Hsla,
    pub title_bar: Hsla,
    pub title_bar_border: Hsla,
    pub radius: Pixels,
    pub font_size: Pixels,
    pub dark: bool,
}

/// Snapshot storage. `OnceLock` cannot be refreshed when the user flips the
/// theme, so a plain `RwLock<Option<_>>` is the right shape here.
fn store() -> &'static RwLock<Option<Palette>> {
    static SNAPSHOT: RwLock<Option<Palette>> = RwLock::new(None);
    &SNAPSHOT
}

fn darker(a: Hsla, b: Hsla) -> Hsla {
    if a.l <= b.l {
        a
    } else {
        b
    }
}

/// Maps the gpui-component theme onto the chrome palette.
///
/// Light and dark differ only in which token values come in; the rules are
/// shared so a future palette cannot make one appearance unreadable while the
/// other stays fine. Component tokens that carry a hover/active companion
/// (`primary_hover`, `primary_active`, `danger_hover`) are used as-is instead
/// of being derived from lightness, so the chrome matches the component
/// library's own states.
fn palette_from(theme: &Theme) -> Palette {
    let colors = &theme.colors;
    // Shadows want the darkest available ink; in dark mode that is the
    // background, in light mode it is the foreground.
    let base = darker(colors.background, colors.foreground);
    Palette {
        surface: colors.background,
        secondary: colors.secondary,
        muted: colors.muted,
        foreground: colors.foreground,
        muted_foreground: colors.muted_foreground,
        primary: colors.primary,
        primary_foreground: colors.primary_foreground,
        selection: colors.selection,
        border: colors.border,
        destructive: colors.danger,
        input: colors.input,
        accent_hover: colors.primary_hover,
        accent_pressed: colors.primary_active,
        danger_hover: colors.danger_hover,
        on_accent: colors.primary_foreground,
        // `danger_foreground` is a mid-red in the dark palette (red-600 on
        // red-400), which disappears on the fill; the background ink is
        // readable in both appearances.
        on_danger: colors.background,
        shadow: base,
        title_bar: colors.title_bar,
        title_bar_border: colors.title_bar_border,
        radius: theme.radius,
        font_size: theme.font_size,
        dark: theme.is_dark(),
    }
}

fn light_theme() -> Theme {
    Theme::from(&*ThemeColor::light())
}

fn light_palette() -> Palette {
    palette_from(&light_theme())
}

fn current() -> Palette {
    let guard = store().read().unwrap_or_else(|e| e.into_inner());
    (*guard).unwrap_or_else(light_palette)
}

/// Replaces the snapshot from the initialized gpui-component theme.
/// Idempotent: applying the same theme twice leaves the same palette.
pub fn apply_theme(theme: &Theme) {
    let palette = palette_from(theme);
    let mut guard = store().write().unwrap_or_else(|e| e.into_inner());
    *guard = Some(palette);
}

/// Initializes gpui-kit (GPUI base, components and their globals) and applies
/// the appearance persisted in `settings.json`. Call once per `App`, before
/// any window opens.
pub fn init(cx: &mut gpui::App) {
    gpui_kit::init(cx);
    set_appearance(cx, &crate::config::get().appearance);
}

/// Component palette for a named appearance.
///
/// Tinted variants only replace the primary / accent / selection tokens: the
/// surface, foreground, border and input tokens stay at the mode defaults so
/// body text keeps its light/dark contrast. For plain `light` / `dark` (or any
/// unknown value) the untouched default palette is returned.
fn themed_colors(name: &str) -> ThemeColor {
    let dark = crate::config::appearance_dark(name);
    let mut colors = if dark {
        *ThemeColor::dark()
    } else {
        *ThemeColor::light()
    };
    let hex = match name.trim().to_ascii_lowercase().as_str() {
        crate::config::APPEARANCE_BLUE | crate::config::APPEARANCE_BLUE_DARK => 0x3B82F6,
        crate::config::APPEARANCE_GREEN | crate::config::APPEARANCE_GREEN_DARK => 0x22B14C,
        crate::config::APPEARANCE_PURPLE | crate::config::APPEARANCE_PURPLE_DARK => 0xA855F7,
        crate::config::APPEARANCE_ORANGE | crate::config::APPEARANCE_ORANGE_DARK => 0xFF8C00,
        _ => return colors,
    };
    let base = Hsla::from(rgb(hex));
    let shift = |delta: f32| hsla(base.h, base.s, (base.l + delta).clamp(0.0, 1.0), base.a);
    colors.primary = base;
    colors.primary_hover = shift(0.08);
    colors.primary_active = shift(-0.08);
    // 纯白或纯黑，保证按钮文字压在主题色上时始终可读。
    colors.primary_foreground = if base.l > 0.55 {
        hsla(0.0, 0.0, 0.0, 1.0)
    } else {
        hsla(0.0, 0.0, 1.0, 1.0)
    };
    colors.accent = base;
    colors.accent_foreground = colors.primary_foreground;
    // selection 仍作半透明选中底色，不用不透明的主题色盖住文字。
    colors.selection = base.opacity(0.20);
    colors
}

/// Switches appearance at runtime through the component theme, refreshes the
/// snapshot, and asks every window (settings included) to repaint.
///
/// `Theme::change` loads the mode's registered palette, so the accent edit has
/// to happen in a second `Theme::update` or it would be overwritten. Its second
/// argument is an unused `Option<&mut Window>`, not a colour.
pub fn set_appearance(cx: &mut gpui::App, name: &str) {
    let mode = if crate::config::appearance_dark(name) {
        ThemeMode::Dark
    } else {
        ThemeMode::Light
    };
    Theme::change(mode, None, cx);
    let colors = themed_colors(name);
    Theme::update(cx, |t| t.colors = colors);
    let theme = Theme::global(cx).clone();
    apply_theme(&theme);
    cx.refresh_windows();
}

/// Historical light/dark entry point, kept for callers that only have a bool.
#[allow(dead_code)]
pub fn set_dark(cx: &mut gpui::App, dark: bool) {
    set_appearance(
        cx,
        if dark {
            crate::config::APPEARANCE_DARK
        } else {
            crate::config::APPEARANCE_LIGHT
        },
    );
}

// ---- chrome tokens ---------------------------------------------------------

/// Deepest chrome ink, used for shadows.
pub fn base() -> Hsla {
    current().shadow
}

/// Toolbar / popup surface. Slightly translucent so the frozen screenshot
/// still shows through, in both appearances.
pub fn surface() -> Hsla {
    current().surface.opacity(0.94)
}

/// Opaque surface for cards and controls that must not show what is behind.
pub fn surface_solid() -> Hsla {
    current().surface
}

/// Hover wash and secondary fill.
pub fn elevated_surface() -> Hsla {
    current().secondary
}

/// Page wash behind cards (settings window).
pub fn muted() -> Hsla {
    current().muted
}

pub fn primary_fg() -> Hsla {
    current().foreground
}

pub fn secondary_fg() -> Hsla {
    current().muted_foreground
}

pub fn disabled_fg() -> Hsla {
    current().muted_foreground.opacity(0.6)
}

/// Selected-tool / switch / primary-button fill.
pub fn accent() -> Hsla {
    current().primary
}

pub fn accent_hover() -> Hsla {
    current().accent_hover
}

// Reserved for pressed-state fills; the color test below pins the value.
#[allow(dead_code)]
pub fn accent_pressed() -> Hsla {
    current().accent_pressed
}

/// Text or icon painted on top of [`accent`].
pub fn on_accent() -> Hsla {
    current().on_accent
}

/// Selected-tool wash. Over the toolbar surface this reads as the tinted
/// square the reference paints behind the active tool.
pub fn active_background() -> Hsla {
    current().primary.opacity(0.14)
}

/// Solid wash for larger selected surfaces (settings nav pills).
pub fn selection_wash() -> Hsla {
    current().selection
}

pub fn border() -> Hsla {
    current().border
}

/// Body font size from the component theme, for consumers that live outside
/// GPUI (the owner-drawn tray menu). Corner radius is carried in [`Palette`]
/// too, but the Win32 popup menu draws its own frame, so no GDI consumer can
/// apply it.
pub fn font_size() -> Pixels {
    current().font_size
}

/// Slider rails and text-input backgrounds.
pub fn input() -> Hsla {
    current().input
}

pub fn danger() -> Hsla {
    current().destructive
}

pub fn danger_hover() -> Hsla {
    current().danger_hover
}

/// Text painted on a [`danger`] fill. The semantic `destructive_foreground`
/// is a mid-red in the dark palette, which disappears on the light red fill,
/// so the background ink (white in light, near-black in dark) is the readable
/// choice instead.
pub fn on_danger() -> Hsla {
    current().on_danger
}

pub fn success() -> Hsla {
    // No semantic success token; this green reads on both appearances.
    Hsla::from(rgb(0x35C96F))
}

pub fn transparent() -> Hsla {
    hsla(0.0, 0.0, 0.0, 0.0)
}

/// ARGB for annotation strokes.
pub const STROKE_ARGB: u32 = 0xFFFA_3C3C;

/// (hex, packed RGB) — the annotation palette, shared by the settings page's
/// default-colour row and the overlay toolbar's colour popup so the two can
/// never offer different colours. The first entry is `STROKE_ARGB`.
pub const SWATCHES: [(&str, u32); 8] = [
    ("#FA3C3C", 0xFA3C3C),
    ("#FF8C00", 0xFF8C00),
    ("#FFD700", 0xFFD700),
    ("#22B14C", 0x22B14C),
    ("#3B82F6", 0x3B82F6),
    ("#A855F7", 0xA855F7),
    ("#1E1E1E", 0x1E1E1E),
    ("#FFFFFF", 0xFFFFFF),
];

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::Rgba;

    fn rgb_of(c: Hsla) -> (u8, u8, u8) {
        let r = Rgba::from(c);
        (
            (r.r * 255.0).round() as u8,
            (r.g * 255.0).round() as u8,
            (r.b * 255.0).round() as u8,
        )
    }

    fn light() -> Palette {
        palette_from(&light_theme())
    }

    fn dark() -> Palette {
        let mut theme = Theme::from(&*ThemeColor::dark());
        theme.mode = ThemeMode::Dark;
        palette_from(&theme)
    }

    fn themed_palette(name: &str) -> Palette {
        let mut theme = Theme::from(&themed_colors(name));
        theme.mode = if crate::config::appearance_dark(name) {
            ThemeMode::Dark
        } else {
            ThemeMode::Light
        };
        palette_from(&theme)
    }

    #[test]
    fn light_and_dark_resolve_different_readable_palettes() {
        let light = light();
        // Light: dark ink on a bright surface.
        assert!(light.surface.l > 0.9, "{light:?}");
        assert!(light.foreground.l < 0.2, "{light:?}");
        assert!(light.muted_foreground.l > light.foreground.l);
        // Accent and its foreground are far apart, so selected tools and
        // primary buttons keep their label readable.
        assert!((light.primary.l - light.primary_foreground.l).abs() > 0.5);
        assert!((light.on_danger.l - light.destructive.l).abs() > 0.3);

        let dark = dark();
        assert!(dark.surface.l < 0.2, "{dark:?}");
        assert!(dark.foreground.l > 0.9, "{dark:?}");
        assert!(dark.muted_foreground.l < dark.foreground.l);
        assert!((dark.primary.l - dark.primary_foreground.l).abs() > 0.5);
        assert!((dark.on_danger.l - dark.destructive.l).abs() > 0.3);

        // The two appearances really differ in the key colours.
        assert_ne!(rgb_of(light.surface), rgb_of(dark.surface));
        assert_ne!(rgb_of(light.foreground), rgb_of(dark.foreground));
        assert_ne!(rgb_of(light.primary), rgb_of(dark.primary));
        assert_ne!(rgb_of(light.input), rgb_of(dark.input));
        assert_ne!(rgb_of(light.border), rgb_of(dark.border));
        // Borders and rails stay distinguishable from the surface they sit on.
        assert_ne!(rgb_of(dark.surface), rgb_of(dark.secondary));
        assert_ne!(rgb_of(dark.surface), rgb_of(dark.input));
        // Shadow ink is dark in both appearances.
        assert!(light.shadow.l < 0.2);
        assert!(dark.shadow.l < 0.2);
    }

    #[test]
    fn tinted_themes_recolour_only_the_accents() {
        use crate::config::{
            APPEARANCE_BLUE, APPEARANCE_BLUE_DARK, APPEARANCE_GREEN, APPEARANCE_GREEN_DARK,
            APPEARANCE_ORANGE, APPEARANCE_ORANGE_DARK, APPEARANCE_PURPLE, APPEARANCE_PURPLE_DARK,
        };

        let light = light();
        let dark = dark();
        let pairs = [
            (APPEARANCE_BLUE, APPEARANCE_BLUE_DARK),
            (APPEARANCE_GREEN, APPEARANCE_GREEN_DARK),
            (APPEARANCE_ORANGE, APPEARANCE_ORANGE_DARK),
            (APPEARANCE_PURPLE, APPEARANCE_PURPLE_DARK),
        ];
        let mut primaries = Vec::new();
        for &(light_name, dark_name) in &pairs {
            let tinted = themed_palette(light_name);
            let tinted_dark = themed_palette(dark_name);
            // 四种主题色的 primary 两两不同，且与默认色不同。
            assert!(!primaries.contains(&rgb_of(tinted.primary)), "{light_name}");
            primaries.push(rgb_of(tinted.primary));
            assert_ne!(rgb_of(tinted.primary), rgb_of(light.primary));
            assert_ne!(rgb_of(tinted_dark.primary), rgb_of(dark.primary));
            // 同一主题色的浅/深变体共享 primary，只换底色。
            assert_eq!(rgb_of(tinted.primary), rgb_of(tinted_dark.primary));
            // 控件填充上的文字始终有对比，selection 保持半透明。
            assert!((tinted.primary.l - tinted.primary_foreground.l).abs() > 0.4);
            assert!((tinted_dark.primary.l - tinted_dark.primary_foreground.l).abs() > 0.4);
            assert!(tinted.selection.a < 1.0);
            assert!(tinted_dark.selection.a < 1.0);
            // 背景与正文仍取自对应模式，可读性不因换主题色而回退。
            assert_eq!(rgb_of(tinted.surface), rgb_of(light.surface));
            assert_eq!(rgb_of(tinted.foreground), rgb_of(light.foreground));
            assert_eq!(rgb_of(tinted.input), rgb_of(light.input));
            assert_eq!(rgb_of(tinted_dark.surface), rgb_of(dark.surface));
            assert_eq!(rgb_of(tinted_dark.foreground), rgb_of(dark.foreground));
            assert_eq!(rgb_of(tinted_dark.input), rgb_of(dark.input));
            // 底色依旧一浅一深。
            assert!(tinted.surface.l > 0.9, "{light_name}");
            assert!(tinted_dark.surface.l < 0.2, "{dark_name}");
        }
        assert_eq!(primaries.len(), 4);
        // 普通 light/dark 不引入自定义色。
        assert_eq!(
            rgb_of(themed_palette("light").primary),
            rgb_of(light.primary)
        );
        assert_eq!(rgb_of(themed_palette("dark").primary), rgb_of(dark.primary));
    }

    #[test]
    fn apply_theme_is_idempotent() {
        // The only test that touches the process-wide snapshot: everything
        // else maps themes through `palette_from`, so parallel tests cannot
        // race each other's appearance.
        let mut t = Theme::from(&*ThemeColor::dark());
        t.mode = ThemeMode::Dark;
        apply_theme(&t);
        let once = current();
        apply_theme(&t);
        assert_eq!(once, current());
        apply_theme(&t);
        assert_eq!(once, current());

        // 带主题色的快照同样可重复应用：set_appearance 连续调用不会漂移。
        let mut tinted = Theme::from(&themed_colors(crate::config::APPEARANCE_PURPLE));
        tinted.mode = ThemeMode::Light;
        apply_theme(&tinted);
        let colored = current();
        apply_theme(&tinted);
        assert_eq!(colored, current());
        assert_ne!(colored.primary, once.primary);
    }

    #[test]
    fn uninitialized_snapshot_falls_back_to_light() {
        // The snapshot is process-wide; emulate "not yet applied" by reading
        // the fallback builder directly.
        let fallback = light_palette();
        assert!(!fallback.dark);
        assert!(fallback.surface.l > 0.9);
        assert_eq!(fallback, light());
    }

    #[test]
    fn surface_and_washes_keep_specified_alpha() {
        let light = light();
        assert!((light.surface.opacity(0.94).a - 0.94).abs() < 0.001);
        assert!((light.primary.opacity(0.14).a - 0.14).abs() < 0.001);
        assert!((light.muted_foreground.opacity(0.6).a - 0.6).abs() < 0.001);
        // Selection is the translucent token, not an opaque fill.
        assert!(light.selection.a < 1.0);
    }

    #[test]
    fn first_swatch_is_the_default_stroke() {
        assert_eq!(0xFF00_0000 | SWATCHES[0].1, STROKE_ARGB);
    }

    #[test]
    fn semantic_tokens_map_onto_the_chrome() {
        let theme = light_theme();
        let tokens = &theme.colors;
        let p = palette_from(&theme);
        assert_eq!(p.surface, tokens.background);
        assert_eq!(p.foreground, tokens.foreground);
        assert_eq!(p.muted, tokens.muted);
        assert_eq!(p.muted_foreground, tokens.muted_foreground);
        assert_eq!(p.primary, tokens.primary);
        assert_eq!(p.primary_foreground, tokens.primary_foreground);
        assert_eq!(p.border, tokens.border);
        assert_eq!(p.input, tokens.input);
        assert_eq!(p.selection, tokens.selection);
        assert_eq!(p.destructive, tokens.danger);
        assert_eq!(p.secondary, tokens.secondary);
        assert_eq!(p.title_bar, tokens.title_bar);
        assert_eq!(p.title_bar_border, tokens.title_bar_border);
        assert_eq!(p.radius, theme.radius);
        assert_eq!(p.font_size, theme.font_size);
        // The pairings the chrome actually paints with.
        assert_eq!(p.on_accent, tokens.primary_foreground);
        assert_eq!(p.on_danger, tokens.background);
        assert_ne!(p.accent_hover, p.primary);
        assert_ne!(p.danger_hover, p.destructive);
        // Component hover/active companions are consumed directly.
        assert_eq!(p.accent_hover, tokens.primary_hover);
        assert_eq!(p.accent_pressed, tokens.primary_active);
        assert_eq!(p.danger_hover, tokens.danger_hover);
    }
}
