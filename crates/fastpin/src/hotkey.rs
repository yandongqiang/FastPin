//! Hotkey matching used by the low-level hook. Tests call these functions
//! without a message loop.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

pub const VK_SNAPSHOT: u16 = 0x2C;
pub const VK_A: u16 = 0x41;
pub const VK_W: u16 = 0x57;

/// fallback 模式下 RegisterHotKey 被其它程序占用而失败的记录。hook 在每次
/// （重）注册前清零、失败时填入；设置页录制完成后取走并显示明确错误。
/// 放在这个无 Win32 依赖的模块里，hook 与 settings 都能直接访问。
static FALLBACK_REGISTER_FAILURE: Mutex<Option<String>> = Mutex::new(None);

fn lock_failure() -> std::sync::MutexGuard<'static, Option<String>> {
    FALLBACK_REGISTER_FAILURE
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

/// 记录一次 fallback / 紧急退出注册失败；多条失败用「、」连接。
pub fn note_fallback_register_failure(detail: &str) {
    let mut guard = lock_failure();
    match guard.as_mut() {
        Some(existing) => {
            existing.push('、');
            existing.push_str(detail);
        }
        None => *guard = Some(detail.to_string()),
    }
}

/// 新录制会话 / 重注册前清零，避免旧失败污染本次结果。
pub fn clear_fallback_register_failure() {
    lock_failure().take();
}

/// 取走并清空最近的 fallback / 紧急退出注册失败。LL 模式没有走
/// RegisterHotKey 时返回 None，设置页据此继续显示「已设为 X」。
pub fn take_fallback_register_failure() -> Option<String> {
    lock_failure().take()
}

/// 紧急退出热键（Ctrl+Alt+Shift+Q）是否真的注册成功。托盘不可用模式里
/// app.rs 的提示文案要据此决定是否告诉用户可以用它退出；不能用上面的失败
/// 文案判断，因为重注册 / 新录制会话会清空那份文案。
static EMERGENCY_QUIT_REGISTERED: AtomicBool = AtomicBool::new(false);

/// hook 在 RegisterHotKey(Ctrl+Alt+Shift+Q) 之后立即写入成败。
pub fn note_emergency_quit_registered(ok: bool) {
    EMERGENCY_QUIT_REGISTERED.store(ok, Ordering::SeqCst);
}

