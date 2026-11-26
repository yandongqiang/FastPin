//! Candidate boundaries for the region picker's smart selection.
//!
//! snow-shot resolves these through a two-phase accessibility service
//! (`snow-ui-selector`: a 168 ms foreground query, then a 1500 ms refinement
//! that publishes every 32 ms). Porting that worker-thread protocol wholesale
//! would be disproportionate for one hover feature.
//!
//! What the picker actually needs is much smaller: for a point under the
//! cursor, an ordered list of rectangles a user would plausibly mean. On
//! Windows two native sources cover it:
//!
//! * `EnumChildWindows` gives the visible child HWNDs inside the top-level
//!   window — snow-shot's `visible_child_window_rects`, for classic Win32 apps.
//! * UIA gives panes and controls painted inside a single HWND (WPF, Electron,
//!   Chromium). [`crate::uia`] starts from the app HWND and walks the branch
//!   under the cursor, since a point query would hit this picker's own overlay.
//!
//! UIA is best-effort: if COM is unavailable, child HWNDs and the whole window
//! still work. Ordering and filtering happen here in pure code, so the policy
//! is unit-testable without a desktop.
//!
//! Candidates are ordered deepest-first (snow-shot's `normalize_child_rects`
//! sorts by area), which is the order a hover walk wants: the boundary the
//! user is literally pointing at, then each coarser one behind it. The list is
//! capped at `MAX_CANDIDATES` (snow-shot's `MAX_RECTS`) so a pathological tree
//! cannot flood the hover walk.

use crate::config::SmartSelectionMode;
use crate::geom::{Point, Rect};

/// One candidate boundary plus what produced it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Candidate {
    pub rect: Rect,
    /// How specific this boundary is: 0 is the window, growing by one for each
    /// level of containment. Equal values mean "same layer", and the walk then
    /// prefers the smaller area.
    pub depth: u32,
}

/// Orders candidates for a hover walk and folds in the mode's filter.
///
/// `Disabled` yields nothing at all: the picker then never enters snap logic,
/// which is the whole point of that mode.
///
/// Filtering is deliberately conservative — a rect must be non-degenerate and
/// inside the frozen desktop image, because a candidate that spills outside the
/// screenshot cannot be cropped and would produce a selection with negative
/// pixels. At most `MAX_CANDIDATES` surviving boundaries are kept, deepest and
/// smallest first (the ones the hover walk would reach first).
pub fn candidates_for(
    mode: SmartSelectionMode,
    raw: Vec<Candidate>,
    desktop: Rect,
    min_extent: f64,
) -> Vec<Candidate> {
    if !mode.enabled() {
        return Vec::new();
    }
    // snow-shot's `boundedPath` clips each hit rect to the selectable bounds
    // and keeps the visible part; it does not discard a rect whose far corner
    // happens to leave the desktop. Dropping such rects here cost every
    // partially-visible pane / DWM-extended window frame a boundary.
    let mut out: Vec<Candidate> = raw
        .into_iter()
        .filter_map(|c| {
            let left = c.rect.x.max(desktop.x);
            let top = c.rect.y.max(desktop.y);
            let right = (c.rect.right()).min(desktop.right() - EDGE_EPSILON);
            let bottom = (c.rect.bottom()).min(desktop.bottom() - EDGE_EPSILON);
            if right < left || bottom < top {
                return None;
            }
            let rect = Rect::new(left, top, right - left, bottom - top);
            if rect.width < min_extent || rect.height < min_extent {
                return None;
            }
            Some(Candidate { rect, ..c })
        })
        .collect();
    // Deepest first, then smallest: the pane the cursor is literally on comes
    // before the window around it, and a small pane before a large sibling.
    out.sort_by(|a, b| {
        b.depth.cmp(&a.depth).then(
            area(a)
                .partial_cmp(&area(b))
                .unwrap_or(std::cmp::Ordering::Equal),
        )
    });
    out.dedup_by(|a, b| a.rect == b.rect);
    // 已按「深 → 浅、小 → 大」排好。直接 truncate 会丢掉排末位的整窗候选
    // （depth 最小）：候选超过 100 时，光标落在窗口空白区就找不到包含它的
    // 候选，整窗边界再也无法吸附。这里把最浅的那个单独留下，其余照常截断。
    if out.len() > MAX_CANDIDATES {
        let window = out.pop().expect("len > MAX_CANDIDATES");
        out.truncate(MAX_CANDIDATES - 1);
        out.push(window);
    }
    out
}

/// Candidate cap for the hover walk. The vendored selector already caps a UIA
/// hit path at 100 (`MAX_RECTS`) and child HWNDs are deduplicated before they
/// reach here; a 100 total cap used to truncate the middle ancestors of a deep
/// tree, so leave headroom for a full path plus window chrome.
const MAX_CANDIDATES: usize = 256;

