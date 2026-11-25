use std::cell::{Cell, RefCell};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime};

use gpui::{App, Application, QuitMode};

fn gpui_app() -> Application {
    gpui_kit::application().with_assets(crate::overlay::AppAssets)
}
use windows::core::PCWSTR;
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::Com::{CoInitializeEx, COINIT_APARTMENTTHREADED};
use windows::Win32::System::Console::{AttachConsole, ATTACH_PARENT_PROCESS};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::ProcessStatus::{
    GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS, PROCESS_MEMORY_COUNTERS_EX,
};
use windows::Win32::System::SystemInformation::GetLocalTime;
use windows::Win32::System::Threading::GetCurrentProcess;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, MessageBoxW, PostMessageW, PostQuitMessage, RegisterClassExW,
    CS_HREDRAW, CS_VREDRAW, MB_ICONERROR, MB_ICONINFORMATION, MB_OK, MB_SETFOREGROUND, WM_DESTROY,
    WM_HOTKEY, WNDCLASSEXW, WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_OVERLAPPED,
};

use crate::capture::{
    capture_for_action, crop_live_frame, crop_sdr_bgra, drop_gpu_resources, prepare_capture,
    CapturedDesktop, Gpu, GpuShaders, WgcLive,
};
use crate::codec::{set_clipboard_dib, write_image_file};
use crate::error::{Error, Result};
use crate::gpui_bridge::drop_freeze_swapchain;
use crate::hotkey::{HotkeyAction, HotkeyBindings};
use crate::overlay::composite::apply_annotations;
use crate::overlay::editor::Element;
use crate::overlay::{show_overlay, OverlayResult};
use crate::perf::{write_perf_summary, PerfSession};
use crate::platform::{
    action_from_wparam, action_wparam, add_tray_icon, handle_tray, install_ll_hook, load_app_icon,
    register_emergency_quit, register_fallback_hotkeys, remove_tray_icon, set_dpi_aware,
    set_hook_target, taskbar_created_message, uninstall_ll_hook, unpack_hotkey,
    unregister_fallback_hotkeys, SingleInstance, TrayCommand, EMERGENCY_QUIT_ID, WM_FASTPIN_HOTKEY,
    WM_FASTPIN_HOTKEY_RECORDED, WM_FASTPIN_PIN_MENU, WM_FASTPIN_SETTINGS, WM_TRAYICON,
};

static BUSY: AtomicBool = AtomicBool::new(false);

/// 截图目标路径的进程内预留集合。去重扫描和「本进程已占用的目标名」必须在
/// 一把锁内，否则两个同时完成的截屏会看到同一个「不存在」的名字，再互相覆盖。
/// 只在内存里预留名字：不再用 `create_new` 在目标路径留下 0 字节占位文件。
static SAVE_PATH_LOCK: std::sync::LazyLock<std::sync::Mutex<HashSet<PathBuf>>> =
    std::sync::LazyLock::new(|| std::sync::Mutex::new(HashSet::new()));

/// 写盘临时文件前缀。进程退出时最多留下带此前缀的半成品，由启动时 sweep 清理。
const IMAGE_TMP_PREFIX: &str = "fastpin-shot-tmp-";

/// 临时文件名自增序号，保证同进程并发截图的临时路径互不相同。
static TMP_SEQ: AtomicU64 = AtomicU64::new(0);

thread_local! {
    static GPUI_APP: RefCell<Option<gpui::AsyncApp>> = const { RefCell::new(None) };
    static BENCH_CTRL: RefCell<Option<BenchControl>> = const { RefCell::new(None) };
}

