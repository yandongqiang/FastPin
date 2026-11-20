//! Hover preview for a running scrolling capture.
//!
//! While the user hovers the scrolling thumbnail the capture pauses and the
//! region under the pointer is rendered from the stitched canvas at full
//! resolution, so a stitched page can be inspected mid-scroll instead of only
//! after 完成. This is the state machine behind that, ported from snow-shot's
//! `ScrollingHoverPreview`: it owns the temporary pause and one outstanding
//! render request, and it drops any result that arrives after the pointer, the
//! session epoch, or the canvas content changed.
//!
//! Three counters guard against stale work, and they answer different questions:
//! `epoch` ticks on every pause/resume transition, so a render requested before
//! a resume can never re-pause anything; `content_revision` ticks whenever the
//! canvas grew, so a rectangle that was valid before an append is re-requested
//! rather than shown with stale pixels; `request_serial` identifies the one
//! request whose result is still wanted, so a superseded render is discarded
//! instead of flashing over a newer one.
//!
//! This module is pure state: it holds no image, starts no thread, and does no
//! rendering. The caller performs the render and reports it back with the
//! serial this module handed out.

/// The canvas rows a preview was asked to render, plus the serial that
/// identifies the request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RegionRequest {
    /// First canvas row, inclusive.
    pub top: u32,
    /// Exclusive end canvas row.
    pub bottom: u32,
    /// Identifier to pass back to [`HoverPreview::accept`]; a newer request
    /// invalidates this one.
    pub serial: u64,
}

impl RegionRequest {
    /// Canvas rows in the request.
    pub fn rows(&self) -> u32 {
        self.bottom.saturating_sub(self.top)
    }
}

/// Hover-driven region preview over a running capture.
#[derive(Debug, Default)]
pub struct HoverPreview {
    enabled: bool,
    /// Rows the pointer currently covers, in canvas coordinates.
    desired: Option<(u32, u32)>,
    /// Rows whose render has already been delivered, so an unchanged hover does
    /// not re-render on every mouse move.
    completed: Option<(u32, u32)>,
    epoch: u64,
    content_revision: u64,
    request_serial: u64,
    paused: bool,
    /// The pause has been honoured by the caller (the in-flight frame finished),
    /// so a render may start.
    ready: bool,
    pending: bool,
    cropping: bool,
}

impl HoverPreview {
    /// A preview that starts disabled: the caller turns it on when the capture
    /// reaches the scrolling phase.
    pub fn new() -> Self {
        Self::default()
    }