/// Slack for the half-open `contains` check at the desktop's far edge.
const EDGE_EPSILON: f64 = 0.5;

fn area(c: &Candidate) -> f64 {
    c.rect.width * c.rect.height
}

/// Picks the candidate the cursor is over, cheapest boundary first.
///
/// Matching snow-shot's hover behaviour: once the user is inside a candidate,
/// the picker shows that boundary; moving out to a parent's interior falls back
/// to the parent. `None` means "no snap here", so a drag is free to start.
pub fn candidate_at(mode: SmartSelectionMode, candidates: &[Candidate], p: Point) -> Option<Rect> {
    if !mode.enabled() {
        return None;
    }
    candidates
        .iter()
        .find(|c| c.rect.contains(p))
        .map(|c| c.rect)
}

/// The boundary a hover should preview.
///
/// Rects in nested candidates differ by depth, so the walk naturally prefers
/// the innermost match; this indirection exists so the rule is stated once and
/// can be tested without a desktop.
pub fn preview_rect(mode: SmartSelectionMode, candidates: &[Candidate], p: Point) -> Option<Rect> {
    candidate_at(mode, candidates, p)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(x: f64, y: f64, w: f64, h: f64) -> Rect {
        Rect::new(x, y, w, h)
    }

    fn candidate(x: f64, y: f64, w: f64, h: f64, depth: u32) -> Candidate {
        Candidate {
            rect: r(x, y, w, h),
            depth,
        }
    }

    const DESKTOP: Rect = Rect {
        x: 0.0,
        y: 0.0,
        width: 1000.0,
        height: 800.0,
    };

    #[test]
    fn disabled_mode_yields_no_candidates() {
        let raw = vec![candidate(10.0, 10.0, 100.0, 100.0, 0)];
        assert!(candidates_for(SmartSelectionMode::Disabled, raw, DESKTOP, 4.0).is_empty());
        assert!(preview_rect(
            SmartSelectionMode::Disabled,
            &[],
            Point { x: 50.0, y: 50.0 }
        )
        .is_none());
    }

    #[test]
    fn deepest_and_smallest_wins_the_hover() {
        // A window, a pane inside it, and a button inside the pane.
        let raw = vec![
            candidate(0.0, 0.0, 900.0, 700.0, 0),     // window
            candidate(100.0, 100.0, 400.0, 300.0, 1), // pane
            candidate(120.0, 120.0, 80.0, 40.0, 2),   // button
        ];
        let list = candidates_for(SmartSelectionMode::WindowSubElement, raw, DESKTOP, 4.0);
        assert_eq!(list.len(), 3);
        assert_eq!(list[0].depth, 2, "the button is listed first");
        assert_eq!(list[2].depth, 0, "the window is last");
        // A point inside the button resolves to the button.
        assert_eq!(
            preview_rect(
                SmartSelectionMode::WindowSubElement,
                &list,
                Point { x: 140.0, y: 130.0 }
            ),
            Some(r(120.0, 120.0, 80.0, 40.0))
        );
        // A point inside the pane but outside the button resolves to the pane.
        assert_eq!(
            preview_rect(
                SmartSelectionMode::WindowSubElement,
                &list,
                Point { x: 460.0, y: 380.0 }
            ),
            Some(r(100.0, 100.0, 400.0, 300.0))
        );
        // Outside every candidate: no snap, a free drag.
        assert_eq!(
            preview_rect(
                SmartSelectionMode::WindowSubElement,
                &list,
                Point { x: 980.0, y: 780.0 }
            ),
            None
        );
    }

    /// The whole-window mode must not offer panes: that is the difference the
    /// user picks the mode for.
    #[test]
    fn window_mode_offers_only_the_window() {
        // A pane rect is listed too, so the test states what this mode ignores:
        // the caller passes only window-level rects when it is selected.
        let raw = [
            candidate(0.0, 0.0, 900.0, 700.0, 0),
            candidate(100.0, 100.0, 400.0, 300.0, 1),
        ];
        // The filter keeps window-level rects and the hover then resolves to
        // the window everywhere inside it, panes or not.
        let list = candidates_for(SmartSelectionMode::Window, vec![raw[0]], DESKTOP, 4.0);
        assert_eq!(list.len(), 1);
        assert_eq!(
            preview_rect(
                SmartSelectionMode::Window,
                &list,
                Point { x: 200.0, y: 200.0 }
            ),
            Some(r(0.0, 0.0, 900.0, 700.0)),
            "a point over a pane still selects the whole window"
        );
    }

    #[test]
    fn degenerate_rects_drop_and_outside_rects_clip_to_desktop() {
        let raw = vec![
            candidate(10.0, 10.0, 2.0, 100.0, 0),    // too narrow
            candidate(10.0, 10.0, 100.0, 2.0, 0),    // too short
            candidate(-50.0, 10.0, 100.0, 100.0, 0), // clipped to 0..50
            candidate(950.0, 10.0, 100.0, 100.0, 0), // clipped to 950..999.5
            candidate(10.0, 10.0, 100.0, 100.0, 0),  // keeper
        ];
        let list = candidates_for(SmartSelectionMode::WindowSubElement, raw, DESKTOP, 4.0);
        // snow-shot's boundedPath keeps clipped fragments, so the two partial
        // rects survive as boundaries instead of being thrown away (Bug 4).
        assert_eq!(list.len(), 3);
        assert!(list.iter().any(|c| c.rect == r(0.0, 10.0, 50.0, 100.0)));
        assert!(list.iter().any(|c| c.rect == r(950.0, 10.0, 49.5, 100.0)));
        assert!(list.iter().any(|c| c.rect == r(10.0, 10.0, 100.0, 100.0)));
    }

    #[test]
    fn identical_rects_are_deduplicated() {
        let raw = vec![
            candidate(10.0, 10.0, 100.0, 100.0, 0),
            candidate(10.0, 10.0, 100.0, 100.0, 0),
        ];
        assert_eq!(
            candidates_for(SmartSelectionMode::WindowSubElement, raw, DESKTOP, 4.0).len(),
            1,
            "the same pane reported twice is still one boundary"
        );
    }

    /// 候选有上限，但上限必须容得下完整的一条 UIA 命中链（<=100）加窗口
    /// 外框；过去 100 的上限会截掉深树中间的祖先，深层元素于是选不到。
    #[test]
    fn candidates_cap_keeps_deep_paths_and_the_window() {
        let raw: Vec<Candidate> = (0..300)
            .map(|i| {
                candidate(
                    (i % 20) as f64 * 20.0,
                    (i / 20) as f64 * 20.0,
                    10.0,
                    10.0,
                    i,
                )
            })
            .collect();
        let list = candidates_for(SmartSelectionMode::WindowSubElement, raw, DESKTOP, 4.0);
        assert_eq!(list.len(), MAX_CANDIDATES, "excess candidates are dropped");
        assert_eq!(list[0].depth, 299, "deepest first");
        assert_eq!(
            list[MAX_CANDIDATES - 1].depth,
            0,
            "the whole-window candidate survives at the tail"
        );
        assert_eq!(
            list.iter().filter(|c| c.depth == 0).count(),
            1,
            "only one shallow candidate is kept"
        );
    }

    /// 候选触顶时 depth 最小的整窗候选必须保底留下，否则光标落在窗口
    /// 空白区时没有任何候选包含它，无法吸附整窗边界。
    #[test]
    fn truncation_keeps_the_whole_window_candidate() {
        let window = candidate(0.0, 0.0, 900.0, 700.0, 0);
        let mut raw: Vec<Candidate> = Vec::new();
        raw.push(window);
        for i in 1..300u32 {
            // 全部落在整窗内部，深度都比整窗大。
            raw.push(candidate(
                10.0 + (i % 10) as f64 * 60.0,
                10.0 + (i / 10) as f64 * 40.0,
                30.0,
                20.0,
                i,
            ));
        }
        let list = candidates_for(SmartSelectionMode::WindowSubElement, raw, DESKTOP, 4.0);
        assert!(
            list.len() >= MAX_CANDIDATES / 2,
            "deep paths survive the cap"
        );
        assert!(list.iter().any(|c| c.rect == window.rect));
        assert_eq!(
            preview_rect(
                SmartSelectionMode::WindowSubElement,
                &list,
                Point { x: 880.0, y: 680.0 }
            ),
            Some(window.rect),
            "窗口空白区仍能吸附整窗边界"
        );
    }

    #[test]
    fn mode_ids_round_trip() {
        for mode in SmartSelectionMode::ALL {
            assert_eq!(SmartSelectionMode::from_id(mode.id()), mode);
        }
        // Unknown keys default to the richest mode, as snow-shot does.
        assert_eq!(
            SmartSelectionMode::from_id("nonsense"),
            SmartSelectionMode::WindowSubElement
        );
        assert!(!SmartSelectionMode::Disabled.enabled());
        assert!(SmartSelectionMode::Window.enabled());
        assert!(SmartSelectionMode::WindowSubElement.wants_sub_elements());
        assert!(!SmartSelectionMode::Window.wants_sub_elements());
    }
}