struct BenchControl {
    samples: Vec<u128>,
    remaining: usize,
    /// After each sample: continue warm loop, quit once-mode, or stress sample.
    kind: BenchKind,
    stress_n: usize,
    stress_done: usize,
    stress_ws10: Option<usize>,
    stress_ws100: Option<usize>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum BenchKind {
    Once,
    Warm,
    Stress,
}

fn bind_gpui_app(cx: &App) {
    GPUI_APP.with(|slot| {
        *slot.borrow_mut() = Some(cx.to_async());
    });
}

fn unbind_gpui_app() {
    GPUI_APP.with(|slot| {
        *slot.borrow_mut() = None;
    });
}

fn with_gpui_app<R>(f: impl FnOnce(&mut App) -> R) -> Option<R> {
    GPUI_APP.with(|slot| {
        let app = slot.borrow().clone()?;
        // gpui-pre 0.3.9: AsyncApp::update returns R directly (panics when the
        // app is gone) instead of the old Result<R>.
        Some(app.update(f))
    })
}

fn install_gpu_lifecycle(cx: &App) {
    bind_gpui_app(cx);
    crate::gpu_lifecycle::init(cx.gpu_generation());
    cx.on_gpu_reset_begin(|| {
        let _ = crate::gpu_lifecycle::begin_reset();
        drop_gpu_resources();
        drop_freeze_swapchain();
    });
    // Patched gpui-pre hook: fires after the platform swapped in the new
    // device set and bumped `gpu_generation()`, before windows are repainted.
    cx.on_gpu_reset_end(|generation| {
        let _ = crate::gpu_lifecycle::note_device_recreated(generation);
    });
}

pub(crate) fn capture_from_app(
    cx: &mut App,
    region: bool,
    region_hint: Option<(i32, i32, u32, u32)>,
    perf: &mut PerfSession,
) -> Result<CapturedDesktop> {
    let generation = cx.gpu_generation();
    // A reset-end callback may have been missed; trust the platform's
    // generation so a capture is not refused forever.
    crate::gpu_lifecycle::observe_generation(generation);
    if !crate::gpu_lifecycle::accepts_new_work() {
        return Err(Error::new("GPU device is resetting; capture refused"));
    }
    let overlay_luid = cx.adapter_luid().unwrap_or((0, 0));
    // Borrow GPU only long enough to clone the device and compile shaders — do not hold
    // with_gpu_context across the parallel WGC wait.
    let (device, shaders) = match cx.with_gpu_context(|device, _ctx| {
        GpuShaders::get_or_compile(generation, device).map(|shaders| (device.clone(), shaders))
    }) {
        Some(Ok(pair)) => pair,
        Some(Err(e)) => return Err(e),
        None => return Err(Error::new("GPU context unavailable")),
    };
    let prepared = prepare_capture(region, region_hint, &device, &shaders, overlay_luid)?;
    match cx
        .with_gpu_context(|device, ctx| capture_for_action(prepared, perf, device, ctx, &shaders))
    {
        Some(r) => r,
        None => Err(Error::new("GPU context unavailable")),
    }
}

pub(crate) fn crop_from_app(
    cx: &mut App,
    cap: &CapturedDesktop,
    x: u32,
    y: u32,
    w: u32,
    h: u32,
) -> Result<(Vec<u8>, u32, u32, u32)> {
    match cx.with_gpu_context(|_device, ctx| crop_sdr_bgra(cap, ctx, x, y, w, h)) {
        Some(r) => r,
        None => Err(Error::new("GPU context unavailable")),
    }
}

/// Long-capture poll tick: a fresh capture cropped to the polling region.
/// `abs_x`/`abs_y` are absolute screen coordinates (the overlay's origin plus
/// the selection), so the crop survives whichever origin the fresh capture
/// ends up with -- a single-monitor fallback shifts it.
///
/// The returned buffer is repacked to a tight `width * 4` stride: the
/// stitcher and the grayscale aligner both index rows directly.
pub(crate) fn capture_region_bgra(
    cx: &mut App,
    abs_x: i32,
    abs_y: i32,
    w: u32,
    h: u32,
) -> Result<(Vec<u8>, u32, u32, u32)> {
    let mut perf = PerfSession::start();
    let cap = capture_from_app(cx, true, Some((abs_x, abs_y, w, h)), &mut perf)?;
    // 跨屏选区会走 capture_all；某屏失败时合成图留有空洞，直接裁剪会把空白
    // 拼进长截图。必须显式失败，而不是把缺口当成正常画面。
    let (right, bottom) = (abs_x as i64 + w as i64, abs_y as i64 + h as i64);
    if cap.missing_regions.iter().any(|m| {
        (m.x as i64) < right
            && (m.x as i64 + m.w as i64) > abs_x as i64
            && (m.y as i64) < bottom
            && (m.y as i64 + m.h as i64) > abs_y as i64
    }) {
        return Err(Error::new(
            "capture region includes a monitor that could not be captured",
        ));
    }
    let x = abs_x - cap.origin_x;
    let y = abs_y - cap.origin_y;
    if x < 0 || y < 0 {
        return Err(Error::new("long-capture region outside the fresh capture"));
    }
    let (px, cw, ch, stride) = crop_from_app(cx, &cap, x as u32, y as u32, w, h)?;
    Ok(repack_tight(px, cw, ch, stride))
}

/// Repacks a BGRA buffer to a tight `width * 4` stride: the stitcher and the
/// grayscale aligner both index rows directly.
fn repack_tight(px: Vec<u8>, cw: u32, ch: u32, stride: u32) -> (Vec<u8>, u32, u32, u32) {
    if stride == cw * 4 {
        return (px, cw, ch, stride);
    }
    let tight = (cw * 4) as usize;
    let mut packed = Vec::with_capacity(tight * ch as usize);
    for row in 0..ch as usize {
        let start = row * stride as usize;
        packed.extend_from_slice(&px[start..start + tight]);
    }
    (packed, cw, ch, cw * 4)
}

/// Long-capture poll tick using a persistent WGC session: crops the session's
/// newest frame instead of starting a fresh capture. `Ok(None)` means WGC has
/// delivered nothing new since the last call -- the caller treats that as
/// "nothing scrolled", not an error.
#[allow(clippy::too_many_arguments, clippy::type_complexity)]
pub(crate) fn capture_region_live(
    cx: &mut App,
    live: &WgcLive,
    gpu: &Gpu,
    sdr_white: f32,
    mon_x: i32,
    mon_y: i32,
    abs_x: i32,
    abs_y: i32,
    w: u32,
    h: u32,
) -> Result<Option<(Vec<u8>, u32, u32, u32)>> {
    let outcome = cx.with_gpu_context(|_device, ctx| -> Result<Option<(Vec<u8>, u32, u32, u32)>> {
        let Some(frame) = live.take_latest(gpu, ctx)? else {
            return Ok(None);
        };
        let cropped = crop_live_frame(
            gpu,
            ctx,
            &frame.texture,
            sdr_white,
            // 会话开始后显示器布局若变了，mon_x/mon_y 可能与选区不再匹配；
            // 直接 as u32 会让负值下溢成巨大尺寸，这里与 capture_region_bgra
            // 一样钳零，绝不把负数送进裁剪。
            (abs_x - mon_x).max(0) as u32,
            (abs_y - mon_y).max(0) as u32,
            w,
            h,
        )?;
        Ok(Some(cropped))
    });
    match outcome {
        Some(Ok(Some((px, cw, ch, stride)))) => Ok(Some(repack_tight(px, cw, ch, stride))),
        Some(Ok(None)) => Ok(None),
        Some(Err(e)) => Err(e),
        None => Err(Error::new("GPU context unavailable")),
    }
}

/// Pin copy / save: tight BGRA, already composited, no marks to apply. Pins
/// start from a region crop, so their history records use the region source.
pub(crate) fn export_pin_pixels(px: Vec<u8>, w: u32, h: u32, write_file: bool, copy: bool) {
    export_pixels_async(px, w, h, w * 4, write_file, copy, None, "region");
}

/// Release builds are Windows-subsystem, so a shell-launched process starts with
/// no standard handles and every `println!` goes nowhere. The CLI subcommands
/// document their output, so borrow the parent's console before producing any.
fn attach_parent_console() {
    unsafe {
        let _ = AttachConsole(ATTACH_PARENT_PROCESS);
    }
}

/// Release builds are windows-subsystem with `panic = "abort"`: without this
/// hook a panic dies silently and the tray icon just vanishes. The message and
/// location land in crash.log so field reports have something to attach.
/// `panic = "abort"` means the process stops the instant this hook returns, so
/// writing must be synchronous here — enqueuing to the background log writer
/// would lose the message before it ever reaches disk.
fn install_panic_hook() {
    std::panic::set_hook(Box::new(|info| {
        let name = std::thread::current()
            .name()
            .unwrap_or("<unnamed>")
            .to_string();
        let msg = format!("panic on thread {name}: {info}");
        crate::perf::log_crash_sync(&msg);
        eprintln!("{msg}");
    }));
}

pub fn run() -> Result<()> {
    install_panic_hook();
    // 迁移与下载清理会跨进程改目录/删文件，必须在参数解析之后、GUI 路径的
    // 单实例 acquire 之后执行（见 run_tray）；CLI 子命令不应碰真实用户目录。
    let args: Vec<String> = std::env::args().collect();
    if args.len() > 1 {
        attach_parent_console();
    }
    match args.get(1).map(String::as_str) {
        Some("--once-capture") => {
            let path = args
                .get(2)
                .cloned()
                .unwrap_or_else(|| "shot.png".to_string());
            return once_capture(PathBuf::from(path));
        }
        Some("--self-test") => return self_test(),
        Some("--bench-once") => return bench_once(),
        Some("--bench-overlay") => {
            let mode = arg_value(&args, "--mode").unwrap_or_else(|| "warm".into());
            let n = arg_value(&args, "--n")
                .and_then(|s| s.parse().ok())
                .unwrap_or(30);
            return match mode.as_str() {
                "first" => bench_overlay_first(n),
                "warm" => bench_overlay_warm(n),
                other => Err(Error::new(format!("unknown --mode {other}"))),
            };
        }
        Some("--stress-overlay") => {
            let n = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(100usize);
            return stress_overlay(n);
        }
        _ => {}
    }
    run_tray()
}

fn arg_value(args: &[String], flag: &str) -> Option<String> {
    args.iter()
        .position(|a| a == flag)
        .and_then(|i| args.get(i + 1).cloned())
}

pub fn once_capture(path: PathBuf) -> Result<()> {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
    }
    set_dpi_aware();
    let outcome = std::rc::Rc::new(RefCell::new(None::<Result<(u32, u32)>>));
    gpui_app().run({
        let outcome = outcome.clone();
        let path = path.clone();
        move |cx: &mut App| {
            crate::overlay::theme::init(cx);
            install_gpu_lifecycle(cx);
            let mut perf = PerfSession::start();
            perf.mark("T0");
            perf.mark("T1");
            perf.mark("T2");
            let r = (|| {
                let cap = capture_from_app(cx, false, None, &mut perf)?;
                let (px, w, h, stride) = crop_from_app(cx, &cap, 0, 0, cap.width, cap.height)?;
                // `--once-capture` always writes PNG: the path comes from the
                // command line, so honouring a configured JPG/WebP format here
                // would put WebP bytes in a file the caller named `.png`.
                write_image_file(
                    &path,
                    crate::config::ImageFormat::Png,
                    90,
                    &px,
                    w,
                    h,
                    stride,
                )?;
                let _ = perf.write_log();
                Ok((w, h))
            })();
            *outcome.borrow_mut() = Some(r);
            cx.quit();
        }
    });
    unbind_gpui_app();
    let result = outcome.borrow_mut().take();
    match result {
        Some(Ok((w, h))) => {
            println!("wrote {} ({}x{})", path.display(), w, h);
            Ok(())
        }
        Some(Err(e)) => Err(e),
        None => Err(Error::new("once-capture did not run")),
    }
}

fn self_test() -> Result<()> {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
    }
    set_dpi_aware();
    let cleanup_hwnd = Rc::new(RefCell::new(None::<HWND>));
    let boot_error = Rc::new(RefCell::new(None::<Error>));
    let ready = Rc::new(Cell::new(false));

    gpui_app().run({
        let cleanup_hwnd = cleanup_hwnd.clone();
        let boot_error = boot_error.clone();
        let ready = ready.clone();
        move |cx: &mut App| {
            crate::overlay::theme::init(cx);
            let mut perf = PerfSession::start();
            perf.mark("T0");
            let result = (|| -> Result<()> {
                let _instance = SingleInstance::acquire()?;
                let hwnd = create_hidden_overlapped_window()?;
                *cleanup_hwnd.borrow_mut() = Some(hwnd);
                let bindings = HotkeyBindings::poc_defaults();
                set_hook_target(hwnd, bindings);
                let hook_ok = install_ll_hook().is_ok();
                // LL hook 可用时不装 RegisterHotKey 兜底：两条通路同时注册会让
                // 同一次按键发出 WM_FASTPIN_HOTKEY 和 WM_HOTKEY，动作执行两次。
                if !hook_ok {
                    register_fallback_hotkeys(hwnd, &bindings);
                }
                let tray_ok = add_tray_icon(hwnd).is_ok();
                perf.mark("T1");
                let _ = perf.write_log();
                println!("fastpin-ready hook={hook_ok} tray={tray_ok}");
                ready.set(true);
                Ok(())
            })();
            if let Err(e) = result {
                eprintln!("{e}");
                *boot_error.borrow_mut() = Some(e);
            }
            cx.quit();
        }
    });

    if let Some(hwnd) = cleanup_hwnd.borrow_mut().take() {
        remove_tray_icon(hwnd);
        unregister_fallback_hotkeys(hwnd);
    }
    uninstall_ll_hook();

    if let Some(e) = boot_error.borrow_mut().take() {
        return Err(e);
    }
    if !ready.get() {
        return Err(Error::new("self-test did not reach ready"));
    }
    Ok(())
}

