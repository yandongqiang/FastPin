//! Local PP-OCRv6 recognition. Screenshots never leave the machine.

pub mod reading_order;

use std::path::PathBuf;
use std::sync::{Arc, Condvar, Mutex, Once};
use std::time::{Duration, Instant};

use image::RgbImage;
use oar_ocr::domain::tasks::TextDetectionConfig;
use oar_ocr::oarocr::{OAROCRBuilder, OAROCR};

use crate::error::{Error, Result};
use reading_order::Block;

const DET_MODEL: &str = "PP-OCRv6_det_small.onnx";
const REC_MODEL: &str = "PP-OCRv6_rec_small.onnx";
const DICT: &str = "ppocrv6_dict.txt";

/// Loading the models costs ~160 ms, so the session is worth keeping between
/// clicks — but a tray-resident app must not sit on an ORT arena forever.
const IDLE_TIMEOUT: Duration = Duration::from_secs(60);

struct Engine {
    /// Behind an `Arc` so inference can run after the engine mutex is
    /// released — `OAROCR::predict` only needs `&self`.
    ocr: Arc<OAROCR>,
    last_used: Instant,
}

static ENGINE: Mutex<Option<Engine>> = Mutex::new(None);
/// Paired with `ENGINE`: the single idle thread waits here and is woken by
/// every request, so new work restarts the 60 s timer instead of spawning a
/// fresh sleeper thread per recognition.
static IDLE: Condvar = Condvar::new();
static IDLE_THREAD: Once = Once::new();

/// Blocking: ~130 ms warm, ~290 ms cold. Call it off the UI thread.
pub fn recognize(bgra: &[u8], w: u32, h: u32, stride: u32) -> Result<String> {
    let image = to_rgb(bgra, w, h, stride)?;

    ensure_idle_thread();

    let ocr = {
        let mut guard = lock();
        if guard.is_none() {
            *guard = Some(Engine {
                ocr: Arc::new(build()?),
                last_used: Instant::now(),
            });
        }
        let engine = guard.as_mut().expect("just populated");
        engine.last_used = Instant::now();
        // Restart the idle timer under the same lock the waiter uses, so a
        // wakeup can never be lost between the update and the notify.
        IDLE.notify_all();
        Arc::clone(&engine.ocr)
    }; // the mutex is released before the (slow) inference starts

    let result = ocr.predict(vec![image]);

    let pages = result.map_err(|e| Error::new(format!("OCR failed: {e}")))?;
    let blocks: Vec<Block> = pages
        .into_iter()
        .flat_map(|page| page.text_regions)
        .filter_map(|region| {
            Some(Block {
                text: region.text?.to_string(),
                poly: region
                    .bounding_box
                    .points
                    .iter()
                    .map(|p| (p.x as f64, p.y as f64))
                    .collect(),
            })
        })
        .collect();
    Ok(reading_order::to_plain_text(&blocks))
}

/// True when `dir` holds all three model files with bytes in them. A zero-byte
/// file is what an interrupted copy or download leaves behind, and ORT refuses
/// it later, so it counts as missing. Unreadable entries count as missing too.
pub fn models_present_in(dir: &std::path::Path) -> bool {
    [DET_MODEL, REC_MODEL, DICT].iter().all(|file| {
        dir.join(file)
            .metadata()
            .map(|meta| meta.is_file() && meta.len() > 0)
            .unwrap_or(false)
    })
}

/// Whether the resolved model directory can drive OCR. There is no fallback
/// recognizer in this build, so a missing model means OCR is simply off.
pub fn models_available() -> bool {
    models_dir()
        .map(|dir| models_present_in(&dir))
        .unwrap_or(false)
}

/// Where [`engine_status`] found the models. The distinction exists so the
/// settings page never paints a green "installed" dot for a `cargo run` build
/// that is merely leaning on the source tree.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ModelOrigin {
    /// A user-chosen directory, `FASTPIN_MODELS`, or `models\v6` next to the
    /// executable — a real installation.
    Installed,
    /// Only `<CARGO_MANIFEST_DIR>/../../models/v6` exists. OCR works here,
    /// but the machine itself has nothing installed.
    DevFallback,
}

