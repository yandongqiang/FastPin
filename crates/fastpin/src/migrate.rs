//! One-shot migration from the pre-rebrand data locations.
//!
//! Called at the top of `run()`, before the settings file is read, so a user
//! upgrading in place keeps their settings, history, autostart entry and
//! translation key. Every step is best-effort and idempotent:
//!
//! * a destination that already holds real data is never touched, so repeated
//!   starts cannot copy over newer data; an empty leftover folder does not
//!   block the one-time migration;
//! * the legacy folder is kept when `rename` fails and the copy succeeds, so a
//!   half-migrated profile is never the only copy;
//! * failures are logged and never abort startup.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Folder name the old build used under `%LOCALAPPDATA%` and `Pictures`.
const LEGACY_DIR: &str = "Zshot";
/// Folder name this build uses in those two places.
const DIR: &str = "FastPin";

/// Runs every migration step and logs what happened. Returns the notes, which
/// tests inspect.
pub fn migrate_legacy_paths() -> Vec<String> {
    let mut notes = Vec::new();
    let mut complete = true;

    if let Some(base) = std::env::var_os("LOCALAPPDATA").map(PathBuf::from) {
        match migrate_dir(&base.join(LEGACY_DIR), &base.join(DIR)) {
            Ok(n) => notes.extend(n),
            Err(e) => {
                complete = false;
                notes.push(e);
            }
        }
    }
    match crate::config::pictures_dir() {
        Ok(pictures) => match migrate_dir(&pictures.join(LEGACY_DIR), &pictures.join(DIR)) {
            Ok(n) => notes.extend(n),
            Err(e) => {
                complete = false;
                notes.push(e);
            }
        },
        Err(e) => {
            complete = false;
            notes.push(format!("pictures: known folder unavailable ({e})"));
        }
    }

    notes.extend(crate::settings::system::migrate_legacy_run_value());
    notes.extend(crate::translate::migrate_legacy_vault());

    for note in &notes {
        // Writing to app.log creates the data folder. Only do that once every
        // folder step is done; an early FastPin folder would otherwise look
        // like a real destination to `has_real_content` on the next start.
        if complete {
            crate::perf::log_app(&format!("migrate: {note}"));
        } else {
            eprintln!("migrate: {note}");
        }
    }
    notes
}

/// Moves `old` to `new` when `new` holds no real data yet.
///
/// `rename` is atomic and free; when it fails (cross-volume profile, a file
/// held open by a still-running old build) the tree is copied into a staging
/// folder and only renamed into place once complete. The old folder is left
/// behind in that case — the user can delete it, and a retry next start is a
/// no-op because `new` now holds real data.
///
/// A bare `new.exists()` check used to skip the whole migration when an
/// installer (or a crash before the rename) left an empty folder behind.
pub(crate) fn migrate_dir(old: &Path, new: &Path) -> Result<Vec<String>, String> {
    let mut notes = Vec::new();
    cleanup_stale_staging(new, &mut notes, STALE_STAGING_AFTER);
    if !old.is_dir() {
        return Ok(notes);
    }
    if has_real_content(new) {
        // 非空目标绝不覆盖：记录警告并跳过，新旧两边的数据都保留。
        notes.push(format!(
            "目标目录已有数据，跳过迁移以保留两边的内容：{}",
            new.display()
        ));
        return Ok(notes);
    }
    // Windows 的 rename 即 MoveFileExW(REPLACE_EXISTING)，它无法覆盖一个
    // 已存在的目录。安装器或上次崩溃留下的空目录必须先移除，否则 rename 与
    // 回退复制都会失败，旧配置/历史会被遗弃。
    if new.is_dir() {
        if let Err(e) = fs::remove_dir(new) {
            return Err(format!(
                "目标为空目录但无法移除 {}（{e}）；保留旧数据以避免覆盖",
                new.display()
            ));
        }
    }
    match fs::rename(old, new) {
        Ok(()) => {
            notes.push(format!("renamed {} -> {}", old.display(), new.display()));
            Ok(notes)
        }
        Err(rename_err) => {
            let staging = staging_path(new);
            let copied = copy_tree(old, &staging).and_then(|n| {
                fs::rename(&staging, new)?;
                Ok(n)
            });
            match copied {
                Ok(n) => {
                    notes.push(format!(
                        "copied {n} files {} -> {} ({rename_err}); legacy folder kept",
                        old.display(),
                        new.display()
                    ));
                    Ok(notes)
                }
                Err(copy_err) => {
                    let _ = fs::remove_dir_all(&staging);
                    Err(format!(
                        "could not migrate {} ({rename_err}; copy failed: {copy_err})",
                        old.display()
                    ))
                }
            }
        }
    }
}

