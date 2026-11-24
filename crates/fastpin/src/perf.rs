use std::fs::{create_dir_all, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::sync::OnceLock;
use std::time::Instant;

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
        let mut f = OpenOptions::new().create(true).append(true).open(path)?;
        f.write_all(msg.as_bytes())?;
        f.write_all(b"\n")?;
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

/// 全局唯一的发送端。`OnceLock` 保证只初始化一次且线程安全；
/// `try_send(&self)` 无需加锁即可被任意线程并发调用。
static LOG_SENDER: OnceLock<SyncSender<String>> = OnceLock::new();

fn log_sender() -> &'static SyncSender<String> {
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

fn append_log(target: Target, msg: &str) {
    let line = format!("{} {msg}\n", now_stamp());
    enqueue(target, line);
}

/// 非阻塞入队：队列满（Full）或 writer 已退出（Disconnected）时丢弃该行。
fn enqueue(target: Target, line: String) {
    let msg = format!("{}{TAG_SEP}{line}", target.tag());
    let _ = log_sender().try_send(msg);
}

fn parse_msg(msg: String) -> Option<(Target, String)> {
    let (tag, line) = msg.split_once(TAG_SEP)?;
    Some((Target::from_tag(tag)?, line.to_owned()))
}

/// 向某个长开句柄追加一行；句柄不存在时按需打开（失败则静默丢弃）。
fn write_line(handle: &mut Option<BufWriter<std::fs::File>>, path: &Path, line: &str) {
    if handle.is_none() {
        match OpenOptions::new().create(true).append(true).open(path) {
            Ok(f) => *handle = Some(BufWriter::new(f)),
            Err(_) => return,
        }
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
fn writer_loop(rx: Receiver<String>) {
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
    let mut app_f: Option<BufWriter<std::fs::File>> = None;
    let mut crash_f: Option<BufWriter<std::fs::File>> = None;
    let mut diag_f: Option<BufWriter<std::fs::File>> = None;

    // 阻塞等待第一条；所有发送端 drop 时 recv 返回 Err -> 线程退出。
    while let Ok(first) = rx.recv() {
        let mut batch: Vec<(Target, String)> = Vec::new();
        if let Some(parsed) = parse_msg(first) {
            batch.push(parsed);
        }
        while let Ok(more) = rx.try_recv() {
            if let Some(parsed) = parse_msg(more) {
                batch.push(parsed);
            }
        }

        for (target, line) in batch {
            match target {
                Target::App => write_line(&mut app_f, &app_path, &line),
                Target::Crash => write_line(&mut crash_f, &crash_path, &line),
                Target::Diag => {
                    if diag_f.is_none() {
                        // 首次 diag：按顺序尝试 profile / exe 旁，选第一个能打开的。
                        for p in [&diag.profile, &diag.exe_side].into_iter().flatten() {
                            if let Ok(f) = OpenOptions::new().create(true).append(true).open(p) {
                                diag_f = Some(BufWriter::new(f));
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
        flush(&mut app_f);
        flush(&mut crash_f);
        flush(&mut diag_f);
    }
    // 退出前尽力冲刷，减少进程退出时丢失最后一批的可能。
    flush(&mut app_f);
    flush(&mut crash_f);
    flush(&mut diag_f);
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

    #[test]
    fn crash_line_is_written_sync_and_readable() {
        // 用 std 构造唯一临时目录，避免为单测新增依赖；路径可注入，
        // 因此不必依赖 LOCALAPPDATA 或后台 writer 即可验证同步落盘。
        // 沙箱拒绝写 %TEMP%，与其它测试一致放在 target/ 下。
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join(format!(
                "fastpin-crash-test-{}-{}",
                std::process::id(),
                SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            ));
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
}