fn bench_once() -> Result<()> {
    std::env::set_var("FASTPIN_BENCH", "1");
    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
    }
    set_dpi_aware();
    BENCH_CTRL.with(|slot| {
        *slot.borrow_mut() = Some(BenchControl {
            samples: Vec::new(),
            remaining: 1,
            kind: BenchKind::Once,
            stress_n: 0,
            stress_done: 0,
            stress_ws10: None,
            stress_ws100: None,
        });
    });
    gpui_app().run(|cx: &mut App| {
        crate::overlay::theme::init(cx);
        install_gpu_lifecycle(cx);
        start_bench_iteration(cx);
    });
    unbind_gpui_app();
    Ok(())
}

fn bench_overlay_first(n: usize) -> Result<()> {
    let exe = std::env::current_exe().map_err(|e| Error::new(e.to_string()))?;
    let mut samples = Vec::with_capacity(n);
    for i in 0..n {
        let out = Command::new(&exe)
            .env("FASTPIN_BENCH", "1")
            .arg("--bench-once")
            .output()
            .map_err(|e| Error::new(e.to_string()))?;
        if !out.status.success() {
            let stderr = String::from_utf8_lossy(&out.stderr);
            return Err(Error::new(format!(
                "bench-once #{i} failed status={:?}: {stderr}",
                out.status.code()
            )));
        }
        let stdout = String::from_utf8_lossy(&out.stdout);
        let Some(ms) = parse_bench_sample(&stdout) else {
            return Err(Error::new(format!(
                "bench-once #{i} missing bench-sample line: {stdout}"
            )));
        };
        samples.push(ms);
        println!("first sample {}/{} t2_t7_ms={ms}", i + 1, n);
    }
    let path = write_perf_summary("first", &mut samples)?;
    println!("wrote {}", path.display());
    Ok(())
}

fn bench_overlay_warm(n: usize) -> Result<()> {
    std::env::set_var("FASTPIN_BENCH", "1");
    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
    }
    set_dpi_aware();
    BENCH_CTRL.with(|slot| {
        *slot.borrow_mut() = Some(BenchControl {
            samples: Vec::with_capacity(n),
            remaining: n,
            kind: BenchKind::Warm,
            stress_n: 0,
            stress_done: 0,
            stress_ws10: None,
            stress_ws100: None,
        });
    });
    gpui_app().run(|cx: &mut App| {
        crate::overlay::theme::init(cx);
        install_gpu_lifecycle(cx);
        start_bench_iteration(cx);
    });
    unbind_gpui_app();
    let mut samples = BENCH_CTRL.with(|slot| {
        slot.borrow_mut()
            .take()
            .map(|c| c.samples)
            .unwrap_or_default()
    });
    if samples.is_empty() {
        return Err(Error::new("warm bench produced no samples"));
    }
    let path = write_perf_summary("warm", &mut samples)?;
    println!("wrote {}", path.display());
    Ok(())
}

fn stress_overlay(n: usize) -> Result<()> {
    std::env::set_var("FASTPIN_BENCH", "1");
    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
    }
    set_dpi_aware();
    BENCH_CTRL.with(|slot| {
        *slot.borrow_mut() = Some(BenchControl {
            samples: Vec::new(),
            remaining: n,
            kind: BenchKind::Stress,
            stress_n: n,
            stress_done: 0,
            stress_ws10: None,
            stress_ws100: None,
        });
    });
    gpui_app().run(|cx: &mut App| {
        crate::overlay::theme::init(cx);
        install_gpu_lifecycle(cx);
        start_bench_iteration(cx);
    });
    unbind_gpui_app();
    let ctrl = BENCH_CTRL.with(|slot| slot.borrow_mut().take());
    let Some(ctrl) = ctrl else {
        return Err(Error::new("stress bench state missing"));
    };
    println!(
        "stress n={} ws10_bytes={:?} ws100_bytes={:?}",
        ctrl.stress_n, ctrl.stress_ws10, ctrl.stress_ws100
    );
    if let (Some(a), Some(b)) = (ctrl.stress_ws10, ctrl.stress_ws100) {
        let delta = b as i64 - a as i64;
        println!("stress ws_delta_10_to_100={delta}");
    }
    Ok(())
}

fn parse_bench_sample(stdout: &str) -> Option<u128> {
    for line in stdout.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("bench-sample t2_t7_ms=") {
            return rest.trim().parse().ok();
        }
    }
    None
}

fn start_bench_iteration(cx: &mut App) {
    let mut perf = PerfSession::start();
    perf.mark("T2");
    match capture_from_app(cx, true, None, &mut perf) {
        Ok(cap) => {
            let _ = show_overlay(cx, cap, &mut perf, false);
        }
        Err(e) => {
            eprintln!("bench capture failed: {e}");
            cx.quit();
        }
    }
}

fn private_working_set_bytes() -> Option<usize> {
    unsafe {
        let mut counters = PROCESS_MEMORY_COUNTERS_EX {
            cb: std::mem::size_of::<PROCESS_MEMORY_COUNTERS_EX>() as u32,
            ..Default::default()
        };
        GetProcessMemoryInfo(
            GetCurrentProcess(),
            &mut counters as *mut _ as *mut PROCESS_MEMORY_COUNTERS,
            counters.cb,
        )
        .ok()?;
        Some(counters.PrivateUsage)
    }
}

fn on_bench_sample(cx: &mut App, ms: u128) {
    let mut quit = false;
    BENCH_CTRL.with(|slot| {
        let mut guard = slot.borrow_mut();
        let Some(ctrl) = guard.as_mut() else {
            quit = true;
            return;
        };
        println!("bench-sample t2_t7_ms={ms}");
        match ctrl.kind {
            BenchKind::Once => {
                ctrl.samples.push(ms);
                ctrl.remaining = 0;
                quit = true;
            }
            BenchKind::Warm => {
                ctrl.samples.push(ms);
                ctrl.remaining = ctrl.remaining.saturating_sub(1);
                if ctrl.remaining == 0 {
                    quit = true;
                }
            }
            BenchKind::Stress => {
                ctrl.stress_done += 1;
                let done = ctrl.stress_done;
                if done == 10 {
                    ctrl.stress_ws10 = private_working_set_bytes();
                }
                if done == 100 {
                    ctrl.stress_ws100 = private_working_set_bytes();
                }
                ctrl.remaining = ctrl.remaining.saturating_sub(1);
                if ctrl.remaining == 0 {
                    quit = true;
                }
            }
        }
    });
    if quit {
        cx.quit();
        return;
    }
    // Yield a frame so hide settles, then next iteration.
    cx.spawn(async move |cx| {
        cx.background_executor()
            .timer(Duration::from_millis(16))
            .await;
        cx.update(start_bench_iteration);
    })
    .detach();
}

