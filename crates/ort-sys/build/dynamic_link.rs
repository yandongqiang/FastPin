#[cfg(feature = "copy-dylibs")]
use std::path::Path;

use crate::vars;

pub fn prefer_dynamic_linking() -> bool {
	match vars::get(vars::PREFER_DYNAMIC_LINK) {
		Some(val) => val == "1" || val.to_lowercase() == "true",
		None => false
	}
}

#[cfg(feature = "copy-dylibs")]
pub fn copy_dylibs(lib_dir: &Path, out_dir: &Path) {
	use std::fs;

	// FastPin patch: upstream unconditionally unwrapped symlink/copy operations.
	// On a fresh CI runner the target sub-directories (examples, deps) do not
	// exist yet, so the Windows symlink fallback `fs::copy` failed with "path
	// not found" (code 3) and aborted the whole build. Create each destination
	// directory first and treat a symlink/copy failure as best-effort (the
	// dylibs are only needed by examples/tests; the library links statically).
	let out_dir = out_dir.ancestors().nth(3).unwrap();
	for out_dir in [out_dir.to_path_buf(), out_dir.join("examples"), out_dir.join("deps")] {
		if fs::create_dir_all(&out_dir).is_err() {
			continue;
		}
		#[cfg(windows)]
		let mut copy_fallback = false;
		#[cfg(not(windows))]
		let copy_fallback = false;

		let Ok(lib_files) = fs::read_dir(lib_dir) else {
			crate::log::warning!("skipping dylib copy: cannot read `{}`", lib_dir.display());
			break;
		};
		for lib_file in lib_files.filter(|e| {
			e.as_ref().ok().is_some_and(|e| {
				e.file_type().is_ok_and(|e| !e.is_dir()) && [".dll", ".so", ".dylib"].into_iter().any(|v| e.path().to_string_lossy().contains(v))
			})
		}) {
			let Ok(lib_file) = lib_file else { continue };
			let lib_path = lib_file.path();
			let Some(lib_name) = lib_path.file_name() else { continue };
			let out_path = out_dir.join(lib_name);
			if out_path.is_symlink() {
				let _ = fs::remove_file(&out_path);
			}
			if !out_path.exists() {
				#[cfg(windows)]
				if std::os::windows::fs::symlink_file(&lib_path, &out_path).is_err() {
					copy_fallback = true;
					if let Err(e) = fs::copy(&lib_path, &out_path) {
						crate::log::warning!(
							"could not copy dylib `{}` to `{}`: {e}",
							lib_path.display(), out_path.display()
						);
						break;
					}
				}
				#[cfg(unix)]
				let _ = std::os::unix::fs::symlink(&lib_path, &out_path);
			}
			if !copy_fallback {
				println!("cargo:rerun-if-changed={}", out_path.display());
			}
		}

		// If we had to fallback to copying files on Windows, break early to avoid copying to 3 different directories
		if copy_fallback {
			crate::log::warning!(
				"had to copy dylibs because Windows Developer Mode is not enabled, or the cache dir is on a different drive. examples & tests will not be able to access the dylibs"
			);
			break;
		}
	}
}
