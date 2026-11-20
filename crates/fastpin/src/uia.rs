//! UI Automation sub-element discovery for the region picker.
//!
//! `EnumChildWindows` finds the panes of classic Win32 apps, but modern apps
//! (WPF, Electron, Chromium, the Windows shell) paint their UI inside a single
//! HWND. UIA exposes those panes as elements, which is the "element" half of
//! the picker's `WindowSubElement` mode.
//!
//! We query by *window handle* rather than `ElementFromPoint`: the picker's own
//! overlay is always on top and hit-testable, so a point query would return our
//! window instead of the app underneath. Starting from the app HWND and walking
//! down only the branch whose rect contains the cursor avoids that, and is far
//! cheaper than asking UIA for the whole subtree.

use std::cell::RefCell;

use windows::Win32::Foundation::{HWND, POINT, RECT};
use windows::Win32::System::Com::{CoCreateInstance, CLSCTX_INPROC_SERVER};
use windows::Win32::UI::Accessibility::{
    CUIAutomation, IUIAutomation, IUIAutomationElement, IUIAutomationTreeWalker,
};

/// Bounds the tree walk so a pathological accessibility tree cannot stall the
/// picker. UIA calls cross COM, so each visit has a real cost.
const MAX_DEPTH: u32 = 8;
const MAX_VISITS: usize = 96;
/// Ignore hairline elements (separators, borders) — they make poor snap edges.
const MIN_EXTENT: i32 = 6;

thread_local! {
    // COM interfaces are apartment-bound, so a thread-local cache is the only
    // safe place to keep the automation object between cursor moves.
    static AUTOMATION: RefCell<Option<IUIAutomation>> = const { RefCell::new(None) };
}

fn automation() -> Option<IUIAutomation> {
    AUTOMATION.with(|slot| {
        let mut slot = slot.borrow_mut();
        if let Some(auto) = slot.as_ref() {
            return Some(auto.clone());
        }
        let auto = unsafe {
            CoCreateInstance::<_, IUIAutomation>(&CUIAutomation, None, CLSCTX_INPROC_SERVER).ok()?
        };
        *slot = Some(auto.clone());
        Some(auto)
    })
}

/// One UIA element rect and its nesting depth (0 = the app window, growing
/// toward the leaf under the cursor).
#[derive(Clone, Copy)]
pub struct ElementRect {
    pub rect: RECT,
    pub depth: u32,
}

/// Collects UIA element rects inside `window` whose bounds contain `pt`.
///
/// Returns an empty list on any COM failure (UIA is best-effort: the child-HWND
/// candidates and the whole window remain available). The caller is on the UI
/// thread, which `run_tray` initializes as a single-threaded COM apartment.
pub fn elements_at_point(window: HWND, pt: POINT) -> Vec<ElementRect> {
    let Some(auto) = automation() else {
        return Vec::new();
    };
    unsafe {
        let Ok(root) = auto.ElementFromHandle(window) else {
            return Vec::new();
        };
        // The control view leaves out the raw tree's non-visual nodes: fewer
        // COM calls and only boundaries a user would actually recognise.
        let Ok(walker) = auto.ControlViewWalker() else {
            return Vec::new();
        };
        let Some(rect) = visible_rect(&root, pt) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        let mut visits = 0usize;
        // The root is the window itself, already supplied by `detect_candidates`;
        // only publish its descendants as sub-element candidates.
        descend(&walker, &root, rect, pt, 0, &mut visits, &mut out);
        out
    }
}

/// Returns the element's rect when it is visible, large enough, and contains
/// the cursor. Every UIA property read is a COM call, so all the rejection
/// rules live in this one place.
fn visible_rect(element: &IUIAutomationElement, pt: POINT) -> Option<RECT> {
    unsafe {
        if element
            .CurrentIsOffscreen()
            .map(|off| off.as_bool())
            .unwrap_or(false)
        {
            return None;
        }
        let rect = element.CurrentBoundingRectangle().ok()?;
        if rect.right - rect.left < MIN_EXTENT || rect.bottom - rect.top < MIN_EXTENT {
            return None;
        }
        if pt.x < rect.left || pt.x >= rect.right || pt.y < rect.top || pt.y >= rect.bottom {
            return None;
        }
        Some(rect)
    }
}

/// Walks down the one child branch that still contains the cursor. Each
/// containing descendant is a candidate; siblings outside that branch cannot
/// contribute a boundary and are skipped after the first containing match.
fn descend(
    walker: &IUIAutomationTreeWalker,
    element: &IUIAutomationElement,
    rect: RECT,
    pt: POINT,
    depth: u32,
    visits: &mut usize,
    out: &mut Vec<ElementRect>,
) {
    // Depth 0 is the window, not a sub-element.
    if depth > 0 {
        out.push(ElementRect { rect, depth });
    }
    if depth >= MAX_DEPTH || *visits >= MAX_VISITS {
        return;
    }
    unsafe {
        let Ok(mut child) = walker.GetFirstChildElement(element) else {
            return;
        };
        loop {
            if *visits >= MAX_VISITS {
                return;
            }
            *visits += 1;
            if let Some(child_rect) = visible_rect(&child, pt) {
                descend(walker, &child, child_rect, pt, depth + 1, visits, out);
                // Only one sibling can own the point; the remaining ones are
                // irrelevant to this hover.
                return;
            }
            let Ok(next) = walker.GetNextSiblingElement(&child) else {
                return;
            };
            child = next;
        }
    }
}
