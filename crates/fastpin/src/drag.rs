//! Native OLE drag-out, so a screenshot can be dropped into Explorer, a chat
//! window or an editor as a real file.
//!
//! The data object is the shell's own, built from the file's PIDL, so we get
//! `CF_HDROP` plus every shell format (and the preferred-drop-effect channel)
//! without implementing `IDataObject` by hand. Only the drop source is ours,
//! which is what decides when the drag ends.
//!
//! The loop runs on its own thread: `DoDragDrop` does not return until the user
//! drops or cancels, and parking the GPUI thread for that long would freeze
//! every pin the app owns.

use std::path::Path;

use windows::core::{implement, Result as WinResult, BOOL, PCWSTR};
use windows::Win32::Foundation::HWND;
use windows::Win32::System::Com::{CoTaskMemFree, IDataObject, IEnumFORMATETC, DATADIR_GET};
use windows::Win32::System::Ole::{
    DoDragDrop, IDropSource, IDropSource_Impl, OleInitialize, OleUninitialize, DROPEFFECT,
    DROPEFFECT_COPY, DROPEFFECT_NONE,
};
use windows::Win32::System::SystemServices::{MK_LBUTTON, MODIFIERKEYS_FLAGS};
use windows::Win32::UI::Shell::{Common::ITEMIDLIST, SHCreateDataObject, SHParseDisplayName};

/// What the drag loop ended with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DragOutcome {
    /// The receiver accepted a copy.
    Copied,
    /// The user let go somewhere that refused it.
    Rejected,
    /// Escape, or the button came up before a target accepted.
    Cancelled,
    /// The data object or the OLE loop could not start.
    Failed,
}

impl DragOutcome {
    /// Whether the target took the file, for the retention ledger's accounting.
    pub fn accepted(self) -> bool {
        matches!(self, DragOutcome::Copied)
    }
}

/// Our drop source: the shell asks whether the drag continues and what cursor
/// to show, and we answer from the mouse-button and escape state OLE reports.
#[implement(IDropSource)]
struct DropSource;

impl IDropSource_Impl for DropSource_Impl {
    fn QueryContinueDrag(
        &self,
        escape_pressed: BOOL,
        key_state: MODIFIERKEYS_FLAGS,
    ) -> windows::core::HRESULT {
        // Escape cancels; releasing the left button drops. Anything else keeps
        // the loop running, which is what makes the drag feel native.
        if escape_pressed.as_bool() {
            return windows::Win32::Foundation::DRAGDROP_S_CANCEL;
        }
        if !key_state.contains(MK_LBUTTON) {
            return windows::Win32::Foundation::DRAGDROP_S_DROP;
        }
        windows::Win32::Foundation::S_OK
    }

    fn GiveFeedback(&self, _effect: DROPEFFECT) -> windows::core::HRESULT {
        // Let OLE draw its own cursor: it already knows whether the target
        // under the pointer accepts a copy.
        windows::Win32::Foundation::DRAGDROP_S_USEDEFAULTCURSORS
    }
}

/// Drags `path` out of the process, blocking until the user drops or cancels.
///
/// Must be called on a thread that has OLE initialised; [`drag_file_async`]
/// takes care of that. Returns what the drop ended with so the caller can tell
/// the retention ledger.
pub fn drag_file_blocking(owner: Option<HWND>, path: &Path) -> DragOutcome {
    let wide = match to_wide(path.as_os_str().to_string_lossy().as_ref()) {
        Some(w) => w,
        None => return DragOutcome::Failed,
    };

    unsafe {
        let mut pidl: *mut ITEMIDLIST = std::ptr::null_mut();
        let parsed = SHParseDisplayName(PCWSTR(wide.as_ptr()), None, &mut pidl, 0, None);
        if parsed.is_err() || pidl.is_null() {
            return DragOutcome::Failed;
        }
        // The data object copies what it needs from the PIDL, so the PIDL can be
        // released as soon as the object exists.
        let data: WinResult<IDataObject> = SHCreateDataObject(None, Some(&[pidl]), None);
        CoTaskMemFree(Some(pidl as *const core::ffi::c_void));
        let Ok(data) = data else {
            return DragOutcome::Failed;
        };

        let source: windows::Win32::System::Ole::IDropSource = DropSource.into();
        let mut effect = DROPEFFECT_NONE;
        let hr = DoDragDrop(&data, &source, DROPEFFECT_COPY, &mut effect);
        if hr == windows::Win32::Foundation::DRAGDROP_S_CANCEL {
            return DragOutcome::Cancelled;
        }
        if hr.is_err() {
            return DragOutcome::Failed;
        }
        let _ = owner;
        if effect.contains(DROPEFFECT_COPY) {
            DragOutcome::Copied
        } else {
            DragOutcome::Rejected
        }
    }
}

/// Starts a drag on a worker thread and reports the outcome once it ends.
///
/// The callback runs on that thread: it must not touch GPUI state directly.
/// `owner` is a raw `HWND` value rather than the typed handle because the drag
/// loop lives on another thread and `HWND` is not `Send`; the handle is only
/// ever handed back to OLE, never dereferenced here.
pub fn drag_file_async<F>(path: std::path::PathBuf, done: F)
where
    F: FnOnce(DragOutcome) + Send + 'static,
{
    std::thread::spawn(move || {
        // OLE needs an apartment on this thread before DoDragDrop. An STA is
        // what the shell's data object expects; a thread that already joined a
        // different apartment reports RPC_E_CHANGED_MODE, which still lets the
        // drag run through the caller's apartment.
        let joined = unsafe { OleInitialize(None).is_ok() };
        let outcome = drag_file_blocking(None, &path);
        if joined {
            unsafe { OleUninitialize() };
        }
        done(outcome);
    });
}

fn to_wide(text: &str) -> Option<Vec<u16>> {
    if text.is_empty() {
        return None;
    }
    let mut wide: Vec<u16> = text.encode_utf16().collect();
    wide.push(0);
    Some(wide)
}

// `IEnumFORMATETC` and `DATADIR_GET` are only referenced so the shell data
// object's format enumeration keeps compiling against this feature set.
#[allow(dead_code)]
fn _format_enumeration_is_available(data: &IDataObject) -> WinResult<IEnumFORMATETC> {
    unsafe { data.EnumFormatEtc(DATADIR_GET.0 as u32) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_copy_counts_as_accepted() {
        assert!(DragOutcome::Copied.accepted());
        assert!(!DragOutcome::Rejected.accepted());
        assert!(!DragOutcome::Cancelled.accepted());
        assert!(!DragOutcome::Failed.accepted());
    }

    #[test]
    fn an_empty_path_cannot_be_encoded() {
        assert!(to_wide("").is_none());
    }

    #[test]
    fn a_path_is_nul_terminated_utf16() {
        let wide = to_wide("C:\\a.png").unwrap();
        assert_eq!(wide.last(), Some(&0u16));
        assert_eq!(wide.len(), "C:\\a.png".encode_utf16().count() + 1);
    }
}
