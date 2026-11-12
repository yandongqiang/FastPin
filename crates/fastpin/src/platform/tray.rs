use std::fs::{create_dir_all, OpenOptions};
use std::io::Write;
use std::path::PathBuf;

use windows::core::{GUID, PCWSTR};
use windows::Win32::Foundation::{HWND, LPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Shell::{
    Shell_NotifyIconW, NIF_GUID, NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE,
    NIM_SETVERSION, NOTIFYICONDATAW, NOTIFYICON_VERSION_4,
};
use windows::Win32::UI::WindowsAndMessaging::{
    ChangeWindowMessageFilterEx, LoadIconW, LoadImageW, RegisterWindowMessageW, HICON,
    IDI_APPLICATION, IMAGE_ICON, LR_DEFAULTSIZE, LR_SHARED, MSGFLT_ALLOW, WM_LBUTTONUP,
    WM_RBUTTONUP,
};

use crate::error::Result;
use crate::hotkey;
use crate::platform::tray_menu::{
    ID_CLOSE_ALL, ID_COPY, ID_EXIT, ID_FULL, ID_REGION, ID_SETTINGS,
};
use crate::platform::WM_TRAYICON;

/// Stable identity for the notification icon. Windows keys the user's
/// "always show" setting on a GUID, and a GUID (rather than the per-process
/// `(hWnd, uID)` pair) survives crashes that skip `NIM_DELETE`, so a stale
/// "ghost" entry cannot block the next `NIM_ADD`. Once an icon is registered
/// with `NIF_GUID`, *every* later call for it (delete/modify/setversion) must
/// carry the same GUID, so add and remove share this constant.
const TRAY_GUID: GUID = GUID::from_values(
    0x5A7D31C9,
    0xB642,
    0x4E18,
    [0x9F, 0x0C, 0x71, 0xD3, 0x2A, 0x86, 0x4B, 0x0E],
);
const TRAY_UID: u32 = 1;

#[derive(Clone, Copy, Debug)]
pub enum TrayCommand {
    Region,
    Fullscreen,
    CopyOnly,
    CloseAllPins,
    Settings,
    Exit,
}

/// Explorer broadcasts this after a restart; every shell icon is gone by then and
/// has to be re-added, otherwise the tray entry disappears for the process lifetime.
pub fn taskbar_created_message() -> u32 {
    static MSG: std::sync::OnceLock<u32> = std::sync::OnceLock::new();
    *MSG.get_or_init(|| unsafe {
        let name: Vec<u16> = "TaskbarCreated"
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        RegisterWindowMessageW(PCWSTR(name.as_ptr()))
    })
}

/// Embedded by `build.rs` as resource 1 (`winres` `set_icon`).
// MAKEINTRESOURCEW(1) puts the resource id in the pointer slot by design; it
// is not a dereferenceable address, which is exactly what the lint assumes.
#[allow(clippy::manual_dangling_ptr)]
pub fn load_app_icon() -> Result<HICON> {
    unsafe {
        let module = GetModuleHandleW(None)?;
        let handle = LoadImageW(
            Some(module.into()),
            PCWSTR(1 as *const u16),
            IMAGE_ICON,
            0,
            0,
            LR_DEFAULTSIZE | LR_SHARED,
        );
        match handle {
            Ok(h) => Ok(HICON(h.0)),
            Err(_) => Ok(LoadIconW(None, IDI_APPLICATION)?),
        }
    }
}

/// Release builds have no console, so every shell step is mirrored into
/// `%LOCALAPPDATA%\FastPin\logs\tray.log` through its own append+flush channel.
/// It deliberately does NOT rely on `perf::log_app`: field reports showed
/// app.log full of capture/clipboard lines but no `[tray]` ladder, so tray
/// diagnostics get an independent file with no buffering or global state.
/// If the profile dir is missing or denied (locked-down / sandboxed token),
/// fall back to a file next to the executable so the ladder still lands.
/// Best-effort: a logging failure must never affect tray startup.
fn tray_log(msg: &str) {
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let line = format!("t={ts} [tray] {msg}\n");

    let mut targets: Vec<PathBuf> = Vec::with_capacity(2);
    if let Some(base) = std::env::var_os("LOCALAPPDATA") {
        targets.push(
            PathBuf::from(base)
                .join("FastPin")
                .join("logs")
                .join("tray.log"),
        );
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            targets.push(dir.join("tray.log"));
        }
    }
    for path in targets {
        if let Some(parent) = path.parent() {
            if create_dir_all(parent).is_err() {
                continue;
            }
        }
        if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(&path) {
            let _ = f.write_all(line.as_bytes());
            let _ = f.flush();
            break;
        }
    }

    // Legacy shared channel, kept so app.log still shows the ladder where it
    // is writable; eprintln is only useful in dev / with an attached console.
    crate::perf::log_app(&format!("[tray] {msg}"));
    eprintln!("[tray] {msg}");
}

