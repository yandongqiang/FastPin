//! Dragging an export out of FastPin as a real file.
//!
//! A drag hands the receiver a *path*, not pixels, so the image has to exist on
//! disk before the drag loop starts. That file cannot be deleted when the drag
//! loop returns: Windows shell receivers (Explorer, chat apps, mail clients)
//! commonly copy it after the caller's `DoDragDrop` has already returned, and a
//! dropped handle would leave them with a broken copy. The retention ledger in
//! this module is what keeps those files alive for a bounded window while
//! refusing to let unbounded drags fill the temp directory.
//!
//! Ported from snow-shot's `screenshot_pinned_drag_export.cpp` / `.h`:
//! `MaximumFileCount` 64, `MaximumBytes` 512 MiB, `TransferGraceMilliseconds`
//! 30 minutes, and the same reservation accounting.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// Drags kept alive at once. A user dragging out a dozen screenshots at a time
/// is already unusual; 64 bounds a stuck loop that never expires.
pub const MAX_RETAINED_FILES: usize = 64;

/// Total bytes the retained drag files may occupy.
pub const MAX_RETAINED_BYTES: u64 = 512 * 1024 * 1024;

/// How long a dragged-out file survives after the drag loop returns.
///
/// The receiver may read it well after the drop: Explorer's copy, a browser
/// upload, or a paste into an editor that lazily opens the path. 30 minutes
/// matches upstream and is long enough that a user never sees a missing file.
pub const TRANSFER_GRACE: Duration = Duration::from_secs(30 * 60);

/// One reserved drag file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Lease {
    /// The file the receiver was offered.
    pub path: PathBuf,
    /// Bytes the reservation accounts for.
    pub bytes: u64,
}

/// Bounded bookkeeping for drag files that must outlive their drag.
///
/// A reservation is taken *before* the drag starts so a full ledger refuses the
/// drag instead of leaving an orphan file behind. `complete` starts the grace
/// clock; `expire` deletes what has aged out. Expiry is driven by the caller
/// (a timer tick) rather than a background thread, because the pin windows
/// already tick and a stray thread per drag would be worse than one sweep.
#[derive(Debug, Default)]
pub struct Retention {
    live: Vec<Live>,
    bytes: u64,
    /// Files whose grace already elapsed but whose deletion failed, with the
    /// time to retry. A receiver can still hold a Windows handle, and dropping
    /// the record would leak the file with no owner left to clean it up.
    retry: Vec<(PathBuf, u64, Instant)>,
}

#[derive(Debug)]
struct Live {
    lease: Lease,
    /// `None` while the drag loop is still running: the grace clock only starts
    /// once the drop returns, so a long drag cannot eat the transfer window.
    expires_at: Option<Instant>,
}

impl Retention {
    pub fn new() -> Self {
        Self::default()
    }

    /// Files currently held (dragging or inside their grace window).
    pub fn retained_files(&self) -> usize {
        self.live.len()
    }

    /// Bytes currently reserved.
    pub fn retained_bytes(&self) -> u64 {
        self.bytes
    }

    /// Reserves one file before its drag starts.
    ///
    /// Returns `None` when the file count or byte budget is exhausted, which
    /// the caller reports as "export queue is full" rather than starting a
    /// drag whose file would be deleted mid-transfer.
    pub fn reserve(&mut self, path: PathBuf, bytes: u64) -> Option<Lease> {
        self.expire();
        if self.live.len() >= MAX_RETAINED_FILES || bytes > MAX_RETAINED_BYTES - self.bytes {
            return None;
        }
        let lease = Lease { path, bytes };
        self.bytes += bytes;
        self.live.push(Live {
            lease: lease.clone(),
            expires_at: None,
        });
        Some(lease)
    }