/// Everything the resources page renders about the engine. Recomputed on every
/// render, so deleting or replacing model files outside the app updates the
/// dot and the label together.
#[derive(Clone, Debug)]
pub struct EngineStatus {
    /// Green dot only when an installed directory is complete.
    pub available: bool,
    /// The directory OCR actually reads, shown so the page cannot imply the
    /// program directory when the resolved path is elsewhere.
    pub dir: Option<PathBuf>,
    pub label: &'static str,
}

/// Pure decision behind [`engine_status`], split out so the non-green branches
/// are testable without the real machine's model folder.
fn classify(present: bool, origin: ModelOrigin, configured: bool) -> (bool, &'static str) {
    match (origin, present) {
        (ModelOrigin::Installed, true) => (true, "PP-OCRv6 Small"),
        (ModelOrigin::Installed, false) if configured => (false, "该目录缺少模型文件"),
        (ModelOrigin::Installed, false) => (false, "模型缺失，识别不可用"),
        // `cargo run` builds can OCR from the repo, but nothing is installed.
        (ModelOrigin::DevFallback, true) => (false, "开发目录模型，未安装"),
        (ModelOrigin::DevFallback, false) => (false, "模型缺失，识别不可用"),
    }
}

pub fn engine_status() -> EngineStatus {
    let configured = crate::config::get().ocr.models_dir.is_some();
    let Ok((dir, origin)) = resolve_models_dir() else {
        return EngineStatus {
            available: false,
            dir: None,
            label: "模型缺失，识别不可用",
        };
    };
    let (available, label) = classify(models_present_in(&dir), origin, configured);
    EngineStatus {
        available,
        dir: Some(dir),
        label,
    }
}

/// A panic while holding the lock must not disable OCR for the rest of the
/// session — the session behind it is still usable.
fn lock() -> std::sync::MutexGuard<'static, Option<Engine>> {
    ENGINE.lock().unwrap_or_else(|e| e.into_inner())
}

/// 丢弃已缓存的引擎并唤醒空闲线程，使下一次 `recognize` 按当前的模型目录重建。
/// 模型目录被修改 / 清空、或设置被重置后必须调用：否则已加载的 `ENGINE` 最长
/// 60 秒内仍会用旧模型识别，用户换了目录却看不到变化。
pub fn invalidate_engine() {
    *lock() = None;
    // 让正在 wait_timeout 的空闲线程立刻醒来（它看到 None 会重新等待），
    // 避免旧的等待状态与新引擎相互干扰。
    IDLE.notify_all();
}

fn build() -> Result<OAROCR> {
    let dir = models_dir()?;
    for file in [DET_MODEL, REC_MODEL, DICT] {
        let usable = dir
            .join(file)
            .metadata()
            .map(|meta| meta.is_file() && meta.len() > 0)
            .unwrap_or(false);
        if !usable {
            return Err(Error::new(format!(
                "OCR model missing: {}",
                dir.join(file).display()
            )));
        }
    }
    OAROCRBuilder::new(
        dir.join(DET_MODEL).to_string_lossy().into_owned(),
        dir.join(REC_MODEL).to_string_lossy().into_owned(),
        dir.join(DICT).to_string_lossy().into_owned(),
    )
    // PaddleOCR's own PP-OCRv6 defaults; they hold up on screen text.
    .text_detection_config(TextDetectionConfig {
        score_threshold: 0.2,
        box_threshold: 0.45,
        unclip_ratio: 1.4,
        max_candidates: 3000,
        ..Default::default()
    })
    .build()
    .map_err(|e| Error::new(format!("OCR init failed: {e}")))
}

