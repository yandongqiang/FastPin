mod d3d;
mod dxgi_dup;
mod monitors;
mod wgc;

pub use d3d::{drop_gpu_resources, texture_desc, Gpu, GpuShaders, RestoreGpu};
pub use monitors::{
    dxgi_allowed, enumerate_monitors, foreground_covers_monitor, monitor_from_foreground,
    query_d3d_fullscreen_notification, query_monitor_exclusive, virtual_desktop, MonitorInfo,
    VirtualDesktop,
};
pub use wgc::{start_live, WgcLive};

use windows::Win32::Graphics::Direct3D11::{ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_R16G16B16A16_FLOAT,
};

use crate::error::{Error, Result};
use crate::exclusive::{is_exclusive_fullscreen, ExclusiveHints};
use crate::hdr::REFERENCE_WHITE_NITS;
use crate::perf::PerfSession;

/// How long a single monitor may take to hand back a frame. This sits on the
/// hotkey-to-overlay path, so it is a user-visible stall, not a background wait.
pub(super) const FRAME_WAIT: std::time::Duration = std::time::Duration::from_millis(500);

pub struct CapturedDesktop {
    pub gpu: Gpu,
    /// Device generation this capture was taken on. A capture from an older
    /// generation must never be cropped/presented on the replacement device.
    pub generation: u64,
    /// GPU BGRA8 display texture used as overlay freeze background (no CPU blit).
    pub sdr_display: ID3D11Texture2D,
    pub width: u32,
    pub height: u32,
    pub origin_x: i32,
    pub origin_y: i32,
    pub sdr_white_nits: f32,
    pub hdr: bool,
    pub used_dxgi_dup: bool,
    pub exclusive: bool,
}

enum WgcOutcome {
    Ready(wgc::WgcReady),
    /// No usable frame: session never started, wait failed, or worker died.
    Missing,
}

/// WGC sessions started and frames waited off the GPU context borrow.
pub struct PreparedCapture {
    gpu: Gpu,
    monitors: Vec<MonitorInfo>,
    target_indices: Vec<usize>,
    capture_all: bool,
    /// 每个 target 一份独占预判（per-monitor），capture 阶段再用本屏的 WGC 近黑
    /// 信号补充。不能用一个全局 guess 决定所有屏是否跳过 WGC。
    exclusive_hints: Vec<ExclusiveHints>,
    any_hdr: bool,
    sdr_white: f32,
    overlay_luid: (u32, i32),
    wgc: Vec<WgcOutcome>,
}

