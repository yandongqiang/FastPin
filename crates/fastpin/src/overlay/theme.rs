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

use gpui::{hsla, rgb, Hsla, Pixels, SharedString};
use gpui_kit::component::{Theme, ThemeColor, ThemeMode, ThemeRegistry};

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

/// 磁盘上的一个 gpui-kit 主题族。设置文件现在直接写变体在 registry 里的
/// 注册名；light/dark key 只用于兼容历史配置文件里的族 id / `-dark` 写法。
#[derive(Clone, Debug)]
struct ThemeFamily {
    /// 族名，如 "CitePix" / "Catppuccin"。保留用于诊断输出。
    #[allow(dead_code)]
    name: String,
    /// 设置文件里的稳定 id，目前就是族名。
    id: String,
    /// 第一个浅色变体的注册名；族内没有浅色变体时为 None。
    light_key: Option<String>,
    /// 第一个深色变体的注册名；族内没有深色变体时为 None。
    dark_key: Option<String>,
    /// 全部变体注册名，允许 appearance 直接写变体名。
    variants: Vec<String>,
}

/// 进程内的主题族映射。gpui-kit 的 registry 会丢掉族名，只按变体名注册，
/// 族 → 变体的对应关系只能在这里自己维护。
fn families() -> &'static RwLock<Vec<ThemeFamily>> {
    static FAMILIES: RwLock<Vec<ThemeFamily>> = RwLock::new(Vec::new());
    &FAMILIES
}

/// 从主题文件文本里读出族映射。用 `serde_json::Value` 而不是强类型
/// `ThemeSet`：文件可能新增字段，这里只关心 name / themes[].name / mode。
fn parse_family(text: &str) -> Option<ThemeFamily> {
    let value: serde_json::Value = serde_json::from_str(text).ok()?;
    let name = value.get("name")?.as_str()?.trim().to_string();
    if name.is_empty() {
        return None;
    }
    let mut light_key = None;
    let mut dark_key = None;
    let mut variants = Vec::new();
    if let Some(themes) = value.get("themes").and_then(|v| v.as_array()) {
        for variant in themes {
            let Some(key) = variant.get("name").and_then(|v| v.as_str()) else {
                continue;
            };
            if key.trim().is_empty() {
                continue;
            }
            let dark = variant
                .get("mode")
                .and_then(|v| v.as_str())
                .is_some_and(|mode| mode.eq_ignore_ascii_case("dark"));
            if dark {
                dark_key.get_or_insert_with(|| key.to_string());
            } else {
                light_key.get_or_insert_with(|| key.to_string());
            }
            variants.push(key.to_string());
        }
    }
    if variants.is_empty() {
        return None;
    }
    Some(ThemeFamily {
        id: name.clone(),
        name,
        light_key,
        dark_key,
        variants,
    })
}

/// 主题文件查找路径：优先 exe 旁的 `themes/`（便携版），其次是当前工作
/// 目录下的 `themes/`（源码目录直接运行时）。
fn theme_dirs() -> Vec<std::path::PathBuf> {
    let mut dirs = Vec::new();
    if let Ok(exe) = std::env::current_exe() {
        if let Some(parent) = exe.parent() {
            dirs.push(parent.join("themes"));
        }
    }
    if let Ok(cwd) = std::env::current_dir() {
        let dir = cwd.join("themes");
        if !dirs.contains(&dir) {
            dirs.push(dir);
        }
    }
    dirs
}