/// `settings.json` wins, then `FASTPIN_MODELS`, then the install layout the C#
/// build already uses, then the in-repo copy a `cargo run` build sits beside.
/// The origin travels with the path so the UI can tell an install from a
/// developer-only fallback.
fn resolve_models_dir() -> Result<(PathBuf, ModelOrigin)> {
    if let Some(dir) = crate::config::get().ocr.models_dir {
        return Ok((dir, ModelOrigin::Installed));
    }
    if let Some(dir) = std::env::var_os("FASTPIN_MODELS") {
        return Ok((PathBuf::from(dir), ModelOrigin::Installed));
    }
    let exe = std::env::current_exe()?;
    let base = exe
        .parent()
        .ok_or_else(|| Error::new("executable has no parent directory"))?;
    let shipped = base.join("models").join("v6");
    if shipped.is_dir() {
        return Ok((shipped, ModelOrigin::Installed));
    }
    // ponytail: a `cargo run` build sits in `target\`, where the packaged
    // `models\v6` is not. The repo copy is the same three files, so fall back
    // to it rather than making every developer set FASTPIN_MODELS.
    Ok((PathBuf::from(REPO_MODELS), ModelOrigin::DevFallback))
}

fn models_dir() -> Result<PathBuf> {
    resolve_models_dir().map(|(dir, _)| dir)
}

/// The in-repo copy, resolved at compile time. Absent on a user machine, which
/// is why the directory next to the exe is checked first.
const REPO_MODELS: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../models/v6");

/// Pure form of the idle policy, split out so the 60 s rule is testable
/// without sleeping or loading a model.
fn should_drop(last_used: Instant, now: Instant) -> bool {
    now.duration_since(last_used) >= IDLE_TIMEOUT
}

/// One detached thread for the whole process. It re-arms a 60 s timeout on
/// every request (via `IDLE.notify_all`) and releases the engine only after a
/// full idle period really elapsed, so the ORT arena is freed without a new
/// sleeper thread per recognition.
fn idle_loop() {
    let mut guard = lock();
    loop {
        let (g, timeout) = IDLE
            .wait_timeout(guard, IDLE_TIMEOUT)
            .unwrap_or_else(|e| e.into_inner());
        guard = g;
        if timeout.timed_out()
            && guard
                .as_ref()
                .is_some_and(|e| should_drop(e.last_used, Instant::now()))
        {
            *guard = None;
        }
    }
}

fn ensure_idle_thread() {
    IDLE_THREAD.call_once(|| {
        // Best effort: if the thread cannot spawn, OCR still works, it just
        // keeps the engine loaded until the process exits.
        let _ = std::thread::Builder::new()
            .name("fastpin-ocr-idle".into())
            .spawn(idle_loop);
    });
}