/// Start WGC (CreateFreeThreaded) and wait for frames in parallel. Do not call while
/// holding `with_gpu_context` — only the device (via `Gpu`) is needed here.
pub fn prepare_capture(
    region: bool,
    region_hint: Option<(i32, i32, u32, u32)>,
    device: &ID3D11Device,
    shaders: &GpuShaders,
    overlay_luid: (u32, i32),
) -> Result<PreparedCapture> {
    // Cursor composition is a per-WGC-session switch. Read it once here so
    // every monitor captured by this shot agrees, even if settings are edited
    // while the parallel waits are in flight.
    let show_cursor = crate::config::get().screenshot.show_cursor;
    let gpu = Gpu::bind(device, shaders);
    let monitors = enumerate_monitors()?;
    if monitors.is_empty() {
        return Err(Error::new("no monitors"));
    }
    let fg_idx = monitor_from_foreground(&monitors);
    let focus_idx = fg_idx.unwrap_or(0);

    // QUNS 是整机标志：任意屏的全屏游戏都会置位，不能据此跳过所有屏的 WGC。
    // 这里只记录一次，是否算本屏独占由每 target 的 ExclusiveHints 决定。
    let d3d_fs = query_d3d_fullscreen_notification();

    // 独占现在逐 target 处理，不能因为一块屏独占就把整个虚拟桌面降级成单屏捕获。
    let capture_all = region;
    // A region hint that lands fully inside one monitor only needs that monitor:
    // long-capture polls this every 450 ms, and capturing the whole virtual
    // desktop each tick just to crop it back down is wasted WGC/composite work.
    let hint_idx = region_hint.and_then(|(x, y, w, h)| {
        monitors.iter().position(|m| {
            x >= m.x && y >= m.y && x + w as i32 <= m.x + m.w && y + h as i32 <= m.y + m.h
        })
    });
    let (focus_idx, capture_all) = match hint_idx {
        Some(idx) => (idx, false),
        None => (focus_idx, capture_all),
    };
    let target_indices: Vec<usize> = if capture_all {
        (0..monitors.len()).collect()
    } else {
        vec![focus_idx]
    };

    // 每个 target 独立探测：本屏 DXGI 输出脱离桌面是直接证据；整机 QUNS 只有
    // 在本屏被前台窗口覆盖时才参与判断。
    let exclusive_hints: Vec<ExclusiveHints> = target_indices
        .iter()
        .map(|&i| {
            let mon = &monitors[i];
            ExclusiveHints {
                dxgi_output_detached: query_monitor_exclusive(mon),
                d3d_fullscreen_notification: d3d_fs,
                foreground_covers_monitor: foreground_covers_monitor(mon),
                wgc_frame_near_black: false,
            }
        })
        .collect();

    let any_hdr = target_indices.iter().any(|&i| monitors[i].hdr);
    // 多 HDR 屏的 SDR 白点可能不同。每个 SDR piece 的 white-lift 会按自己那块屏
    // 的 sdr_white_nits 做；最终 tonemap 作用于整张合成图，只能取一个值，这里取
    // 所有目标 HDR 屏的最大值以保留亮部、避免整体偏暗（取舍说明见 capture_for_action）。
    let sdr_white = target_indices
        .iter()
        .map(|&i| &monitors[i])
        .filter(|m| m.hdr)
        .map(|m| m.sdr_white_nits)
        .fold(REFERENCE_WHITE_NITS, f32::max);

    let want_hdr = any_hdr;
    let mut wgc: Vec<WgcOutcome> = Vec::with_capacity(target_indices.len());
    let mut joins = Vec::new();

    for (slot, &idx) in target_indices.iter().enumerate() {
        if is_exclusive_fullscreen(exclusive_hints[slot])
            && dxgi_allowed(&monitors[idx], overlay_luid)
        {
            // 只跳过这一块屏的 WGC，改走 DXGI 独占捕获；其它屏照常 WGC。
            // DXGI 被 LUID 挡住时仍让 WGC 试一次，免得直接留下黑块。
            wgc.push(WgcOutcome::Missing);
            continue;
        }
        match wgc::start_monitor(&gpu, &monitors[idx], want_hdr, show_cursor) {
            Ok(pending) => {
                // Placeholder until the wait thread returns.
                wgc.push(WgcOutcome::Missing);
                joins.push(std::thread::spawn(move || (slot, wgc::wait_frame(pending))));
            }
            Err(_) => wgc.push(WgcOutcome::Missing),
        }
    }

    for join in joins {
        // Wait failure or a panicked worker leaves the slot Missing.
        if let Ok((slot, Ok(ready))) = join.join() {
            wgc[slot] = WgcOutcome::Ready(ready);
        }
    }

    Ok(PreparedCapture {
        gpu,
        monitors,
        target_indices,
        capture_all,
        exclusive_hints,
        any_hdr,
        sdr_white,
        overlay_luid,
        wgc,
    })
}

