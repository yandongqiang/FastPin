use std::sync::atomic::{AtomicIsize, AtomicU8, Ordering};

use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, RegisterHotKey, UnregisterHotKey, HOT_KEY_MODIFIERS, MOD_ALT, MOD_CONTROL,
    MOD_NOREPEAT, VK_CONTROL, VK_LWIN, VK_MENU, VK_RWIN, VK_SHIFT,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, PostMessageW, SetWindowsHookExW, UnhookWindowsHookEx, HHOOK, KBDLLHOOKSTRUCT,
    WH_KEYBOARD_LL, WM_KEYDOWN, WM_KEYUP, WM_SYSKEYDOWN, WM_SYSKEYUP,
};

use crate::hotkey::{
    decide, hotkey_from_mask, is_modifier_vk, key_event_from_mask, modifier_mask, vk_name, Hotkey,
    HotkeyAction, HotkeyBindings, KeyEvent, VK_SNAPSHOT,
};
use crate::platform::{WM_FASTPIN_HOTKEY, WM_FASTPIN_HOTKEY_RECORDED};

const VK_ESCAPE: u16 = 0x1B;

/// Id of the tray-less emergency quit hotkey. Kept clear of the 1..3 ids used
/// by the RegisterHotKey fallback bindings.
pub const EMERGENCY_QUIT_ID: i32 = 9;

static TARGET: AtomicIsize = AtomicIsize::new(0);
static HOOK: AtomicIsize = AtomicIsize::new(0);
static RECORD_HOOK: AtomicIsize = AtomicIsize::new(0);
static BINDINGS: std::sync::Mutex<Option<HotkeyBindings>> = std::sync::Mutex::new(None);

/// PrtSc 的修饰键锁存：keydown 写入，keyup 用 swap 读走并清零。用户常常先松
/// 修饰键再松 PrtSc，抬键现场读不到修饰键；同一时刻只有一个 PrtSc 动作在途，
/// 单个 AtomicU8 足够，swap 保证读与清一步完成。
static PRTSC_MODS: AtomicU8 = AtomicU8::new(0);

pub fn set_hook_target(hwnd: HWND, bindings: HotkeyBindings) {
    TARGET.store(hwnd.0 as isize, Ordering::SeqCst);
    *BINDINGS.lock().unwrap() = Some(bindings);
}

/// The hidden message window, so a rebind can re-apply itself without threading
/// the handle through the settings view.
pub fn hook_target() -> HWND {
    HWND(TARGET.load(Ordering::SeqCst) as *mut _)
}

/// Swaps the action hook for a recording hook. The action hook has to go, or
/// the combo being bound fires a screenshot; a plain suspend is not enough
/// because gpui produces no keystroke at all for PrtSc, so the recorder needs
/// raw virtual keys of its own.
pub fn start_recording() {
    // 新的录制会话丢弃此前（例如启动时紧急退出热键）的注册失败，避免把旧的
    // 占用误报成这次录制的结果。
    crate::hotkey::clear_fallback_register_failure();
    // 切到录制钩子前清掉上一次动作钩子可能残留的 PrtSc 锁存值。
    PRTSC_MODS.store(0, Ordering::SeqCst);
    // Clicking a second hotkey field while one is already recording must not
    // strand the first hook: overwriting RECORD_HOOK would leak the handle and
    // leave two recorders swallowing the same keys.
    unhook_recorder();
    uninstall_ll_hook();
    // 兜底模式下 RegisterHotKey 的 id 1..3 还会把同一个组合投递成 WM_HOTKEY。
    // 只卸 LL hook 不够：录制时 record_proc 记录组合的同时，hidden_proc 会立刻
    // 执行截图 / 写剪贴板。录制期间必须把整条动作通路断开，id 9（紧急退出）
    // 不在这里，仍然保持可用；stop_recording() 负责对称恢复。
    unregister_fallback_bindings(hook_target());
    unsafe {
        match SetWindowsHookExW(WH_KEYBOARD_LL, Some(record_proc), None, 0) {
            Ok(h) => {
                RECORD_HOOK.store(h.0 as isize, Ordering::SeqCst);
            }
            Err(e) => {
                eprintln!("hotkey recording unavailable: {e}");
                // 录制钩子装不上就必须把动作通路恢复：先装回 LL，LL 也失败时
                // 用当前 target/bindings 注册 RegisterHotKey 兜底，否则设置页
                // 里按什么都没有反应。
                if install_ll_hook().is_ok() {
                    unregister_fallback_bindings(hook_target());
                } else {
                    register_fallback_for_target();
                }
            }
        }
    }
}

