use std::fs::{create_dir_all, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use crate::error::Result;

pub struct PerfSession {
    pub t0: Instant,
    marks: Vec<(&'static str, u128)>,
}

impl PerfSession {
    pub fn start() -> Self {
        Self {
            t0: Instant::now(),
            marks: Vec::new(),
        }
    }

    pub fn mark(&mut self, name: &'static str) {
        self.marks.push((name, self.t0.elapsed().as_millis()));
    }

    pub fn mark_ms(&self, name: &str) -> Option<u128> {
        self.marks
            .iter()
            .rev()
            .find(|(n, _)| *n == name)
            .map(|(_, ms)| *ms)
    }

    pub fn has_mark(&self, name: &str) -> bool {
        self.mark_ms(name).is_some()
    }

    /// Elapsed ms from mark `from` to mark `to` (absolute timestamps on the same session).
    pub fn delta_ms(&self, from: &str, to: &str) -> Option<u128> {
        let a = self.mark_ms(from)?;
        let b = self.mark_ms(to)?;
        Some(b.saturating_sub(a))
    }

    pub fn write_log(&self) -> Result<PathBuf> {
        let dir = log_dir()?;
        create_dir_all(&dir)?;
        let path = dir.join("perf.log");
        let mut f = OpenOptions::new().create(true).append(true).open(&path)?;
        let ts = now_stamp();
        write!(f, "{ts}")?;
        for (name, ms) in &self.marks {
            write!(f, " {name}={ms}ms")?;
        }
        writeln!(f)?;
        Ok(path)
    }
}

/// Nearest-rank percentile on a pre-sorted ascending slice.
/// `p` is in 0..=100. Empty input → 0.
pub fn percentile(sorted: &[u128], p: f64) -> u128 {
    if sorted.is_empty() {
        return 0;
    }
    let p = p.clamp(0.0, 100.0);
    if p <= 0.0 {
        return sorted[0];
    }
    let n = sorted.len();
    let rank = ((p / 100.0) * n as f64).ceil() as usize;
    let idx = rank.saturating_sub(1).min(n - 1);
    sorted[idx]
}

pub fn write_perf_summary(mode: &str, samples_ms: &mut [u128]) -> Result<PathBuf> {
    samples_ms.sort_unstable();
    let n = samples_ms.len();
    let p50 = percentile(samples_ms, 50.0);
    let p95 = percentile(samples_ms, 95.0);
    let p99 = percentile(samples_ms, 99.0);
    let dir = log_dir()?;
    create_dir_all(&dir)?;
    let path = dir.join("perf-summary.json");
    let body = format!(
        "{{\n  \"mode\": \"{mode}\",\n  \"n\": {n},\n  \"t2_t7_ms\": {{ \"p50\": {p50}, \"p95\": {p95}, \"p99\": {p99} }}\n}}\n"
    );
    std::fs::write(&path, body)?;
    Ok(path)
}

/// 日志目录只解析一次；后续调用直接克隆缓存值，避免每次读环境变量。
static LOG_DIR_CACHE: OnceLock<Option<PathBuf>> = OnceLock::new();

fn cached_log_dir() -> Option<&'static PathBuf> {
    LOG_DIR_CACHE
        .get_or_init(|| {
            std::env::var_os("LOCALAPPDATA")
                .map(|base| PathBuf::from(base).join("FastPin").join("logs"))
        })
        .as_ref()
}

/// diag 的两个候选路径（profile 优先、exe 旁兜底）同样只解析一次，
/// 避免每次 log_diag 都调用 `var_os` / `current_exe`。
struct DiagPaths {
    profile: Option<PathBuf>,
    exe_side: Option<PathBuf>,
}

static DIAG_PATHS: OnceLock<DiagPaths> = OnceLock::new();

fn diag_paths() -> &'static DiagPaths {
    DIAG_PATHS.get_or_init(|| DiagPaths {
        profile: std::env::var_os("LOCALAPPDATA").map(|base| {
            PathBuf::from(base)
                .join("FastPin")
                .join("logs")
                .join("diag.log")
        }),
        exe_side: std::env::current_exe()
            .ok()
            .and_then(|exe| exe.parent().map(|dir| dir.join("diag.log"))),
    })
}

