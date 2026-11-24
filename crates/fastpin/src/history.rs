//! Screenshot history — a JSON index plus one directory per captured image.
//!
//! Modelled on snow-shot's capture-history repository, but deliberately small:
//! history is a *copy* of the saved image, so deleting or moving the file in the
//! user's Pictures folder never erases the record. The index is rewritten
//! atomically (temp file + rename) and deletions are two-phase (commit the
//! removal to the index first, then unlink the directory on disk) so a crash
//! mid-delete can never leave an index pointing at a half-removed record.
//!
//! The store layer is pure filesystem + serde: no GPUI, no threads, no global
//! state, which keeps it trivially testable with a scratch directory.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

/// Bumped only when the on-disk layout changes incompatibly. Unknown future
/// versions are rejected on read instead of being silently mis-decoded.
pub const INDEX_VERSION: u32 = 1;
const INDEX_FILE: &str = "index.json";
const INDEX_TMP: &str = "index.json.tmp";
const RECORDS_DIR: &str = "records";

static ID_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Revision of the history index. Bumped by `HistoryStore::add()` right after
/// the index is rewritten, so open UI views can notice a new capture without
/// the bare export thread ever needing a GPUI context.
pub static HISTORY_REVISION: AtomicU64 = AtomicU64::new(0);

/// Current history revision. Views poll this cheap atomic and reload when it
/// changes; a single counter keeps the record path independent of GPUI.
pub fn history_revision() -> u64 {
    HISTORY_REVISION.load(Ordering::Acquire)
}

/// Serializes all index read-modify-write cycles. Several export threads can
/// finish at once; without this, two `add` calls could read the same index and
/// the later write would drop the earlier record.
static STORE_LOCK: Mutex<()> = Mutex::new(());

fn lock() -> std::sync::MutexGuard<'static, ()> {
    STORE_LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