/// Safe to call when not recording; the settings window calls it on cancel,
/// on close and on losing focus, and a stuck recorder would be far worse than
/// an extra no-op.
pub fn stop_recording() {
    if !unhook_recorder() {
        return;
    }
    if install_ll_hook().is_ok() {
        // LL 恢复后清掉可能残留的 1..3 兜底注册，避免同一次按键既产生
        // WM_FASTPIN_HOTKEY 又产生 WM_HOTKEY（双截图/双剪贴板写入）。
        // 紧急退出 id 不在这里，托盘不可用模式仍然保留它。
        unregister_fallback_bindings(hook_target());
    } else {
        // 以前这里静默忽略失败：取消/超时路径不会走 apply_bindings，LL 与
        // fallback 会同时缺席，热键从此全部失效。立即注册兜底保底。
        register_fallback_for_target();
    }
}

/// 用当前 target/bindings 注册 RegisterHotKey 兜底。BINDINGS 由
/// `set_hook_target` 在启动时写入，settings 的 apply_bindings 也会更新。
fn register_fallback_for_target() {
    let hwnd = hook_target();
    let bindings = BINDINGS.lock().ok().and_then(|g| *g);
    match bindings {
        Some(bindings) => register_fallback_hotkeys(hwnd, &bindings),
        None => eprintln!("hotkey fallback unavailable: no target/bindings set"),
    }
}

/// Returns whether a recorder was actually taken down.
fn unhook_recorder() -> bool {
    let h = RECORD_HOOK.swap(0, Ordering::SeqCst);
    if h == 0 {
        return false;
    }
    unsafe {
        let _ = UnhookWindowsHookEx(HHOOK(h as *mut _));
    }
    true
}

/// ponytail: swallows only the key it is about to bind, never modifiers and
/// never Escape. A recorder that swallowed everything would leave the keyboard
/// dead if the window ever got stuck in this mode.
unsafe extern "system" fn record_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code >= 0 && lparam.0 != 0 {
        let info = unsafe { &*(lparam.0 as *const KBDLLHOOKSTRUCT) };
        let msg = wparam.0 as u32;
        let key_up = msg == WM_KEYUP || msg == WM_SYSKEYUP;
        let key_down = msg == WM_KEYDOWN || msg == WM_SYSKEYDOWN;
        let vk = info.vkCode as u16;
        let bindable = vk_name(vk).is_some() && !is_modifier_vk(vk) && vk != VK_ESCAPE;
        if bindable && (key_up || key_down) {
            // Same edge the matcher uses, so PrtSc — which usually reports only
            // key-up — is recordable at all.
            let complete = if vk == VK_SNAPSHOT { key_up } else { key_down };
            // 和动作钩子一样：PrtSc 的抬键用 keydown 锁存的修饰键，避免先松
            // 修饰键时把组合录成裸键；swap 读完即清零。
            let mask = if vk == VK_SNAPSHOT && key_up {
                PRTSC_MODS.swap(0, Ordering::SeqCst)
            } else {
                let live = modifier_mask(
                    down(VK_CONTROL.0),
                    down(VK_MENU.0),
                    down(VK_SHIFT.0),
                    down(VK_LWIN.0) || down(VK_RWIN.0),
                );
                if vk == VK_SNAPSHOT && key_down {
                    PRTSC_MODS.store(live, Ordering::SeqCst);
                }
                live
            };
            let hotkey = hotkey_from_mask(vk, mask);
            let valid = hotkey.is_bindable();
            // 裸键不投递也不吞：无效绑定不该被保存，也不该吃掉普通输入。
            if complete && valid {
                unsafe {
                    let _ = PostMessageW(
                        Some(hook_target()),
                        WM_FASTPIN_HOTKEY_RECORDED,
                        WPARAM(pack_hotkey(hotkey)),
                        LPARAM(0),
                    );
                }
            }
            // PrtSc 两个边沿都吞，否则系统截图覆盖层会在设置页后面弹出来；
            // 有效组合照旧吞。裸键不再吞，录制卡住时也不会吃掉普通输入。
            if vk == VK_SNAPSHOT || valid {
                return LRESULT(1);
            }
        }
    }
    unsafe { CallNextHookEx(None, code, wparam, lparam) }
}