fn run_tray() -> Result<()> {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
    }
    set_dpi_aware();
    let mut perf_boot = PerfSession::start();
    perf_boot.mark("T0");
    let _instance = match SingleInstance::acquire() {
        Ok(i) => i,
        Err(_) => {
            // 第二实例以前静默退出，用户以为程序没启动成功。给一个可见提示，
            // 不引入 IPC：告诉用户热键已可用、设置从已有实例的托盘打开。
            crate::perf::log_app("second instance exited: FastPin is already running");
            // 这条日志也要赶在进程结束前落盘。
            crate::perf::flush_logs();
            let text = wide("FastPin 已在运行，热键可直接截图；设置请从托盘打开。\0");
            let title = wide("FastPin\0");
            unsafe {
                let _ = MessageBoxW(
                    None,
                    PCWSTR(text.as_ptr()),
                    PCWSTR(title.as_ptr()),
                    MB_OK | MB_ICONINFORMATION | MB_SETFOREGROUND,
                );
            }
            return Ok(());
        }
    };
    // 与历史维护同理：迁移和下载清理会整份改目录/删文件，STORE_LOCK 挡不住
    // 第二实例，必须在 acquire 之后、且只在 GUI 路径执行。放在任何设置读取
    // （cleanup_stale_image_temps 会用截图目录）之前完成迁移。
    crate::migrate::migrate_legacy_paths();
    crate::update::cleanup_downloads();
    // 清掉上次退出遗留的半截截图临时文件。必须在单实例 acquire 成功之后：
    // 若第二实例在早退前清理，会按前缀删掉第一实例正在写/等待 rename 的临时文件
    // （Windows 下 FILE_SHARE_DELETE 允许跨进程删除未关闭句柄的文件），令截图丢失。
    // 放在 CoInitializeEx 之后：默认截图目录要走 SHGetKnownFolderPath。
    cleanup_stale_image_temps();
    // 回收崩溃/强杀后没人认领的拖出临时目录（FastPin-drag-*）。拖出文件由
    // 接收方持有句柄，不能像截图临时文件那样启动时立即删；按 24h 龄清理。
    // 同样必须在 acquire 成功之后，否则第二实例早退前会删掉第一实例正在
    // 供接收方读取的拖出文件。
    cleanup_stale_drag_files();
    // 清理上次写盘失败残留的 settings.tmp-*，同样必须在 acquire 成功之后，
    // 否则第二实例早退前会删掉第一实例正在写的临时配置。
    crate::config::cleanup_stale_temp_files();
    // 回收中断删除留下的历史目录，并应用停机期间累积记录的保留策略。同样必须
    // 在单实例 acquire 成功之后：cleanup_pending()/prune() 会整份重写 index.json，
    // 而 STORE_LOCK 只是进程内锁，挡不住另一个进程。若第二实例在“已在运行”
    // 早退前先 prune，会和第一实例并发读改写索引——旧快照覆盖新记录（目录成
    // 孤儿），或把第一实例的记录当 victim 删掉。早退路径绝不执行到此处。
    // （history 关闭时只做回收、不 prune；config 由 config::get() 按需加载。）
    crate::history::startup_maintenance();

    let cleanup_hwnd = std::rc::Rc::new(std::cell::RefCell::new(None::<HWND>));
    let boot_error = std::rc::Rc::new(std::cell::RefCell::new(None::<Error>));

    gpui_app().run({
        let cleanup_hwnd = cleanup_hwnd.clone();
        let boot_error = boot_error.clone();
        move |cx: &mut App| match boot_resident(cx, cleanup_hwnd) {
            Ok(()) => {
                perf_boot.mark("T1");
                let _ = perf_boot.write_log();
            }
            Err(e) => {
                eprintln!("{e}");
                *boot_error.borrow_mut() = Some(e);
                cx.quit();
            }
        }
    });

    if let Some(hwnd) = cleanup_hwnd.borrow_mut().take() {
        remove_tray_icon(hwnd);
        unregister_fallback_hotkeys(hwnd);
    }
    uninstall_ll_hook();

    // 应用退出：拖出文件的账本只清记录、不删文件（接收方可能还在读，
    // 留下来的临时文件由 OS 清理）。不清会让残留记录一直占着预算。
    crate::pin::drag_retention()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .forget();

    unbind_gpui_app();

    // 所有退出路径最终都汇到这里（托盘 Exit、紧急退出热键、WM_DESTROY、
    // Explicit quit）。writer 的发送端是进程级单例、永不 drop，靠它自然退出时的
    // 收尾 flush 永远走不到，必须显式等一次回执再返回 main。
    crate::perf::flush_logs();

    if let Some(e) = boot_error.borrow_mut().take() {
        return Err(e);
    }
    Ok(())
}

fn boot_resident(
    cx: &mut App,
    cleanup_hwnd: std::rc::Rc<std::cell::RefCell<Option<HWND>>>,
) -> Result<()> {
    // gpui-kit base globals (dialogs, inputs, focus traps) plus the persisted
    // appearance, before any window exists. Every later window inherits the
    // global theme, and the chrome snapshot is refreshed on each switch.
    crate::overlay::theme::init(cx);
    install_gpu_lifecycle(cx);
    // Tray-resident: closing the settings window (possibly the only window)
    // must not quit the process. The tray Exit menu is this app's only quit
    // path. gpui-pre 0.3.9: Explicit quit mode instead of the 0.2.2 patch's
    // set_quit_on_last_window_close(false).
    cx.set_quit_mode(QuitMode::Explicit);
    let hwnd = create_hidden_overlapped_window()?;
    *cleanup_hwnd.borrow_mut() = Some(hwnd);

    let bindings = HotkeyBindings::from_settings(&crate::config::get().hotkeys);
    set_hook_target(hwnd, bindings);
    // 只在这条主热键通路不可用时才注册 RegisterHotKey 兜底。两者并存时同一次
    // 按键会分别产生 WM_FASTPIN_HOTKEY（hook）与 WM_HOTKEY（fallback），
    // hidden_proc 对后者再次 post_action，全屏截图因此保存两份 PNG、剪贴板写两次。
    if let Err(e) = install_ll_hook() {
        eprintln!("LL hook failed ({e}); using RegisterHotKey fallback");
        register_fallback_hotkeys(hwnd, &bindings);
    }
    // Shell_NotifyIconW(NIM_ADD) can be denied by shell/Explorer state
    // (restart, UIPI, a stale icon entry). The LL hook and RegisterHotKey
    // fallback still deliver hotkeys, so the tray icon is optional infra:
    // propagating this error out of run() made main() exit(1) immediately,
    // which on a windows-subsystem release build is an invisible flash exit.
    // The TaskbarCreated handler re-adds the icon after the next Explorer
    // restart, so a transient failure here is recoverable.
    if let Err(e) = add_tray_icon(hwnd) {
        // Degraded mode: 热键通路与托盘独立，但托盘 Exit 是主要的退出入口，
        // 所以补注册紧急退出热键并提示用户。
        degrade_without_tray(hwnd, &e.to_string());
    }
    // 拖出文件的宽限期时钟由 complete() 启动，真正删除靠这里的周期 sweep。
    // 复用 GPUI 后台执行器的定时器，不新增线程；任务随 app 退出而终止。
    // 退出时的账本清理在 run_tray 的收尾路径里调用 forget()。
    cx.spawn(async move |cx| loop {
        cx.background_executor()
            .timer(Duration::from_secs(60))
            .await;
        // 三步走：锁内只摘快照、只按结果回填；阻塞的 remove_file / 删目录
        // 放在锁外，避免慢盘或杀软扫描时卡住其它拖出的 reserve() / complete()。
        let retention = crate::pin::drag_retention();
        let targets = retention
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .expired_targets(Instant::now());
        let results = crate::dragexport::Retention::finish_deletes(targets);
        retention
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .apply_deletes(results);
    })
    .detach();
    Ok(())
}

/// 统一的可见错误提示。release 构建下 `log_app` 只写一个本地日志文件，
/// 用户根本看不到；裁剪 / 合成 / 写盘这类"点了按钮却什么都没发生"的失败
/// 必须用 Win32 消息框直接告知。消息框本身是 `hwnd = NULL` 的顶层框，不会
/// 依赖任何 GPUI 窗口（失败时截图窗口可能正在关闭）。
///
/// 这里刻意用原子布尔标志而不是互斥量串行化：MessageBoxW 是模态调用，
/// 它自己的消息循环里本进程的 LL 热键 hook 仍然工作，用户在弹框期间再按
/// PrtSc 会在同一线程重入本函数；若此时持有非重入的 std Mutex，重入加锁
/// 会永久阻塞，连第一个框都无法关闭，整个 UI 挂死。跨线程持锁弹框同样
/// 会冻结 UI 线程。标志用 `swap` 抢占：已有框在显示时直接丢弃新的错误，
/// 避免弹框堆积；并发错误被合并/丢弃是有意的取舍，保证绝不在锁内做模态调用。
pub fn report_error(msg: &str) {
    static DIALOG_ACTIVE: AtomicBool = AtomicBool::new(false);
    if DIALOG_ACTIVE.swap(true, Ordering::SeqCst) {
        return;
    }
    let text = wide(&format!("{msg}\0"));
    let title = wide("FastPin\0");
    unsafe {
        let _ = MessageBoxW(
            None,
            PCWSTR(text.as_ptr()),
            PCWSTR(title.as_ptr()),
            MB_OK | MB_ICONERROR | MB_SETFOREGROUND,
        );
    }
    // MessageBoxW 即使返回 Err 也必须复位标志，否则以后所有错误提示都被吞掉。
    DIALOG_ACTIVE.store(false, Ordering::SeqCst);
}

