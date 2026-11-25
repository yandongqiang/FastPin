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

use std::collections::HashSet;
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

/// 记录 id 的宽松上限。`next_id` 实际只产出 20 个字符（13 位十六进制毫秒 +
/// `-` + 6 位十六进制序号），留这么多余量是为了兼容将来扩展的 id 格式。
const MAX_RECORD_ID_LEN: usize = 64;

/// 历史记录允许出现的图片文件名。`add()` 只会写这些名字；索引里出现别的
/// 名字一律视为被篡改，拒绝参与读图。
const HISTORY_IMAGE_FILES: [&str; 4] = ["image.png", "image.jpg", "image.jpeg", "image.webp"];

/// 校验来自 index.json 的记录 id。id 会被拼到 `records/` 下面，而
/// `PathBuf::join` 会吞掉绝对路径与 `..`；一旦篡改的索引写进 `"..\\..\\x"`，
/// 下游的 `remove_dir_all` 就能删掉历史目录之外的任意目录。只允许 ASCII
/// 字母/数字/连字符，顺带拒绝空串、超长串、路径分隔符与 `..`。
fn valid_record_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= MAX_RECORD_ID_LEN
        && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
        && !id.contains("..")
        && !Path::new(id).is_absolute()
}

/// 校验来自 index.json 的图片文件名：必须恰好是 `image.<已知扩展名>`。
/// 文件名直接决定缩略图解码读哪个文件，绝不能让索引把它指向
/// `C:\secret.png` 之类的绝对路径或带 `..` 的相对路径。
fn valid_image_file(file: &str) -> bool {
    HISTORY_IMAGE_FILES.contains(&file)
        && !file.contains('/')
        && !file.contains('\\')
        && !Path::new(file).is_absolute()
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

impl Index {
    /// 在读取边界剔除被篡改的索引项。id 用于目录删除，文件名用于读图，二者
    /// 都必须先过白名单。id 非法的记录直接丢弃；id 合法但文件名非法的记录
    /// 留在索引里（其目录仍可按安全 id 删除），只是不会出现在 `list()` 中。
    fn sanitize(&mut self) {
        self.records.retain(|r| {
            if !valid_record_id(&r.id) {
                eprintln!("history: 索引含非法记录 id，已跳过：{:?}", r.id);
                return false;
            }
            if !valid_image_file(&r.file) {
                eprintln!(
                    "history: 记录 {:?} 的图片文件名非法，已禁止读图：{:?}",
                    r.id, r.file
                );
            }
            true
        });
        self.pending_deletions.retain(|p| {
            if !valid_record_id(&p.id) {
                eprintln!("history: 待删除项含非法 id，已跳过：{:?}", p.id);
                return false;
            }
            true
        });
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

    /// `records/<id>`。id 必须先通过白名单校验；非法 id 直接拒绝，绝不交给
    /// `PathBuf::join`（它会把绝对路径或 `..` 当成合法路径组件）。
    fn record_dir(&self, id: &str) -> Result<PathBuf> {
        if !valid_record_id(id) {
            eprintln!("history: 拒绝非法记录 id：{id:?}");
            return Err(Error::new("非法历史记录 id"));
        }
        Ok(self.records_dir().join(id))
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
        let mut index: Index =
            serde_json::from_str(&text).map_err(|e| Error::new(format!("{e}")))?;
        if index.format_version != INDEX_VERSION {
            return Err(Error::new(format!(
                "unsupported history index version {} (expected {INDEX_VERSION})",
                index.format_version
            )));
        }
        // 索引文件位于用户可写目录，可能被别的程序或手工编辑改坏。所有下游
        // 消费（删除、读图、清理、清理待删项）都在这一道过滤之后进行。
        index.sanitize();
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
        let mut records: Vec<HistoryRecord> = self
            .read_index()?
            .records
            .into_iter()
            // 双保险：`read_index` 已经在读取时过滤过，这里再挡一次，
            // 保证 UI 绝不会拿到一个会令 `image_path` 读任意文件的记录。
            .filter(|r| valid_record_id(&r.id) && valid_image_file(&r.file))
            .collect();
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
        let dir = self.record_dir(&id)?;
        let file = format!("image.{}", extension);
        // 扩展名来自配置/导出管线；只写白名单里的名字，避免索引里出现
        // UI 之后会拒绝读取的文件名，也避免拼接出带路径分隔符的文件名。
        if !valid_image_file(&file) {
            return Err(Error::new(format!("不支持的历史图片扩展名：{extension:?}")));
        }
        if let Err(e) = std::fs::create_dir_all(&dir) {
            let _ = std::fs::remove_dir_all(&dir);
            return Err(e.into());
        }
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

    /// 校验过的结果：只有 id 与文件名都在白名单里才返回真实路径。这是会给
    /// 出「拒绝」信号的公开接口；下面的 `image_path` 为兼容现有调用方保留了
    /// 返回 `PathBuf` 的旧签名。
    pub fn image_path_checked(&self, record: &HistoryRecord) -> Result<PathBuf> {
        if !valid_record_id(&record.id) || !valid_image_file(&record.file) {
            eprintln!(
                "history: 拒绝非法记录路径：id={:?}, file={:?}",
                record.id, record.file
            );
            return Err(Error::new("非法历史记录路径"));
        }
        Ok(self.record_dir(&record.id)?.join(&record.file))
    }

    /// Absolute path to a record's image, for the UI to decode.
    ///
    /// 非法记录返回一个位于 `records/` 下、不存在的哨兵路径：调用方只会读图
    /// 失败，绝不会按索引里写的绝对路径或 `..` 去读任意文件。需要区分
    /// 「被拒绝」的调用方请用 [`Self::image_path_checked`]。
    pub fn image_path(&self, record: &HistoryRecord) -> PathBuf {
        self.image_path_checked(record)
            .unwrap_or_else(|_| self.records_dir().join("__invalid_record__"))
    }

    /// Two-phase removal: commit the record's disappearance to the index,
    /// then unlink the directory. A crash between the two leaves an orphan
    /// directory that `cleanup_pending` reclaims on next start.
    ///
    /// 落盘按 dragexport 的「锁内快照 -> 锁外删除 -> 锁内回填」三段完成：
    /// `remove_dir_all` 是阻塞 I/O，Windows 上缩略图解码持有文件句柄时还会失败，
    /// 绝不能在 `STORE_LOCK` 里做，否则会卡住其它导出线程的 `add` 和 UI 操作。
    pub fn remove(&self, id: &str) -> Result<()> {
        self.remove_many(&[id.to_string()])
    }

    fn remove_many(&self, ids: &[String]) -> Result<()> {
        if ids.is_empty() {
            return Ok(());
        }

        // 第一阶段（持锁、只动索引）：把记录从 index 摘下并追加到 pending，
        // 目录路径只做成快照交给锁外。这里不广播 revision——磁盘文件还没删完，
        // UI 提前移除卡片会让人误以为已经删干净了。
        let (mut to_remove, removed_any) = {
            let _guard = lock();
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
            let removed_any = !removed.is_empty();
            if !removed_any {
                return Ok(());
            }
            // 非法 id 已在 record_dir 记日志；绝不把拼接出的路径交给 remove_dir_all。
            // 保留 id 与目录的配对，锁外删除前还能再核对一次 id 是否已被并发写回。
            let targets: Vec<(String, PathBuf)> = removed
                .iter()
                .filter_map(|p| self.record_dir(&p.id).ok().map(|dir| (p.id.clone(), dir)))
                .collect();
            index.pending_deletions.extend(removed);
            self.write_index(&index)?;
            (targets, removed_any)
        };
        // guard 已离开作用域：下面阻塞的删除不再持有全局锁。

        // 第二阶段（锁外）：真正删目录。失败只记日志，pending 条目保留，
        // 下次 cleanup_pending 或删除时重试。
        // 删除前短暂持锁核对：阶段一已把这些 id 从 records 摘掉，但锁外期间
        // 若被并发 add / 脏索引重新写回，绝不能删掉在用记录的图片目录。
        {
            let _guard = lock();
            let live_ids: HashSet<String> = self
                .read_index()?
                .records
                .into_iter()
                .map(|r| r.id)
                .collect();
            to_remove.retain(|(id, _)| {
                if live_ids.contains(id) {
                    eprintln!("history: 待删 id 重新出现在记录中，跳过删除：{id:?}");
                    return false;
                }
                true
            });
        }
        for (_, dir) in &to_remove {
            if let Err(e) = std::fs::remove_dir_all(dir) {
                eprintln!("history: 删除记录目录 {} 失败：{e}", dir.display());
            }
        }

        // 第三阶段（重新持锁）：重读索引再 finalize，避免覆盖锁外期间并发 add
        // 写入的新记录；目录仍在（被占用）的 pending 保留重试。只有在 finalize
        // 写盘成功后才广播，UI 看到的"已删除"才与磁盘状态一致。
        let _guard = lock();
        let mut index = self.read_index()?;
        // 与 cleanup_pending 同理：id 仍在 records 里的 pending 是自相矛盾的
        // 脏数据，finalize 时必须清掉，否则下次启动会被当成待删项重试而误删
        // 在用记录的目录；也不能把它当作"目录被占用"继续保留。
        let live_ids: HashSet<String> = index.records.iter().map(|r| r.id.clone()).collect();
        index.pending_deletions.retain(|p| {
            if live_ids.contains(&p.id) {
                eprintln!(
                    "history: 待删除项 id 仍在使用中，已清除脏 pending：{:?}",
                    p.id
                );
                return false;
            }
            match self.record_dir(&p.id) {
                Ok(dir) => dir.exists(),
                // 非法 id 不再保留；它的目录也绝不会被触碰。
                Err(_) => false,
            }
        });
        self.write_index(&index)?;
        if removed_any {
            HISTORY_REVISION.fetch_add(1, Ordering::Release);
        }
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
        // 第一阶段（持锁、只动索引）：挑出仍存在的待删目录做成快照。非法 id
        // 只记日志、不进快照，既不删目录也不检查存在性；目录已不存在的项直接
        // 从 pending 摘掉（两步删除的第二步其实已完成）。
        let dirs: Vec<PathBuf> = {
            let _guard = lock();
            let mut index = self.read_index()?;
            let before = index.pending_deletions.len();
            // 自相矛盾的索引可能让同一个 id 同时出现在 records 与 pending：
            // 若只按目录存在性把它当待删项锁外重试，就会删掉在用记录的图片，
            // 而索引仍列着这条记录，用户点开历史项发现图片丢失。先把 live ids
            // 收集出来，这类脏 pending 直接清除，既不删目录也不进删除快照。
            let live_ids: HashSet<String> = index.records.iter().map(|r| r.id.clone()).collect();
            let mut dirs = Vec::new();
            index.pending_deletions.retain(|p| {
                if live_ids.contains(&p.id) {
                    eprintln!(
                        "history: 待删除项 id 仍在使用中，已清除脏 pending：{:?}",
                        p.id
                    );
                    return false;
                }
                let dir = match self.record_dir(&p.id) {
                    Ok(dir) => dir,
                    // 非法 id 已记日志：安全跳过，不删任何目录也不检查存在性。
                    Err(_) => return false,
                };
                if !dir.exists() {
                    return false;
                }
                // 删除结果要等锁外确认，先把 pending 留在索引里。
                dirs.push(dir);
                true
            });
            if index.pending_deletions.len() != before {
                self.write_index(&index)?;
            }
            dirs
        };
        if dirs.is_empty() {
            return Ok(());
        }
        // guard 已离开作用域：阻塞删除不再持锁。

        // 第二阶段（锁外）：真正重试删除。慢盘、杀软扫描、缩略图占用句柄
        // 只影响本次重试，不会卡住 add / 删除 / 清空。
        for dir in &dirs {
            if let Err(e) = std::fs::remove_dir_all(dir) {
                // 仍被占用（例如历史窗口正在读缩略图），保留待下次重试。
                eprintln!("history: 重试删除目录 {} 失败：{e}", dir.display());
            }
        }

        // 第三阶段（重新持锁）：重读索引并按目录是否还存在回填 pending，
        // 避免覆盖锁外期间其它线程写入的记录；删除成功的 pending 才清除。
        let _guard = lock();
        let mut index = self.read_index()?;
        // 锁外期间该 id 若被写回 records，同样不能继续当待删项保留，
        // 否则下一次 cleanup_pending 又会尝试删掉在用记录的目录。
        let live_ids: HashSet<String> = index.records.iter().map(|r| r.id.clone()).collect();
        index.pending_deletions.retain(|p| {
            if live_ids.contains(&p.id) {
                return false;
            }
            match self.record_dir(&p.id) {
                Ok(dir) => dir.exists(),
                // 非法 id 不再保留；它的目录也绝不会被触碰。
                Err(_) => false,
            }
        });
        self.write_index(&index)?;
        Ok(())
    }

    /// Oldest-first prune by age, count, and total bytes. A record is removed
    /// if it is older than `retention_days` (unless `keep_permanently`), or if
    /// the count/size budgets are exceeded. Protected records are never older
    /// than "now" in this simplified model, so there is no pin concept here.
    /// Returns how many records were removed.
    pub fn prune(&self, retention: &Retention) -> Result<usize> {
        // 读索引到选出 victim 必须在同一把锁保护区；否则另一个截屏线程的 add
        // 可能读到这里尚未反映的旧索引，后续写回会把新记录丢掉。真正的删除
        // 不在这里做：guard 选完 victim 就释放，由 remove_many 的三段式重新
        // 持锁读 index 再写回，因此并发 add 的新记录同样不会丢。
        let victims = {
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
            // 损坏/被篡改的索引可能把 bytes 写成接近 u64::MAX 的值；普通求和在
            // debug 下溢出 panic、release 下回绕成很小的值，导致错误的清理决策。
            let total: u64 = ordered
                .iter()
                .fold(0u64, |acc, r| acc.saturating_add(r.bytes));
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
            victims
        };
        // guard 已释放：remove_many 会自己重新持锁摘快照、锁外删、再持锁回填。
        self.remove_many(&victims)?;
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
        // 索引损坏（未知版本 / 坏 JSON）时这些操作会失败，记到日志里，不再静默跳过。
        if let Err(e) = store.cleanup_pending() {
            crate::perf::log_app(&format!("history cleanup failed: {e}"));
        }
        if settings.history.enabled {
            if let Err(e) = store.prune(&settings.history.retention()) {
                crate::perf::log_app(&format!("history prune failed: {e}"));
            }
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

    /// 脏索引里同一个 id 既在 records 又在 pending_deletions 时，
    /// cleanup_pending 必须只清掉 pending，绝不能删掉仍在使用的记录目录。
    #[test]
    fn cleanup_pending_keeps_a_record_still_listed_in_the_index() {
        let root = scratch();
        let store = HistoryStore::new(root.clone());
        let rec = sample(&store, 8, 8);
        let dir = store.image_path(&rec).parent().unwrap().to_path_buf();
        let png = vec![0x89, b'P', b'N', b'G'];

        // 人为制造自相矛盾的索引：记录仍在，却又被标成待删除。
        let mut index = store.read_index().unwrap();
        index.pending_deletions.push(PendingDeletion {
            id: rec.id.clone(),
            bytes: rec.bytes,
        });
        store.write_index(&index).unwrap();
        assert!(dir.exists(), "precondition: 记录目录存在");
        assert_eq!(store.pending_count(), 1);

        store.cleanup_pending().unwrap();
        assert_eq!(
            store.pending_count(),
            0,
            "自相矛盾的脏 pending 应被直接清除"
        );
        assert!(dir.exists(), "在用记录的目录不能被 cleanup 删掉");
        let list = store.list().unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].id, rec.id);
        assert_eq!(
            std::fs::read(store.image_path(&list[0])).unwrap(),
            png,
            "记录的图片仍应可读"
        );
    }

    /// remove_many 的 finalize 不能把仍在 records 里的 id 当待删项保留，
    /// 否则下一次启动重试 cleanup 时会误删在用记录。
    #[test]
    fn remove_many_clears_pending_ids_that_are_still_live() {
        let root = scratch();
        let store = HistoryStore::new(root.clone());
        let live = sample(&store, 4, 4);
        let victim = sample(&store, 5, 5);
        let live_dir = store.image_path(&live).parent().unwrap().to_path_buf();

        let mut index = store.read_index().unwrap();
        index.pending_deletions.push(PendingDeletion {
            id: live.id.clone(),
            bytes: live.bytes,
        });
        store.write_index(&index).unwrap();

        store.remove(&victim.id).unwrap();
        assert_eq!(
            store.pending_count(),
            0,
            "finalize 必须清掉仍在使用中 id 的脏 pending"
        );
        assert!(live_dir.exists(), "在用记录的目录不能被动过");
        let list = store.list().unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].id, live.id);
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

    #[test]
    fn record_id_validation_rejects_traversal_and_absolute_paths() {
        assert!(valid_record_id("0000000000001-000000")); // next_id 的真实格式
        assert!(valid_record_id("2026-11-24"));
        assert!(!valid_record_id(""));
        assert!(!valid_record_id(".."));
        assert!(!valid_record_id("../evil"));
        assert!(!valid_record_id(r"..\..\evil"));
        assert!(!valid_record_id(r"C:\Windows\Temp"));
        assert!(!valid_record_id("/etc/passwd"));
        assert!(!valid_record_id("a b"));
        assert!(!valid_record_id("a.b"));
        assert!(!valid_record_id(&"a".repeat(MAX_RECORD_ID_LEN + 1)));
    }

    #[test]
    fn image_file_validation_accepts_only_known_names() {
        for file in ["image.png", "image.jpg", "image.jpeg", "image.webp"] {
            assert!(valid_image_file(file), "{file:?}");
        }
        for file in [
            r"C:\secret.png",
            "../image.png",
            r"..\image.png",
            "image.gif",
            "evil.png",
            "image.png.bak",
            "",
        ] {
            assert!(!valid_image_file(file), "{file:?}");
        }
    }

    /// 被篡改的索引：id 带 `..` 想借 cleanup_pending 删掉历史目录之外的目录，
    /// file 是绝对路径想借缩略图读取任意文件。两者都必须被拒绝。
    #[test]
    fn a_tampered_index_cannot_delete_or_read_arbitrary_paths() {
        let root = scratch();
        let store = HistoryStore::new(root.clone());
        // 受害者目录位于 root 之外：`records\..\..\evil` 若拼接成功就会命中它。
        let victim_name = format!("evil-{}", std::process::id());
        let victim = root.parent().unwrap().join(&victim_name);
        let _ = std::fs::remove_dir_all(&victim);
        std::fs::create_dir_all(&victim).unwrap();

        let evil_id = format!(r"..\..\{victim_name}");
        let mut index = store.read_index().unwrap();
        index.pending_deletions.push(PendingDeletion {
            id: evil_id.clone(),
            bytes: 0,
        });
        index.records.push(HistoryRecord {
            id: evil_id.clone(),
            created_ms: now_ms(),
            width: 1,
            height: 1,
            bytes: 0,
            extension: "png".into(),
            file: r"C:\secret.png".into(),
            source: "region".into(),
        });
        store.write_index(&index).unwrap();

        assert!(store.record_dir(&evil_id).is_err());
        assert!(store.image_path_checked(&index.records[0]).is_err());
        // 公开接口退化成不存在的哨兵路径，绝不指向索引写的绝对路径。
        let safe = store.image_path(&index.records[0]);
        assert_eq!(
            safe.file_name().unwrap().to_string_lossy(),
            "__invalid_record__"
        );
        assert!(!safe.to_string_lossy().contains("secret"));
        assert!(!safe.to_string_lossy().contains(".."));
        // 非法项既不出现在列表里，也不参与删除。
        assert!(store.list().unwrap().is_empty());
        store.cleanup_pending().unwrap();
        assert!(victim.exists(), "非法 id 不得删除历史目录之外的目录");
        let _ = std::fs::remove_dir_all(&victim);
    }

    /// bytes 被写成 u64::MAX 时普通 `.sum()` 在 debug 下溢出 panic、release 下
    /// 回绕；饱和求和必须让 prune 正常完成。
    #[test]
    fn pruning_does_not_overflow_when_bytes_are_corrupt() {
        let store = HistoryStore::new(scratch());
        let mut index = store.read_index().unwrap();
        for i in 0..2 {
            index.records.push(HistoryRecord {
                id: next_id(now_ms() + i),
                created_ms: now_ms(),
                width: 1,
                height: 1,
                bytes: u64::MAX,
                extension: "png".into(),
                file: "image.png".into(),
                source: "region".into(),
            });
        }
        store.write_index(&index).unwrap();
        let removed = store
            .prune(&Retention {
                limit: 100,
                retention_days: 0,
                max_disk_bytes: u64::MAX,
                keep_permanently: true,
            })
            .unwrap();
        assert_eq!(removed, 0);
    }
}