/// True while one of our keyboard hooks is still in the chain. The hooks
/// deliver a combo as `WM_FASTPIN_HOTKEY`, the RegisterHotKey fallback as
/// `WM_HOTKEY`; both being live makes `hidden_proc` post the same action twice
/// (two fullscreen PNGs, two clipboard writes). Recording also counts: its hook
/// is temporarily swapped in during a rebind, so the fallback must not sneak
/// back in at that moment either.
fn hook_chain_active() -> bool {
    HOOK.load(Ordering::SeqCst) != 0 || RECORD_HOOK.load(Ordering::SeqCst) != 0
}

/// Re-applies a rebind to the active path. The LL hook reads `BINDINGS` directly,
/// so when it (or the recorder) is installed there is nothing to refresh in the
/// desktop hotkey table — registering the fallback as well would double-fire.
/// Only in fallback mode does the desktop table need the new bindings.
pub fn apply_bindings(bindings: HotkeyBindings) {
    let hwnd = hook_target();
    set_hook_target(hwnd, bindings);
    if hook_chain_active() {
        return;
    }
    register_fallback_hotkeys(hwnd, &bindings);
}

pub fn pack_hotkey(h: Hotkey) -> usize {
    let mods =
        (h.ctrl as usize) | (h.alt as usize) << 1 | (h.shift as usize) << 2 | (h.win as usize) << 3;
    (mods << 16) | h.vk as usize
}

pub fn unpack_hotkey(packed: usize) -> Hotkey {
    let mods = packed >> 16;
    Hotkey::new(
        mods & 1 != 0,
        mods & 2 != 0,
        mods & 4 != 0,
        mods & 8 != 0,
        (packed & 0xFFFF) as u16,
    )
}

pub fn install_ll_hook() -> windows::core::Result<HHOOK> {
    unsafe {
        let h = SetWindowsHookExW(WH_KEYBOARD_LL, Some(hook_proc), None, 0)?;
        HOOK.store(h.0 as isize, Ordering::SeqCst);
        Ok(h)
    }
}

pub fn uninstall_ll_hook() {
    let h = HOOK.swap(0, Ordering::SeqCst);
    if h != 0 {
        unsafe {
            let _ = UnhookWindowsHookEx(HHOOK(h as *mut _));
        }
    }
}

unsafe extern "system" fn hook_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code >= 0 && lparam.0 != 0 {
        let info = &*(lparam.0 as *const KBDLLHOOKSTRUCT);
        let msg = wparam.0 as u32;
        let key_up = msg == WM_KEYUP || msg == WM_SYSKEYUP;
        let key_down = msg == WM_KEYDOWN || msg == WM_SYSKEYDOWN;
        if key_up || key_down {
            let vk = info.vkCode as u16;
            // PrtSc 的抬键可能发生在修饰键松开之后；keydown 时锁存修饰键，
            // keyup 用锁存值判定并清零，避免组合失效、系统截图工具弹出。
            let ev = if vk == VK_SNAPSHOT && key_up {
                key_event_from_mask(vk, PRTSC_MODS.swap(0, Ordering::SeqCst), true)
            } else {
                let ctrl = down(VK_CONTROL.0);
                let alt = down(VK_MENU.0);
                let shift = down(VK_SHIFT.0);
                let win = down(VK_LWIN.0) || down(VK_RWIN.0);
                if vk == VK_SNAPSHOT && key_down {
                    PRTSC_MODS.store(modifier_mask(ctrl, alt, shift, win), Ordering::SeqCst);
                }
                KeyEvent {
                    vk,
                    ctrl,
                    alt,
                    shift,
                    win,
                    key_up,
                }
            };
            if let Some(bindings) = BINDINGS.lock().ok().and_then(|g| *g) {
                let d = decide(&bindings, ev);
                if d.swallow {
                    if let Some(action) = d.action {
                        let hwnd = HWND(TARGET.load(Ordering::SeqCst) as *mut _);
                        let _ = PostMessageW(
                            Some(hwnd),
                            WM_FASTPIN_HOTKEY,
                            WPARAM(action_wparam(action)),
                            LPARAM(0),
                        );
                    }
                    return LRESULT(1);
                }
            }
        }
    }
    unsafe { CallNextHookEx(None, code, wparam, lparam) }
}

fn down(vk: u16) -> bool {
    unsafe { GetAsyncKeyState(vk as i32) as u16 & 0x8000 != 0 }
}