/// 扫描主题目录，把每个族的所有变体注册进 gpui-kit 并记下族映射。
/// 目录缺失、文件损坏或注册失败都静默降级到 gpui-kit 默认主题，绝不影响启动。
pub fn init_themes(cx: &mut gpui::App) {
    // 正常由 init() 在 gpui_kit::init 之后调用；单独调用时不 panic，静默跳过。
    if !cx.has_global::<ThemeRegistry>() {
        *families().write().unwrap_or_else(|e| e.into_inner()) = Vec::new();
        return;
    }
    let mut loaded: Vec<ThemeFamily> = Vec::new();
    for dir in theme_dirs() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        let mut files: Vec<_> = entries
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| {
                path.extension()
                    .and_then(|ext| ext.to_str())
                    .is_some_and(|ext| ext.eq_ignore_ascii_case("json"))
            })
            .collect();
        files.sort();
        for path in files {
            let Ok(text) = std::fs::read_to_string(&path) else {
                continue;
            };
            let Some(family) = parse_family(&text) else {
                continue;
            };
            if loaded
                .iter()
                .any(|known| known.id.eq_ignore_ascii_case(&family.id))
            {
                continue;
            }
            if let Err(e) = ThemeRegistry::global_mut(cx).load_themes_from_str(&text) {
                eprintln!("主题 {} 注册失败：{e}", path.display());
                continue;
            }
            loaded.push(family);
        }
        // 命中一个目录即可：exe 旁有主题时不再读工作目录的同名文件。
        if !loaded.is_empty() {
            break;
        }
    }
    *families().write().unwrap_or_else(|e| e.into_inner()) = loaded;
}

/// 在已知主题族里解析 appearance id，返回 registry 里的变体注册名。
/// 同时接受族 id（`Catppuccin` / `Catppuccin-dark`）与具体变体名
///（`Catppuccin Mocha`），便于手工编辑设置文件。
fn family_key_in(families: &[ThemeFamily], name: &str) -> Option<String> {
    let name = name.trim();
    if name.is_empty() {
        return None;
    }
    let lower = name.to_ascii_lowercase();
    for family in families {
        if let Some(variant) = family
            .variants
            .iter()
            .find(|key| key.to_ascii_lowercase() == lower)
        {
            return Some(variant.clone());
        }
        let (base, want_dark) = match lower.strip_suffix("-dark") {
            Some(base) => (base, true),
            None => (lower.as_str(), false),
        };
        if base == family.id.to_ascii_lowercase() {
            let key = if want_dark {
                family.dark_key.as_ref().or(family.light_key.as_ref())
            } else {
                family.light_key.as_ref().or(family.dark_key.as_ref())
            };
            if let Some(key) = key {
                return Some(key.clone());
            }
        }
    }
    None
}

fn family_theme_key(name: &str) -> Option<String> {
    let guard = families().read().unwrap_or_else(|e| e.into_inner());
    family_key_in(&guard, name)
}

/// 设置页下拉用的 (id, 标签)：完全来自 themes 目录，按文件 / 族顺序列出每个
/// 主题族的全部变体。id 与标签都是变体在 gpui-kit registry 里的注册名，
/// 直接作为 `appearance` 写进设置文件。
///
/// 极端情况（目录缺失、全部文件损坏）下一个变体都没有时，回退到 gpui-kit
/// 默认注册的两项，保证下拉不为空。
fn theme_options_from(families: &[ThemeFamily]) -> Vec<(String, String)> {
    let options: Vec<(String, String)> = families
        .iter()
        .flat_map(|family| family.variants.iter().map(|key| (key.clone(), key.clone())))
        .collect();
    if options.is_empty() {
        return vec![
            (
                crate::config::DEFAULT_APPEARANCE.to_string(),
                crate::config::DEFAULT_APPEARANCE.to_string(),
            ),
            (
                crate::config::DEFAULT_DARK_APPEARANCE.to_string(),
                crate::config::DEFAULT_DARK_APPEARANCE.to_string(),
            ),
        ];
    }
    options
}

/// 设置页下拉内容；变体列表在 `init_themes` 后不再变化，开窗时取一次即可。
pub fn available_themes() -> Vec<(String, String)> {
    let guard = families().read().unwrap_or_else(|e| e.into_inner());
    theme_options_from(&guard)
}