/// 托盘不可用时统一降级：补注册紧急退出热键并给出一次性可见提示。启动和
/// Explorer 重启（TaskbarCreated）后的重新添加失败都要走这里，否则进程会
/// 既没有托盘图标也没有退出入口。
fn degrade_without_tray(hwnd: HWND, reason: &str) {
    crate::perf::log_app(&format!("tray icon unavailable: {reason}"));
    eprintln!("tray icon unavailable ({reason}); continuing without it");
    register_emergency_quit(hwnd);
    show_tray_unavailable_hint();
}

/// One-time, best-effort visible notice for tray-less mode. Shown on a helper
/// thread so it never blocks gpui startup; suppress with `FASTPIN_NO_TRAY_HINT`
/// (e.g. automated smoke tests). Never fake an icon we do not have.
fn show_tray_unavailable_hint() {
    static SHOWN: AtomicBool = AtomicBool::new(false);
    if SHOWN.swap(true, Ordering::SeqCst) || std::env::var_os("FASTPIN_NO_TRAY_HINT").is_some() {
        return;
    }
    let _ = std::thread::Builder::new()
        .name("fastpin-tray-hint".into())
        .spawn(|| {
            // 紧急退出热键本身也可能注册失败（被别的程序占用）。失败时不能再
            // 让用户按一个根本不存在的组合键，改为明确指向任务管理器。
            // hook 模块是 platform 的私有模块；同一份注册状态由公开模块 hotkey 持有，
            // platform::hook::emergency_quit_registered() 也只是转发到它。
            let quit_hint = if crate::hotkey::emergency_quit_registered() {
                "退出程序: 按 Ctrl+Alt+Shift+Q, 或在任务管理器结束 FastPin.exe。"
            } else {
                "紧急退出热键被占用, 无法使用。\n\
                 退出程序: 请在任务管理器结束 FastPin.exe。"
            };
            // 双通道全失败（LL hook 装不上、RegisterHotKey 兜底又被占用）时
            // 不能再宣称「热键截图仍可用」。take 是取走即清空，这里只在构造
            // 启动提示时调用一次；截图专属标志把「仅紧急退出键被占用」排除，
            // 那种情况下截图热键确实仍可用，保留原文案。
            let hotkey_detail = crate::hotkey::take_fallback_register_failure();
            let screenshot_failed = crate::hotkey::fallback_screenshot_failed();
            let hotkey_hint = if screenshot_failed {
                let detail = hotkey_detail.unwrap_or_else(|| "注册失败".into());
                format!(
                    "热键注册失败（{detail}），截图热键不可用，请在设置中重新绑定或检查冲突软件。\n\n"
                )
            } else {
                "热键截图仍可用。\n\n".to_string()
            };
            let text = wide(&format!(
                "FastPin 已在后台运行，但托盘图标未能创建 \
                 (Shell_NotifyIconW(NIM_ADD) 被系统拒绝, 0x80070005)。\n\n\
                 {hotkey_hint}\
                 {quit_hint}\n\
                 详情见 %LOCALAPPDATA%\\FastPin\\logs\\app.log。\0"
            ));
            let title = wide("FastPin 托盘图标不可用\0");
            unsafe {
                let _ = MessageBoxW(
                    None,
                    PCWSTR(text.as_ptr()),
                    PCWSTR(title.as_ptr()),
                    MB_OK | MB_ICONINFORMATION | MB_SETFOREGROUND,
                );
            }
        });
}

/// Never dispatch inline: this is reached from the LL hook chain (which Windows
/// times out) and from inside the tray menu's modal loop.
fn post_action(hwnd: HWND, action: HotkeyAction) {
    unsafe {
        let _ = PostMessageW(
            Some(hwnd),
            WM_FASTPIN_HOTKEY,
            WPARAM(action_wparam(action)),
            LPARAM(0),
        );
    }
}

fn create_hidden_overlapped_window() -> Result<HWND> {
    let name = wide("FastPinHidden\0");
    let icon = load_app_icon().unwrap_or_default();
    let wc = WNDCLASSEXW {
        cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
        style: CS_HREDRAW | CS_VREDRAW,
        lpfnWndProc: Some(hidden_proc),
        hInstance: unsafe { GetModuleHandleW(None)?.into() },
        hIcon: icon,
        hIconSm: icon,
        lpszClassName: PCWSTR(name.as_ptr()),
        ..Default::default()
    };
    unsafe {
        let _ = RegisterClassExW(&wc);
        let hwnd = CreateWindowExW(
            WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE | WS_EX_LAYERED,
            PCWSTR(name.as_ptr()),
            PCWSTR(wide("FastPin\0").as_ptr()),
            WS_OVERLAPPED,
            0,
            0,
            0,
            0,
            None,
            None,
            GetModuleHandleW(None).ok().map(Into::into),
            None,
        )?;
        // Match the working `tray-icon` helper window: these ex-styles keep it
        // out of the taskbar, and a WS_OVERLAPPED window created without
        // WS_VISIBLE is already hidden, so no ShowWindow(SW_HIDE) is needed.
        Ok(hwnd)
    }
}

unsafe extern "system" fn hidden_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if msg == WM_FASTPIN_HOTKEY {
        if let Some(action) = action_from_wparam(wparam.0) {
            if let Err(e) = dispatch_action(action) {
                crate::perf::log_app(&format!("action {action:?} failed: {e}"));
                // 全屏截图这类同步导出失败此前也只进日志，用户按了热键什么都没发生。
                report_error(&format!("截图失败：{e}"));
            }
        }
        return LRESULT(0);
    }
    if msg == WM_HOTKEY {
        if wparam.0 as i32 == EMERGENCY_QUIT_ID {
            crate::perf::log_app("emergency quit hotkey pressed; exiting");
            // 进程即将结束：显式冲刷后台日志队列，不能等 writer 自然退出。
            crate::perf::flush_logs();
            PostQuitMessage(0);
            return LRESULT(0);
        }
        // RegisterHotKey ids mirror action_wparam.
        if let Some(action) = action_from_wparam(wparam.0) {
            post_action(hwnd, action);
        }
        return LRESULT(0);
    }
    if msg == WM_FASTPIN_HOTKEY_RECORDED {
        let hotkey = unpack_hotkey(wparam.0);
        with_gpui_app(move |cx| crate::settings::on_hotkey_recorded(cx, hotkey));
        return LRESULT(0);
    }
    if msg == WM_FASTPIN_SETTINGS {
        with_gpui_app(crate::settings::open);
        return LRESULT(0);
    }
    if msg == WM_FASTPIN_PIN_MENU {
        let id = wparam.0 as u64;
        if let Some(Some(items)) = with_gpui_app(|cx| crate::pin::menu_items(cx, id)) {
            if let Some(cmd) = crate::pin::track_menu(hwnd, &items) {
                with_gpui_app(move |cx| crate::pin::run_menu_command(cx, id, cmd));
            }
        }
        return LRESULT(0);
    }
    // Tray menu: the popup runs its own modal loop inside `handle_tray`, so
    // there is no owner-draw traffic on the hidden window any more.
    if msg == taskbar_created_message() {
        if let Err(e) = add_tray_icon(hwnd) {
            // 不能静默吞掉：没有托盘图标时同样需要紧急退出通路和可见提示。
            degrade_without_tray(hwnd, &e.to_string());
        }
        return LRESULT(0);
    }
    if msg == WM_TRAYICON {
        if let Some(cmd) = handle_tray(hwnd, lparam) {
            match cmd {
                TrayCommand::Region => post_action(hwnd, HotkeyAction::Region),
                TrayCommand::Fullscreen => post_action(hwnd, HotkeyAction::Fullscreen),
                TrayCommand::CopyOnly => post_action(hwnd, HotkeyAction::CopyOnly),
                TrayCommand::CloseAllPins => {
                    let _ = with_gpui_app(crate::pin::close_all);
                }
                TrayCommand::Settings => unsafe {
                    let _ = PostMessageW(Some(hwnd), WM_FASTPIN_SETTINGS, WPARAM(0), LPARAM(0));
                },
                TrayCommand::Exit => {
                    remove_tray_icon(hwnd);
                    // 与紧急退出一致：进程即将结束，显式冲刷后台日志队列。
                    crate::perf::flush_logs();
                    PostQuitMessage(0);
                }
            }
        }
        return LRESULT(0);
    }
    if msg == WM_DESTROY {
        PostQuitMessage(0);
        return LRESULT(0);
    }
    DefWindowProcW(hwnd, msg, wparam, lparam)
}