/// Build a fully-populated NID. `cb_size` / `use_guid` are parameterised so the
/// caller can try several struct versions and identities; never pair a small
/// (pre-V3) cbSize with `use_guid`, since `guidItem` lies past that boundary.
fn make_nid(hwnd: HWND, icon: HICON, cb_size: u32, use_guid: bool) -> NOTIFYICONDATAW {
    let mut flags = NIF_MESSAGE | NIF_ICON | NIF_TIP;
    if use_guid {
        flags |= NIF_GUID;
    }
    let mut nid = NOTIFYICONDATAW {
        cbSize: cb_size,
        hWnd: hwnd,
        uID: TRAY_UID,
        uFlags: flags,
        uCallbackMessage: WM_TRAYICON,
        hIcon: icon,
        ..Default::default()
    };
    let tip: Vec<u16> = "FastPin — 左键截图\0".encode_utf16().collect();
    nid.szTip[..tip.len()].copy_from_slice(&tip);
    if use_guid {
        nid.guidItem = TRAY_GUID;
    }
    nid
}

/// Best-effort `NIM_DELETE` under one identity. Returns the HRESULT (0 = ok).
fn delete_icon(hwnd: HWND, use_guid: bool) -> u32 {
    unsafe {
        let mut nid = NOTIFYICONDATAW {
            cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
            hWnd: hwnd,
            uID: TRAY_UID,
            ..Default::default()
        };
        if use_guid {
            nid.uFlags = NIF_GUID;
            nid.guidItem = TRAY_GUID;
        }
        match Shell_NotifyIconW(NIM_DELETE, &nid).ok() {
            Ok(()) => 0,
            Err(e) => e.code().0 as u32,
        }
    }
}

/// `NIM_SETVERSION` -> `NOTIFYICON_VERSION_4`. Under both v3 and v4 the low
/// word of the callback lParam is the mouse message, so `handle_tray` keeps
/// working. Best-effort; returns the HRESULT (0 = ok).
fn set_version(hwnd: HWND, icon: HICON, use_guid: bool) -> u32 {
    unsafe {
        let mut nid = make_nid(
            hwnd,
            icon,
            std::mem::size_of::<NOTIFYICONDATAW>() as u32,
            use_guid,
        );
        nid.Anonymous.uVersion = NOTIFYICON_VERSION_4;
        match Shell_NotifyIconW(NIM_SETVERSION, &nid).ok() {
            Ok(()) => 0,
            Err(e) => e.code().0 as u32,
        }
    }
}