/// `copy_to_owned` / DXGI / composite — must run inside `with_gpu_context`.
pub fn capture_for_action(
    prepared: PreparedCapture,
    perf: &mut PerfSession,
    _device: &ID3D11Device,
    ctx: &ID3D11DeviceContext,
    _shaders: &GpuShaders,
) -> Result<CapturedDesktop> {
    let _restore = d3d::RestoreGpu(ctx);
    let PreparedCapture {
        gpu,
        monitors,
        target_indices,
        capture_all,
        mut exclusive_hints,
        any_hdr,
        sdr_white,
        overlay_luid,
        mut wgc,
    } = prepared;
    let vd = virtual_desktop();
    let want_hdr = any_hdr;

    let mut used_dxgi = false;
    // 每块 piece 带上自己显示器的 SDR 白点，合成时各自 white-lift。
    let mut pieces: Vec<(i32, i32, ID3D11Texture2D, bool, f32)> = Vec::new();
    let mut saw_exclusive = false;

    for (slot, &idx) in target_indices.iter().enumerate() {
        let mon = &monitors[idx];
        let hints = &mut exclusive_hints[slot];
        let mut got: Option<(ID3D11Texture2D, bool)> = None;

        if let WgcOutcome::Ready(ready) = std::mem::replace(&mut wgc[slot], WgcOutcome::Missing) {
            if let Ok(frame) = ready.into_owned(&gpu, ctx) {
                // A near-black frame means WGC missed an exclusive-fullscreen app
                // on **this** monitor; the global QUNS flag alone must not decide.
                if gpu.texture_is_near_black(ctx, &frame.texture) {
                    hints.wgc_frame_near_black = true;
                } else {
                    got = Some((frame.texture, frame.hdr));
                }
            }
        }

        // Every way of reaching `got == None` — create failure, wait failure, a
        // near-black frame, or a session skipped under exclusive fullscreen — wants
        // the same DuplicateOutput1 fallback, so there is nothing left to branch on.
        // Note: DXGI DuplicateOutput1 cannot composite the cursor into a captured
        // frame (unlike WGC's SetIsCursorCaptureEnabled), so `show_cursor` only
        // takes effect on the WGC path above; a DXGI-fallback frame is always
        // cursor-free. This is a platform limitation, not a missed setting.
        if got.is_none() {
            if dxgi_allowed(mon, overlay_luid) {
                match dxgi_dup::capture_monitor(&gpu, ctx, mon, want_hdr, overlay_luid) {
                    Ok((tex, hdr)) => {
                        used_dxgi = true;
                        got = Some((tex, hdr));
                    }
                    Err(e) => {
                        if target_indices.len() == 1 {
                            return Err(e);
                        }
                    }
                }
            } else if target_indices.len() == 1 {
                return Err(Error::new(
                    "capture unsupported: no WGC frame and DXGI blocked by adapter LUID mismatch",
                ));
            }
        }

        // 独占状态按本屏汇总：预判（含整机 QUNS + 本屏前台覆盖）或本屏 WGC 近黑。
        saw_exclusive |= is_exclusive_fullscreen(*hints);

        if let Some((tex, hdr)) = got {
            pieces.push((mon.x, mon.y, tex, hdr, mon.sdr_white_nits));
        }
    }
    perf.mark("T3");

    if pieces.is_empty() {
        return Err(Error::new("no capture frames"));
    }

    let (origin_x, origin_y, width, height) = if capture_all {
        (vd.x, vd.y, vd.w.max(1) as u32, vd.h.max(1) as u32)
    } else {
        let t0 = &monitors[target_indices[0]];
        (t0.x, t0.y, t0.w.max(1) as u32, t0.h.max(1) as u32)
    };

    let format = if any_hdr {
        DXGI_FORMAT_R16G16B16A16_FLOAT
    } else {
        DXGI_FORMAT_B8G8R8A8_UNORM
    };
    let composite = d3d::create_texture(
        &gpu.device,
        width,
        height,
        format,
        windows::Win32::Graphics::Direct3D11::D3D11_BIND_SHADER_RESOURCE
            | windows::Win32::Graphics::Direct3D11::D3D11_BIND_RENDER_TARGET,
        windows::Win32::Graphics::Direct3D11::D3D11_USAGE_DEFAULT,
        0,
    )?;

    for (mx, my, tex, piece_hdr, piece_white) in &pieces {
        let dst_x = (*mx - origin_x).max(0) as u32;
        let dst_y = (*my - origin_y).max(0) as u32;
        if any_hdr && !*piece_hdr {
            // 每块 SDR piece 用自己那块屏的白点做 white-lift。以前所有 piece 共用
            // 第一块 HDR 屏的 sdr_white_nits，多 HDR 屏白点不同时颜色会错。
            gpu.white_lift_into(ctx, tex, &composite, dst_x, dst_y, *piece_white)?;
        } else {
            unsafe {
                ctx.CopySubresourceRegion(&composite, 0, dst_x, dst_y, 0, tex, 0, None);
            }
        }
    }
    perf.mark("T4");

    // 最终 tonemap 作用于整张合成图，只能用一个白点；与 prepare_capture 中的
    // sdr_white 计算保持一致（目标 HDR 屏白点的最大值）。
    let sdr_display = gpu.to_sdr_bgra(ctx, &composite, sdr_white)?;
    perf.mark("T5");

    let generation = gpu.generation();
    Ok(CapturedDesktop {
        gpu,
        generation,
        sdr_display,
        width,
        height,
        origin_x,
        origin_y,
        sdr_white_nits: sdr_white,
        hdr: any_hdr,
        used_dxgi_dup: used_dxgi,
        exclusive: saw_exclusive,
    })
}

pub fn crop_sdr_bgra(
    cap: &CapturedDesktop,
    ctx: &ID3D11DeviceContext,
    x: u32,
    y: u32,
    w: u32,
    h: u32,
) -> Result<(Vec<u8>, u32, u32, u32)> {
    if !crate::gpu_lifecycle::is_usable(cap.generation) {
        return Err(Error::new(format!(
            "stale capture: generation {} is no longer usable",
            cap.generation
        )));
    }
    let _restore = d3d::RestoreGpu(ctx);
    cap.gpu.read_bgra_region(ctx, &cap.sdr_display, x, y, w, h)
}

/// Fresh crop from a live session: tonemap/copy the frame to BGRA8 then read
/// the region back. Mirrors `capture_for_action` + `crop_sdr_bgra`'s pipeline
/// but for a single already-owned frame texture instead of a full desktop
/// composite.
#[allow(clippy::too_many_arguments)]
pub fn crop_live_frame(
    gpu: &Gpu,
    ctx: &ID3D11DeviceContext,
    frame: &ID3D11Texture2D,
    sdr_white_nits: f32,
    x: u32,
    y: u32,
    w: u32,
    h: u32,
) -> Result<(Vec<u8>, u32, u32, u32)> {
    let _restore = d3d::RestoreGpu(ctx);
    let display = gpu.to_sdr_bgra(ctx, frame, sdr_white_nits)?;
    gpu.read_bgra_region(ctx, &display, x, y, w, h)
}