/// app.rs 查询紧急退出热键是否可用（hotkey 是 pub 模块，可直接按此路径调用）。
pub fn emergency_quit_registered() -> bool {
    EMERGENCY_QUIT_REGISTERED.load(Ordering::SeqCst)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HotkeyAction {
    Region,
    CopyOnly,
    Fullscreen,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Hotkey {
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
    pub win: bool,
    pub vk: u16,
}

impl Hotkey {
    pub const fn new(ctrl: bool, alt: bool, shift: bool, win: bool, vk: u16) -> Self {
        Self {
            ctrl,
            alt,
            shift,
            win,
            vk,
        }
    }

    pub const fn has_modifier(self) -> bool {
        self.ctrl || self.alt || self.shift || self.win
    }

    /// 除 PrtSc 单键外至少要有一个修饰键。裸键会被 LL 钩子全局吞掉，
    /// 用户从此在系统里输入不了这个字母，所以视为无效绑定。
    pub const fn is_bindable(self) -> bool {
        self.vk == VK_SNAPSHOT || self.has_modifier()
    }

    pub fn matches(&self, ev: KeyEvent) -> bool {
        self.vk == ev.vk
            && self.ctrl == ev.ctrl
            && self.alt == ev.alt
            && self.shift == ev.shift
            && self.win == ev.win
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HotkeyBindings {
    pub region: Hotkey,
    pub copy_only: Hotkey,
    pub fullscreen: Hotkey,
}

impl HotkeyBindings {
    /// Defaults: Ctrl+Alt+A (region), Ctrl+Alt+W (copy only), PrtSc (fullscreen).
    pub fn poc_defaults() -> Self {
        Self {
            region: Hotkey::new(true, true, false, false, VK_A),
            copy_only: Hotkey::new(true, true, false, false, VK_W),
            fullscreen: Hotkey::new(false, false, false, false, VK_SNAPSHOT),
        }
    }

    /// An unparsable entry keeps its default rather than leaving the action
    /// unbound — a hand-edited typo should not silently disable screenshots.
    ///
    /// Duplicate combos are resolved at load time: `action_for_combo` would
    /// otherwise always pick the first action and silently kill the other.
    /// In Region → CopyOnly → Fullscreen order the first keeps its combo; a
    /// later duplicate falls back to its own default (or a spare F-key when
    /// that default is taken too) and the conflict is warned about.
    pub fn from_settings(stored: &crate::config::Hotkeys) -> Self {
        let d = Self::poc_defaults();
        let mut bindings = Self {
            region: parse(&stored.region).unwrap_or(d.region),
            copy_only: parse(&stored.copy_only).unwrap_or(d.copy_only),
            fullscreen: parse(&stored.fullscreen).unwrap_or(d.fullscreen),
        };
        let mut used: Vec<Hotkey> = Vec::new();
        for (action, default) in [
            (HotkeyAction::Region, d.region),
            (HotkeyAction::CopyOnly, d.copy_only),
            (HotkeyAction::Fullscreen, d.fullscreen),
        ] {
            let current = bindings.binding_for(action);
            if used.contains(&current) {
                let replacement = spare_binding(&used, default);
                eprintln!(
                    "hotkey conflict: {:?} shares {} with an earlier action; using {} instead",
                    action,
                    format(current),
                    format(replacement)
                );
                bindings.set_binding(action, replacement);
                used.push(replacement);
            } else {
                used.push(current);
            }
        }
        bindings
    }

    fn set_binding(&mut self, action: HotkeyAction, hotkey: Hotkey) {
        match action {
            HotkeyAction::Region => self.region = hotkey,
            HotkeyAction::CopyOnly => self.copy_only = hotkey,
            HotkeyAction::Fullscreen => self.fullscreen = hotkey,
        }
    }

    pub fn binding_for(&self, action: HotkeyAction) -> Hotkey {
        match action {
            HotkeyAction::Region => self.region,
            HotkeyAction::CopyOnly => self.copy_only,
            HotkeyAction::Fullscreen => self.fullscreen,
        }
    }

    pub fn action_for_combo(&self, ev: KeyEvent) -> Option<HotkeyAction> {
        if self.region.matches(ev) {
            return Some(HotkeyAction::Region);
        }
        if self.copy_only.matches(ev) {
            return Some(HotkeyAction::CopyOnly);
        }
        if self.fullscreen.matches(ev) {
            return Some(HotkeyAction::Fullscreen);
        }
        None
    }
}

const DIGITS: [&str; 10] = ["0", "1", "2", "3", "4", "5", "6", "7", "8", "9"];
const LETTERS: [&str; 26] = [
    "A", "B", "C", "D", "E", "F", "G", "H", "I", "J", "K", "L", "M", "N", "O", "P", "Q", "R", "S",
    "T", "U", "V", "W", "X", "Y", "Z",
];
const F_KEYS: [&str; 24] = [
    "F1", "F2", "F3", "F4", "F5", "F6", "F7", "F8", "F9", "F10", "F11", "F12", "F13", "F14", "F15",
    "F16", "F17", "F18", "F19", "F20", "F21", "F22", "F23", "F24",
];

/// Display name for a virtual key. Only the keys worth binding to a global
/// screenshot shortcut; everything else is rejected at recording time rather
/// than stored under a name we cannot show back.
pub fn vk_name(vk: u16) -> Option<&'static str> {
    Some(match vk {
        0x08 => "Backspace",
        0x09 => "Tab",
        0x0D => "Enter",
        0x1B => "Esc",
        0x20 => "Space",
        0x21 => "PageUp",
        0x22 => "PageDown",
        0x23 => "End",
        0x24 => "Home",
        0x25 => "Left",
        0x26 => "Up",
        0x27 => "Right",
        0x28 => "Down",
        0x2C => "PrtSc",
        0x2D => "Insert",
        0x2E => "Delete",
        0x30..=0x39 => DIGITS[(vk - 0x30) as usize],
        0x41..=0x5A => LETTERS[(vk - 0x41) as usize],
        0x70..=0x87 => F_KEYS[(vk - 0x70) as usize],
        _ => return None,
    })
}

pub fn vk_from_name(name: &str) -> Option<u16> {
    (1u16..=0xFF).find(|vk| vk_name(*vk).is_some_and(|n| n.eq_ignore_ascii_case(name)))
}

/// Modifier order matches the C# `HotkeyInput` display: Ctrl, Alt, Shift, Win.
pub fn format(h: Hotkey) -> String {
    let mut out = String::new();
    for (on, name) in [
        (h.ctrl, "Ctrl"),
        (h.alt, "Alt"),
        (h.shift, "Shift"),
        (h.win, "Win"),
    ] {
        if on {
            out.push_str(name);
            out.push('+');
        }
    }
    out.push_str(vk_name(h.vk).unwrap_or("?"));
    out
}

pub fn parse(text: &str) -> Option<Hotkey> {
    let (mut ctrl, mut alt, mut shift, mut win) = (false, false, false, false);
    let mut vk = None;
    for part in text.split('+') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        match part.to_ascii_lowercase().as_str() {
            "ctrl" | "control" => ctrl = true,
            "alt" => alt = true,
            "shift" => shift = true,
            "win" => win = true,
            _ => vk = vk_from_name(part),
        }
    }
    let hotkey = Hotkey::new(ctrl, alt, shift, win, vk?);
    // 裸键（非 PrtSc）会被全局吞键，拒绝成为有效绑定；调用方据此回退默认值。
    if !hotkey.is_bindable() {
        return None;
    }
    Some(hotkey)
}

/// Finds a combo none of the already-kept actions uses. The preferred one is
/// the action's own default; if that is taken too, spare F-keys with the same
/// modifiers are tried so every action ends up with a distinct combo.
fn spare_binding(used: &[Hotkey], preferred: Hotkey) -> Hotkey {
    if !used.contains(&preferred) && preferred.is_bindable() {
        return preferred;
    }
    // 裸键（除 PrtSc）不是有效绑定；备用键至少补一个 Ctrl，否则 decide
    // 会把它当成无效组合，冲突让位后动作反而彻底失效。
    let (ctrl, alt, shift, win) = if preferred.has_modifier() {
        (
            preferred.ctrl,
            preferred.alt,
            preferred.shift,
            preferred.win,
        )
    } else {
        (true, false, false, false)
    };
    for vk in 0x70u16..=0x7B {
        let candidate = Hotkey::new(ctrl, alt, shift, win, vk);
        if !used.contains(&candidate) {
            return candidate;
        }
    }
    // 三个动作不可能占满 12 个带修饰键的 F 键；保底也返回一个有效组合。
    Hotkey::new(ctrl, alt, shift, win, 0x70)
}

/// True for the modifier keys themselves. Recording ignores these: they refine
/// a combo, they never complete one.
pub fn is_modifier_vk(vk: u16) -> bool {
    matches!(
        vk,
        0x10 | 0x11 | 0x12 | 0xA0 | 0xA1 | 0xA2 | 0xA3 | 0xA4 | 0xA5 | 0x5B | 0x5C
    )
}

/// 修饰键位掩码：Ctrl=1、Alt=2、Shift=4、Win=8。PrtSc 的抬键现场修饰键
/// 可能已经被松开，所以 keydown 时把掩码锁存到原子量，keyup 用它还原事件。
pub const MOD_CTRL: u8 = 1;
pub const MOD_ALT: u8 = 2;
pub const MOD_SHIFT: u8 = 4;
pub const MOD_WIN: u8 = 8;

pub fn modifier_mask(ctrl: bool, alt: bool, shift: bool, win: bool) -> u8 {
    (if ctrl { MOD_CTRL } else { 0 })
        | (if alt { MOD_ALT } else { 0 })
        | (if shift { MOD_SHIFT } else { 0 })
        | (if win { MOD_WIN } else { 0 })
}

/// 用锁存的掩码组装 KeyEvent，判定逻辑因此保持为可单测的纯函数。
pub fn key_event_from_mask(vk: u16, mask: u8, key_up: bool) -> KeyEvent {
    KeyEvent {
        vk,
        ctrl: mask & MOD_CTRL != 0,
        alt: mask & MOD_ALT != 0,
        shift: mask & MOD_SHIFT != 0,
        win: mask & MOD_WIN != 0,
        key_up,
    }
}

/// 录制钩子在 PrtSc 抬键时用同一份锁存掩码还原组合。
pub fn hotkey_from_mask(vk: u16, mask: u8) -> Hotkey {
    let ev = key_event_from_mask(vk, mask, false);
    Hotkey::new(ev.ctrl, ev.alt, ev.shift, ev.win, vk)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct KeyEvent {
    pub vk: u16,
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
    pub win: bool,
    pub key_up: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HotkeyDecision {
    pub action: Option<HotkeyAction>,
    pub swallow: bool,
}

/// Decide whether this keyboard event is a bound screenshot combo.
///
/// `VK_SNAPSHOT` often arrives as KeyUp-only; we fire the action on KeyUp for
/// PrintScreen and on KeyDown for every other key. Both edges are swallowed
/// when the modifiers match a binding. Unbound variants (e.g. Win+PrtSc when
/// fullscreen is plain PrtSc) are not swallowed.
pub fn decide(bindings: &HotkeyBindings, ev: KeyEvent) -> HotkeyDecision {
    // 旧配置里可能残留无修饰键的裸键绑定；这里按当前事件的组合再挡一层，
    // 裸键（除 PrtSc 外）绝不吞系统按键。`action_for_combo` 返回的是动作枚举，
    // 绑定合法性要看事件对应的键组合本身。
    let combo = Hotkey::new(ev.ctrl, ev.alt, ev.shift, ev.win, ev.vk);
    let bound = combo
        .is_bindable()
        .then(|| bindings.action_for_combo(ev))
        .flatten();
    let swallow = bound.is_some();
    let action = bound.filter(|_| {
        if ev.vk == VK_SNAPSHOT {
            ev.key_up
        } else {
            !ev.key_up
        }
    });
    HotkeyDecision { action, swallow }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(vk: u16, ctrl: bool, alt: bool, shift: bool, win: bool, key_up: bool) -> KeyEvent {
        KeyEvent {
            vk,
            ctrl,
            alt,
            shift,
            win,
            key_up,
        }
    }

    /// The settings page writes `format` into settings.json and `parse` reads
    /// it back; a mismatch would silently reset the user's hotkey on restart.
    #[test]
    fn format_and_parse_round_trip() {
        for h in [
            Hotkey::new(false, false, false, false, VK_SNAPSHOT),
            Hotkey::new(true, true, false, false, VK_A),
            Hotkey::new(true, true, true, true, VK_W),
            Hotkey::new(false, false, true, false, 0x70),
            Hotkey::new(true, false, false, false, 0x30),
        ] {
            assert_eq!(parse(&format(h)), Some(h), "{}", format(h));
        }
    }

    #[test]
    fn parse_accepts_the_shipped_defaults() {
        let stored = crate::config::Hotkeys::default();
        assert_eq!(
            HotkeyBindings::from_settings(&stored),
            HotkeyBindings::poc_defaults()
        );
    }

    #[test]
    fn parse_is_case_insensitive_and_tolerates_spaces() {
        let expected = Hotkey::new(true, true, false, false, VK_A);
        assert_eq!(parse("ctrl + ALT + a"), Some(expected));
    }

    /// A typo must not leave the action unbound.
    #[test]
    fn a_broken_entry_falls_back_to_its_default() {
        let stored = crate::config::Hotkeys {
            region: "Ctrl+Nonsense".into(),
            copy_only: String::new(),
            fullscreen: "Shift+F5".into(),
        };
        let b = HotkeyBindings::from_settings(&stored);
        assert_eq!(b.region, HotkeyBindings::poc_defaults().region);
        assert_eq!(b.copy_only, HotkeyBindings::poc_defaults().copy_only);
        assert_eq!(b.fullscreen, Hotkey::new(false, false, true, false, 0x74));
    }

    /// 两个动作配了相同组合：早的保留，晚的改用备用键，`action_for_combo`
    /// 不再让两个动作抢同一个键而静默失效。
    #[test]
    fn duplicate_combos_are_resolved_on_load() {
        let stored = crate::config::Hotkeys {
            region: "Ctrl+Alt+A".into(),
            copy_only: "Ctrl+Alt+A".into(),
            fullscreen: "PrtSc".into(),
        };
        let b = HotkeyBindings::from_settings(&stored);
        assert_eq!(b.region, HotkeyBindings::poc_defaults().region);
        assert_eq!(b.fullscreen, HotkeyBindings::poc_defaults().fullscreen);
        assert_ne!(b.copy_only, b.region, "重复组合必须重新分配，不能静默失效");
        assert_eq!(
            b.action_for_combo(ev(VK_A, true, true, false, false, false)),
            Some(HotkeyAction::Region)
        );
        let combos = [b.region, b.copy_only, b.fullscreen];
        for i in 0..combos.len() {
            for j in (i + 1)..combos.len() {
                assert_ne!(combos[i], combos[j], "每个动作的组合必须互不相同");
            }
        }
    }

    /// 重复动作的默认值恰好也被先出现的动作占用时，仍要找到空闲键。
    #[test]
    fn duplicate_combo_whose_default_is_taken_gets_a_spare_key() {
        let stored = crate::config::Hotkeys {
            region: "Ctrl+Alt+W".into(),
            copy_only: "Ctrl+Alt+W".into(),
            fullscreen: "PrtSc".into(),
        };
        let b = HotkeyBindings::from_settings(&stored);
        assert_eq!(b.region, HotkeyBindings::poc_defaults().copy_only);
        assert_ne!(b.copy_only, b.region);
        assert_ne!(b.copy_only, HotkeyBindings::poc_defaults().fullscreen);
    }

    /// Win32 的 RegisterHotKey 占用无法纯单测，但「记录 → 取走 → 清空」的
    /// 语义可以：设置页正是靠它判断要不要把「已设为 X」换成失败提示。
    #[test]
    fn fallback_register_failure_is_taken_once() {
        clear_fallback_register_failure();
        assert_eq!(take_fallback_register_failure(), None);
        note_fallback_register_failure("区域截图（已被占用）");
        note_fallback_register_failure("全屏截图（已被占用）");
        let failure = take_fallback_register_failure().expect("记录到失败");
        assert!(failure.contains("区域截图"), "{failure}");
        assert!(failure.contains("全屏截图"), "{failure}");
        assert_eq!(take_fallback_register_failure(), None);
    }

    #[test]
    fn modifier_keys_never_complete_a_combo() {
        for vk in [0x10, 0x11, 0x12, 0xA2, 0xA3, 0x5B] {
            assert!(is_modifier_vk(vk), "{vk:#x}");
        }
        assert!(!is_modifier_vk(VK_A));
        assert!(!is_modifier_vk(VK_SNAPSHOT));
    }

    #[test]
    fn prtsc_keyup_only_fires_fullscreen() {
        let b = HotkeyBindings::poc_defaults();
        let d = decide(&b, ev(VK_SNAPSHOT, false, false, false, false, true));
        assert_eq!(d.action, Some(HotkeyAction::Fullscreen));
        assert!(d.swallow);
    }

    #[test]
    fn prtsc_keydown_swallows_but_does_not_double_fire() {
        let b = HotkeyBindings::poc_defaults();
        let d = decide(&b, ev(VK_SNAPSHOT, false, false, false, false, false));
        assert_eq!(d.action, None);
        assert!(d.swallow);
    }

    #[test]
    fn win_prtsc_is_not_swallowed_when_unbound() {
        let b = HotkeyBindings::poc_defaults();
        let d = decide(&b, ev(VK_SNAPSHOT, false, false, false, true, true));
        assert_eq!(d.action, None);
        assert!(!d.swallow);
    }

    #[test]
    fn ctrl_alt_a_keydown_fires_region() {
        let b = HotkeyBindings::poc_defaults();
        let d = decide(&b, ev(VK_A, true, true, false, false, false));
        assert_eq!(d.action, Some(HotkeyAction::Region));
        assert!(d.swallow);
    }

    #[test]
    fn ctrl_alt_a_with_shift_does_not_match() {
        let b = HotkeyBindings::poc_defaults();
        let d = decide(&b, ev(VK_A, true, true, true, false, false));
        assert_eq!(d.action, None);
        assert!(!d.swallow);
    }

    #[test]
    fn ctrl_alt_w_fires_copy_only() {
        let b = HotkeyBindings::poc_defaults();
        let d = decide(&b, ev(VK_W, true, true, false, false, false));
        assert_eq!(d.action, Some(HotkeyAction::CopyOnly));
        assert!(d.swallow);
    }

    /// 裸键（无修饰键）会被 LL 钩子全局吞掉，必须在解析入口就拒绝；
    /// PrtSc 单键是唯一例外。
    #[test]
    fn bare_keys_are_rejected_by_parse() {
        assert_eq!(parse("A"), None);
        assert_eq!(parse("F5"), None);
        assert_eq!(parse("Space"), None);
        assert_eq!(
            parse("Ctrl+A"),
            Some(Hotkey::new(true, false, false, false, VK_A))
        );
        assert_eq!(
            parse("PrtSc"),
            Some(Hotkey::new(false, false, false, false, VK_SNAPSHOT))
        );
    }

    #[test]
    fn from_settings_replaces_bare_keys_with_defaults() {
        let stored = crate::config::Hotkeys {
            region: "A".into(),
            copy_only: "F5".into(),
            fullscreen: "PrtSc".into(),
        };
        let b = HotkeyBindings::from_settings(&stored);
        let d = HotkeyBindings::poc_defaults();
        assert_eq!(b.region, d.region);
        assert_eq!(b.copy_only, d.copy_only);
        assert_eq!(b.fullscreen, d.fullscreen);
    }

    /// 旧配置里残留的裸键绑定也必须被 decide 拦下，两个边沿都不吞。
    #[test]
    fn bare_key_binding_is_never_swallowed() {
        let b = HotkeyBindings {
            region: Hotkey::new(false, false, false, false, VK_A),
            copy_only: HotkeyBindings::poc_defaults().copy_only,
            fullscreen: HotkeyBindings::poc_defaults().fullscreen,
        };
        for key_up in [false, true] {
            let d = decide(&b, ev(VK_A, false, false, false, false, key_up));
            assert_eq!(d.action, None);
            assert!(!d.swallow, "key_up={key_up}");
        }
    }

    #[test]
    fn duplicate_plain_prtsc_gets_a_bindable_spare() {
        let stored = crate::config::Hotkeys {
            region: "PrtSc".into(),
            copy_only: "Ctrl+Alt+A".into(),
            fullscreen: "PrtSc".into(),
        };
        let b = HotkeyBindings::from_settings(&stored);
        assert_eq!(
            b.region,
            Hotkey::new(false, false, false, false, VK_SNAPSHOT)
        );
        assert_ne!(b.fullscreen, b.region);
        assert!(b.fullscreen.is_bindable(), "备用组合不能是裸键");
        assert!(b.fullscreen.has_modifier(), "备用组合应补上修饰键");
    }

    /// PrtSc 抬键时用 keydown 锁存的修饰键（而不是现场状态）判定。
    #[test]
    fn prtsc_keyup_uses_latched_mods() {
        let b = HotkeyBindings {
            region: HotkeyBindings::poc_defaults().region,
            copy_only: HotkeyBindings::poc_defaults().copy_only,
            fullscreen: Hotkey::new(true, false, false, false, VK_SNAPSHOT),
        };
        // 用户先松 Ctrl 再松 PrtSc：现场读到 0，但 keydown 锁存了 Ctrl。
        let latched = modifier_mask(true, false, false, false);
        let d = decide(&b, key_event_from_mask(VK_SNAPSHOT, latched, true));
        assert_eq!(d.action, Some(HotkeyAction::Fullscreen));
        assert!(d.swallow);
        // 若误用现场状态则不触发也不吞键，系统截图工具会弹出来。
        let live = 0;
        let d_live = decide(&b, key_event_from_mask(VK_SNAPSHOT, live, true));
        assert_eq!(d_live.action, None);
        assert!(!d_live.swallow);
    }

    #[test]
    fn modifier_mask_round_trip() {
        let mask = modifier_mask(true, true, false, true);
        let ev = key_event_from_mask(VK_A, mask, false);
        assert_eq!(
            (ev.ctrl, ev.alt, ev.shift, ev.win),
            (true, true, false, true)
        );
        assert_eq!(
            hotkey_from_mask(VK_A, mask),
            Hotkey::new(true, true, false, true, VK_A)
        );
    }
}
