use windows::core::{Interface, BOOL};
use windows::Win32::Devices::Display::{
    DisplayConfigGetDeviceInfo, GetDisplayConfigBufferSizes, QueryDisplayConfig,
    DISPLAYCONFIG_DEVICE_INFO_GET_SDR_WHITE_LEVEL, DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME,
    DISPLAYCONFIG_DEVICE_INFO_HEADER, DISPLAYCONFIG_MODE_INFO, DISPLAYCONFIG_PATH_INFO,
    DISPLAYCONFIG_SDR_WHITE_LEVEL, DISPLAYCONFIG_SOURCE_DEVICE_NAME, QDC_ONLY_ACTIVE_PATHS,
};
use windows::Win32::Foundation::{LPARAM, RECT};
use windows::Win32::Graphics::Dxgi::{IDXGIAdapter1, IDXGIOutput, IDXGIOutput6, DXGI_OUTPUT_DESC1};
use windows::Win32::Graphics::Gdi::{
    EnumDisplayMonitors, GetMonitorInfoW, HDC, HMONITOR, MONITORINFO, MONITORINFOEXW,
};
use windows::Win32::UI::WindowsAndMessaging::{GetForegroundWindow, GetWindowRect};

use windows::Win32::UI::Shell::{SHQueryUserNotificationState, QUNS_RUNNING_D3D_FULL_SCREEN};

use crate::error::Result;
use crate::exclusive::dxgi_output_is_exclusive;
use crate::hdr::{is_hdr_color_space, sdr_white_nits_from_displayconfig, REFERENCE_WHITE_NITS};

use super::d3d::dxgi_factory;

#[derive(Clone)]
pub struct MonitorInfo {
    pub handle: HMONITOR,
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
    pub hdr: bool,
    pub sdr_white_nits: f32,
    pub output: Option<IDXGIOutput>,
    pub adapter_luid: Option<(u32, i32)>,
}

pub fn dxgi_allowed(mon: &MonitorInfo, overlay_luid: (u32, i32)) -> bool {
    mon.adapter_luid == Some(overlay_luid)
}