pub fn log_dir() -> Result<PathBuf> {
    cached_log_dir().cloned().ok_or_else(missing_localappdata)
}

/// Append one line to `logs/app.log` via the background writer. Release builds
/// have no console, so this is where operational failures end up. Only formats
/// It only formats and enqueues: never blocks and never fails, so a logging
/// error cannot break a screenshot.
pub fn log_app(msg: &str) {
    append_log(Target::App, msg);
}

/// Append one line to `logs/crash.log`, the panic hook's target. Kept apart
/// from app.log so a crash is greppable; same never-fail rule.
pub fn log_crash(msg: &str) {
    append_log(Target::Crash, msg);
}

/// panic hook 专用：直接算出 `logs/crash.log` 的绝对路径，不依赖后台 writer
/// 的 `LOG_DIR_CACHE` 是否已初始化，也不依赖 writer 线程已经启动。
/// LOCALAPPDATA 缺失时返回 None，调用方静默降级。
pub fn crash_log_path_sync() -> Option<PathBuf> {
    std::env::var_os("LOCALAPPDATA").map(|base| {
        PathBuf::from(base)
            .join("FastPin")
            .join("logs")
            .join("crash.log")
    })
}

/// 同步把一行追加到指定日志 `path`：建目录、打开、写入、flush 全在调用线程
/// 完成。`panic = "abort"` 下 hook 返回即终止，后台 writer 来不及落盘，所以
/// 崩溃前必须留下的面包屑都走这条同步路径。失败时静默返回 false（崩溃 hook
/// 里要避免二次 panic），调用方可据此在多个候选路径间回退。
fn write_log_line_sync(path: &Path, msg: &str) -> bool {
    let result = (|| -> std::io::Result<()> {
        if let Some(parent) = path.parent() {
            create_dir_all(parent)?;
        }
        // 同步路径每次按 path 新开句柄、只追加不轮转：轮转由后台 writer 独占。
        // 若同步也轮转，后台长开句柄会指向已改名的备份文件，后续日志全部写错位置；
        // 每次 open by path 则永远写当前文件，不会持有失效句柄。
        let mut f = OpenOptions::new().create(true).append(true).open(path)?;
        // msg 与换行合成一次写入：同一进程内 O_APPEND 的单次 write 是原子的，
        // 因此不会与后台 writer 交错把两行粘成一行。
        let line = format!("{msg}\n");
        f.write_all(line.as_bytes())?;
        f.flush()?;
        Ok(())
    })();
    result.is_ok()
}

pub(crate) fn write_crash_line_sync(path: &Path, msg: &str) {
    let _ = write_log_line_sync(path, msg);
}

/// panic hook 入口：按默认路径同步落盘一行崩溃信息，失败静默。
pub fn log_crash_sync(msg: &str) {
    if let Some(path) = crash_log_path_sync() {
        write_crash_line_sync(&path, msg);
    }
}

/// 同步版 app.log：路径直接解析，不依赖后台 writer 的缓存或线程是否已启动。
/// 只给"崩溃前面包屑"这类极少数关键点用（如 capture start），热路径仍走
/// 非阻塞的 `log_app`，避免每条日志都同步刷盘拖慢截图。
pub fn log_app_sync(msg: &str) {
    let Some(base) = std::env::var_os("LOCALAPPDATA") else {
        return;
    };
    let path = PathBuf::from(base)
        .join("FastPin")
        .join("logs")
        .join("app.log");
    let line = format!("{} {msg}", now_stamp());
    let _ = write_log_line_sync(&path, &line);
}

/// 同步版 diag.log：注解链路里"崩了也要留痕"的关键节点使用，格式与回退
/// 顺序都与异步 `log_diag` 一致（profile 优先，exe 旁兜底）。
pub fn log_diag_sync(msg: &str) {
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let line = format!("t={ts} [diag] {msg}");
    let paths = diag_paths();
    for p in [&paths.profile, &paths.exe_side].into_iter().flatten() {
        if write_log_line_sync(p, &line) {
            break;
        }
    }
}