    /// Turns the preview on or off. Disabling resumes the capture immediately
    /// and cancels any outstanding request.
    pub fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
        self.reconcile();
    }

    pub fn enabled(&self) -> bool {
        self.enabled
    }

    /// Reports whether the pointer is over the thumbnail instead of over a
    /// trim handle, which changes how the caller presents the region.
    pub fn set_cropping(&mut self, cropping: bool) {
        if self.cropping != cropping {
            self.completed = None;
        }
        self.cropping = cropping;
        self.reconcile();
    }

    pub fn cropping(&self) -> bool {
        self.cropping
    }

    /// Moves the hover rectangle. `None` clears it, which resumes the capture.
    pub fn set_hover(&mut self, rows: Option<(u32, u32)>) {
        let rows = rows.and_then(|(top, bottom)| (bottom > top).then_some((top, bottom)));
        self.desired = rows;
        self.reconcile();
    }

    pub fn hover(&self) -> Option<(u32, u32)> {
        self.desired
    }

    /// Reports that the canvas grew (or was replaced): anything already
    /// rendered no longer matches the canvas, so it is requested again.
    pub fn content_changed(&mut self) {
        self.content_revision = self.content_revision.wrapping_add(1);
        self.completed = None;
        self.reconcile();
    }

    /// Drops every pending request without touching the enabled flag, for use
    /// when the session ends. The capture is always resumed.
    pub fn reset(&mut self) {
        self.enabled = false;
        self.desired = None;
        self.cropping = false;
        self.completed = None;
        self.epoch = self.epoch.wrapping_add(1);
        self.request_serial = self.request_serial.wrapping_add(1);
        self.pending = false;
        self.ready = false;
        self.paused = false;
        self.content_revision = self.content_revision.wrapping_add(1);
    }

    /// Whether the caller must hold the capture. While this is true no frame is
    /// submitted, so the canvas cannot change under the region being rendered.
    pub fn paused(&self) -> bool {
        self.paused
    }

    /// Whether a render is waiting on the caller to finish its in-flight frame.
    /// The caller reports completion with [`Self::pause_ready`].
    pub fn want_pause(&self) -> bool {
        self.paused && !self.ready
    }

    /// Reports that the capture is now quiescent, so a render may start.
    pub fn pause_ready(&mut self) {
        if !self.paused || self.ready {
            return;
        }
        self.ready = true;
        self.reconcile();
    }

    /// The render the caller should perform now, if any.
    ///
    /// Returns `None` when the preview is off, the pointer is off the
    /// thumbnail, the pause has not been honoured yet, a render is already
    /// outstanding, or the current rectangle has already been delivered.
    pub fn next_request(&mut self) -> Option<RegionRequest> {
        if !self.paused || !self.ready || self.pending {
            return None;
        }
        let (top, bottom) = self.desired?;
        if self.completed == Some((top, bottom)) {
            return None;
        }
        self.request_serial = self.request_serial.wrapping_add(1);
        self.pending = true;
        Some(RegionRequest {
            top,
            bottom,
            serial: self.request_serial,
        })
    }

    /// Reports the outcome of a render.
    ///
    /// `pixels` is `None` when the render failed or produced nothing; the hover
    /// is then forgotten so the caller stops asking for it. A result whose
    /// serial, epoch, content revision, or rectangle no longer matches is
    /// dropped: it describes a state the user has already left.
    ///
    /// Returns `true` when the result should be presented.
    pub fn accept(
        &mut self,
        request: RegionRequest,
        epoch: u64,
        content_revision: u64,
        rendered: bool,
    ) -> bool {
        if request.serial != self.request_serial {
            return false;
        }
        self.pending = false;
        if !rendered {
            // The render produced nothing: treat the rectangle as done so the
            // caller does not retry it in a loop, and let the next hover (a new
            // rectangle) try again.
            self.completed = Some((request.top, request.bottom));
            self.reconcile();
            return false;
        }
        let still_wanted = self.paused
            && self.ready
            && epoch == self.epoch
            && content_revision == self.content_revision
            && self.desired == Some((request.top, request.bottom));
        if still_wanted {
            self.completed = Some((request.top, request.bottom));
            self.reconcile();
            return true;
        }
        self.reconcile();
        false
    }

    /// The epoch to hand back to [`Self::accept`] for a request.
    pub fn epoch(&self) -> u64 {
        self.epoch
    }

    /// The content revision to hand back to [`Self::accept`] for a request.
    pub fn content_revision(&self) -> u64 {
        self.content_revision
    }

    /// Decides whether the capture should be held, and queues a render when it
    /// should not.
    fn reconcile(&mut self) {
        let wanted = self.enabled && self.desired.is_some();
        if !wanted {
            if !self.paused {
                return;
            }
            self.epoch = self.epoch.wrapping_add(1);
            self.paused = false;
            self.ready = false;
            self.completed = None;
            self.pending = false;
            return;
        }
        if self.paused {
            return;
        }
        self.paused = true;
        self.ready = false;
        self.epoch = self.epoch.wrapping_add(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Drives the state machine through the pause handshake the overlay uses:
    /// the caller notices `want_pause`, finishes its frame, and calls
    /// `pause_ready`.
    fn settle(preview: &mut HoverPreview) {
        if preview.want_pause() {
            preview.pause_ready();
        }
    }

    #[test]
    fn a_disabled_preview_never_pauses() {
        let mut preview = HoverPreview::new();
        preview.set_hover(Some((0, 100)));
        assert!(!preview.paused());
        assert!(preview.next_request().is_none());
    }

    #[test]
    fn hovering_pauses_until_the_pointer_leaves() {
        let mut preview = HoverPreview::new();
        preview.set_enabled(true);
        preview.set_hover(Some((10, 60)));
        assert!(preview.paused());
        preview.set_hover(None);
        assert!(!preview.paused());
    }

    #[test]
    fn a_render_waits_for_the_pause_to_be_honoured() {
        let mut preview = HoverPreview::new();
        preview.set_enabled(true);
        preview.set_hover(Some((0, 40)));
        assert!(preview.want_pause());
        assert!(preview.next_request().is_none());
        preview.pause_ready();
        let request = preview.next_request().expect("a request after the pause");
        assert_eq!(request.top, 0);
        assert_eq!(request.bottom, 40);
        assert_eq!(request.rows(), 40);
    }

    #[test]
    fn one_request_is_outstanding_at_a_time() {
        let mut preview = HoverPreview::new();
        preview.set_enabled(true);
        preview.set_hover(Some((0, 40)));
        settle(&mut preview);
        let first = preview.next_request().unwrap();
        assert!(preview.next_request().is_none());
        let epoch = preview.epoch();
        let revision = preview.content_revision();
        assert!(preview.accept(first, epoch, revision, true));
        // The same rectangle is not requested twice.
        assert!(preview.next_request().is_none());
    }

    #[test]
    fn an_empty_rectangle_is_not_a_hover() {
        let mut preview = HoverPreview::new();
        preview.set_enabled(true);
        preview.set_hover(Some((40, 40)));
        assert!(!preview.paused());
        assert!(preview.next_request().is_none());
    }

    #[test]
    fn moving_the_pointer_requests_the_new_rectangle() {
        let mut preview = HoverPreview::new();
        preview.set_enabled(true);
        preview.set_hover(Some((0, 40)));
        settle(&mut preview);
        let first = preview.next_request().unwrap();
        let epoch = preview.epoch();
        let revision = preview.content_revision();
        assert!(preview.accept(first, epoch, revision, true));

        preview.set_hover(Some((40, 80)));
        let second = preview.next_request().expect("a request for the new rows");
        assert_eq!((second.top, second.bottom), (40, 80));
    }

    #[test]
    fn a_superseded_render_is_dropped() {
        let mut preview = HoverPreview::new();
        preview.set_enabled(true);
        preview.set_hover(Some((0, 40)));
        settle(&mut preview);
        let stale = preview.next_request().unwrap();
        let epoch = preview.epoch();
        let revision = preview.content_revision();
        // The pointer moves before the render lands.
        preview.set_hover(Some((100, 140)));
        assert!(!preview.accept(stale, epoch, revision, true));
        // The current rectangle is still renderable.
        assert!(preview.next_request().is_some());
    }

    #[test]
    fn a_result_from_an_earlier_epoch_is_dropped() {
        let mut preview = HoverPreview::new();
        preview.set_enabled(true);
        preview.set_hover(Some((0, 40)));
        settle(&mut preview);
        let request = preview.next_request().unwrap();
        let epoch = preview.epoch();
        let revision = preview.content_revision();
        // Leave and come back: same rows, but a new pause.
        preview.set_hover(None);
        preview.set_hover(Some((0, 40)));
        settle(&mut preview);
        assert!(!preview.accept(request, epoch, revision, true));
    }

    #[test]
    fn growth_requests_the_region_again() {
        let mut preview = HoverPreview::new();
        preview.set_enabled(true);
        preview.set_hover(Some((0, 40)));
        settle(&mut preview);
        let first = preview.next_request().unwrap();
        let epoch = preview.epoch();
        let revision = preview.content_revision();
        assert!(preview.accept(first, epoch, revision, true));

        preview.content_changed();
        let again = preview.next_request().expect("re-rendered after growth");
        assert_eq!((again.top, again.bottom), (0, 40));
    }

    #[test]
    fn a_failed_render_is_not_retried_until_the_hover_moves() {
        let mut preview = HoverPreview::new();
        preview.set_enabled(true);
        preview.set_hover(Some((0, 40)));
        settle(&mut preview);
        let request = preview.next_request().unwrap();
        let epoch = preview.epoch();
        let revision = preview.content_revision();
        assert!(!preview.accept(request, epoch, revision, false));
        assert!(preview.next_request().is_none());
        preview.set_hover(Some((0, 41)));
        assert!(preview.next_request().is_some());
    }

    #[test]
    fn reset_resumes_and_forgets() {
        let mut preview = HoverPreview::new();
        preview.set_enabled(true);
        preview.set_hover(Some((0, 40)));
        settle(&mut preview);
        let request = preview.next_request().unwrap();
        preview.reset();
        assert!(!preview.paused());
        assert!(!preview.enabled());
        assert!(preview.hover().is_none());
        assert!(!preview.accept(request, 0, 0, true));
    }

    #[test]
    fn cropping_is_tracked_separately_from_the_pause() {
        let mut preview = HoverPreview::new();
        preview.set_enabled(true);
        preview.set_cropping(true);
        // Cropping alone does not start a hover.
        assert!(!preview.paused());
        preview.set_hover(Some((0, 20)));
        assert!(preview.paused());
        assert!(preview.cropping());
    }
}