pub struct VirtualDesktop {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

/// 虚拟桌面尺寸必须从 `enumerate_monitors` 的物理像素 rect 推导。`GetSystemMetrics`
/// 的 `SM_XVIRTUALSCREEN` 系列是非 DPI 感知的：登录时 100%、之后改缩放时，返回值
/// 与每屏 PMv2 rect 不在同一坐标系，混用会让合成图整体偏移 / 出现黑边。
pub fn virtual_desktop(monitors: &[MonitorInfo]) -> VirtualDesktop {
    let rects: Vec<(i32, i32, i32, i32)> = monitors.iter().map(|m| (m.x, m.y, m.w, m.h)).collect();
    let (x, y, w, h) = union_bounds(&rects).unwrap_or((0, 0, 1, 1));
    VirtualDesktop { x, y, w, h }
}

/// `(x, y, w, h)` 列表的并集，返回 `(min_x, min_y, width, height)`；空列表为 `None`。
fn union_bounds(monitors: &[(i32, i32, i32, i32)]) -> Option<(i32, i32, i32, i32)> {
    let mut iter = monitors.iter().copied();
    let (x, y, w, h) = iter.next()?;
    let (mut min_x, mut min_y) = (x, y);
    let (mut max_x, mut max_y) = (x + w, y + h);
    for (x, y, w, h) in iter {
        min_x = min_x.min(x);
        min_y = min_y.min(y);
        max_x = max_x.max(x + w);
        max_y = max_y.max(y + h);
    }
    Some((min_x, min_y, max_x - min_x, max_y - min_y))
}

pub fn enumerate_monitors() -> Result<Vec<MonitorInfo>> {
    let mut raw: Vec<(HMONITOR, RECT)> = Vec::new();
    unsafe {
        let _ = EnumDisplayMonitors(
            None,
            None,
            Some(enum_proc),
            LPARAM(&mut raw as *mut _ as isize),
        );
    }
    struct EnumeratedOutput {
        output: IDXGIOutput,
        /// `DXGI_OUTPUT_DESC::DeviceName`，与 GDI 的 `MONITORINFOEXW::szDevice`
        /// 是同一个 `\\.\DISPLAYn` 命名空间，是跨 API 的稳定配对键。
        name: String,
        /// `GetDesc().Monitor`：设备名取不到时的回退键。
        monitor: HMONITOR,
        hdr: bool,
        adapter_luid: Option<(u32, i32)>,
    }

    let factory = dxgi_factory()?;
    let mut adapter_i = 0;
    let mut outputs: Vec<EnumeratedOutput> = Vec::new();
    loop {
        let adapter: IDXGIAdapter1 = match unsafe { factory.EnumAdapters1(adapter_i) } {
            Ok(a) => a,
            Err(_) => break,
        };
        adapter_i += 1;
        let adapter_luid = unsafe { adapter.GetDesc() }
            .ok()
            .map(|d| (d.AdapterLuid.LowPart, d.AdapterLuid.HighPart));
        let mut out_i = 0;
        loop {
            let output = match unsafe { adapter.EnumOutputs(out_i) } {
                Ok(o) => o,
                Err(_) => break,
            };
            out_i += 1;
            let desc = unsafe { output.GetDesc()? };
            let hdr = output_is_hdr(&output).unwrap_or(false);
            outputs.push(EnumeratedOutput {
                output,
                name: wide_to_string(&desc.DeviceName),
                monitor: desc.Monitor,
                hdr,
                adapter_luid,
            });
        }
    }

    // One QueryDisplayConfig for the whole enumeration, keyed by GDI device name.
    let white_levels = sdr_white_by_device();

    let mut monitors = Vec::new();
    for (handle, rect) in raw {
        let w = rect.right - rect.left;
        let h = rect.bottom - rect.top;
        let mut hdr = false;
        let mut output = None;
        let mut adapter_luid = None;
        let name = device_name(handle);
        for o in &outputs {
            // GDI 的 MONITORINFOEXW.szDevice 与 DXGI 的 DeviceName 都是
            // `\\.\DISPLAYn`，是稳定键。克隆/复制模式下两块输出共享原点，
            // DPI 取整也可能让 DesktopCoordinates 与 GDI rect 差几个像素，
            // 因此不再按原点精确相等配对；Monitor 句柄作为回退。
            if name.as_deref() == Some(o.name.as_str())
                || (!o.monitor.0.is_null() && o.monitor == handle)
            {
                hdr = o.hdr;
                output = Some(o.output.clone());
                adapter_luid = o.adapter_luid;
                break;
            }
        }
        let sdr_white_nits = name
            .as_deref()
            .and_then(|name| {
                white_levels
                    .iter()
                    .find(|(dev, _)| dev.as_str() == name)
                    .map(|(_, nits)| *nits)
            })
            .unwrap_or(REFERENCE_WHITE_NITS);
        monitors.push(MonitorInfo {
            handle,
            x: rect.left,
            y: rect.top,
            w,
            h,
            hdr,
            sdr_white_nits,
            output,
            adapter_luid,
        });
    }
    Ok(monitors)
}

fn output_is_hdr(output: &IDXGIOutput) -> Result<bool> {
    let output6: IDXGIOutput6 = output.cast()?;
    let desc: DXGI_OUTPUT_DESC1 = unsafe { output6.GetDesc1()? };
    Ok(is_hdr_color_space(desc.ColorSpace.0))
}

/// GDI device name (`\\.\DISPLAY1`) for an HMONITOR, used to pair it with a display path.
fn device_name(handle: HMONITOR) -> Option<String> {
    let mut info = MONITORINFOEXW {
        monitorInfo: MONITORINFO {
            cbSize: std::mem::size_of::<MONITORINFOEXW>() as u32,
            ..Default::default()
        },
        ..Default::default()
    };
    let ok = unsafe { GetMonitorInfoW(handle, &mut info as *mut _ as *mut MONITORINFO) };
    if !ok.as_bool() {
        return None;
    }
    Some(wide_to_string(&info.szDevice))
}

fn wide_to_string(buf: &[u16]) -> String {
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..end])
}