/// Annotation event-chain diagnostics. Mirrors the whole "button id -> tool ->
/// draft -> commit -> render" ladder into its own `logs/diag.log`, independent
/// of app.log's capture/clipboard noise, with a fallback to a file next to the
/// exe when the profile dir is locked down. Only formats + enqueues; all disk
/// I/O belongs to the background writer. Best-effort: never affects a screenshot.
pub fn log_diag(msg: &str) {
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let line = format!("t={ts} [diag] {msg}\n");
    enqueue(Target::Diag, line);
}

// ── 进程级单例后台 writer ──────────────────────────────────────────────
// 高频日志只做「格式化 + 入队」；磁盘 I/O 全部集中到后台线程，
// 从而消除每帧 / 每次截图时调用方所在的磁盘阻塞。

/// 有界队列容量：入队永不阻塞，满了就丢弃最新行。
const LOG_QUEUE_CAPACITY: usize = 4096;
/// 消息中「目标标记」与「正文」之间的分隔符（正文即便含有也只按首个切分）。
const TAG_SEP: char = '\u{1F}';

#[derive(Clone, Copy)]
enum Target {
    App,
    Crash,
    Diag,
}

impl Target {
    fn tag(self) -> &'static str {
        match self {
            Target::App => "A",
            Target::Crash => "C",
            Target::Diag => "D",
        }
    }

    fn from_tag(tag: &str) -> Option<Self> {
        match tag {
            "A" => Some(Target::App),
            "C" => Some(Target::Crash),
            "D" => Some(Target::Diag),
            _ => None,
        }
    }
}

/// 后台 writer 的消息：普通日志行，或「写完前面所有行并 flush 后回执」的同步信号。
/// 用专用命令消息而非哨兵字符串，避免与任意日志正文冲突。
enum LogMsg {
    Line(String),
    Flush(mpsc::SyncSender<()>),
}

/// 全局唯一的发送端。`OnceLock` 保证只初始化一次且线程安全；
/// `try_send(&self)` 无需加锁即可被任意线程并发调用。
static LOG_SENDER: OnceLock<SyncSender<LogMsg>> = OnceLock::new();

fn log_sender() -> &'static SyncSender<LogMsg> {
    LOG_SENDER.get_or_init(|| {
        let (tx, rx) = mpsc::sync_channel(LOG_QUEUE_CAPACITY);
        if std::thread::Builder::new()
            .name("fastpin-log-writer".into())
            .spawn(move || writer_loop(rx))
            .is_err()
        {
            // 线程创建失败：接收端随闭包一起 drop，发送端随之断开，
            // 后续 try_send 返回 Err，日志静默降级为丢弃，绝不影响主流程。
        }
        tx
    })
}

/// 退出前调用：把队列里尚未落盘的日志写完并逐句柄 flush，返回时日志已安全落盘。
/// `LOG_SENDER` 是进程级单例、永不 drop，writer 原先只能靠 `recv` 返回 Err 才退出，
/// 末尾的「退出前尽力冲刷」因此永远走不到；这里改为显式发一条 Flush 命令并等回执。
/// 阻塞发送保证 Flush 排在此前所有 try_send 的行之后（FIFO），回执时它们都已落盘。
/// 超时上限防止慢盘把退出路径挂死；writer 不存在或回执超时一律静默降级。
pub fn flush_logs() {
    let tx = log_sender();
    let (reply_tx, reply_rx) = mpsc::sync_channel(1);
    if tx.send(LogMsg::Flush(reply_tx)).is_err() {
        return; // writer 未启动/已退出，没有可冲刷的日志
    }
    let _ = reply_rx.recv_timeout(Duration::from_secs(2));
}

fn append_log(target: Target, msg: &str) {
    let line = format!("{} {msg}\n", now_stamp());
    enqueue(target, line);
}

/// 非阻塞入队：队列满（Full）或 writer 已退出（Disconnected）时丢弃该行。
fn enqueue(target: Target, line: String) {
    let msg = format!("{}{TAG_SEP}{line}", target.tag());
    let _ = log_sender().try_send(LogMsg::Line(msg));
}

fn parse_msg(msg: String) -> Option<(Target, String)> {
    let (tag, line) = msg.split_once(TAG_SEP)?;
    Some((Target::from_tag(tag)?, line.to_owned()))
}