    /// Marks a drag finished. `accepted` is whether the target took the drop.
    ///
    /// A rejected drop still gets the grace window on purpose: OLE cannot prove
    /// the receiver declined (a drag onto an app that pre-reads the path during
    /// `DragEnter` reports `DROPEFFECT_NONE` while still holding the bytes), so
    /// expiring early would break a transfer that did happen.
    pub fn complete(&mut self, lease: &Lease, _accepted: bool) {
        let Some(live) = self.live.iter_mut().find(|l| l.lease.path == lease.path) else {
            return;
        };
        live.expires_at = Some(Instant::now() + TRANSFER_GRACE);
    }

    /// Deletes everything whose grace window has passed and retries deletes a
    /// receiver's open handle previously blocked.
    pub fn expire(&mut self) {
        let now = Instant::now();
        let mut keep = Vec::with_capacity(self.live.len());
        for live in self.live.drain(..) {
            let Some(deadline) = live.expires_at else {
                // Still dragging.
                keep.push(live);
                continue;
            };
            if deadline > now {
                keep.push(live);
                continue;
            }
            if std::fs::remove_file(&live.lease.path).is_err() && live.lease.path.exists() {
                self.retry
                    .push((live.lease.path.clone(), live.lease.bytes, now));
                continue;
            }
            self.bytes -= live.lease.bytes;
        }
        self.live = keep;

        let mut still_blocked = Vec::with_capacity(self.retry.len());
        for (path, bytes, since) in self.retry.drain(..) {
            if !path.exists() || std::fs::remove_file(&path).is_ok() {
                // The file is gone: its reservation is over, regardless of how
                // long it spent in the retry queue. Waiting for TRANSFER_GRACE
                // here leaked the bytes whenever the receiver released the
                // handle on the very next sweep, eventually freezing reserve().
                self.bytes = self.bytes.saturating_sub(bytes);
                continue;
            }
            // A receiver still holds it. Try again on the next sweep, and keep
            // its budget reserved so the ledger never over-admits.
            still_blocked.push((path, bytes, since));
        }
        self.retry = still_blocked;
    }

    /// Drops every reservation record without touching the files.
    ///
    /// Used at shutdown: leaving the files is the safe outcome, since a running
    /// receiver may still be reading them and the OS temp cleanup owns them.
    pub fn forget(&mut self) {
        self.live.clear();
        self.retry.clear();
        self.bytes = 0;
    }
}

/// Writes `bytes` into a fresh directory under `root`, returning the file path.
///
/// Each drag gets its own directory so two drags of the same filename cannot
/// collide, matching upstream's `QTemporaryDir("snow-shot-drag-XXXXXX")`.
pub fn stage_in(root: &Path, bytes: &[u8], extension: &str) -> std::io::Result<PathBuf> {
    let dir = root.join(format!(
        "FastPin-drag-{}-{}",
        std::process::id(),
        unique_counter()
    ));
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(format!(
        "FastPin-drag.{}",
        extension.trim_start_matches('.')
    ));
    std::fs::write(&path, bytes)?;
    Ok(path)
}

/// Stages a drag file under the system temp directory.
pub fn stage(bytes: &[u8], extension: &str) -> std::io::Result<PathBuf> {
    stage_in(&std::env::temp_dir(), bytes, extension)
}

/// Deletes the directory `stage` created, if the file inside it is the only
/// thing there. Best effort: a failure leaves the file for `Retention` or the
/// OS temp sweep.
pub fn discard(path: &Path) {
    let Some(dir) = path.parent() else {
        return;
    };
    let _ = std::fs::remove_file(path);
    if dir
        .file_name()
        .is_some_and(|n| n.to_string_lossy().starts_with("FastPin-drag-"))
    {
        let _ = std::fs::remove_dir(dir);
    }
}