/// SDR white level per display path, keyed by GDI device name. A single level for
/// the whole desktop is wrong once one monitor is HDR and another is not.
fn sdr_white_by_device() -> Vec<(String, f32)> {
    let mut out = Vec::new();
    unsafe {
        let mut path_count = 0u32;
        let mut mode_count = 0u32;
        if GetDisplayConfigBufferSizes(QDC_ONLY_ACTIVE_PATHS, &mut path_count, &mut mode_count).0
            != 0
        {
            return out;
        }
        let mut paths = vec![DISPLAYCONFIG_PATH_INFO::default(); path_count as usize];
        let mut modes = vec![DISPLAYCONFIG_MODE_INFO::default(); mode_count as usize];
        if QueryDisplayConfig(
            QDC_ONLY_ACTIVE_PATHS,
            &mut path_count,
            paths.as_mut_ptr(),
            &mut mode_count,
            modes.as_mut_ptr(),
            None,
        )
        .0 != 0
        {
            return out;
        }
        for path in &paths[..path_count as usize] {
            let mut src = DISPLAYCONFIG_SOURCE_DEVICE_NAME {
                header: DISPLAYCONFIG_DEVICE_INFO_HEADER {
                    r#type: DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME,
                    size: std::mem::size_of::<DISPLAYCONFIG_SOURCE_DEVICE_NAME>() as u32,
                    adapterId: path.sourceInfo.adapterId,
                    id: path.sourceInfo.id,
                },
                ..Default::default()
            };
            if DisplayConfigGetDeviceInfo(&mut src.header) != 0 {
                continue;
            }
            let mut sdr = DISPLAYCONFIG_SDR_WHITE_LEVEL {
                header: DISPLAYCONFIG_DEVICE_INFO_HEADER {
                    r#type: DISPLAYCONFIG_DEVICE_INFO_GET_SDR_WHITE_LEVEL,
                    size: std::mem::size_of::<DISPLAYCONFIG_SDR_WHITE_LEVEL>() as u32,
                    adapterId: path.targetInfo.adapterId,
                    id: path.targetInfo.id,
                },
                SDRWhiteLevel: 1000,
            };
            let nits = if DisplayConfigGetDeviceInfo(&mut sdr.header) == 0 && sdr.SDRWhiteLevel > 0
            {
                sdr_white_nits_from_displayconfig(sdr.SDRWhiteLevel)
            } else {
                REFERENCE_WHITE_NITS
            };
            out.push((wide_to_string(&src.viewGdiDeviceName), nits));
        }
    }
    out
}

unsafe extern "system" fn enum_proc(
    monitor: HMONITOR,
    _hdc: HDC,
    lprect: *mut RECT,
    lparam: LPARAM,
) -> BOOL {
    unsafe {
        let list = &mut *(lparam.0 as *mut Vec<(HMONITOR, RECT)>);
        if !lprect.is_null() {
            list.push((monitor, *lprect));
        }
        BOOL(1)
    }
}

pub fn foreground_covers_monitor(mon: &MonitorInfo) -> bool {
    unsafe {
        let hwnd = GetForegroundWindow();
        if hwnd.0.is_null() {
            return false;
        }
        let mut rc = RECT::default();
        if GetWindowRect(hwnd, &mut rc).is_err() {
            return false;
        }
        let w = rc.right - rc.left;
        let h = rc.bottom - rc.top;
        (w - mon.w).abs() <= 4
            && (h - mon.h).abs() <= 4
            && (rc.left - mon.x).abs() <= 4
            && (rc.top - mon.y).abs() <= 4
    }
}