// ── 日志轮转 ────────────────────────────────────────────────────────────
// 三个日志共同的问题：长开句柄或频繁追加，文件只增不减（diag.log 每 tick 一行）。
// 这里做最简按大小轮转：只保留一代备份 `*.log.1`，超阈值时覆盖旧备份。

/// 单个日志文件的轮转阈值：8 MiB。
const LOG_ROTATE_BYTES: u64 = 8 * 1024 * 1024;

/// 磁盘上的文件是否达到轮转阈值。
fn log_exceeds(path: &Path, max_bytes: u64) -> bool {
    std::fs::metadata(path)
        .map(|m| m.len() >= max_bytes)
        .unwrap_or(false)
}

/// 把 `path` 改名为 `*.log.1`。Windows 上 rename 目标已存在会失败，但绝不先删
/// 旧备份：先把 `path` 挪到临时名，再删旧 `.1`，最后把临时名改成 `.1`；
/// 任一步失败都把临时名改回 `path` 并返回 false，保住当前日志与旧备份。
fn rotate_file(path: &Path) -> bool {
    let rotated = path.with_extension("log.1");
    let temp = path.with_extension("log.rotating");
    if std::fs::rename(path, &temp).is_err() {
        return false;
    }
    // 旧备份只在确实存在时删除；删不掉就回滚，绝不带着半成品继续。
    if rotated.exists() && std::fs::remove_file(&rotated).is_err() {
        let _ = std::fs::rename(&temp, path);
        return false;
    }
    if std::fs::rename(&temp, &rotated).is_err() {
        let _ = std::fs::rename(&temp, path);
        return false;
    }
    true
}

/// 若 `path` 达到 `max_bytes` 就轮转成 `*.log.1`，返回是否发生轮转。
/// 阈值由参数传入而非写死，便于单测用极小文件覆盖轮转分支。
/// 生产路径已禁止同步轮转，此函数仅保留给单测验证轮转细节。
#[cfg(test)]
fn rotate_if_needed(path: &Path, max_bytes: u64) -> bool {
    log_exceeds(path, max_bytes) && rotate_file(path)
}

/// 写之前检查长开句柄对应的文件：超限则先 drop 句柄（顺带 flush，并释放
/// 文件句柄，否则 Windows 上无法 rename），改名后把句柄置空，下一行写入时
/// 由 `open_rotated` 打开全新的文件。
fn rotate_target(handle: &mut Option<BufWriter<std::fs::File>>, path: &Path) {
    if log_exceeds(path, LOG_ROTATE_BYTES) {
        // take() 会 drop BufWriter，未 flush 的缓冲会被静默丢弃。先显式 flush，
        // 失败则放弃本轮轮转，避免丢掉最多一批日志。
        if let Some(f) = handle.as_mut() {
            if f.flush().is_err() {
                return;
            }
        }
        let _ = handle.take();
        rotate_file(path);
    }
}

/// 打开日志前先按需轮转，避免从上一轮残留的大文件继续追加。
fn open_rotated(path: &Path) -> Option<BufWriter<std::fs::File>> {
    if log_exceeds(path, LOG_ROTATE_BYTES) {
        rotate_file(path);
    }
    OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .ok()
        .map(BufWriter::new)
}

/// 向某个长开句柄追加一行；句柄不存在时按需打开（打开前按需轮转，失败则静默丢弃）。
fn write_line(handle: &mut Option<BufWriter<std::fs::File>>, path: &Path, line: &str) {
    if handle.is_none() {
        *handle = open_rotated(path);
    }
    if let Some(f) = handle.as_mut() {
        let _ = f.write_all(line.as_bytes());
    }
}

fn flush(handle: &mut Option<BufWriter<std::fs::File>>) {
    if let Some(f) = handle.as_mut() {
        let _ = f.flush();
    }
}