fn unique_counter() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch() -> PathBuf {
        // The sandbox denies writes to the system temp dir, so tests stage
        // inside the workspace's own target directory instead.
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join("test-tmp")
            .join(format!("fastpin-drag-{}", unique_counter()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn lease(dir: &Path, name: &str, bytes: u64) -> (PathBuf, Lease) {
        let path = dir.join(name);
        std::fs::write(&path, vec![0u8; bytes as usize]).unwrap();
        let mut retention = Retention::new();
        let lease = retention.reserve(path.clone(), bytes).unwrap();
        (path, lease)
    }

    #[test]
    fn a_reservation_holds_a_file_until_its_grace_ends() {
        let dir = scratch();
        let (path, lease) = lease(&dir, "a.png", 16);
        let mut retention = Retention::new();
        let held = retention.reserve(path.clone(), 16).unwrap();

        retention.complete(&held, true);
        retention.expire();
        // Inside the grace window the file must still be readable: a receiver
        // may be copying it right now.
        assert!(path.exists());
        assert_eq!(retention.retained_files(), 1);

        // Age the lease past its grace, then sweep.
        for live in retention.live.iter_mut() {
            live.expires_at = Some(Instant::now() - Duration::from_secs(1));
        }
        retention.expire();
        assert!(!path.exists());
        assert_eq!(retention.retained_files(), 0);
        assert_eq!(retention.retained_bytes(), 0);
        let _ = lease;
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_retry_sweep_returns_bytes_as_soon_as_the_file_is_deletable() {
        let dir = scratch();
        // A directory cannot be removed by remove_file, so the first sweep
        // defers the lease to the retry queue, still reserving its bytes.
        let path = dir.join("retried.png");
        std::fs::create_dir(&path).unwrap();
        let mut retention = Retention::new();
        let lease = retention.reserve(path.clone(), 24).unwrap();
        retention.complete(&lease, true);
        for live in retention.live.iter_mut() {
            live.expires_at = Some(Instant::now() - Duration::from_secs(1));
        }
        retention.expire();
        assert_eq!(retention.retry.len(), 1);
        assert_eq!(retention.retained_bytes(), 24);

        // The receiver let go long before TRANSFER_GRACE: the very next sweep
        // must return the bytes or a run of short drags freezes reserve().
        std::fs::remove_dir(&path).unwrap();
        std::fs::write(&path, vec![0u8; 24]).unwrap();
        retention.expire();
        assert_eq!(retention.retry.len(), 0);
        assert_eq!(retention.retained_files(), 0);
        assert_eq!(retention.retained_bytes(), 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_drag_still_in_flight_is_never_expired() {
        let dir = scratch();
        let path = dir.join("b.png");
        std::fs::write(&path, b"x").unwrap();
        let mut retention = Retention::new();
        retention.reserve(path.clone(), 1).unwrap();

        // `complete` was never called, so the grace clock has not started.
        retention.expire();
        assert!(path.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_file_budget_refuses_an_oversized_drag() {
        let mut retention = Retention::new();
        let path = std::env::temp_dir().join("fastpin-oversized.png");
        assert!(retention
            .reserve(path.clone(), MAX_RETAINED_BYTES)
            .is_some());
        // The budget is now fully committed.
        assert!(retention.reserve(path, 1).is_none());
    }

    #[test]
    fn the_count_budget_stops_an_endless_drag_loop() {
        let mut retention = Retention::new();
        for i in 0..MAX_RETAINED_FILES {
            let path = std::env::temp_dir().join(format!("fastpin-count-{i}.png"));
            assert!(retention.reserve(path, 1).is_some());
        }
        let one_more = std::env::temp_dir().join("fastpin-count-last.png");
        assert!(retention.reserve(one_more, 1).is_none());
    }

    #[test]
    fn staging_writes_the_bytes_and_uses_the_asked_for_extension() {
        let path = stage_in(&scratch(), b"payload", "jpg").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"payload");
        assert_eq!(path.extension().unwrap(), "jpg");
        discard(&path);
        assert!(!path.exists());
    }

    #[test]
    fn discarding_a_staged_file_removes_its_directory() {
        let path = stage_in(&scratch(), b"payload", "png").unwrap();
        let dir = path.parent().unwrap().to_path_buf();
        discard(&path);
        assert!(!dir.exists(), "the per-drag directory should be gone too");
    }
}