fn dispatch_action(action: HotkeyAction) -> Result<()> {
    if BUSY.swap(true, Ordering::SeqCst) {
        return Ok(());
    }
    // Overlay flows clear BUSY from complete_region_shot once the user is done.
    match take_shot(action) {
        Ok(ShotFlow::OverlayOpen) => Ok(()),
        Ok(ShotFlow::Finished) => {
            BUSY.store(false, Ordering::SeqCst);
            Ok(())
        }
        Err(e) => {
            BUSY.store(false, Ordering::SeqCst);
            Err(e)
        }
    }
}

enum ShotFlow {
    Finished,
    OverlayOpen,
}

fn take_shot(action: HotkeyAction) -> Result<ShotFlow> {
    let mut perf = PerfSession::start();
    perf.mark("T2");
    // Crash breadcrumbs: if the process dies mid-capture, app.log shows how far it got.
    // 同步落盘：panic=abort / 强杀时后台 writer 来不及 flush 最后一批，
    // 这条最关键的路标必须当场写完；其余高频日志仍走异步 log_app。
    crate::perf::log_app_sync(&format!("capture start: {action:?}"));
    // Copy-only freezes the desktop like a region capture; the overlay then
    // confirms the drag straight to the clipboard. Only fullscreen skips the
    // overlay.
    let region = !matches!(action, HotkeyAction::Fullscreen);
    let cap = match with_gpui_app(|cx| capture_from_app(cx, region, None, &mut perf)) {
        Some(Ok(c)) => {
            crate::perf::log_app(&format!(
                "capture ok: {}x{} hdr={} dxgi_dup={}",
                c.width, c.height, c.hdr, c.used_dxgi_dup
            ));
            c
        }
        Some(Err(e)) => {
            crate::perf::log_app(&format!("capture failed: {e}"));
            return Err(e);
        }
        None => return Err(Error::new("GPUI app unavailable")),
    };

    match action {
        HotkeyAction::Region | HotkeyAction::CopyOnly => {
            // Capture is done; do not hold App across overlay interaction.
            let copy_only = matches!(action, HotkeyAction::CopyOnly);
            match with_gpui_app(|cx| show_overlay(cx, cap, &mut perf, copy_only)) {
                Some(_) => {
                    crate::perf::log_app("overlay open");
                    Ok(ShotFlow::OverlayOpen)
                }
                None => Err(Error::new("GPUI app unavailable")),
            }
        }
        HotkeyAction::Fullscreen => {
            let (x, y, w, h) = (0, 0, cap.width, cap.height);
            let (px, cw, ch, stride) = with_gpui_app(|cx| crop_from_app(cx, &cap, x, y, w, h))
                .unwrap_or_else(|| Err(Error::new("GPUI app unavailable")))?;
            // 全屏截图同样遵循「输出」页的 auto_save / auto_copy 开关。
            // 全屏没有选区、也不会进入覆盖层，所以必须保证至少一种输出：
            // 两个开关都关掉时退回复制到剪贴板，避免按下 PrtSc 毫无反应。
            let export = crate::config::get().screenshot.export;
            let write_file = export.auto_save;
            let copy = export.auto_copy || !write_file;
            export_pixels(px, cw, ch, stride, write_file, copy, "fullscreen")?;
            let _ = perf.write_log();
            Ok(ShotFlow::Finished)
        }
    }
}

pub(crate) fn complete_region_shot(
    result: OverlayResult,
    cap: Option<CapturedDesktop>,
    perf: Option<PerfSession>,
    source: &'static str,
    cx: &mut App,
) {
    let bench = crate::overlay::bench_auto_hide_enabled();
    if result.confirmed && !bench {
        if let Some(cap) = cap {
            export_annotated_region(cx, &cap, &result, source);
        }
    }
    let mut sample = None;
    if let Some(perf) = perf {
        if bench {
            sample = perf.delta_ms("T2", "T7");
            if sample.is_none() {
                eprintln!("bench: missing T2/T7 marks (T6=WaitForCommit T7=sentinel)");
            }
        }
        let _ = perf.write_log();
    }
    BUSY.store(false, Ordering::SeqCst);
    if bench {
        match sample {
            Some(ms) => on_bench_sample(cx, ms),
            None => cx.quit(),
        }
    }
}

pub(crate) fn export_annotated_region(
    cx: &mut App,
    cap: &CapturedDesktop,
    result: &OverlayResult,
    source: &'static str,
) {
    match crop_from_app(cx, cap, result.x, result.y, result.w, result.h) {
        Ok((px, cw, ch, stride)) => {
            let marks = Some((result.x as f64, result.y as f64, result.marks.clone()));
            export_pixels_async(
                px,
                cw,
                ch,
                stride,
                result.write_file,
                result.copy,
                marks,
                source,
            );
        }
        Err(e) => {
            crate::perf::log_app(&format!("crop failed: {e}"));
            report_error(&format!("截图裁剪失败：{e}"));
        }
    }
}

/// Runs annotation compositing and the clipboard/PNG export off the UI thread.
/// `crop_from_app`'s GPU readback has to stay on the UI thread, but everything
/// after that -- rasterizing marks, the clipboard DIB copy, the PNG encode --
/// is pure CPU work that would otherwise run before the overlay window gets a
/// chance to hide on the next frame.
#[allow(clippy::too_many_arguments)]
fn export_pixels_async(
    mut px: Vec<u8>,
    cw: u32,
    ch: u32,
    stride: u32,
    write_file: bool,
    copy: bool,
    marks: Option<(f64, f64, Vec<Element>)>,
    source: &'static str,
) {
    std::thread::spawn(move || {
        // default_png_path -> SHGetKnownFolderPath is COM; this is a fresh thread.
        unsafe {
            let _ = windows::Win32::System::Com::CoInitializeEx(
                None,
                windows::Win32::System::Com::COINIT_MULTITHREADED,
            );
        }
        if let Some((ox, oy, marks)) = marks {
            apply_annotations(&mut px, cw, ch, stride, ox, oy, &marks);
        }
        if let Err(e) = export_pixels(px, cw, ch, stride, write_file, copy, source) {
            crate::perf::log_app(&format!("export failed: {e}"));
            // 合成 / 剪贴板失败之前只有日志文件，用户看到的是窗口一闪消失、
            // 文件也没有。弹框明确告知。写盘失败在写线程里单独上报（见下）。
            report_error(&format!("截图导出失败：{e}"));
        }
    });
}

fn export_pixels(
    px: Vec<u8>,
    cw: u32,
    ch: u32,
    stride: u32,
    write_file: bool,
    copy: bool,
    source: &'static str,
) -> Result<()> {
    // Full-screen 4K is ~33 MB; share it with the writer thread instead of cloning.
    export_pixels_shared_with_source(
        std::sync::Arc::new(px),
        cw,
        ch,
        stride,
        write_file,
        copy,
        source,
        None,
    )
}

/// 与 [`export_pixels`] 相同的导出，但写盘线程结束后通过 `done` 把真实
/// 结果回传给调用方（长截图预览用它把"保存中…"更新成"已保存"或显示失败）。
/// 同步失败（路径预留 / 编码 / 剪贴板）也在返回前回传一次，保证调用方一定能
/// 收到一个终态消息。
pub(crate) fn export_pixels_shared_reported(
    px: std::sync::Arc<Vec<u8>>,
    cw: u32,
    ch: u32,
    stride: u32,
    write_file: bool,
    copy: bool,
    done: std::sync::mpsc::Sender<std::result::Result<(), String>>,
) -> Result<()> {
    let outcome = export_pixels_shared_with_source(
        px,
        cw,
        ch,
        stride,
        write_file,
        copy,
        "region",
        Some(done.clone()),
    );
    if let Err(e) = &outcome {
        // 写线程已经回报过的结果不会被覆盖：路径预留失败时写线程根本没启动，
        // 剪贴板失败与写盘是独立的两条终态，接收方取到任意一条失败都算如实反馈。
        let _ = done.send(Err(e.to_string()));
    }
    outcome
}