/// writer 主循环：阻塞取首条，随后尽量排空队列凑成一批，再按目标批量写、
/// 批量 flush（句柄长开复用）。`create_dir_all` 只在这里执行一次。
/// 队列里出现 Flush 命令时，先把此前积攒的整批写完并 flush，再回执。
fn writer_loop(rx: Receiver<LogMsg>) {
    // 目录只创建一次；失败则 writer 静默退出，日志降级为丢弃。
    let Some(dir) = cached_log_dir().cloned() else {
        return;
    };
    if create_dir_all(&dir).is_err() {
        return;
    }
    let diag = diag_paths();
    // exe 旁目录通常已存在，尽力创建即可，不作为退出条件。
    if let Some(p) = &diag.exe_side {
        if let Some(parent) = p.parent() {
            let _ = create_dir_all(parent);
        }
    }

    let app_path = dir.join("app.log");
    let crash_path = dir.join("crash.log");

    // 三个目标各持一个长开句柄，避免每次日志都 open/close。
    // diag 的两个候选路径只解析一次，首次打开时选中并记住，供后续轮转使用。
    let mut app_f: Option<BufWriter<std::fs::File>> = None;
    let mut crash_f: Option<BufWriter<std::fs::File>> = None;
    let mut diag_f: Option<BufWriter<std::fs::File>> = None;
    let mut diag_path: Option<PathBuf> = None;

    // 阻塞等待第一条；所有发送端 drop 时 recv 返回 Err -> 线程退出。
    while let Ok(first) = rx.recv() {
        // 首条就是 Flush：没有新行可写，直接冲刷已有句柄并回执。
        let first_line = match first {
            LogMsg::Line(msg) => parse_msg(msg),
            LogMsg::Flush(reply) => {
                flush_all_handles(&mut app_f, &mut crash_f, &mut diag_f);
                let _ = reply.send(());
                continue;
            }
        };

        let mut batch: Vec<(Target, String)> = Vec::new();
        if let Some(parsed) = first_line {
            batch.push(parsed);
        }
        // 排空队列凑批；遇到 Flush 立即停手，保证回执发生在这批写完并 flush 之后。
        // 排在 Flush 之后的消息留给下一轮，不影响「回执时前面的行都已落盘」。
        let mut pending_reply = None;
        while let Ok(more) = rx.try_recv() {
            match more {
                LogMsg::Line(msg) => {
                    if let Some(parsed) = parse_msg(msg) {
                        batch.push(parsed);
                    }
                }
                LogMsg::Flush(reply) => {
                    pending_reply = Some(reply);
                    break;
                }
            }
        }

        // 每批开始前只检查本批会写到、且已打开的文件：超限则关句柄并轮转，
        // 下一行写入时由 open_rotated 打开新文件。不在逐行循环里 stat，避免热路径开销。
        let mut need_app = false;
        let mut need_crash = false;
        let mut need_diag = false;
        for (target, _) in &batch {
            match target {
                Target::App => need_app = true,
                Target::Crash => need_crash = true,
                Target::Diag => need_diag = true,
            }
        }
        if need_app {
            rotate_target(&mut app_f, &app_path);
        }
        if need_crash {
            rotate_target(&mut crash_f, &crash_path);
        }
        if need_diag {
            if let Some(p) = &diag_path {
                rotate_target(&mut diag_f, p);
            }
        }

        for (target, line) in batch {
            match target {
                Target::App => write_line(&mut app_f, &app_path, &line),
                Target::Crash => write_line(&mut crash_f, &crash_path, &line),
                Target::Diag => {
                    if diag_f.is_none() {
                        // 首次 diag：按顺序尝试 profile / exe 旁，选第一个能打开的；
                        // open_rotated 会在打开前处理上一轮残留的大文件。
                        for p in [&diag.profile, &diag.exe_side].into_iter().flatten() {
                            if let Some(f) = open_rotated(p) {
                                diag_f = Some(f);
                                diag_path = Some(p.clone());
                                break;
                            }
                        }
                    }
                    if let Some(f) = diag_f.as_mut() {
                        let _ = f.write_all(line.as_bytes());
                    }
                }
            }
        }

        // 每批结束统一 flush 一次（而非每次 log_* 调用都 flush）。
        // Flush 回执必须在这之后：调用方据此认为它之前的所有行都已落盘。
        flush_all_handles(&mut app_f, &mut crash_f, &mut diag_f);
        if let Some(reply) = pending_reply {
            let _ = reply.send(());
        }
    }
    // 退出前尽力冲刷，减少进程退出时丢失最后一批的可能。
    flush_all_handles(&mut app_f, &mut crash_f, &mut diag_f);
}

