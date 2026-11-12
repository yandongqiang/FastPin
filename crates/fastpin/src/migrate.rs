//! One-shot migration from the pre-rebrand data locations.
//!
//! Called at the top of `run()`, before the settings file is read, so a user
//! upgrading in place keeps their settings, history, autostart entry and
//! translation key. Every step is best-effort and idempotent:
//!
//! * a destination that already exists is never touched, so repeated starts
//!   cannot copy over newer data;
//! * the legacy folder is kept when `rename` fails and the copy succeeds, so a
//!   half-migrated profile is never the only copy;
//! * failures are logged and never abort startup.

use std::fs;
use std::path::{Path, PathBuf};

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
        // folder step is done: an early empty FastPin folder would make the
        // next start think the migration had already happened.
        if complete {
            crate::perf::log_app(&format!("migrate: {note}"));
        } else {
            eprintln!("migrate: {note}");
        }
    }
    notes
}

/// Moves `old` to `new` when `new` does not exist yet.
///
/// `rename` is atomic and free; when it fails (cross-volume profile, a file
/// held open by a still-running old build) the tree is copied into a staging
/// folder and only renamed into place once complete. The old folder is left
/// behind in that case — the user can delete it, and a retry next start is a
/// no-op because `new` now exists.
pub(crate) fn migrate_dir(old: &Path, new: &Path) -> Result<Vec<String>, String> {
    if !old.is_dir() || new.exists() {
        return Ok(Vec::new());
    }
    match fs::rename(old, new) {
        Ok(()) => Ok(vec![format!(
            "renamed {} -> {}",
            old.display(),
            new.display()
        )]),
        Err(rename_err) => {
            let staging = staging_path(new);
            let copied = copy_tree(old, &staging).and_then(|n| {
                fs::rename(&staging, new)?;
                Ok(n)
            });
            match copied {
                Ok(n) => Ok(vec![format!(
                    "copied {n} files {} -> {} ({rename_err}); legacy folder kept",
                    old.display(),
                    new.display()
                )]),
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
    fn scratch(name: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "fastpin-migrate-{}-{name}",
            std::process::id()
        ));
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

        assert!(migrate_dir(&old, &new).unwrap().is_empty());
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
        assert!(crate::perf::log_dir().unwrap().ends_with(
            Path::new("FastPin").join("logs")
        ));
    }
}