/// Same as [`export_pixels`] with an explicit capture-origin tag for the
/// per-source history filter. `&'static str` because every caller passes a
/// literal, which also keeps the tag safe to move into the export threads.
#[allow(clippy::too_many_arguments)]
fn export_pixels_shared_with_source(
    px: std::sync::Arc<Vec<u8>>,
    cw: u32,
    ch: u32,
    stride: u32,
    write_file: bool,
    copy: bool,
    source: &'static str,
    done: Option<std::sync::mpsc::Sender<std::result::Result<(), String>>>,
) -> Result<()> {
    if write_file {
        let mut export = crate::config::get().screenshot.export.clone();
        // A format this machine cannot encode falls back to PNG, so a config
        // copied from another PC still saves the screenshot instead of failing.
        // 设置页用同一个 effective_format 显示当前实际格式，并与这里的回退
        // 保持一致：本机缺 WebP 时不展示质量滑块、提示实际写的是 PNG。
        export.format = crate::codec::effective_format(export.format);
        let (path, temp) = default_image_path(&export)?;
        let px_file = px.clone();
        std::thread::spawn(move || {
            unsafe {
                let _ = windows::Win32::System::Com::CoInitializeEx(
                    None,
                    windows::Win32::System::Com::COINIT_MULTITHREADED,
                );
            }
            // 先写临时文件、成功后再原子 rename：进程在写盘中途被关闭时，目标
            // 路径不会出现 0 字节或半截 PNG；残留的临时文件由启动时 sweep 清理。
            let written = write_image_atomically(&temp, &path, |p| {
                write_image_file(
                    p,
                    export.format,
                    export.clamped_quality(),
                    &px_file,
                    cw,
                    ch,
                    stride,
                )
            });
            let published = match written {
                Ok(p) => p,
                Err(e) => {
                    release_reserved_path(&path);
                    crate::perf::log_app(&format!("image write failed: {e}"));
                    // 这是导出线程里的失败，用户看不到日志文件。弹窗明确告知，
                    // 同时回传给长截图预览（若有）更新状态。
                    report_error(&format!("截图写盘失败：{e}"));
                    if let Some(done) = &done {
                        let _ = done.send(Err(format!("写盘失败：{e}")));
                    }
                    return;
                }
            };
            // Read back the exact bytes the user saved so the history copy is
            // byte-identical (and uses the same format/extension) without a
            // second encode. Failures here never affect the save itself.
            if let Ok(bytes) = std::fs::read(&published) {
                crate::history::record_saved_image(
                    &bytes,
                    export.format.extension(),
                    cw,
                    ch,
                    source,
                );
            }
            // 真正写盘成功后再回报，绝不提前置「已保存」。
            if let Some(done) = &done {
                let _ = done.send(Ok(()));
            }
        });
    }
    if copy {
        // Not swallowed: copy-only exports have no PNG to prove they ran, so a
        // silent failure here reads as "the hotkey is dead".
        // A CF_DIB copy needs a second, uncompressed ~1 GiB buffer for a 16k
        // image. Above the shared budget paste PNG via the registered format
        // instead; an allocation failure there would abort the whole process.
        let copied = match crate::codec::dib_bytes(cw, ch) {
            Some(_) => set_clipboard_dib(&px, cw, ch, stride),
            None => {
                let png = crate::codec::encode_png_bgra(&px, cw, ch, stride)?;
                crate::codec::set_clipboard_png(&png)
            }
        };
        match copied {
            Ok(()) => {
                crate::perf::log_app(&format!("clipboard: {cw}x{ch} copied"));
                // Copy-only captures (no saved file) still get a history record
                // so copied screenshots appear in the gallery, mirroring
                // CitePix. When the same capture also wrote a file, that file
                // record is the canonical one and we don't duplicate it here.
                // History failures never fail the copy.
                if !write_file {
                    // PNG-encode on a worker thread: a full-screen 4K buffer is
                    // ~30 MB and synchronous encoding here would stall the UI.
                    // The clipboard DIB is already written above on this thread;
                    // history is bookkeeping and may finish slightly later.
                    let px_hist = px.clone();
                    std::thread::spawn(move || {
                        if let Ok(png) = crate::codec::encode_png_bgra(&px_hist, cw, ch, stride) {
                            crate::history::record_saved_image(&png, "png", cw, ch, source);
                        }
                    });
                }
            }
            Err(e) => {
                crate::perf::log_app(&format!("clipboard write failed: {e}"));
                return Err(e);
            }
        }
    }
    Ok(())
}

/// 计算本次截图的最终路径与临时路径，并在进程内预留最终名。
/// 不再用 `create_new` 在目标路径预留 0 字节文件：进程在写盘前退出时它会永久残留。
/// 写盘线程只写带 `IMAGE_TMP_PREFIX` 的临时文件，写完再原子 rename 到目标。
fn default_image_path(export: &crate::config::Export) -> Result<(PathBuf, PathBuf)> {
    // 配置里保存的是用户可见的原始路径；落盘前转成 `\\?\` 长路径，
    // 否则深 save_dir + 长文件名会在 create_dir_all / 写盘时撞上 MAX_PATH。
    let dir = crate::config::extended_path(&crate::config::screenshot_dir()?);
    std::fs::create_dir_all(&dir)?;
    // Local time, not UTC: the name is for the user reading their own folder.
    let t = unsafe { GetLocalTime() };
    let date = format!("{:04}{:02}{:02}", t.wYear, t.wMonth, t.wDay);
    let time = format!("{:02}{:02}{:02}", t.wHour, t.wMinute, t.wSecond);
    // 文件名主干按目录剩余预算截断，与长路径前缀一起双保险。
    let stem = export.expand_name_for(&date, &time, Some(&dir));
    let extension = export.format.extension();

    // 从查重到预留目标名全程持锁：进程内两个同时完成的截屏不能拿到同一个名字。
    // 只在内存里记账，不在磁盘上留下任何占位文件。
    let mut reserved = SAVE_PATH_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let candidate = unique_path(&dir, &stem, extension);
    let target = std::iter::once(candidate)
        .chain((2..).map(|n| dir.join(format!("{stem}_{n}.{extension}"))))
        .find(|p| !p.exists() && !reserved.contains(p))
        .ok_or_else(|| Error::new("无法分配截图文件名"))?;

    // 临时名带 pid + 自增序号：既避开同进程并发，也避开上次退出遗留的同名文件。
    let seq = TMP_SEQ.fetch_add(1, Ordering::Relaxed);
    let temp = dir.join(format!(
        "{IMAGE_TMP_PREFIX}{}-{seq}.{extension}",
        std::process::id()
    ));
    reserved.insert(target.clone());
    Ok((target, temp))
}

/// 候选名是否已被另一次截图预留。`publish_image` 在首选名被外部占用时会
/// 退到 `<stem>_2.ext` 等名字，这些名字可能正是别的截图的预留目标。
fn is_reserved_path(path: &Path) -> bool {
    SAVE_PATH_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .contains(path)
}

/// 写盘失败时把预留的目标名还回池子，避免一次失败永久占住这个文件名。
fn release_reserved_path(path: &Path) {
    let mut reserved = SAVE_PATH_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    reserved.remove(path);
}

/// 先写临时文件、成功后再原子 rename 到目标路径。写盘或发布失败都删除临时文件，
/// 保证退出时最多留下带 `IMAGE_TMP_PREFIX` 的半成品（由启动时 sweep 清理），
/// 绝不在目标路径留下 0 字节或半截 PNG。`write` 只负责把字节写进传入的临时路径。
fn write_image_atomically(
    temp: &Path,
    target: &Path,
    write: impl FnOnce(&Path) -> Result<()>,
) -> Result<PathBuf> {
    // 写盘/删除处再转一次：幂等，确保直接调用本函数时深路径也能落盘。
    let temp = crate::config::extended_path(temp);
    if let Err(e) = write(&temp) {
        let _ = std::fs::remove_file(&temp);
        return Err(e);
    }
    match publish_image(&temp, target) {
        Ok(published) => Ok(published),
        Err(e) => {
            let _ = std::fs::remove_file(&temp);
            Err(e)
        }
    }
}

/// 把写完的临时文件发布到目标路径：目标不存在就直接 rename；若被进程外占用
/// 则退到 `<stem>_2.<ext>` 等带序号后缀，绝不覆盖已有文件。返回真正落盘的路径。
fn publish_image(temp: &Path, target: &Path) -> Result<PathBuf> {
    let dir = target.parent().unwrap_or_else(|| Path::new("."));
    let stem = target
        .file_stem()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned();
    let ext = target
        .extension()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned();
    // 候选名无限递增在数学上不会耗尽，但文件系统异常（权限、命名冲突、长
    // 路径边界）可能让每一次 rename 都失败。release 下 panic=abort 会让
    // unreachable! 直接杀死托盘进程，所以这里必须返回可上报的错误。
    const MAX_CANDIDATES: u32 = 999;
    let temp = crate::config::extended_path(temp);
    for n in 0..MAX_CANDIDATES {
        let candidate = if n == 0 {
            target.to_path_buf()
        } else {
            // 首选名是本调用自己预留的；后续避让名若被别的截图预留，必须跳过。
            let candidate = dir.join(format!("{stem}_{}.{ext}", n + 1));
            if is_reserved_path(&candidate) {
                continue;
            }
            candidate
        };
        // 查重与 rename 都用长路径形式，但返回给用户的是原始候选路径。
        let io_candidate = crate::config::extended_path(&candidate);
        if io_candidate.exists() {
            continue;
        }
        match std::fs::rename(&temp, &io_candidate) {
            Ok(()) => return Ok(candidate),
            // rename 与 exists 之间被外部抢占：换下一个候选名重试。
            Err(_) if io_candidate.exists() => continue,
            Err(e) => return Err(e.into()),
        }
    }
    Err(Error::new("无法为截图分配文件名，请检查保存目录后重试"))
}