/// Initializes gpui-kit (GPUI base, components and their globals), loads the
/// shipped theme families, and applies the appearance persisted in
/// `settings.json`. Call once per `App`, before any window opens.
pub fn init(cx: &mut gpui::App) {
    gpui_kit::init(cx);
    init_themes(cx);
    set_appearance(cx, &crate::config::get().appearance);
}

/// Switches appearance at runtime through the component theme, refreshes the
/// snapshot, and asks every window (settings included) to repaint.
///
/// `name` 是 themes 目录里变体的注册名（下拉直接写这个名字）；历史配置文件里
/// 的族 id（`Catppuccin` / `Catppuccin-dark`）仍由 `family_theme_key` 解析到
/// 族内变体。明暗与颜色一概取自 registry 里的 `ThemeConfig`，不再叠加任何
/// 人造 tint。找不到的名字回退到 gpui-kit 默认浅色变体，绝不 panic。
pub fn set_appearance(cx: &mut gpui::App, name: &str) {
    let cfg = {
        let registry = ThemeRegistry::global(cx);
        let key = family_theme_key(name).unwrap_or_else(|| name.trim().to_string());
        registry
            .themes()
            .get(&SharedString::from(key))
            .cloned()
            .or_else(|| {
                registry
                    .themes()
                    .get(&SharedString::from(crate::config::DEFAULT_APPEARANCE))
                    .cloned()
            })
            .or_else(|| registry.default_themes().get(&ThemeMode::Light).cloned())
            .or_else(|| registry.themes().values().next().cloned())
    };
    if let Some(cfg) = cfg {
        Theme::update(cx, |t| t.apply_config(&cfg));
        let theme = Theme::global(cx).clone();
        apply_theme(&theme);
        cx.refresh_windows();
    }
}

