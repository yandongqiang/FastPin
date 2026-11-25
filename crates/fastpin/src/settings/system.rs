//! The bits of the general page that touch Windows rather than settings.json.

use std::ffi::c_void;
use std::path::{Path, PathBuf};

use windows::core::PCWSTR;
use windows::Win32::Foundation::ERROR_CANCELLED;
use windows::Win32::System::Com::{CoCreateInstance, CoTaskMemFree, CLSCTX_INPROC_SERVER};
use windows::Win32::System::Registry::{
    RegDeleteKeyValueW, RegGetValueW, RegSetKeyValueW, HKEY_CURRENT_USER, REG_SZ, RRF_RT_REG_SZ,
};
use windows::Win32::UI::Shell::{
    FileOpenDialog, IFileOpenDialog, IShellItem, SHCreateItemFromParsingName, ShellExecuteW,
    FOS_PICKFOLDERS, SIGDN_FILESYSPATH,
};
use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

use crate::config::shell_path;
use crate::error::{Error, Result};

const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
const RUN_VALUE: &str = "FastPin";
/// Pre-rebrand value name; `migrate_legacy_run_value` moves it over.
const LEGACY_RUN_VALUE: &str = "Zshot";

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// `%LOCALAPPDATA%\FastPin` — settings.json and the log folder live under it.
pub fn data_dir() -> Result<PathBuf> {
    let base = std::env::var("LOCALAPPDATA").map_err(|_| Error::new("LOCALAPPDATA is not set"))?;
    Ok(PathBuf::from(base).join("FastPin"))
}

/// The registry is the source of truth: the user can drop the entry from Task
/// Manager's startup tab, and a cached copy in settings.json would then lie.
pub fn autostart_enabled() -> bool {
    let mut buf = [0u16; 1024];
    let mut len = (buf.len() * 2) as u32;
    let status = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            PCWSTR(wide(RUN_KEY).as_ptr()),
            PCWSTR(wide(RUN_VALUE).as_ptr()),
            RRF_RT_REG_SZ,
            None,
            Some(buf.as_mut_ptr() as *mut c_void),
            Some(&mut len),
        )
    };
    status.is_ok()
}

pub fn set_autostart(enabled: bool) -> Result<()> {
    let key = wide(RUN_KEY);
    let name = wide(RUN_VALUE);
    if !enabled {
        // Absent already is the desired state; a real delete failure must be
        // reported instead of letting the UI claim success while the checkbox
        // keeps showing enabled after a re-read.
        let status = unsafe {
            RegDeleteKeyValueW(
                HKEY_CURRENT_USER,
                PCWSTR(key.as_ptr()),
                PCWSTR(name.as_ptr()),
            )
        };
        // WIN32_ERROR is a plain newtype: 0 means success, ERROR_FILE_NOT_FOUND
        // means already absent. Any other code is a real failure to report.
        if status != windows::Win32::Foundation::ERROR_SUCCESS
            && status != windows::Win32::Foundation::ERROR_FILE_NOT_FOUND
        {
            return Err(crate::error::Error::new(format!(
                "关闭开机启动失败：{}",
                status.0
            )));
        }
        return Ok(());
    }
    let exe = std::env::current_exe()?;
    // Quoted: the path has spaces under "Program Files", and an unquoted Run
    // entry would launch the first token instead.
    let command = wide(&format!("\"{}\"", exe.display()));
    let bytes = command.len() * 2;
    unsafe {
        RegSetKeyValueW(
            HKEY_CURRENT_USER,
            PCWSTR(key.as_ptr()),
            PCWSTR(name.as_ptr()),
            REG_SZ.0,
            Some(command.as_ptr() as *const c_void),
            bytes as u32,
        )
        .ok()?;
    }
    Ok(())
}