/// 启动时清理上次退出遗留的截图临时文件。调用方须在单实例 acquire 成功之后
/// 调用，此刻才保证没有本程序在途写盘，目录里带本程序前缀的文件都是崩溃/强杀
/// 残留，可直接删除（比 update.rs 的按龄清理更及时：用户很快重启也不该看到半截
/// 文件）。失败一律静默。
fn cleanup_stale_image_temps() {
    let Ok(dir) = crate::config::screenshot_dir() else {
        return;
    };
    // 启动清理同样要能穿透深目录；对每个最终路径（不只是父目录）调用
    // extended_path：父目录低于阈值时未加前缀，但拼上临时名可能已超 260，
    // is_file() / remove_file 对这种裸长路径都会返回 false。
    let dir = crate::config::extended_path(&dir);
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = crate::config::extended_path(&entry.path());
        let is_temp = path
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.starts_with(IMAGE_TMP_PREFIX));
        if is_temp && path.is_file() {
            let _ = std::fs::remove_file(&path);
        }
    }
}

/// 拖出临时目录前缀，与 dragexport::stage_in 的命名保持一致。
const DRAG_TMP_PREFIX: &str = "FastPin-drag-";
/// 拖出文件可能仍被接收方打开，只有满 24h 未修改的目录才判定为孤儿。
const DRAG_STALE_AGE: Duration = Duration::from_secs(24 * 60 * 60);

/// 按修改时间判断拖出临时目录是否已成孤儿。时钟回拨（modified 在未来）时
/// duration_since 返回 Err，宁可保留也不误删仍被使用的文件。
fn is_stale_drag_dir(modified: SystemTime, now: SystemTime) -> bool {
    now.duration_since(modified)
        .is_ok_and(|age| age >= DRAG_STALE_AGE)
}

/// 启动时回收上次崩溃遗留的拖出临时目录。必须在单实例 acquire 成功之后调用：
/// 第二实例早退前若也清理，会删掉第一实例正在给接收方读取的拖出文件。
/// 只处理临时目录下带 FastPin-drag- 前缀、且满 24h 未修改的目录；失败一律静默。
fn cleanup_stale_drag_files() {
    let Ok(entries) = std::fs::read_dir(std::env::temp_dir()) else {
        return;
    };
    let now = SystemTime::now();
    for entry in entries.flatten() {
        let path = entry.path();
        let is_drag = path
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.starts_with(DRAG_TMP_PREFIX));
        if !is_drag || !path.is_dir() {
            continue;
        }
        let modified = match entry.metadata().and_then(|m| m.modified()) {
            Ok(t) => t,
            Err(_) => continue,
        };
        if is_stale_drag_dir(modified, now) {
            let _ = std::fs::remove_dir_all(path);
        }
    }
}

/// Two shots inside the same second share a stem; a screenshot silently
/// overwriting the previous one is the worst way to lose one.
fn unique_path(dir: &Path, stem: &str, extension: &str) -> PathBuf {
    let first = dir.join(format!("{stem}.{extension}"));
    if !first.exists() {
        return first;
    }
    (2..)
        .map(|n| dir.join(format!("{stem}_{n}.{extension}")))
        .find(|p| !p.exists())
        .unwrap_or(first)
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unique_path_suffixes_only_on_collision() {
        // Scratch under target/: the harness sandbox denies writes to %TEMP%.
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join(format!("unique-path-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");

        let first = unique_path(&dir, "FastPin_20260828_143052", "png");
        assert_eq!(first.file_name().unwrap(), "FastPin_20260828_143052.png");

        std::fs::write(&first, b"").expect("write");
        let second = unique_path(&dir, "FastPin_20260828_143052", "png");
        assert_eq!(second.file_name().unwrap(), "FastPin_20260828_143052_2.png");

        std::fs::write(&second, b"").expect("write");
        let third = unique_path(&dir, "FastPin_20260828_143052", "png");
        assert_eq!(third.file_name().unwrap(), "FastPin_20260828_143052_3.png");

        // A different format does not collide with the PNGs above.
        let other = unique_path(&dir, "FastPin_20260828_143052", "jpg");
        assert_eq!(other.file_name().unwrap(), "FastPin_20260828_143052.jpg");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn write_image_atomically_publishes_and_cleans_temp() {
        // Scratch under target/: the harness sandbox denies writes to %TEMP%.
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join(format!("atomic-image-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        let target = dir.join("shot.png");
        let temp = dir.join(format!("{IMAGE_TMP_PREFIX}{}-0.png", std::process::id()));

        // 成功路径：目标内容正确，临时文件不残留。
        let published = write_image_atomically(&temp, &target, |p| {
            std::fs::write(p, b"png-bytes").map_err(|e| Error::new(e.to_string()))
        })
        .expect("原子写应成功");
        assert_eq!(published, target);
        assert_eq!(std::fs::read(&target).unwrap(), b"png-bytes".to_vec());
        assert!(!temp.exists(), "成功后不应残留临时文件");

        // 目标已存在：退到带序号后缀，不覆盖原目标，临时文件同样不残留。
        let temp2 = dir.join(format!("{IMAGE_TMP_PREFIX}{}-1.png", std::process::id()));
        let published2 = write_image_atomically(&temp2, &target, |p| {
            std::fs::write(p, b"second").map_err(|e| Error::new(e.to_string()))
        })
        .expect("目标被占用时应换名发布");
        assert_eq!(published2, dir.join("shot_2.png"));
        assert_eq!(
            std::fs::read(&target).unwrap(),
            b"png-bytes",
            "不得覆盖已有文件"
        );
        assert_eq!(std::fs::read(&published2).unwrap(), b"second".to_vec());
        assert!(!temp2.exists(), "冲突发布后不应残留临时文件");

        // 失败路径：临时文件被删除，目标不被创建。
        let temp3 = dir.join(format!("{IMAGE_TMP_PREFIX}{}-2.png", std::process::id()));
        let missing = dir.join("never-created.png");
        assert!(write_image_atomically(&temp3, &missing, |_p| {
            Err(Error::new("encode failed"))
        })
        .is_err());
        assert!(!temp3.exists(), "失败后应删除临时文件");
        assert!(!missing.exists(), "失败后不应留下目标文件");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn export_expands_and_sanitizes_the_filename_template() {
        let mut export = crate::config::Export::default();
        assert_eq!(
            export.expand_name("20260828", "143052"),
            "FastPin_20260828_143052"
        );

        export.filename_template = "shot-{datetime}".into();
        assert_eq!(
            export.expand_name("20260828", "143052"),
            "shot-20260828_143052"
        );

        // Characters Windows refuses in a name become underscores.
        export.filename_template = "a/b:c".into();
        assert_eq!(export.expand_name("20260828", "143052"), "a_b_c");

        // A template made only of illegal characters sanitizes to underscores:
        // non-empty, so it is used as-is rather than silently swapped.
        export.filename_template = "???".into();
        assert_eq!(export.expand_name("20260828", "143052"), "___");

        // A blank template is the default, not an empty stem.
        export.filename_template = "   ".into();
        assert_eq!(
            export.expand_name("20260828", "143052"),
            "FastPin_20260828_143052"
        );
    }

    #[test]
    fn stale_drag_dir_uses_24h_age_boundary() {
        // 固定时钟避免测试受真实时间影响；取足够大的纪元偏移，减法不会下溢。
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(2_000_000_000);
        let day = DRAG_STALE_AGE;

        assert!(
            !is_stale_drag_dir(now - day + Duration::from_secs(1), now),
            "差一秒未满 24h 时不能回收"
        );
        assert!(is_stale_drag_dir(now - day, now), "满 24h 即视为孤儿");
        assert!(
            is_stale_drag_dir(now - day - Duration::from_secs(1), now),
            "超过 24h 必须回收"
        );
        assert!(
            !is_stale_drag_dir(now + Duration::from_secs(1), now),
            "时钟回拨（modified 在未来）时宁可保留"
        );
    }
}