pub fn add_tray_icon(hwnd: HWND) -> Result<()> {
    unsafe {
        let icon = load_app_icon()?;
        let cb_full = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
        // Version-sized cbSizes: V3 ends before hBalloonIcon, V1 before szInfo.
        // The working reference (tray-icon crate) uses cb_full, so that is the
        // primary; the smaller ones are diagnostic fallbacks in case a shell
        // rejects the V4-sized struct when only V1 flags are set.
        let cb_v3 = std::mem::offset_of!(NOTIFYICONDATAW, hBalloonIcon) as u32;
        let cb_v1 = std::mem::offset_of!(NOTIFYICONDATAW, szInfo) as u32;
        tray_log(&format!(
            "begin hwnd={:?} icon={:?} cb_full={cb_full} cb_v3={cb_v3} cb_v1={cb_v1} \
             guid={:08X}-{:04X}-{:04X} uid={TRAY_UID} cbMsg=0x{:04X}",
            hwnd.0, icon.0, TRAY_GUID.data1, TRAY_GUID.data2, TRAY_GUID.data3, WM_TRAYICON,
        ));

        // (1) UIPI: let the shell's TaskbarCreated broadcast and our private
        //     callback message reach this window across integrity levels.
        //     Mirrors the working tray-icon crate.
        let tbc = taskbar_created_message();
        let f_tbc = ChangeWindowMessageFilterEx(hwnd, tbc, MSGFLT_ALLOW, None);
        let f_cb = ChangeWindowMessageFilterEx(hwnd, WM_TRAYICON, MSGFLT_ALLOW, None);
        tray_log(&format!(
            "ChangeWindowMessageFilterEx taskbar_ok={} callback_ok={}",
            f_tbc.is_ok(),
            f_cb.is_ok(),
        ));

        // (2) Ladder of identity / struct-version combinations. The first
        //     entry is byte-for-byte the original FastPin NIM_ADD and what the
        //     working `tray-icon` crate does: plain (hWnd,uID) identity, full
        //     cbSize, NIF_MESSAGE|NIF_ICON|NIF_TIP, no NIF_GUID. Keep it first
        //     so a healthy shell accepts the icon before any GUID-based call
        //     touches the notification area; the GUID variants stay as fallbacks.
        let strategies: [(&str, u32, bool, usize); 4] = [
            ("A/uid+full", cb_full, false, 3),
            ("B/uid+v3", cb_v3, false, 2),
            ("C/uid+v1", cb_v1, false, 2),
            ("D/guid+full", cb_full, true, 3),
        ];
        let mut last_hr: u32 = 0;
        for (label, cb, use_guid, retries) in strategies {
            let nid = make_nid(hwnd, icon, cb, use_guid);
            for attempt in 0..retries {
                match Shell_NotifyIconW(NIM_ADD, &nid).ok() {
                    Ok(()) => {
                        tray_log(&format!(
                            "NIM_ADD OK label={label} cb={cb} flags=0x{:X} guid={use_guid} attempt={attempt}",
                            nid.uFlags.0,
                        ));
                        let hr = set_version(hwnd, icon, use_guid);
                        tray_log(&format!("NIM_SETVERSION(uVersion=4) hr=0x{hr:08X}"));
                        return Ok(());
                    }
                    Err(e) => {
                        last_hr = e.code().0 as u32;
                        tray_log(&format!(
                            "NIM_ADD FAIL label={label} cb={cb} flags=0x{:X} guid={use_guid} attempt={attempt} hr=0x{last_hr:08X}",
                            nid.uFlags.0,
                        ));
                    }
                }
                if attempt + 1 < retries {
                    std::thread::sleep(std::time::Duration::from_millis(50 * (attempt as u64 + 1)));
                }
            }
        }

        // (3) Every add was refused. Clear a ghost entry left by a previous run
        //     (or a GUID icon registered at a higher integrity level) and make
        //     one final attempt with the original plain identity. Both deletes
        //     are best-effort; their errors are expected and non-fatal.
        let del_guid = delete_icon(hwnd, true);
        let del_uid = delete_icon(hwnd, false);
        tray_log(&format!(
            "NIM_DELETE cleanup guid_hr=0x{del_guid:08X} uid_hr=0x{del_uid:08X}",
        ));
        let nid = make_nid(hwnd, icon, cb_full, false);
        for attempt in 0..3 {
            match Shell_NotifyIconW(NIM_ADD, &nid).ok() {
                Ok(()) => {
                    tray_log(&format!("NIM_ADD OK after cleanup attempt={attempt}"));
                    let hr = set_version(hwnd, icon, false);
                    tray_log(&format!("NIM_SETVERSION(uVersion=4) hr=0x{hr:08X}"));
                    return Ok(());
                }
                Err(e) => {
                    last_hr = e.code().0 as u32;
                    tray_log(&format!(
                        "NIM_ADD after cleanup FAIL attempt={attempt} hr=0x{last_hr:08X}",
                    ));
                }
            }
            if attempt + 1 < 3 {
                std::thread::sleep(std::time::Duration::from_millis(50 * (attempt as u64 + 1)));
            }
        }
        tray_log(&format!(
            "NIM_ADD exhausted; all strategies denied, last_hr=0x{last_hr:08X}; tray icon unavailable",
        ));
        Err(crate::error::Error::new(format!(
            "Shell_NotifyIconW(NIM_ADD) failed (0x{last_hr:08X}); see app.log for the full ladder",
        )))
    }
}