/// Rebrand migration for the autostart entry: the old build registered under
/// the pre-rebrand value name, which the new build no longer reads. Only touches
/// the registry when that value exists, so users who never enabled autostart
/// are unaffected. Idempotent: after the move the legacy value is gone.
pub(crate) fn migrate_legacy_run_value() -> Vec<String> {
    let key = wide(RUN_KEY);
    let legacy = wide(LEGACY_RUN_VALUE);
    let mut buf = [0u16; 1024];
    let mut len = (buf.len() * 2) as u32;
    let found = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            PCWSTR(key.as_ptr()),
            PCWSTR(legacy.as_ptr()),
            RRF_RT_REG_SZ,
            None,
            Some(buf.as_mut_ptr() as *mut c_void),
            Some(&mut len),
        )
    };
    if found.is_err() {
        return Vec::new();
    }
    let Ok(exe) = std::env::current_exe() else {
        return vec!["autostart: legacy entry found but current_exe failed".into()];
    };
    let name = wide(RUN_VALUE);
    let command = wide(&format!("\"{}\"", exe.display()));
    let write = unsafe {
        RegSetKeyValueW(
            HKEY_CURRENT_USER,
            PCWSTR(key.as_ptr()),
            PCWSTR(name.as_ptr()),
            REG_SZ.0,
            Some(command.as_ptr() as *const c_void),
            (command.len() * 2) as u32,
        )
    };
    if write.is_err() {
        return vec!["autostart: could not write FastPin entry".into()];
    }
    let removed = unsafe {
        RegDeleteKeyValueW(
            HKEY_CURRENT_USER,
            PCWSTR(key.as_ptr()),
            PCWSTR(legacy.as_ptr()),
        )
    };
    if removed.is_ok() {
        vec!["autostart: moved legacy -> FastPin".into()]
    } else {
        vec!["autostart: FastPin written, legacy entry not removed".into()]
    }
}

pub fn open_folder(path: &Path) -> Result<()> {
    // 建目录可以继续用 `\\?\` 长路径，但 ShellExecuteW 不解析该前缀，
    // 传进去会静默失败；这里统一剥掉前缀（普通路径原样返回）。
    std::fs::create_dir_all(path)?;
    let display = shell_path(path);
    let verb = wide("open");
    let target = wide(&display.display().to_string());
    let result = unsafe {
        ShellExecuteW(
            None,
            PCWSTR(verb.as_ptr()),
            PCWSTR(target.as_ptr()),
            PCWSTR::null(),
            PCWSTR::null(),
            SW_SHOWNORMAL,
        )
    };
    // ShellExecuteW returns a fake HINSTANCE; <= 32 is the error range.
    if result.0 as usize <= 32 {
        return Err(Error::new(format!("无法打开 {}", display.display())));
    }
    Ok(())
}

/// Deletes the files in the log folder, keeping the folder itself. Returns how
/// many went away so the page can say something more useful than "done".
pub fn clear_logs() -> Result<usize> {
    let dir = crate::perf::log_dir()?;
    if !dir.is_dir() {
        return Ok(0);
    }
    let mut removed = 0;
    for entry in std::fs::read_dir(&dir)? {
        let path = entry?.path();
        // A log the running process still holds open cannot be deleted on
        // Windows; skipping it is better than aborting the whole sweep.
        if path.is_file() && std::fs::remove_file(&path).is_ok() {
            removed += 1;
        }
    }
    Ok(removed)
}

/// Total bytes under `path`, a bounded number of subdirectory levels deep —
/// enough for the data folder and the log folder without walking an entire
/// drive (and without recursing forever through a self-pointing junction).
pub fn dir_size(path: &Path) -> u64 {
    /// `metadata()` 会跟随 junction/符号链接；`symlink_metadata()` 不会，
    /// 因此 reparse point 一律跳过。再加上深度上限，目录成环时递归必然收敛，
    /// 不会在 `panic = "abort"` 下把整个进程爆栈带走。
    fn walk(path: &Path, depth: u32) -> u64 {
        const MAX_DEPTH: u32 = 8;
        let Ok(entries) = std::fs::read_dir(path) else {
            return 0;
        };
        entries
            .filter_map(|e| e.ok())
            .map(|e| {
                let path = e.path();
                let Ok(md) = std::fs::symlink_metadata(&path) else {
                    return 0;
                };
                if md.file_type().is_symlink() {
                    return 0;
                }
                if md.is_file() {
                    md.len()
                } else if md.is_dir() && depth < MAX_DEPTH {
                    walk(&path, depth + 1)
                } else {
                    0
                }
            })
            .sum()
    }
    walk(path, 0)
}