/// 整机级 D3D 全屏通知（`QUNS_RUNNING_D3D_FULL_SCREEN`）。任意显示器进入
/// 全屏独占都会置位，所以只能作辅助信号：是否作用于某块屏由
/// `crate::exclusive::is_exclusive_fullscreen` 结合该屏的前台覆盖情况决定。
pub fn query_d3d_fullscreen_notification() -> bool {
    unsafe {
        SHQueryUserNotificationState()
            .ok()
            .map(|s| s == QUNS_RUNNING_D3D_FULL_SCREEN)
            .unwrap_or(false)
    }
}

/// 每显示器独占判据：只有本输出的 `AttachedToDesktop == false`（脱离 DWM 桌面）
/// 才算独占。以前把整机 QUNS 标志直接 OR 进来，导致显示器 1 运行全屏游戏时
/// 显示器 2 也被误判为独占、WGC 全部跳过并采到黑帧。
pub fn query_monitor_exclusive(mon: &MonitorInfo) -> bool {
    let attached = mon
        .output
        .as_ref()
        .and_then(|o| unsafe { o.GetDesc().ok() })
        .map(|d| d.AttachedToDesktop.as_bool())
        .unwrap_or(true);
    dxgi_output_is_exclusive(attached)
}

pub fn monitor_from_foreground(monitors: &[MonitorInfo]) -> Option<usize> {
    unsafe {
        let hwnd = GetForegroundWindow();
        if hwnd.0.is_null() {
            return None;
        }
        let mut rc = RECT::default();
        GetWindowRect(hwnd, &mut rc).ok()?;
        let cx = (rc.left + rc.right) / 2;
        let cy = (rc.top + rc.bottom) / 2;
        monitors
            .iter()
            .position(|m| cx >= m.x && cy >= m.y && cx < m.x + m.w && cy < m.y + m.h)
    }
}

#[cfg(test)]
pub fn dummy_monitor(adapter_luid: Option<(u32, i32)>) -> MonitorInfo {
    MonitorInfo {
        handle: HMONITOR(std::ptr::null_mut()),
        x: 0,
        y: 0,
        w: 1920,
        h: 1080,
        hdr: false,
        sdr_white_nits: REFERENCE_WHITE_NITS,
        output: None,
        adapter_luid,
    }
}

#[cfg(test)]
mod luid_tests {
    use super::*;

    #[test]
    fn dxgi_allowed_only_when_luid_matches() {
        let overlay = (1, 0);
        let same = dummy_monitor(Some((1, 0)));
        let other = dummy_monitor(Some((2, 0)));
        let unknown = dummy_monitor(None);
        assert!(dxgi_allowed(&same, overlay));
        assert!(!dxgi_allowed(&other, overlay));
        assert!(!dxgi_allowed(&unknown, overlay));
    }
}

#[cfg(test)]
mod virtual_desktop_tests {
    use super::*;

    #[test]
    fn secondary_monitor_on_the_left_extends_origin_left() {
        let bounds = union_bounds(&[(-1920, 0, 1920, 1080), (0, 0, 2560, 1440)]);
        assert_eq!(bounds, Some((-1920, 0, 4480, 1440)));
    }

    #[test]
    fn single_monitor_is_returned_unchanged() {
        let bounds = union_bounds(&[(100, 200, 1920, 1080)]);
        assert_eq!(bounds, Some((100, 200, 1920, 1080)));
    }

    #[test]
    fn empty_list_yields_none_and_virtual_desktop_falls_back() {
        assert_eq!(union_bounds(&[]), None);
        let vd = virtual_desktop(&[]);
        assert_eq!((vd.x, vd.y, vd.w, vd.h), (0, 0, 1, 1));
    }
}