/// Historical light/dark entry point, kept for callers that only have a bool.
#[allow(dead_code)]
pub fn set_dark(cx: &mut gpui::App, dark: bool) {
    set_appearance(
        cx,
        if dark {
            crate::config::DEFAULT_DARK_APPEARANCE
        } else {
            crate::config::DEFAULT_APPEARANCE
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

    /// 变体配置的应用逻辑：明暗、圆角和主色都来自 `ThemeConfig`，
    /// 这正是 `set_appearance` 通过 `Theme::apply_config` 走的路径。
    #[test]
    fn applying_a_variant_config_sets_mode_and_tokens() {
        use std::rc::Rc;

        use gpui_kit::component::ThemeConfig;

        let config: ThemeConfig = serde_json::from_value(serde_json::json!({
            "name": "Catppuccin Mocha",
            "mode": "dark",
            "radius": 12,
            "colors": { "primary.background": "#A6E3A1", "background": "#1E1E2E" }
        }))
        .expect("parses variant config");
        let config = Rc::new(config);
        let mut theme = light_theme();
        assert!(!theme.is_dark());

        theme.apply_config(&config);
        assert!(theme.is_dark());
        assert_eq!(theme.radius, gpui::px(12.));
        let palette = palette_from(&theme);
        assert!(palette.dark);
        assert_eq!(rgb_of(palette.primary), rgb_of(Hsla::from(rgb(0xA6E3A1))));
        assert_eq!(rgb_of(palette.surface), rgb_of(Hsla::from(rgb(0x1E1E2E))));
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

        // 切换变体后再重复应用同样不漂移。
        let mut other = Theme::from(&*ThemeColor::light());
        other.mode = ThemeMode::Light;
        apply_theme(&other);
        let light = current();
        apply_theme(&other);
        assert_eq!(light, current());
        assert_ne!(light.primary, once.primary);
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

    /// 族映射的纯逻辑测试：不依赖 cx，也不触碰进程内的注册表。
    #[test]
    fn parses_theme_family_from_json() {
        let text = r#"{
            "name": "Catppuccin",
            "themes": [
                {"name": "Catppuccin Latte", "mode": "light"},
                {"name": "Catppuccin Frappe", "mode": "dark"},
                {"name": "Catppuccin Mocha", "mode": "dark"}
            ]
        }"#;
        let family = parse_family(text).expect("parses family");
        assert_eq!(family.name, "Catppuccin");
        assert_eq!(family.id, "Catppuccin");
        assert_eq!(family.light_key.as_deref(), Some("Catppuccin Latte"));
        assert_eq!(family.dark_key.as_deref(), Some("Catppuccin Frappe"));
        assert_eq!(family.variants.len(), 3);
        // 缺字段、空族和坏 JSON 都不能产生映射。
        assert!(parse_family(r#"{"themes": []}"#).is_none());
        assert!(parse_family(r#"{"name": "X"}"#).is_none());
        assert!(parse_family("not json").is_none());
    }

    /// 下拉只列 themes 目录的真实变体，id / 标签都是注册名；空目录回退默认两项。
    #[test]
    fn available_themes_list_every_variant_in_family_order() {
        let option = |name: &str| (name.to_string(), name.to_string());
        let families = vec![
            parse_family(
                r#"{
                    "name": "Catppuccin",
                    "themes": [
                        {"name": "Catppuccin Latte", "mode": "light"},
                        {"name": "Catppuccin Frappe", "mode": "dark"},
                        {"name": "Catppuccin Mocha", "mode": "dark"}
                    ]
                }"#,
            )
            .unwrap(),
            parse_family(
                r#"{
                    "name": "CitePix",
                    "themes": [
                        {"name": "CitePix Light", "mode": "light"},
                        {"name": "CitePix Dark", "mode": "dark"}
                    ]
                }"#,
            )
            .unwrap(),
        ];
        assert_eq!(
            theme_options_from(&families),
            vec![
                option("Catppuccin Latte"),
                option("Catppuccin Frappe"),
                option("Catppuccin Mocha"),
                option("CitePix Light"),
                option("CitePix Dark"),
            ]
        );
        assert_eq!(
            theme_options_from(&[]),
            vec![
                option(crate::config::DEFAULT_APPEARANCE),
                option(crate::config::DEFAULT_DARK_APPEARANCE),
            ]
        );
    }

    #[test]
    fn family_ids_and_variant_names_resolve_to_registry_keys() {
        let families = vec![
            parse_family(
                r#"{
                    "name": "Catppuccin",
                    "themes": [
                        {"name": "Catppuccin Latte", "mode": "light"},
                        {"name": "Catppuccin Frappe", "mode": "dark"}
                    ]
                }"#,
            )
            .unwrap(),
            parse_family(
                r#"{
                    "name": "Adventure",
                    "themes": [{"name": "Adventure", "mode": "dark"}]
                }"#,
            )
            .unwrap(),
        ];
        // 族 id 按 mode 选变体；只有深色变体时浅色 id 也回落到它。
        assert_eq!(
            family_key_in(&families, "Catppuccin").as_deref(),
            Some("Catppuccin Latte")
        );
        assert_eq!(
            family_key_in(&families, "Catppuccin-dark").as_deref(),
            Some("Catppuccin Frappe")
        );
        assert_eq!(
            family_key_in(&families, "  catppuccin-DARK  ").as_deref(),
            Some("Catppuccin Frappe")
        );
        // 变体名与空白、大小写都接受。
        assert_eq!(
            family_key_in(&families, "catppuccin frappe").as_deref(),
            Some("Catppuccin Frappe")
        );
        assert_eq!(
            family_key_in(&families, " Adventure ").as_deref(),
            Some("Adventure")
        );
        // 默认主题名等非主题族值不能误命中主题族。
        assert_eq!(
            family_key_in(&families, crate::config::DEFAULT_APPEARANCE),
            None
        );
        assert_eq!(family_key_in(&families, ""), None);
    }
}