/// 只有 settings.json、历史目录或其它任何条目才算真实内容；空目录（安装器
/// 残留 / 上次崩溃）不能挡住一次迁移，否则用户的旧配置会被永久遗弃。
fn has_real_content(dir: &Path) -> bool {
    if dir.join("settings.json").exists() || dir.join("capture_history").is_dir() {
        return true;
    }
    fs::read_dir(dir)
        .ok()
        .and_then(|mut entries| entries.next())
        .is_some()
}

/// 迁移暂存目录至少老化一天才回收，避免误删另一个进程正在进行的复制。
const STALE_STAGING_AFTER: Duration = Duration::from_secs(24 * 3600);

/// 清理上次崩溃遗留的 `<new>.migrating-<pid>` 目录。当前进程自己的暂存目录
/// 可能仍在使用，绝不删除；只处理名字匹配且已老化的兄弟目录。
fn cleanup_stale_staging(new: &Path, notes: &mut Vec<String>, stale_after: Duration) {
    let Some(parent) = new.parent() else { return };
    let Some(name) = new.file_name().map(|n| n.to_string_lossy().into_owned()) else {
        return;
    };
    let prefix = format!("{name}.migrating-");
    let own = format!("{prefix}{}", std::process::id());
    let Ok(entries) = fs::read_dir(parent) else {
        return;
    };
    for entry in entries.flatten() {
        let file_name = entry.file_name().to_string_lossy().into_owned();
        if !file_name.starts_with(&prefix) || file_name == own {
            continue;
        }
        let aged = entry
            .metadata()
            .and_then(|meta| meta.modified())
            .map(|modified| {
                modified
                    .elapsed()
                    .map(|age| age > stale_after)
                    .unwrap_or(false)
            })
            .unwrap_or(false);
        if !aged {
            continue;
        }
        match fs::remove_dir_all(entry.path()) {
            Ok(()) => notes.push(format!("清理遗留暂存目录 {}", entry.path().display())),
            Err(e) => notes.push(format!(
                "无法清理遗留暂存目录 {}（{e}）",
                entry.path().display()
            )),
        }
    }
}

fn staging_path(new: &Path) -> PathBuf {
    let name = new
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| DIR.to_string());
    new.with_file_name(format!("{name}.migrating-{}", std::process::id()))
}

