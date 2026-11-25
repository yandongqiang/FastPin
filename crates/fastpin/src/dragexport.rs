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

/// retry 条目最多允许失败的删除次数；到达后放弃，避免接收方长期持有句柄时
/// 永久占用一个文件名额。
pub const MAX_RETRY_ATTEMPTS: usize = 4;

/// retry 条目的最长保留时间；即使次数未用尽，超时也要放弃并归还名额。
pub const RETRY_GIVE_UP_AFTER: Duration = Duration::from_secs(5 * 60);

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
    retry: Vec<Retry>,
}

#[derive(Debug)]
struct Live {
    lease: Lease,
    /// `None` while the drag loop is still running: the grace clock only starts
    /// once the drop returns, so a long drag cannot eat the transfer window.
    expires_at: Option<Instant>,
}

/// 删除失败后转入 retry 的记录：`since` 与 `attempts` 共同决定何时放弃，
/// 防止一直删不掉的文件永远占住 64 个名额中的一个。
#[derive(Debug, Clone)]
struct Retry {
    path: PathBuf,
    bytes: u64,
    /// 首次转入 retry 的时刻，用于判断放弃期限。
    since: Instant,
    /// 已经失败的删除次数。
    attempts: usize,
}

/// 锁内摘取的删除目标：仅复制路径与预算交给锁外处理，不直接改动账本。
#[derive(Debug, Clone)]
pub struct DeleteTarget {
    path: PathBuf,
    bytes: u64,
    /// 来自 retry 队列：删除失败时留在 retry，不需要再次入队。
    retrying: bool,
    since: Instant,
    /// retry 条目已经失败的删除次数，live 条目为 0。
    attempts: usize,
}

/// 锁外删除的结果，交回 `apply_deletes` 才能更新账本与预算。
#[derive(Debug)]
pub struct DeleteResult {
    target: DeleteTarget,
    deleted: bool,
}

impl Retention {
    pub fn new() -> Self {
        Self::default()
    }