/// 三个长开句柄一起 flush；抽出供每批收尾与 Flush 回执共用。
fn flush_all_handles(
    app_f: &mut Option<BufWriter<std::fs::File>>,
    crash_f: &mut Option<BufWriter<std::fs::File>>,
    diag_f: &mut Option<BufWriter<std::fs::File>>,
) {
    flush(app_f);
    flush(crash_f);
    flush(diag_f);
}
fn missing_localappdata() -> crate::error::Error {
    crate::error::Error::new("LOCALAPPDATA is not set")
}

fn now_stamp() -> String {
    // Local time via Windows would be nicer; this is enough to correlate runs.
    let d = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    format!("t={}", d.as_secs())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn p95_of_20_is_near_tail() {
        let v: Vec<u128> = (1..=20).collect();
        assert_eq!(percentile(&v, 50.0), 10);
        assert!(percentile(&v, 95.0) >= 19);
    }

    #[test]
    fn percentile_empty_is_zero() {
        assert_eq!(percentile(&[], 50.0), 0);
    }

    /// 沙箱拒绝写 %TEMP%，与其它测试一致把临时目录放在 target/ 下；
    /// 进程 id + 自增序号保证并发测试进程间不冲突。
    fn test_dir(tag: &str) -> PathBuf {
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join(format!(
                "fastpin-{tag}-test-{}-{}",
                std::process::id(),
                SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            ))
    }

    #[test]
    fn sync_log_line_appends_and_is_readable() {
        // log_app_sync / log_diag_sync 共用的同步入口：路径可注入，
        // 不必依赖 LOCALAPPDATA 缓存或后台 writer 即可验证内容与追加语义。
        let dir = test_dir("sync-append");
        let _ = create_dir_all(&dir);
        let path = dir.join("app.log");

        assert!(write_log_line_sync(&path, "first line"));
        let first = std::fs::read_to_string(&path).expect("同步写入后应能立即读回");
        assert_eq!(first, "first line\n");

        // 重复调用是追加而非覆盖，且同步可见。
        assert!(write_log_line_sync(&path, "second line"));
        let both = std::fs::read_to_string(&path).expect("追加后应能立即读回");
        assert_eq!(both, format!("{first}second line\n"));
        assert_eq!(both.bytes().filter(|b| *b == b'\n').count(), 2);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn crash_line_is_written_sync_and_readable() {
        // 用 std 构造唯一临时目录，避免为单测新增依赖；路径可注入，
        // 因此不必依赖 LOCALAPPDATA 或后台 writer 即可验证同步落盘。
        // 沙箱拒绝写 %TEMP%，与其它测试一致放在 target/ 下。
        let dir = test_dir("crash");
        let _ = create_dir_all(&dir);
        let path = dir.join("crash.log");

        write_crash_line_sync(&path, "panic on thread main: boom");
        let first = std::fs::read_to_string(&path).expect("同步写入后应能立即读回");
        assert!(first.contains("panic on thread main: boom"));
        assert_eq!(first.bytes().filter(|b| *b == b'\n').count(), 1);

        // 再次调用是追加而非覆盖，且同步可见。
        write_crash_line_sync(&path, "second line");
        let both = std::fs::read_to_string(&path).expect("追加后应能立即读回");
        assert_eq!(both, format!("{first}second line\n"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn concurrent_sync_writes_never_join_lines() {
        // 路径可注入，直接让多线程并发调用同步写。若 msg 与换行分两次 write，
        // 就会与别的线程交错，产生粘连行（两行的标记落在同一行里）。
        let dir = test_dir("sync-concurrent");
        let _ = create_dir_all(&dir);
        let path = dir.join("app.log");

        const THREADS: usize = 8;
        const LINES: usize = 50;
        let handles: Vec<_> = (0..THREADS)
            .map(|t| {
                let path = path.clone();
                std::thread::spawn(move || {
                    for i in 0..LINES {
                        // 长行 + 重复正文，便于检测行首/行尾是否被截断或粘连。
                        let msg = format!("thread-{t:02} line-{i:03} {}", "x".repeat(64));
                        assert!(write_log_line_sync(&path, &msg));
                    }
                })
            })
            .collect();
        for h in handles {
            h.join().expect("同步写线程不应 panic");
        }

        let content = std::fs::read_to_string(&path).expect("并发写后应能读回");
        let lines: Vec<&str> = content.lines().collect();
        assert_eq!(lines.len(), THREADS * LINES, "行数应完整，不得粘连");
        for line in lines {
            assert_eq!(
                line.matches("thread-").count(),
                1,
                "每行只能有一个行首标记: {line}"
            );
            assert!(line.ends_with("xxxxxxxx"), "行尾必须完整: {line}");
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn sync_and_buffered_writer_lines_never_join_lines() {
        // 模仿后台 writer：一个线程持长开 BufWriter，分批 write_all + flush；
        // 同时多个线程走同步路径（每次按 path 新开句柄）。两种写入交错时，
        // 每行仍应自带完整标记与行尾，不得粘成一行。
        let dir = test_dir("sync-vs-buffered");
        let _ = create_dir_all(&dir);
        let path = dir.join("app.log");

        const SYNC_THREADS: usize = 4;
        const SYNC_LINES: usize = 40;
        const BUFFERED_LINES: usize = 40;

        let mut handles: Vec<std::thread::JoinHandle<()>> = Vec::new();
        for t in 0..SYNC_THREADS {
            let path = path.clone();
            handles.push(std::thread::spawn(move || {
                for i in 0..SYNC_LINES {
                    let msg = format!("sync-{t:02}-{i:03} {}", "y".repeat(32));
                    assert!(write_log_line_sync(&path, &msg));
                }
            }));
        }
        let bg_path = path.clone();
        handles.push(std::thread::spawn(move || {
            let mut f = BufWriter::new(
                OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&bg_path)
                    .expect("后台 writer 应能打开日志"),
            );
            for i in 0..BUFFERED_LINES {
                let line = format!("bg-{i:03} {}\n", "z".repeat(32));
                f.write_all(line.as_bytes()).expect("批量写应成功");
                if i % 8 == 7 {
                    f.flush().expect("批量 flush 应成功");
                }
            }
            f.flush().expect("收尾 flush 应成功");
        }));
        for h in handles {
            h.join().expect("写线程不应 panic");
        }

        let content = std::fs::read_to_string(&path).expect("并发写后应能读回");
        let lines: Vec<&str> = content.lines().collect();
        assert_eq!(
            lines.len(),
            SYNC_THREADS * SYNC_LINES + BUFFERED_LINES,
            "同步与后台写入的行数应完整，不得粘连"
        );
        for line in lines {
            let marks = line.matches("sync-").count() + line.matches("bg-").count();
            assert_eq!(marks, 1, "每行只能有一个标记: {line}");
            let tail_ok =
                line.ends_with("y".repeat(32).as_str()) || line.ends_with("z".repeat(32).as_str());
            assert!(tail_ok, "行尾必须完整: {line}");
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rotate_if_needed_splits_log_and_overwrites_backup() {
        let dir = test_dir("rotate");
        let _ = create_dir_all(&dir);
        let path = dir.join("app.log");
        let backup = dir.join("app.log.1");
        let old = "old content longer than four bytes";
        std::fs::write(&path, old).expect("准备旧日志");

        // 未达阈值：什么都不做。
        assert!(!rotate_if_needed(&path, 1024));
        assert!(path.exists());
        assert!(!backup.exists());

        // 超阈值：原文件改名为 .1，原路径消失（下次写入时重新创建）。
        assert!(rotate_if_needed(&path, 4));
        assert!(backup.exists(), "应生成 .1 备份");
        assert!(!path.exists(), "原文件应已改名");
        assert_eq!(std::fs::read_to_string(&backup).expect("读回备份"), old);

        // 再次超阈值：覆盖旧 .1，而不是因目标已存在而失败。
        let newer = "second generation";
        std::fs::write(&path, newer).expect("准备第二代日志");
        assert!(rotate_if_needed(&path, 4));
        assert_eq!(
            std::fs::read_to_string(&backup).expect("读回覆盖后的备份"),
            newer
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}