fn copy_tree(src: &Path, dst: &Path) -> std::io::Result<usize> {
    fs::create_dir_all(dst)?;
    let mut files = 0;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let target = dst.join(entry.file_name());
        let kind = entry.file_type()?;
        if kind.is_dir() {
            files += copy_tree(&entry.path(), &target)?;
        } else if kind.is_file() {
            fs::copy(entry.path(), &target)?;
            files += 1;
        }
    }
    Ok(files)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Unique scratch root per test; never touches the real profile folders.
    /// Lives under target/ because %TEMP% is blocked by the harness sandbox.
    fn scratch(name: &str) -> PathBuf {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join("migrate-tests")
            .join(format!("fastpin-migrate-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        root
    }

    #[test]
    fn renames_legacy_tree_and_is_idempotent() {
        let root = scratch("rename");
        let old = root.join("Zshot");
        let new = root.join("FastPin");
        fs::create_dir_all(old.join("logs")).unwrap();
        fs::write(old.join("settings.json"), "{\"a\":1}").unwrap();
        fs::write(old.join("logs").join("app.log"), "line").unwrap();

        let first = migrate_dir(&old, &new).unwrap();
        assert_eq!(first.len(), 1, "{first:?}");
        assert_eq!(
            fs::read_to_string(new.join("settings.json")).unwrap(),
            "{\"a\":1}"
        );
        assert_eq!(
            fs::read_to_string(new.join("logs").join("app.log")).unwrap(),
            "line"
        );
        assert!(!old.exists(), "rename should not keep a copy");

        let second = migrate_dir(&old, &new).unwrap();
        assert!(second.is_empty(), "second start must be a no-op");
        assert_eq!(
            fs::read_to_string(new.join("settings.json")).unwrap(),
            "{\"a\":1}"
        );

        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn existing_destination_is_never_overwritten() {
        let root = scratch("existing");
        let old = root.join("Zshot");
        let new = root.join("FastPin");
        fs::create_dir_all(&old).unwrap();
        fs::create_dir_all(&new).unwrap();
        fs::write(old.join("settings.json"), "old").unwrap();
        fs::write(new.join("settings.json"), "new").unwrap();

        let notes = migrate_dir(&old, &new).unwrap();
        assert!(
            notes.iter().any(|n| n.contains("跳过迁移")),
            "non-empty destination must be reported: {notes:?}"
        );
        assert_eq!(
            fs::read_to_string(new.join("settings.json")).unwrap(),
            "new"
        );
        assert!(old.exists());

        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn copy_fallback_keeps_the_legacy_folder() {
        let root = scratch("copy");
        let old = root.join("Zshot");
        let new = root.join("FastPin");
        fs::create_dir_all(old.join("nested")).unwrap();
        fs::write(old.join("settings.json"), "{}").unwrap();
        fs::write(old.join("nested").join("shot.png"), [1u8, 2, 3]).unwrap();

        let copied = copy_tree(&old, &new).unwrap();
        assert_eq!(copied, 2);
        assert!(old.exists(), "copy must not remove the source");
        assert!(new.join("settings.json").is_file());
        assert!(new.join("nested").join("shot.png").is_file());

        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn missing_legacy_folder_is_a_noop() {
        let root = scratch("missing");
        assert!(migrate_dir(&root.join("Zshot"), &root.join("FastPin"))
            .unwrap()
            .is_empty());
        assert!(!root.join("FastPin").exists());
        fs::remove_dir_all(&root).unwrap();
    }

    /// 空的目标目录（安装器或崩溃残留）不能让整次迁移被跳过。
    #[test]
    fn empty_destination_does_not_block_migration() {
        let root = scratch("empty-dest");
        let old = root.join("Zshot");
        let new = root.join("FastPin");
        fs::create_dir_all(&old).unwrap();
        fs::write(old.join("settings.json"), "{\"a\":1}").unwrap();
        fs::create_dir_all(&new).unwrap();

        let first = migrate_dir(&old, &new).unwrap();
        assert_eq!(first.len(), 1, "{first:?}");
        assert_eq!(
            fs::read_to_string(new.join("settings.json")).unwrap(),
            "{\"a\":1}"
        );
        // 第二次启动目标里已有真实数据，必须是 no-op。
        assert!(migrate_dir(&old, &new).unwrap().is_empty());

        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn stale_staging_directory_is_reclaimed() {
        let root = scratch("staging");
        let new = root.join("FastPin");
        fs::create_dir_all(&new).unwrap();
        let stale = root.join(format!("FastPin.migrating-{}", std::process::id() + 1));
        fs::create_dir_all(stale.join("nested")).unwrap();
        fs::write(stale.join("nested").join("shot.png"), [1u8]).unwrap();

        let mut notes = Vec::new();
        cleanup_stale_staging(&new, &mut notes, Duration::ZERO);
        assert!(!stale.exists(), "aged staging directory must be removed");
        assert!(notes.iter().any(|n| n.contains("清理遗留暂存目录")));

        // 当前进程自己的暂存目录可能正在使用，不能删。
        let own = root.join(format!("FastPin.migrating-{}", std::process::id()));
        fs::create_dir_all(&own).unwrap();
        cleanup_stale_staging(&new, &mut notes, Duration::ZERO);
        assert!(own.exists());

        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn default_locations_end_with_the_new_name() {
        assert!(crate::config::path()
            .unwrap()
            .ends_with(Path::new("FastPin").join("settings.json")));
        assert!(crate::config::default_screenshot_dir()
            .unwrap()
            .ends_with(Path::new("FastPin")));
        assert!(crate::settings::system::data_dir()
            .unwrap()
            .ends_with(Path::new("FastPin")));
        assert!(crate::perf::log_dir()
            .unwrap()
            .ends_with(Path::new("FastPin").join("logs")));
    }
}