/// Atomically moves `from` over `to`. `std::fs::rename` cannot replace an
/// existing file on Windows, so use the Win32 API that can; on other platforms
/// rename already has replace semantics.
#[cfg(windows)]
fn replace_file(from: &Path, to: &Path) -> Result<()> {
    use windows::core::HSTRING;
    use windows::Win32::Storage::FileSystem::{
        MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH,
    };

    let from = HSTRING::from(from.as_os_str());
    let to = HSTRING::from(to.as_os_str());
    unsafe {
        MoveFileExW(
            &from,
            &to,
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    }
    .map_err(|e| Error::new(format!("替换历史索引失败: {e}")))
}

#[cfg(not(windows))]
fn replace_file(from: &Path, to: &Path) -> Result<()> {
    std::fs::rename(from, to).map_err(Into::into)
}

/// Unix time in milliseconds. Falls back to 0 on a clock before 1970, which
/// can only happen on a badly configured machine; sorting still works.
pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// A unique, lexicographically sortable record id: `<millis>-<counter>`.
fn next_id(now: u64) -> String {
    let n = ID_COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("{now:013x}-{n:06x}")
}

/// Human-readable UTC timestamp for the UI. The civil-from-days algorithm
/// avoids pulling a date crate in for a single label.
pub fn format_utc(ms: u64) -> String {
    let secs = (ms / 1000) as i64;
    let days = secs.div_euclid(86_400);
    let sod = secs.rem_euclid(86_400);
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let mut year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = mp + if mp < 10 { 3 } else { -9 };
    if month <= 2 {
        year += 1;
    }
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02} UTC",
        year,
        month,
        day,
        sod / 3600,
        (sod % 3600) / 60
    )
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistoryRecord {
    pub id: String,
    pub created_ms: u64,
    pub width: u32,
    pub height: u32,
    pub bytes: u64,
    /// Extension without the dot: "png" / "jpg" / "webp".
    pub extension: String,
    /// File name inside `records/<id>/`.
    pub file: String,
    #[serde(default)]
    pub source: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PendingDeletion {
    id: String,
    bytes: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Index {
    #[serde(default = "default_version")]
    format_version: u32,
    #[serde(default)]
    records: Vec<HistoryRecord>,
    #[serde(default)]
    pending_deletions: Vec<PendingDeletion>,
}

fn default_version() -> u32 {
    INDEX_VERSION
}

impl Default for Index {
    fn default() -> Self {
        Self {
            format_version: INDEX_VERSION,
            records: Vec::new(),
            pending_deletions: Vec::new(),
        }
    }
}

/// Retention knobs, mirrored from config into the store so the store itself
/// stays independent of the settings cache.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Retention {
    pub limit: usize,
    pub retention_days: u64,
    pub max_disk_bytes: u64,
    pub keep_permanently: bool,
}

impl Default for Retention {
    fn default() -> Self {
        Self {
            limit: 100,
            retention_days: 7,
            max_disk_bytes: 1024 * 1024 * 1024,
            keep_permanently: false,
        }
    }
}

pub struct HistoryStore {
    root: PathBuf,
}

impl HistoryStore {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn index_path(&self) -> PathBuf {
        self.root.join(INDEX_FILE)
    }

    fn records_dir(&self) -> PathBuf {
        self.root.join(RECORDS_DIR)
    }

    fn record_dir(&self, id: &str) -> PathBuf {
        self.records_dir().join(id)
    }

    /// Reads the index. A missing file is an empty index; a future/unknown
    /// version or corrupt JSON is an error rather than an empty history, so a
    /// broken file never silently "forgets" the user's captures.
    pub fn read_index(&self) -> Result<Index> {
        let text = match std::fs::read_to_string(self.index_path()) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Index::default()),
            Err(e) => return Err(e.into()),
        };
        let index: Index = serde_json::from_str(&text).map_err(|e| Error::new(format!("{e}")))?;
        if index.format_version != INDEX_VERSION {
            return Err(Error::new(format!(
                "unsupported history index version {} (expected {INDEX_VERSION})",
                index.format_version
            )));
        }
        Ok(index)
    }

    fn write_index(&self, index: &Index) -> Result<()> {
        std::fs::create_dir_all(&self.root)?;
        let tmp = self.root.join(INDEX_TMP);
        let text = serde_json::to_string_pretty(index).map_err(|e| Error::new(format!("{e}")))?;
        std::fs::write(&tmp, text)?;
        // Replace atomically: a reader never observes a missing index, and a
        // crash mid-write leaves either the old file or the complete new one.
        replace_file(&tmp, &self.index_path())
    }

    /// Newest first. The index is kept in insertion order; sorting here keeps
    /// `add` O(1) and makes the UI independent of on-disk order.
    pub fn list(&self) -> Result<Vec<HistoryRecord>> {
        let mut records = self.read_index()?.records;
        records.sort_by(|a, b| {
            b.created_ms
                .cmp(&a.created_ms)
                .then_with(|| b.id.cmp(&a.id))
        });
        Ok(records)
    }

    /// Copies `bytes` into `records/<id>/image.<ext>` and appends the record to
    /// the index. The id embeds the timestamp so two captures in the same
    /// millisecond still get distinct directories.
    pub fn add(
        &self,
        bytes: &[u8],
        extension: &str,
        width: u32,
        height: u32,
        source: &str,
    ) -> Result<HistoryRecord> {
        let _guard = lock();
        // 先读索引再落图片：索引损坏时直接失败，不会留下已写图片却没有记录的孤立目录。
        let mut index = self.read_index()?;
        std::fs::create_dir_all(self.records_dir())?;
        let now = now_ms();
        let id = next_id(now);
        let dir = self.record_dir(&id);
        if let Err(e) = std::fs::create_dir_all(&dir) {
            let _ = std::fs::remove_dir_all(&dir);
            return Err(e.into());
        }
        let file = format!("image.{}", extension);
        let path = dir.join(&file);
        if let Err(e) = std::fs::write(&path, bytes) {
            let _ = std::fs::remove_dir_all(&dir);
            return Err(e.into());
        }
        let record = HistoryRecord {
            id,
            created_ms: now,
            width,
            height,
            bytes: bytes.len() as u64,
            extension: extension.to_string(),
            file,
            source: source.to_string(),
        };
        index.records.push(record.clone());
        if let Err(e) = self.write_index(&index) {
            let _ = std::fs::remove_dir_all(&dir);
            return Err(e);
        }
        // Publish the new record to open views. `add` still holds the store
        // lock here, so the bump cannot interleave with another writer's
        // index rewrite.
        HISTORY_REVISION.fetch_add(1, Ordering::Release);
        Ok(record)
    }

    /// Absolute path to a record's image, for the UI to decode.
    pub fn image_path(&self, record: &HistoryRecord) -> PathBuf {
        self.record_dir(&record.id).join(&record.file)
    }

    /// Two-phase removal: commit the record's disappearance to the index,
    /// then unlink the directory. A crash between the two leaves an orphan
    /// directory that `cleanup_pending` reclaims on next start.
    pub fn remove(&self, id: &str) -> Result<()> {
        self.remove_many(&[id.to_string()])
    }

    fn remove_many(&self, ids: &[String]) -> Result<()> {
        if ids.is_empty() {
            return Ok(());
        }
        let _guard = lock();
        self.remove_many_locked(ids)
    }

    /// Removal with `STORE_LOCK` already held. `prune` reads the index and then
    /// deletes victims under the same guard; calling `remove_many` there would
    /// try to take the (non-reentrant) mutex twice and deadlock.
    fn remove_many_locked(&self, ids: &[String]) -> Result<()> {
        let mut index = self.read_index()?;
        let mut removed = Vec::new();
        index.records.retain(|r| {
            if ids.contains(&r.id) {
                removed.push(PendingDeletion {
                    id: r.id.clone(),
                    bytes: r.bytes,
                });
                false
            } else {
                true
            }
        });
        if removed.is_empty() {
            return Ok(());
        }
        let to_remove: Vec<String> = removed.iter().map(|p| p.id.clone()).collect();
        index.pending_deletions.extend(removed);
        self.write_index(&index)?;
        // 删除已提交到索引：广播给所有打开的历史视图，它们才能立刻移除卡片。
        HISTORY_REVISION.fetch_add(1, Ordering::Release);

        for id in &to_remove {
            let dir = self.record_dir(id);
            if let Err(e) = std::fs::remove_dir_all(&dir) {
                // 删除失败（目录被占用等）时 pending 条目会保留，下次启动重试。
                eprintln!("history: 删除记录目录 {} 失败：{e}", dir.display());
            }
        }

        // Finalize: drop the pending entries whose directories are gone (or
        // were never created). A locked directory stays pending for retry.
        let mut index = self.read_index()?;
        index
            .pending_deletions
            .retain(|p| self.record_dir(&p.id).exists());
        self.write_index(&index)?;
        Ok(())
    }

    /// Removes every record. Kept as a batched two-phase delete so a crash
    /// cannot resurrect a partial clear.
    pub fn clear(&self) -> Result<()> {
        let ids: Vec<String> = self
            .read_index()?
            .records
            .into_iter()
            .map(|r| r.id)
            .collect();
        self.remove_many(&ids)
    }

    /// Retries directories left behind by an interrupted delete. Safe to call
    /// on every startup; it never touches live records.
    pub fn cleanup_pending(&self) -> Result<()> {
        let _guard = lock();
        let mut index = self.read_index()?;
        let before = index.pending_deletions.len();
        index.pending_deletions.retain(|p| {
            let dir = self.record_dir(&p.id);
            if !dir.exists() {
                // 目录已不存在：两步删除的第二步其实已完成。
                return false;
            }
            match std::fs::remove_dir_all(&dir) {
                Ok(()) => false,
                Err(e) => {
                    // 仍被占用（例如历史窗口正在读缩略图），保留待下次重试。
                    eprintln!("history: 重试删除目录 {} 失败：{e}", dir.display());
                    true
                }
            }
        });
        if index.pending_deletions.len() != before {
            self.write_index(&index)?;
        }
        Ok(())
    }

    /// Oldest-first prune by age, count, and total bytes. A record is removed
    /// if it is older than `retention_days` (unless `keep_permanently`), or if
    /// the count/size budgets are exceeded. Protected records are never older
    /// than "now" in this simplified model, so there is no pin concept here.
    /// Returns how many records were removed.
    pub fn prune(&self, retention: &Retention) -> Result<usize> {
        // 读索引到删 victim 必须是同一把锁保护区；否则另一个截屏线程的 add
        // 可能读到这里尚未反映的旧索引，后续写回会把新记录丢掉。
        let _guard = lock();
        let index = self.read_index()?;
        let mut ordered: Vec<HistoryRecord> = index.records.clone();
        ordered.sort_by(|a, b| {
            a.created_ms
                .cmp(&b.created_ms)
                .then_with(|| a.id.cmp(&b.id))
        });

        let now = now_ms();
        let cutoff = now.saturating_sub(retention.retention_days.saturating_mul(86_400_000));
        let total: u64 = ordered.iter().map(|r| r.bytes).sum();
        let mut count = ordered.len();
        let mut bytes = total;
        let mut victims = Vec::new();

        for r in &ordered {
            let expired = !retention.keep_permanently
                && retention.retention_days > 0
                && r.created_ms < cutoff;
            let over_count = count > retention.limit;
            let over_bytes = bytes > retention.max_disk_bytes;
            if expired || over_count || over_bytes {
                victims.push(r.id.clone());
                count = count.saturating_sub(1);
                bytes = bytes.saturating_sub(r.bytes);
            } else {
                break;
            }
        }

        // 已经持有锁：走 locked 变体，避免重复加锁死锁。
        self.remove_many_locked(&victims)?;
        Ok(victims.len())
    }

    /// Test-only inspection of pending deletions.
    #[cfg(test)]
    pub fn pending_count(&self) -> usize {
        self.read_index()
            .map(|i| i.pending_deletions.len())
            .unwrap_or(0)
    }
}

