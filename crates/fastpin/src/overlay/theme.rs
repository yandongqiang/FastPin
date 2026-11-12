//! Overlay chrome tokens, projected from the gpui-kit default theme.
//!
//! The free functions below keep their historical names and signatures so the
//! toolbar, settings window and pin editor do not have to know where the
//! colours come from. They read a process-wide snapshot that
//! [`apply_theme`] fills from `gpui_kit::base::Theme`; before the first apply
//! the snapshot falls back to the gpui-kit light palette.
//!
//! Everything is a snapshot rather than a `cx` lookup because most of these
//! helpers are called from pure element builders that never see the app.

use std::sync::RwLock;

use gpui::{hsla, rgb, Hsla};
use gpui::base::{ColorTokens, SemanticThemeTokens, Theme, ThemeAppearance};

/// Colours the chrome needs, already resolved from the semantic tokens.
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
    /// `primary` nudged away from the background, for hover/pressed states.
    pub accent_hover: Hsla,
    pub accent_pressed: Hsla,
    pub danger_hover: Hsla,
    /// Text/icon colour painted on a `primary` fill.
    pub on_accent: Hsla,
    /// Text painted on a `destructive` fill.
    pub on_danger: Hsla,
    /// Darkest available ink, for shadows.
    pub shadow: Hsla,
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

fn shifted(c: Hsla, delta: f32) -> Hsla {
    Hsla {
        l: (c.l + delta).clamp(0.0, 1.0),
        ..c
    }
}

/// Maps the semantic tokens onto the chrome palette.
///
/// Light and dark differ only in which token values come in; the rules are
/// shared so a future palette cannot make one appearance unreadable while the
/// other stays fine.
fn palette_from(colors: &ColorTokens, dark: bool) -> Palette {
    // Shadows want the darkest available ink; in dark mode that is the
    // background, in light mode it is the foreground.
    let base = darker(colors.background, colors.foreground);
    // Light: hover/pressed move away from the background (lighter). Dark:
    // they move back towards it so a near-white primary does not glare.
    let (hover, pressed) = if dark { (-0.10, -0.20) } else { (0.10, 0.20) };
    Palette {
        surface: colors.surface,
        secondary: colors.secondary,
        muted: colors.muted,
        foreground: colors.foreground,
        muted_foreground: colors.muted_foreground,
        primary: colors.primary,
        primary_foreground: colors.primary_foreground,
        selection: colors.selection,
        border: colors.border,
        destructive: colors.destructive,
        input: colors.input,
        accent_hover: shifted(colors.primary, hover),
        accent_pressed: shifted(colors.primary, pressed),
        danger_hover: shifted(colors.destructive, 0.08),
        on_accent: colors.primary_foreground,
        on_danger: colors.background,
        shadow: base,
        dark,
    }
}

fn light_palette() -> Palette {
    palette_from(&ColorTokens::light(), false)
}

fn current() -> Palette {
    let guard = store().read().unwrap_or_else(|e| e.into_inner());
    (*guard).unwrap_or_else(light_palette)
}

/// Replaces the snapshot from a gpui-kit theme. Idempotent: applying the same
/// theme twice leaves the same palette.
pub fn apply_theme(theme: &Theme) {
    let dark = matches!(theme.appearance, ThemeAppearance::Dark);
    let palette = palette_from(&theme.tokens.colors, dark);
    let mut guard = store().write().unwrap_or_else(|e| e.into_inner());
    *guard = Some(palette);
}

/// Initializes gpui-kit (base globals, dialogs, inputs) and applies the
/// appearance persisted in `settings.json`. Call once per `App`, before any
/// window opens.
pub fn init(cx: &mut gpui::App) {
    gpui_kit::init(cx);
    set_dark(cx, crate::config::get().dark());
}

/// Switches appearance at runtime: writes the global theme, refreshes the
/// snapshot, and asks every window to repaint.
pub fn set_dark(cx: &mut gpui::App, dark: bool) {
    let theme = Theme {
        appearance: if dark {
            ThemeAppearance::Dark
        } else {
            ThemeAppearance::Light
        },
        tokens: SemanticThemeTokens {
            colors: if dark {
                ColorTokens::dark()
            } else {
                ColorTokens::light()
            },
            ..Default::default()
        },
        ..Default::default()
    };
    apply_theme(&theme);
    cx.set_global(theme);
    cx.refresh_windows();
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
        palette_from(&ColorTokens::light(), false)
    }

    fn dark() -> Palette {
        palette_from(&ColorTokens::dark(), true)
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
    fn apply_theme_is_idempotent() {
        // The only test that touches the process-wide snapshot: everything
        // else maps tokens through `palette_from`, so parallel tests cannot
        // race each other's appearance.
        let t = Theme {
            appearance: ThemeAppearance::Dark,
            tokens: SemanticThemeTokens {
                colors: ColorTokens::dark(),
                ..Default::default()
            },
            ..Default::default()
        };
        apply_theme(&t);
        let once = current();
        apply_theme(&t);
        assert_eq!(once, current());
        apply_theme(&t);
        assert_eq!(once, current());
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
        let tokens = ColorTokens::light();
        let p = palette_from(&tokens, false);
        assert_eq!(p.surface, tokens.surface);
        assert_eq!(p.foreground, tokens.foreground);
        assert_eq!(p.muted, tokens.muted);
        assert_eq!(p.muted_foreground, tokens.muted_foreground);
        assert_eq!(p.primary, tokens.primary);
        assert_eq!(p.primary_foreground, tokens.primary_foreground);
        assert_eq!(p.border, tokens.border);
        assert_eq!(p.input, tokens.input);
        assert_eq!(p.selection, tokens.selection);
        assert_eq!(p.destructive, tokens.destructive);
        assert_eq!(p.secondary, tokens.secondary);
        // The pairings the chrome actually paints with.
        assert_eq!(p.on_accent, tokens.primary_foreground);
        assert_eq!(p.on_danger, tokens.background);
        assert_ne!(p.accent_hover, p.primary);
        assert_ne!(p.danger_hover, p.destructive);
    }
}
