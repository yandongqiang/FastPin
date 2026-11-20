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
//! user is literally pointing at, then each coarser one behind it.

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
/// pixels.
pub fn candidates_for(
    mode: SmartSelectionMode,
    raw: Vec<Candidate>,
    desktop: Rect,
    min_extent: f64,
) -> Vec<Candidate> {
    if !mode.enabled() {
        return Vec::new();
    }
    let mut out: Vec<Candidate> = raw
        .into_iter()
        .filter(|c| c.rect.width >= min_extent && c.rect.height >= min_extent)
        .filter(|c| {
            desktop.contains(Point {
                x: c.rect.x,
                y: c.rect.y,
            }) && desktop.contains(Point {
                // `contains` is half-open, so the far corner is tested one
                // unit inside to allow a rect that ends exactly at the edge.
                x: (c.rect.right() - EDGE_EPSILON).max(c.rect.x),
                y: (c.rect.bottom() - EDGE_EPSILON).max(c.rect.y),
            })
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
    out
}

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
    fn degenerate_and_outside_rects_are_dropped() {
        let raw = vec![
            candidate(10.0, 10.0, 2.0, 100.0, 0),    // too narrow
            candidate(10.0, 10.0, 100.0, 2.0, 0),    // too short
            candidate(-50.0, 10.0, 100.0, 100.0, 0), // starts outside
            candidate(950.0, 10.0, 100.0, 100.0, 0), // ends outside
            candidate(10.0, 10.0, 100.0, 100.0, 0),  // the only keeper
        ];
        let list = candidates_for(SmartSelectionMode::WindowSubElement, raw, DESKTOP, 4.0);
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].rect, r(10.0, 10.0, 100.0, 100.0));
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
