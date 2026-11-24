//! Exclusive-fullscreen heuristic. Uncertain cases try WGC first; this only
//! decides when DXGI duplication should be preferred / forced after a black WGC frame.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ExclusiveHints {
    /// 当前显示器 `DXGI_OUTPUT_DESC::AttachedToDesktop == false`：这是**每显示器**
    /// 的强判据（该输出已脱离 DWM 桌面）。
    pub dxgi_output_detached: bool,
    /// 整机级 `QUNS_RUNNING_D3D_FULL_SCREEN` 通知。任意一块屏进入全屏都会置位，
    /// 因此只能作辅助，且必须与本屏的前台覆盖同时成立。
    pub d3d_fullscreen_notification: bool,
    /// 前台窗口是否覆盖**当前**显示器。
    pub foreground_covers_monitor: bool,
    /// **当前**显示器的 WGC 帧是否近黑。
    pub wgc_frame_near_black: bool,
}

/// 每显示器的 DXGI 判据：只有这块输出脱离 DWM 桌面才算独占。
/// 整机的 QUNS 标志不能在这里 OR 进来，否则别的屏上的全屏游戏会误伤本屏。
pub fn dxgi_output_is_exclusive(attached_to_desktop: bool) -> bool {
    !attached_to_desktop
}

/// 独占判定按**每显示器**进行：本屏 DXGI 输出脱离桌面是直接证据；整机 QUNS
/// 标志只作辅助，必须与「前台窗口覆盖本屏」同时成立；近黑 WGC 帧同样要求
/// 前台窗口覆盖本屏，才算典型的独占全屏漏检。
pub fn is_exclusive_fullscreen(h: ExclusiveHints) -> bool {
    if h.dxgi_output_detached {
        return true;
    }
    let shell_fullscreen = h.d3d_fullscreen_notification && h.foreground_covers_monitor;
    shell_fullscreen || (h.foreground_covers_monitor && h.wgc_frame_near_black)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hints(detached: bool, shell_fs: bool, covers: bool, black: bool) -> ExclusiveHints {
        ExclusiveHints {
            dxgi_output_detached: detached,
            d3d_fullscreen_notification: shell_fs,
            foreground_covers_monitor: covers,
            wgc_frame_near_black: black,
        }
    }

    #[test]
    fn dxgi_detached_output_wins() {
        assert!(is_exclusive_fullscreen(hints(true, false, false, false)));
    }

    #[test]
    fn covering_plus_black_wgc_is_exclusive() {
        assert!(is_exclusive_fullscreen(hints(false, false, true, true)));
    }

    #[test]
    fn covering_alone_is_not_enough() {
        assert!(!is_exclusive_fullscreen(hints(false, false, true, false)));
    }

    #[test]
    fn global_d3d_flag_needs_this_monitor_covered() {
        // 显示器 1 上的全屏游戏让整机 QUNS 置位，但显示器 2 的前台窗口没有覆盖
        // 本屏时，绝不能把显示器 2 判成独占（否则它的 WGC 被跳过 → 黑帧）。
        assert!(!is_exclusive_fullscreen(hints(false, true, false, false)));
        assert!(is_exclusive_fullscreen(hints(false, true, true, false)));
    }

    #[test]
    fn dxgi_detached_output_is_exclusive() {
        assert!(dxgi_output_is_exclusive(false));
        assert!(!dxgi_output_is_exclusive(true));
    }
}