/// Folder picker for the save-location card. `Ok(None)` is the user pressing
/// cancel, which must not read as a failure — every other error does.
pub fn pick_folder(current: &Path) -> Result<Option<PathBuf>> {
    unsafe {
        let dialog: IFileOpenDialog =
            CoCreateInstance(&FileOpenDialog, None, CLSCTX_INPROC_SERVER)?;
        dialog.SetOptions(dialog.GetOptions()? | FOS_PICKFOLDERS)?;
        // Open where the user already saves. A path that no longer exists just
        // leaves the dialog at the shell default. SHCreateItemFromParsingName
        // 同样不解析 `\\?\`，先还原成普通路径，否则初始目录会被悄悄忽略。
        let start = wide(&shell_path(current).display().to_string());
        if let Ok(item) =
            SHCreateItemFromParsingName::<_, _, IShellItem>(PCWSTR(start.as_ptr()), None)
        {
            let _ = dialog.SetFolder(&item);
        }
        if let Err(e) = dialog.Show(None) {
            if e.code() == ERROR_CANCELLED.to_hresult() {
                return Ok(None);
            }
            return Err(e.into());
        }
        let pw = dialog.GetResult()?.GetDisplayName(SIGDN_FILESYSPATH)?;
        // Freed before the `?`: an early return here would leak the COM string.
        let text = pw.to_string();
        CoTaskMemFree(Some(pw.0 as *const _));
        Ok(Some(PathBuf::from(
            text.map_err(|e| Error::new(e.to_string()))?,
        )))
    }
}

/// Image picker for the watermark asset. `Ok(None)` is a cancel, like
/// `pick_folder`. A broken or cancelled pick never touches the stored path.
pub fn pick_image(current: &Path) -> Result<Option<PathBuf>> {
    unsafe {
        let dialog: IFileOpenDialog =
            CoCreateInstance(&FileOpenDialog, None, CLSCTX_INPROC_SERVER)?;
        let start = wide(&shell_path(current).display().to_string());
        if let Ok(item) =
            SHCreateItemFromParsingName::<_, _, IShellItem>(PCWSTR(start.as_ptr()), None)
        {
            let _ = dialog.SetFolder(&item);
        }
        if let Err(e) = dialog.Show(None) {
            if e.code() == ERROR_CANCELLED.to_hresult() {
                return Ok(None);
            }
            return Err(e.into());
        }
        let pw = dialog.GetResult()?.GetDisplayName(SIGDN_FILESYSPATH)?;
        let text = pw.to_string();
        CoTaskMemFree(Some(pw.0 as *const _));
        Ok(Some(PathBuf::from(
            text.map_err(|e| Error::new(e.to_string()))?,
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch_dir(tag: &str) -> PathBuf {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let path = std::env::temp_dir().join(format!(
            "fastpin-dir-size-{tag}-{}-{stamp}",
            std::process::id()
        ));
        std::fs::create_dir_all(&path).expect("scratch dir");
        path
    }

    #[test]
    fn dir_size_counts_files_in_nested_subdirectories() {
        let root = scratch_dir("sum");
        std::fs::write(root.join("a.bin"), vec![0u8; 10]).expect("write root file");
        let child = root.join("child");
        std::fs::create_dir_all(&child).expect("child dir");
        std::fs::write(child.join("b.bin"), vec![0u8; 25]).expect("write child file");
        assert_eq!(dir_size(&root), 35, "files at both levels are counted");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn dir_size_is_bounded_by_the_depth_limit() {
        let root = scratch_dir("depth");
        let mut dir = root.clone();
        for level in 0..12 {
            dir = dir.join(format!("level{level}"));
            std::fs::create_dir_all(&dir).expect("nested dir");
            std::fs::write(dir.join("payload.bin"), vec![0u8; 4]).expect("payload");
        }
        // 超过上限的层级不再计入；关键是递归有界、不会爆栈。
        let size = dir_size(&root);
        assert!(size <= 12 * 4, "depth-limited sum stays bounded: {size}");
        assert!(size >= 8 * 4, "levels within the limit are counted: {size}");
        let _ = std::fs::remove_dir_all(&root);
    }
}