/// Human-readable label for a history record's source. Shared by the dedicated
/// gallery window and the embedded settings grid so the wording never drifts.
pub fn source_label(source: &str) -> &'static str {
    match source {
        "saved_to_file" => "保存到文件",
        "copied_to_clipboard" => "复制到剪贴板",
        "pinned_to_screen" => "贴图",
        "current_monitor" => "全屏",
        "focused_window" => "窗口",
        // Capture-origin tags written by the current export pipeline. The
        // legacy labels above are kept so records saved by older builds still
        // read the same way.
        "region" => "区域截图",
        "copyOnly" => "仅复制截图",
        "fullscreen" => "全屏截图",
        "currentWindow" => "当前窗口",
        _ => "截图",
    }
}

/// Per-source opt-in for history recording. The three live keys mirror the
/// `record_history_sources` checkboxes in settings. A source we do not recognise
/// (legacy "saved_to_file" / "copied_to_clipboard", a future tag, or a
/// third-party caller) is always allowed so existing flows keep working.
/// `currentWindow` stays in the list only for legacy indexes: no capture emits
/// it and the settings checkbox was removed, so it can never be recorded now.
fn source_is_recordable(allowed: &[String], source: &str) -> bool {
    match source {
        "region" | "copyOnly" | "fullscreen" | "currentWindow" => {
            allowed.iter().any(|key| key == source)
        }
        _ => true,
    }
}