pub fn action_wparam(action: HotkeyAction) -> usize {
    match action {
        HotkeyAction::Region => 1,
        HotkeyAction::CopyOnly => 2,
        HotkeyAction::Fullscreen => 3,
    }
}

pub fn action_from_wparam(wp: usize) -> Option<HotkeyAction> {
    match wp {
        1 => Some(HotkeyAction::Region),
        2 => Some(HotkeyAction::CopyOnly),
        3 => Some(HotkeyAction::Fullscreen),
        _ => None,
    }
}

/// RegisterHotKey 的失败不再被 `let _ =` 丢掉：记到 hotkey 模块，设置页录制
/// 完成后取走并显示明确错误。
fn record_register_result(name: &str, result: windows::core::Result<()>) {
    if let Err(e) = result {
        crate::hotkey::note_fallback_register_failure(&format!("{name}（{e}）"));
    }
}

/// Desktop fallback if the LL hook cannot be installed.
pub fn register_fallback_hotkeys(hwnd: HWND, bindings: &HotkeyBindings) {
    // 每次（重）注册都从干净状态开始：只有本次的失败才该显示给用户。
    crate::hotkey::clear_fallback_register_failure();
    unsafe {
        let _ = UnregisterHotKey(Some(hwnd), 1);
        let _ = UnregisterHotKey(Some(hwnd), 2);
        let _ = UnregisterHotKey(Some(hwnd), 3);
    }
    record_register_result("区域截图", register_one(hwnd, 1, bindings.region));
    record_register_result("仅复制", register_one(hwnd, 2, bindings.copy_only));
    record_register_result("全屏截图", register_one(hwnd, 3, bindings.fullscreen));
}

fn register_one(hwnd: HWND, id: i32, hotkey: Hotkey) -> windows::core::Result<()> {
    let mut mods = MOD_NOREPEAT;
    if hotkey.ctrl {
        mods |= MOD_CONTROL;
    }
    if hotkey.alt {
        mods |= MOD_ALT;
    }
    if hotkey.shift {
        mods |= HOT_KEY_MODIFIERS(0x0004); // MOD_SHIFT
    }
    if hotkey.win {
        mods |= HOT_KEY_MODIFIERS(0x0008); // MOD_WIN
    }
    // PrtSc with no modifiers is valid.
    let vk = if hotkey.vk == 0 {
        VK_SNAPSHOT
    } else {
        hotkey.vk
    };
    unsafe { RegisterHotKey(Some(hwnd), id, mods, vk as u32) }
}

/// Tray-less fallback: the tray Exit menu is normally the only quit path, so
/// when the tray icon is unavailable we register a dedicated quit hotkey
/// (Ctrl+Alt+Shift+Q) that posts `WM_HOTKEY` with `EMERGENCY_QUIT_ID`.
pub fn register_emergency_quit(hwnd: HWND) {
    const VK_Q: u32 = 0x51;
    let mods = MOD_CONTROL | MOD_ALT | HOT_KEY_MODIFIERS(0x0004) | MOD_NOREPEAT;
    // 这里本身就是 RegisterHotKey 兜底（LL 钩子不处理退出键），无需再套一层。
    let result = unsafe { RegisterHotKey(Some(hwnd), EMERGENCY_QUIT_ID, mods, VK_Q) };
    let ok = result.is_ok();
    // 成败写入独立的 AtomicBool：失败文案会被重注册 / 新录制会话清空，
    // app.rs 不能靠它判断这个热键到底能不能用。
    crate::hotkey::note_emergency_quit_registered(ok);
    if let Err(e) = result {
        crate::hotkey::note_fallback_register_failure(&format!("紧急退出热键（{e}）"));
    }
    crate::perf::log_app(&format!(
        "emergency quit hotkey Ctrl+Alt+Shift+Q registered={ok}"
    ));
}

/// 只注销 1..3 的绑定兜底热键，保留托盘不可用模式注册的紧急退出热键。
fn unregister_fallback_bindings(hwnd: HWND) {
    unsafe {
        let _ = UnregisterHotKey(Some(hwnd), 1);
        let _ = UnregisterHotKey(Some(hwnd), 2);
        let _ = UnregisterHotKey(Some(hwnd), 3);
    }
}

pub fn unregister_fallback_hotkeys(hwnd: HWND) {
    unregister_fallback_bindings(hwnd);
    unsafe {
        let _ = UnregisterHotKey(Some(hwnd), EMERGENCY_QUIT_ID);
    }
    crate::hotkey::note_emergency_quit_registered(false);
}