fn to_rgb(bgra: &[u8], w: u32, h: u32, stride: u32) -> Result<RgbImage> {
    if w == 0 || h == 0 {
        return Err(Error::new("OCR input bitmap is invalid"));
    }
    let need = (h - 1) as usize * stride as usize + w as usize * 4;
    if bgra.len() < need {
        return Err(Error::new("OCR input bitmap is invalid"));
    }
    // 3 B/px RGB buffer: validate against the shared budget and allocate
    // fallibly instead of the old unbounded `Vec::with_capacity(w*h*3)`.
    let budget = crate::image_budget::BUDGET;
    let len = budget
        .checked_bytes(w as u64, h as u64, 3)
        .map_err(|e| Error::new(format!("OCR input bitmap is invalid: {e}")))?;
    let mut buf = budget
        .alloc_zeroed(len)
        .map_err(|e| Error::new(format!("OCR input bitmap is invalid: {e}")))?;
    for y in 0..h as usize {
        let row = &bgra[y * stride as usize..];
        for x in 0..w as usize {
            let p = &row[x * 4..];
            let i = (y * w as usize + x) * 3;
            buf[i] = p[2];
            buf[i + 1] = p[1];
            buf[i + 2] = p[0];
        }
    }
    RgbImage::from_raw(w, h, buf).ok_or_else(|| Error::new("OCR input bitmap is invalid"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// Unique scratch directory; the real model folders are never touched.
    /// Lives under target/ because %TEMP% is blocked by the harness sandbox.
    fn scratch(name: &str) -> PathBuf {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join("ocr-tests")
            .join(format!("fastpin-ocr-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        root
    }

    fn write_models(dir: &std::path::Path, bytes: usize) {
        for file in [DET_MODEL, REC_MODEL, DICT] {
            fs::write(dir.join(file), vec![7u8; bytes]).unwrap();
        }
    }

    #[test]
    fn models_need_three_non_empty_files() {
        let dir = scratch("present");
        assert!(!models_present_in(&dir), "empty directory");

        write_models(&dir, 0);
        assert!(!models_present_in(&dir), "zero-byte files");

        fs::remove_file(dir.join(REC_MODEL)).unwrap();
        assert!(!models_present_in(&dir), "one file missing");

        write_models(&dir, 1);
        assert!(models_present_in(&dir), "all three non-empty");

        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn models_in_a_missing_directory_are_unavailable() {
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join("ocr-tests")
            .join("does-not-exist");
        assert!(!models_present_in(&dir));
    }

    #[test]
    fn classify_never_greens_a_missing_or_uninstalled_engine() {
        let (available, label) = classify(true, ModelOrigin::Installed, false);
        assert!(available);
        assert_eq!(label, "PP-OCRv6 Small");

        assert!(!classify(false, ModelOrigin::Installed, false).0);
        assert!(!classify(false, ModelOrigin::Installed, true).0);
        // A complete repo fallback still means "nothing installed".
        let (available, label) = classify(true, ModelOrigin::DevFallback, false);
        assert!(!available);
        assert!(label.contains("未安装"));
        assert!(!classify(false, ModelOrigin::DevFallback, false).0);
    }

    #[test]
    fn to_rgb_drops_alpha_and_honours_stride() {
        // 2x2 BGRA with 4 bytes of row padding.
        let stride = 2 * 4 + 4;
        let mut bgra = vec![0u8; stride * 2];
        for (i, px) in [[10, 20, 30], [40, 50, 60], [70, 80, 90], [100, 110, 120]]
            .iter()
            .enumerate()
        {
            let at = (i / 2) * stride + (i % 2) * 4;
            bgra[at..at + 4].copy_from_slice(&[px[0], px[1], px[2], 255]);
        }
        let img = to_rgb(&bgra, 2, 2, stride as u32).expect("valid bitmap");
        assert_eq!(img.get_pixel(0, 0).0, [30, 20, 10]);
        assert_eq!(img.get_pixel(1, 0).0, [60, 50, 40]);
        assert_eq!(img.get_pixel(0, 1).0, [90, 80, 70]);
        assert_eq!(img.get_pixel(1, 1).0, [120, 110, 100]);
    }

    #[test]
    fn to_rgb_rejects_short_buffers() {
        assert!(to_rgb(&[0; 4], 2, 2, 8).is_err());
        assert!(to_rgb(&[], 0, 0, 0).is_err());
    }

    #[test]
    fn to_rgb_refuses_dimensions_outside_the_budget() {
        // A full, in-range input buffer: only the budget can refuse this.
        let stride = 16_385 * 4;
        let bgra = vec![0u8; 16_385 * 4];
        assert!(to_rgb(&bgra, 16_385, 1, stride).is_err());
        assert!(to_rgb(&[], 0, 10, 40).is_err());
    }

    #[test]
    fn idle_policy_drops_only_after_a_full_timeout() {
        // Use a future base so subtracting 60 s can never underflow the clock.
        let base = Instant::now() + Duration::from_secs(120);
        assert!(!should_drop(base, base));
        assert!(!should_drop(base - Duration::from_secs(59), base));
        assert!(should_drop(base - IDLE_TIMEOUT, base));
        assert!(should_drop(
            base - IDLE_TIMEOUT - Duration::from_secs(1),
            base
        ));
    }
}