pub fn remove_tray_icon(hwnd: HWND) {
    // A GUID-registered icon is not found by a plain (hWnd,uID) delete and
    // vice versa, so try both. Best-effort at shutdown.
    let g = delete_icon(hwnd, true);
    let u = delete_icon(hwnd, false);
    tray_log(&format!(
        "remove_tray_icon guid_hr=0x{g:08X} uid_hr=0x{u:08X}",
    ));
}

pub fn handle_tray(hwnd: HWND, lparam: LPARAM) -> Option<TrayCommand> {
    let msg = lparam.0 as u32 & 0xFFFF;
    if msg == WM_RBUTTONUP {
        return show_menu(hwnd);
    }
    if msg == WM_LBUTTONUP {
        return Some(TrayCommand::Region);
    }
    None
}

fn show_menu(hwnd: HWND) -> Option<TrayCommand> {
    let hotkeys = crate::config::get().hotkeys;
    let items = crate::platform::tray_menu::build(
        &hotkey_hint(&hotkeys.region),
        &hotkey_hint(&hotkeys.fullscreen),
        &hotkey_hint(&hotkeys.copy_only),
        crate::pin::count(),
    );
    let cmd = crate::platform::tray_menu::track(hwnd, items)?;
    Some(match cmd {
        ID_REGION => TrayCommand::Region,
        ID_FULL => TrayCommand::Fullscreen,
        ID_COPY => TrayCommand::CopyOnly,
        ID_CLOSE_ALL => TrayCommand::CloseAllPins,
        ID_SETTINGS => TrayCommand::Settings,
        ID_EXIT => TrayCommand::Exit,
        _ => return None,
    })
}

/// Formats a registered hotkey for the right-aligned shortcut column. Bindings
/// are re-read on every popup so a rebind shows up without a restart.
fn hotkey_hint(stored: &str) -> String {
    hotkey::parse(stored).map(hotkey::format).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    /// The exe icon, the hidden window icon and the tray icon all come from
    /// the resource `build.rs` embeds out of this file. Pinning the sizes
    /// catches a regenerated ICO that lost the small frames Explorer and the
    /// notification area actually pick.
    #[test]
    fn icon_source_has_the_required_sizes() {
        let bytes = std::fs::read(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../assets/logo/fastpin.ico"
        ))
        .expect("fastpin.ico is checked in");
        assert_eq!(&bytes[0..4], &[0, 0, 1, 0], "ICONDIR header");
        let count = u16::from_le_bytes([bytes[4], bytes[5]]) as usize;
        assert!(count >= 7, "expected 7 frames, found {count}");
        let mut sizes = Vec::new();
        for i in 0..count {
            // GRPICONDIRENTRY is 14 bytes: w, h, colours, reserved, planes,
            // bpp, bytes, resource id.
            let dim = bytes[6 + i * 14];
            sizes.push(if dim == 0 { 256 } else { dim as u32 });
        }
        for want in [16, 32, 256] {
            assert!(sizes.contains(&want), "missing {want}px frame in {sizes:?}");
        }
    }
}