/// Records a successfully written image. Called from the export thread after
/// `write_image_file` returns Ok. History failures are logged but never
/// propagated: a broken index must not make the user think their screenshot
/// failed to save.
pub fn record_saved_image(bytes: &[u8], extension: &str, width: u32, height: u32, source: &str) {
    let settings = crate::config::get();
    // The global switch still wins; the per-source list only narrows it.
    if !settings.history.enabled
        || !source_is_recordable(&settings.screenshot.record_history_sources, source)
    {
        return;
    }
    let dir = match settings.history.store_dir() {
        Ok(d) => d,
        Err(e) => {
            crate::perf::log_app(&format!("history dir unavailable: {e}"));
            return;
        }
    };
    let store = HistoryStore::new(dir);
    if let Err(e) = store.add(bytes, extension, width, height, source) {
        crate::perf::log_app(&format!("history record failed: {e}"));
        return;
    }
    // Best-effort prune after each addition. `add` has released the lock by
    // now; `prune` takes it again before reading the index, and it never races
    // with the record we just committed. A failure here is deliberately quiet
    // (the record is already committed).
    let _ = store.prune(&settings.history.retention());
}

/// Cleans up interrupted deletions and prunes once at startup.
pub fn startup_maintenance() {
    let settings = crate::config::get();
    if let Ok(dir) = settings.history.store_dir() {
        let store = HistoryStore::new(dir);
        // 即使当前关闭了历史也要回收上次删除留下的目录，用户随时可能重新开启。
        let _ = store.cleanup_pending();
        if settings.history.enabled {
            let _ = store.prune(&settings.history.retention());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicU64;

    #[test]
    fn source_label_covers_every_known_source_and_falls_back() {
        assert_eq!(source_label("saved_to_file"), "保存到文件");
        assert_eq!(source_label("copied_to_clipboard"), "复制到剪贴板");
        assert_eq!(source_label("pinned_to_screen"), "贴图");
        assert_eq!(source_label("current_monitor"), "全屏");
        assert_eq!(source_label("focused_window"), "窗口");
        assert_eq!(source_label("unknown_future_source"), "截图");
    }

    static TEST_DIR: AtomicU64 = AtomicU64::new(0);

    /// Scratch directory under the workspace target dir: %TEMP% is blocked by
    /// the harness sandbox (PermissionDenied code 5), but target/ is writable.
    fn scratch() -> PathBuf {
        let n = TEST_DIR.fetch_add(1, Ordering::Relaxed);
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join("history-tests")
            .join(format!("case-{}-{}", std::process::id(), n));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        root
    }

    fn sample(store: &HistoryStore, w: u32, h: u32) -> HistoryRecord {
        store
            .add(&[0x89, b'P', b'N', b'G'], "png", w, h, "saved_to_file")
            .unwrap()
    }

    #[test]
    fn a_fresh_store_has_no_records() {
        let store = HistoryStore::new(scratch());
        assert!(store.list().unwrap().is_empty());
    }

    #[test]
    fn adding_copies_the_bytes_and_indexes_them() {
        let root = scratch();
        let store = HistoryStore::new(root.clone());
        let rec = sample(&store, 640, 480);
        assert_eq!(store.list().unwrap().len(), 1);
        let path = store.image_path(&rec);
        assert_eq!(std::fs::read(&path).unwrap(), vec![0x89, b'P', b'N', b'G']);
        assert_eq!(rec.width, 640);
        assert_eq!(rec.extension, "png");
        assert_eq!(rec.source, "saved_to_file");
    }

    #[test]
    fn copied_to_clipboard_records_a_clipboard_source() {
        // A copy-only capture has no saved file; the record's source must be
        // "copied_to_clipboard" so the gallery labels it "复制到剪贴板", and
        // the stored PNG must be byte-identical so thumbnails decode.
        let root = scratch();
        let store = HistoryStore::new(root.clone());
        let png = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
        let rec = store
            .add(&png, "png", 320, 240, "copied_to_clipboard")
            .unwrap();
        let list = store.list().unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(rec.source, "copied_to_clipboard");
        assert_eq!(rec.extension, "png");
        assert_eq!(rec.width, 320);
        assert_eq!(rec.height, 240);
        assert_eq!(std::fs::read(store.image_path(&rec)).unwrap(), png);
    }

    #[test]
    fn the_newest_record_comes_first() {
        let store = HistoryStore::new(scratch());
        let a = sample(&store, 1, 1);
        std::thread::sleep(std::time::Duration::from_millis(2));
        let b = sample(&store, 2, 2);
        let list = store.list().unwrap();
        assert_eq!(list[0].id, b.id);
        assert_eq!(list[1].id, a.id);
    }

    #[test]
    fn removal_is_two_phase_and_reclaimable() {
        let root = scratch();
        let store = HistoryStore::new(root.clone());
        let rec = sample(&store, 10, 10);
        let dir = store.image_path(&rec).parent().unwrap().to_path_buf();
        store.remove(&rec.id).unwrap();
        assert!(store.list().unwrap().is_empty());
        assert!(!dir.exists());
        assert_eq!(store.pending_count(), 0);
    }

    /// 模拟两步删除中途崩溃：索引已移除记录但目录仍在，pending 条目必须能
    /// 真正把目录删掉并把 pending 清空，而不是只做一次 `retain`。
    #[test]
    fn cleanup_pending_retries_directory_removal() {
        let root = scratch();
        let store = HistoryStore::new(root.clone());
        let rec = sample(&store, 1, 1);
        let dir = store.image_path(&rec).parent().unwrap().to_path_buf();

        let mut index = store.read_index().unwrap();
        let pending = index.records.remove(0);
        index.pending_deletions.push(PendingDeletion {
            id: pending.id.clone(),
            bytes: pending.bytes,
        });
        store.write_index(&index).unwrap();
        assert!(dir.exists(), "precondition: crashed before unlinking");
        assert_eq!(store.pending_count(), 1);

        store.cleanup_pending().unwrap();
        assert!(!dir.exists(), "pending directory must be removed");
        assert_eq!(store.pending_count(), 0);
    }

    /// 索引损坏时 `add` 必须在写图片之前失败，否则会留下永远无人回收的目录。
    #[test]
    fn add_with_a_corrupt_index_leaves_no_orphan_directory() {
        let root = scratch();
        std::fs::write(root.join(INDEX_FILE), b"{ not json").unwrap();
        let store = HistoryStore::new(root.clone());
        assert!(store.add(&[1, 2, 3], "png", 1, 1, "region").is_err());
        let records = root.join(RECORDS_DIR);
        if records.exists() {
            assert_eq!(
                std::fs::read_dir(&records).unwrap().count(),
                0,
                "no image directory may be created before the index is read"
            );
        }
    }

    #[test]
    fn pruning_by_count_keeps_the_newest() {
        let store = HistoryStore::new(scratch());
        for i in 0..5 {
            sample(&store, i, i);
        }
        let removed = store
            .prune(&Retention {
                limit: 2,
                retention_days: 0,
                max_disk_bytes: u64::MAX,
                keep_permanently: false,
            })
            .unwrap();
        assert_eq!(removed, 3);
        assert_eq!(store.list().unwrap().len(), 2);
    }

    #[test]
    fn pruning_by_age_respects_keep_permanently() {
        let store = HistoryStore::new(scratch());
        sample(&store, 1, 1);
        let removed = store
            .prune(&Retention {
                limit: 100,
                retention_days: 0,
                max_disk_bytes: u64::MAX,
                keep_permanently: false,
            })
            .unwrap();
        assert_eq!(removed, 0);
        // A zero-day retention with a past timestamp is handled by forcing the
        // created time back through the index directly.
        let mut index = store.read_index().unwrap();
        index.records[0].created_ms = 1;
        store.write_index(&index).unwrap();
        let removed = store
            .prune(&Retention {
                limit: 100,
                retention_days: 1,
                max_disk_bytes: u64::MAX,
                keep_permanently: false,
            })
            .unwrap();
        assert_eq!(removed, 1);
    }

    /// "永久" in the settings UI maps to keep_permanently = true. An old record
    /// must survive an age-based prune in that mode (count/disk limits aside).
    #[test]
    fn keep_permanently_survives_age_pruning() {
        let store = HistoryStore::new(scratch());
        sample(&store, 1, 1);
        let mut index = store.read_index().unwrap();
        index.records[0].created_ms = 1; // far in the past
        store.write_index(&index).unwrap();
        let removed = store
            .prune(&Retention {
                limit: 100,
                retention_days: 1,
                max_disk_bytes: u64::MAX,
                keep_permanently: true,
            })
            .unwrap();
        assert_eq!(removed, 0, "keep_permanently must ignore retention_days");
        assert_eq!(store.list().unwrap().len(), 1);
    }

    #[test]
    fn a_corrupt_index_is_reported_not_silently_emptied() {
        let root = scratch();
        std::fs::write(root.join(INDEX_FILE), b"{ not json").unwrap();
        let store = HistoryStore::new(root);
        assert!(store.read_index().is_err());
    }

    #[test]
    fn utc_format_is_stable() {
        // 2020-01-01 00:00:00 UTC = 1577836800000 ms.
        assert_eq!(format_utc(1_577_836_800_000), "2020-01-01 00:00 UTC");
    }
}