    /// Files currently held (dragging, inside their grace window, or waiting
    /// in retry after a failed delete). Retry entries still occupy a slot.
    pub fn retained_files(&self) -> usize {
        self.live.len() + self.retry.len()
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
        // 这里不再调用 expire()：remove_file / 删目录是阻塞 I/O，若在持锁的
        // reserve() 里执行，会连带卡住其它拖出。过期统一交给 app.rs 的 sweep，
        // 由「锁内快照 -> 锁外删除 -> 锁内回填」三步完成。
        // retry 里的文件仍在磁盘上，必须和 live 一起占名额：否则 64 条删除失败
        // 的记录转到 retry 后 live 归零，下一轮又能放进 64 个新文件，无上限累积。
        if self.live.len() + self.retry.len() >= MAX_RETAINED_FILES
            || bytes > MAX_RETAINED_BYTES - self.bytes
        {
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

    /// 第一步（持锁、无 I/O）：摘出已过期的 live 条目与全部 retry 条目的快照。
    ///
    /// 只 clone 路径与预算，账本原样保留：删除完成前名额与字节仍然占用，
    /// 既不会超发，也不会出现"正在删却被别的拖出抢走名额"的窗口。
    pub fn expired_targets(&self, now: Instant) -> Vec<DeleteTarget> {
        let mut out = Vec::with_capacity(self.live.len() + self.retry.len());
        for live in &self.live {
            if live.expires_at.is_some_and(|deadline| deadline <= now) {
                out.push(DeleteTarget {
                    path: live.lease.path.clone(),
                    bytes: live.lease.bytes,
                    retrying: false,
                    since: now,
                    attempts: 0,
                });
            }
        }
        for retry in &self.retry {
            out.push(DeleteTarget {
                path: retry.path.clone(),
                bytes: retry.bytes,
                retrying: true,
                since: retry.since,
                attempts: retry.attempts,
            });
        }
        out
    }

    /// 第二步（不接触 `self`，可在锁外/任意线程调用）：真正删除文件与目录。
    ///
    /// 慢盘、杀软扫描、接收方持有句柄导致的阻塞都发生在这里，与全局
    /// retention 锁无关，因此其它拖出的 `reserve()` / `complete()` 不受影响。
    pub fn finish_deletes(targets: Vec<DeleteTarget>) -> Vec<DeleteResult> {
        targets
            .into_iter()
            .map(|target| {
                // The file is gone: its reservation is over, regardless of how
                // long it spent in the retry queue. Waiting for TRANSFER_GRACE
                // here leaked the bytes whenever the receiver released the
                // handle on the very next sweep, eventually freezing reserve().
                let deleted = !target.path.exists() || std::fs::remove_file(&target.path).is_ok();
                if deleted {
                    // 文件已删（或接收方已取走）：顺手清掉专属空目录，否则每次
                    // 拖出都只删 PNG、留下一个空的 FastPin-drag-* 目录。
                    discard(&target.path);
                }
                DeleteResult { target, deleted }
            })
            .collect()
    }

    /// 第三步（持锁）：按删除结果更新账本；只有到达放弃条件的 retry 条目
    /// 才会在归还名额时做一次尽力删除的 I/O。
    ///
    /// 成功才归还预算并移除记录；首次删除失败（接收方仍持句柄）转入 retry，
    /// 继续占用预算与名额，等下一次 sweep 重试。retry 达到次数上限或保留期限
    /// 后放弃，避免一条永远删不掉的文件把名额永久占死。
    pub fn apply_deletes(&mut self, results: Vec<DeleteResult>) {
        for DeleteResult { target, deleted } in results {
            if deleted {
                if target.retrying {
                    self.retry.retain(|r| r.path != target.path);
                } else {
                    self.live.retain(|l| l.lease.path != target.path);
                }
                self.bytes = self.bytes.saturating_sub(target.bytes);
            } else if !target.retrying {
                // 首次失败：从 live 移到 retry，名额与字节继续占用。
                self.live.retain(|l| l.lease.path != target.path);
                if !self.retry.iter().any(|r| r.path == target.path) {
                    self.retry.push(Retry {
                        path: target.path,
                        bytes: target.bytes,
                        since: target.since,
                        attempts: 1,
                    });
                }
            } else {
                // retry 再次失败：累计次数，到达上限或超时就放弃该条目。
                let attempts = target.attempts + 1;
                let give_up =
                    attempts >= MAX_RETRY_ATTEMPTS || target.since.elapsed() >= RETRY_GIVE_UP_AFTER;
                if give_up {
                    self.retry.retain(|r| r.path != target.path);
                    self.bytes = self.bytes.saturating_sub(target.bytes);
                    // 最后一次尽力删除；仍删不掉也只记日志，名额必须归还。
                    // 这里的 I/O 只发生在放弃路径（极少数），不会让每次 sweep
                    // 都在持锁状态下做磁盘操作。
                    discard(&target.path);
                    eprintln!(
                        "[FastPin] 拖出文件重试 {} 次后放弃: {}",
                        attempts,
                        target.path.display()
                    );
                } else if let Some(retry) = self.retry.iter_mut().find(|r| r.path == target.path) {
                    retry.attempts = attempts;
                }
            }
        }
    }

    /// 单步便捷路径（仅供测试）：在同一个 `&mut self` 上完成三步。
    /// 生产路径不要用——它仍会在持锁期间做磁盘 I/O。
    pub fn expire(&mut self) {
        let now = Instant::now();
        let targets = self.expired_targets(now);
        let results = Self::finish_deletes(targets);
        self.apply_deletes(results);
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
    if let Err(e) = std::fs::write(&path, bytes) {
        // create_dir_all 已经建好了专属目录；写失败时若不回收，会留下一个
        // 没有任何账本记录能清掉的空 FastPin-drag-* 目录。
        discard(&path);
        return Err(e);
    }
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
        // 加进程 id：nextest 一测试一进程时，避免多个进程都拿到 fastpin-drag-0
        // 并在结束时互相 remove_dir_all 删掉对方的文件。
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join("test-tmp")
            .join(format!(
                "fastpin-drag-{}-{}",
                std::process::id(),
                unique_counter()
            ));
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
    fn failed_deletes_still_count_against_the_file_budget() {
        let dir = scratch();
        // 目录无法被 remove_file 删除：sweep 后这条记录必然进入 retry。
        let blocked = dir.join("blocked.png");
        std::fs::create_dir(&blocked).unwrap();
        let mut retention = Retention::new();
        let blocked_lease = retention.reserve(blocked.clone(), 1).unwrap();
        for i in 0..MAX_RETAINED_FILES - 1 {
            let path = dir.join(format!("live-{i}.png"));
            std::fs::write(&path, b"x").unwrap();
            assert!(retention.reserve(path, 1).is_some());
        }
        retention.complete(&blocked_lease, true);
        for live in retention.live.iter_mut() {
            if live.lease.path == blocked {
                live.expires_at = Some(Instant::now() - Duration::from_secs(1));
            }
        }
        retention.expire();

        assert_eq!(retention.retry.len(), 1, "删除失败应转入 retry");
        assert_eq!(
            retention.live.len(),
            MAX_RETAINED_FILES - 1,
            "失败的条目应从 live 移走"
        );
        assert_eq!(
            retention.retained_files(),
            MAX_RETAINED_FILES,
            "retry 条目仍应占用文件名额"
        );
        // 中危回归点：64 条都在账本里，不能再放进第 65 个文件。
        assert!(
            retention.reserve(dir.join("one-too-many.png"), 1).is_none(),
            "retry 队列不得绕过文件数上限"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn retry_gives_up_after_max_attempts_and_reclaims_the_slot() {
        let dir = scratch();
        // 每次 sweep 都失败，直到累计次数达到上限。
        let blocked = dir.join("always-blocked.png");
        std::fs::create_dir(&blocked).unwrap();
        let mut retention = Retention::new();
        let lease = retention.reserve(blocked.clone(), 7).unwrap();
        retention.complete(&lease, true);
        for live in retention.live.iter_mut() {
            live.expires_at = Some(Instant::now() - Duration::from_secs(1));
        }

        for _ in 0..MAX_RETRY_ATTEMPTS {
            retention.expire();
        }
        assert_eq!(retention.retry.len(), 0, "重试达到上限后应放弃条目");
        assert_eq!(retention.retained_bytes(), 0, "放弃后应归还字节预算");
        assert_eq!(retention.retained_files(), 0, "放弃后应归还文件名额");
        assert!(
            retention.reserve(dir.join("fresh.png"), 1).is_some(),
            "名额回收后新的拖出应能成功"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn retry_gives_up_after_timeout_and_reclaims_the_slot() {
        let dir = scratch();
        let blocked = dir.join("timed-out.png");
        std::fs::create_dir(&blocked).unwrap();
        let mut retention = Retention::new();
        let lease = retention.reserve(blocked.clone(), 3).unwrap();
        retention.complete(&lease, true);
        for live in retention.live.iter_mut() {
            live.expires_at = Some(Instant::now() - Duration::from_secs(1));
        }
        retention.expire();
        assert_eq!(retention.retry.len(), 1, "首次失败应进入 retry");

        // 次数还没用完，但首次失败时间已超过保留期限，也应放弃。
        retention.retry[0].since = Instant::now() - RETRY_GIVE_UP_AFTER;
        retention.expire();
        assert_eq!(retention.retry.len(), 0, "超时后应放弃 retry 条目");
        assert_eq!(retention.retained_bytes(), 0, "超时放弃后应归还字节预算");
        assert_eq!(retention.retained_files(), 0, "超时放弃后应回收文件名额");
        assert!(
            retention
                .reserve(dir.join("after-timeout.png"), 1)
                .is_some(),
            "回收后应能再次 reserve"
        );
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

    #[test]
    fn a_full_drag_lifecycle_stages_reserves_completes_and_expires() {
        let root = scratch();
        let path = stage_in(&root, b"payload", "png").unwrap();
        let dir = path.parent().unwrap().to_path_buf();
        let bytes = std::fs::metadata(&path).unwrap().len();
        let mut retention = Retention::new();
        let lease = retention.reserve(path.clone(), bytes).unwrap();

        // 拖拽进行中：reserve 只记账，complete 之前 sweep 不能删文件。
        retention.expire();
        assert!(path.exists(), "an in-flight drag must never be deleted");
        assert_eq!(retention.retained_bytes(), bytes);

        retention.complete(&lease, true);
        retention.expire();
        // 宽限期内仍可读：接收方可能正在复制。
        assert!(path.exists(), "the receiver may still be copying the file");

        // 越过宽限期后 sweep 删掉文件与专属目录，并回收预算。
        for live in retention.live.iter_mut() {
            live.expires_at = Some(Instant::now() - Duration::from_secs(1));
        }
        retention.expire();
        assert!(!path.exists(), "aged-out drag file should be deleted");
        assert!(
            !dir.exists(),
            "the per-drag directory should be deleted too"
        );
        assert_eq!(retention.retained_files(), 0, "the record must be dropped");
        assert_eq!(retention.retained_bytes(), 0, "the budget must be returned");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_full_ledger_refuses_the_reservation_before_the_file_is_handed_out() {
        let mut retention = Retention::new();
        for i in 0..MAX_RETAINED_FILES {
            let path = std::env::temp_dir().join(format!("fastpin-full-{i}.png"));
            assert!(retention.reserve(path, 1).is_some());
        }
        let root = scratch();
        let staged = stage_in(&root, b"late", "png").unwrap();
        let dir = staged.parent().unwrap().to_path_buf();

        assert!(retention.reserve(staged.clone(), 4).is_none());
        // 生产路径在 reserve 失败时 discard(&path)：文件与目录都不应残留。
        discard(&staged);
        assert!(!staged.exists());
        assert!(!dir.exists(), "the refused drag must not leave a directory");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn three_step_delete_preserves_bytes_until_apply() {
        // 生产路径（app.rs 的 sweep）走的是「摘取 -> 锁外删除 -> 回填」三步，
        // 这里直接驱动三步 API，验证预算只在最后一步才归还。
        let dir = scratch();
        let path = stage_in(&dir, b"payload", "png").unwrap();
        let bytes = std::fs::metadata(&path).unwrap().len();
        let mut retention = Retention::new();
        let lease = retention.reserve(path.clone(), bytes).unwrap();
        retention.complete(&lease, true);
        for live in retention.live.iter_mut() {
            live.expires_at = Some(Instant::now() - Duration::from_secs(1));
        }

        // 第一步：只摘快照，账本与文件原样保留。
        let now = Instant::now();
        let targets = retention.expired_targets(now);
        assert_eq!(targets.len(), 1, "已过期的 lease 应被摘出");
        assert_eq!(retention.retained_bytes(), bytes, "摘取目标不应释放预算");
        assert_eq!(retention.retained_files(), 1, "摘取目标不应移除记录");
        assert!(path.exists(), "摘取目标不应删除文件");

        // 第二步：静态关联函数不接触 self，删除发生在锁外。
        let results = Retention::finish_deletes(targets);
        assert_eq!(results.len(), 1);
        assert!(results[0].deleted, "文件应能被删除");
        assert_eq!(retention.retained_bytes(), bytes, "锁外删除不应释放预算");
        assert_eq!(retention.retained_files(), 1, "锁外删除不应移除记录");

        // 第三步：回填结果才归还预算、移除记录。
        retention.apply_deletes(results);
        assert_eq!(retention.retained_bytes(), 0, "回填后才归还预算");
        assert_eq!(retention.retained_files(), 0, "回填后才移除记录");
        assert!(!path.exists(), "文件应已被删除");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
