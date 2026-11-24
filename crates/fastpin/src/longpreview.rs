//! Post-capture preview for a stitched long screenshot.
//!
//! snow-shot reviews a finished stitch in an `ImageStitchingWindow`
//! (`snow_shot/src/presentation/collage/collagewindow.cpp`): the stitch
//! completes, the window opens over the result, and its actions — Cancel /
//! Copy / Save, wired at `collagewindow.cpp:282-285` — are the only way the
//! image leaves the app. Closing without picking one discards it
//! (`requestClose`, `collagewindow.cpp:1444`). FastPin used to write the PNG
//! and fill the clipboard the instant 完成 was pressed, so a 16000 px tall
//! stitch landed in the screenshot folder before the user ever saw it.
//!
//! This window is that review step, reduced to what one stitched image needs:
//! the picture, a zoom control, and the export actions. `保存并复制` keeps the
//! historical 完成 behaviour (PNG + clipboard); `保存` and `复制` do one of the
//! two. Closing discards the shot, matching snow-shot's Cancel.
//!
//! Rendering needs to own the pixels (`RenderImage` wraps an `RgbaImage`), so
//! opening the window copies the stitched buffer once. Everything after that
//! shares one `Arc`: the export actions hand that same allocation to the
//! worker thread instead of copying a buffer that can be hundreds of
//! megabytes (see `app::export_pixels_shared`).

use std::cell::{Cell, RefCell};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use gpui::{
    actions, div, img, prelude::*, px, size, App, Bounds, Context, CursorStyle, FocusHandle,
    Focusable, FontWeight, ImageSource, KeyBinding, MouseButton, Render, RenderImage, Window,
    WindowBounds, WindowHandle, WindowOptions,
};

use crate::overlay::theme;

actions!(
    long_preview,
    [
        SaveLongPreview,
        CopyLongPreview,
        SaveAndCopyLongPreview,
        CloseLongPreview,
        ZoomInLongPreview,
        ZoomOutLongPreview,
        FitLongPreview,
        SplitLongPreview,
        CollageLongPreview
    ]
);

/// Key context so the bindings only fire while the preview window has focus.
const KEY_CONTEXT: &str = "LongPreview";

const MIN_ZOOM: f32 = 0.05;
const MAX_ZOOM: f32 = 4.0;
const ZOOM_STEP: f32 = 1.25;
/// Gutter between the image and the scroll area's edges.
const PAD: f32 = 16.0;

thread_local! {
    /// Open preview windows, kept so GPUI does not drop the handles (and with
    /// them the windows). Entries remove themselves as they close.
    static PREVIEW_WINDOWS: RefCell<Vec<(u64, WindowHandle<LongPreviewView>)>> =
        const { RefCell::new(Vec::new()) };
}

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

thread_local! {
    /// One-shot guard so the key bindings are registered a single time.
    static KEYS_BOUND: Cell<bool> = const { Cell::new(false) };
}

/// Shows the finished stitch in a review window.
///
/// `pixels` is the tight BGRA buffer `LongWorker::finish` returns, at
/// `w * 4` stride. The window takes ownership and keeps it alive for both the
/// on-screen image and the exports.
pub fn show_long_preview(cx: &mut App, pixels: Vec<u8>, w: u32, h: u32) {
    if w == 0 || h == 0 {
        crate::perf::log_app("long preview: empty image");
        return;
    }
    bind_keys(cx);
    prune_closed(cx);
    // 已有预览窗口时只激活它，不再重复开窗口：每次新开都要复制一份可能
    // 上百 MB 的拼接缓冲，重复打开会成倍占用内存。
    let last = PREVIEW_WINDOWS.with(|slot| slot.borrow().last().map(|(_, handle)| *handle));
    let active = last
        .and_then(|handle| {
            handle
                .update(cx, |_, window, _| window.activate_window())
                .ok()
        })
        .is_some();
    if active {
        return;
    }

    let pixels = Arc::new(pixels);
    let Some((image, permit)) = render_image(&pixels, w, h) else {
        // A size mismatch means the buffer and the reported geometry disagree.
        // Exporting blind is worse than the old behaviour only if the image is
        // wrong, so fall back to the historical path instead of losing the shot.
        crate::perf::log_app("long preview: pixel buffer size mismatch, exporting directly");
        crate::app::export_pin_pixels(pixels.as_ref().clone(), w, h, true, true);
        return;
    };

    // Aim for the image at a comfortable width, then let the view fit-to-width
    // inside it. Very tall stitches still open at a readable width.
    let content_w = (w as f32).clamp(360.0, 1000.0);
    let content_h = (h as f32).clamp(320.0, 720.0);
    let bounds = Bounds::centered(
        None,
        size(px(content_w + PAD * 2.0), px(content_h + PAD * 2.0 + 92.0)),
        cx,
    );

    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    // Keep an Arc for the failure path. The view gets its own clone, so when
    // `open_window` fails and drops it, `try_unwrap` below recovers the buffer
    // without copying a possibly huge stitch.
    let view_pixels = pixels.clone();
    let opened = cx.open_window(
        WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            focus: true,
            show: true,
            is_movable: true,
            is_resizable: true,
            is_minimizable: true,
            window_min_size: Some(size(px(420.), px(360.))),
            ..gpui_kit::component::TitleBar::window_options()
        },
        |window, cx| {
            let fit = fit_zoom(window.viewport_size().width.into(), w as f32);
            cx.new(move |cx| LongPreviewView::new(cx, image, view_pixels, (w, h), fit, permit))
        },
    );

    let handle = match opened {
        Ok(handle) => handle,
        Err(e) => {
            // No window means no Save/Copy action; use the same historical
            // export fallback as the buffer-mismatch branch so the shot is not
            // silently lost.
            crate::perf::log_app(&format!(
                "long preview window failed: {e}; exporting directly"
            ));
            let bytes = Arc::try_unwrap(pixels).unwrap_or_else(|arc| (*arc).clone());
            crate::app::export_pin_pixels(bytes, w, h, true, true);
            return;
        }
    };

    let focused = handle.update(cx, |view, window, cx| {
        window.focus(&view.focus_handle, cx);
        window.on_window_should_close(cx, move |_, cx| {
            forget(id);
            cx.refresh_windows();
            true
        });
    });
    if focused.is_err() {
        return;
    }
    PREVIEW_WINDOWS.with(|slot| slot.borrow_mut().push((id, handle)));
}

/// Same conversion the pins use: the stitched buffer goes in as-is and GPUI's
/// image pipeline handles the channel order at upload time.
fn render_image(
    pixels: &Arc<Vec<u8>>,
    w: u32,
    h: u32,
) -> Option<(Arc<RenderImage>, crate::image_budget::MemoryPermit)> {
    // `from_raw` copies the whole buffer into the RenderImage. Reserve that copy
    // against the process-wide ledger and hand the permit to the view so it is
    // released only when the preview image goes away.
    let bytes = (w as u64).checked_mul(h as u64)?.checked_mul(4)?;
    let permit = crate::image_budget::try_reserve(bytes).ok()?;
    let rgba = image::RgbaImage::from_raw(w, h, pixels.as_ref().clone())?;
    Some((
        Arc::new(RenderImage::new(vec![image::Frame::new(rgba)])),
        permit,
    ))
}

fn bind_keys(cx: &mut App) {
    if KEYS_BOUND.get() {
        return;
    }
    KEYS_BOUND.set(true);
    let context = Some(KEY_CONTEXT);
    cx.bind_keys(vec![
        KeyBinding::new("escape", CloseLongPreview, context),
        KeyBinding::new("enter", SaveAndCopyLongPreview, context),
        KeyBinding::new("ctrl-s", SaveLongPreview, context),
        KeyBinding::new("ctrl-c", CopyLongPreview, context),
        KeyBinding::new("ctrl-plus", ZoomInLongPreview, context),
        KeyBinding::new("ctrl-equal", ZoomInLongPreview, context),
        KeyBinding::new("ctrl-minus", ZoomOutLongPreview, context),
        KeyBinding::new("ctrl-0", FitLongPreview, context),
        KeyBinding::new("ctrl-shift-s", SplitLongPreview, context),
    ]);
}

/// Drops handles whose window the user already closed, so the registry does
/// not grow over a long session.
fn prune_closed(cx: &mut App) {
    PREVIEW_WINDOWS.with(|slot| {
        slot.borrow_mut()
            .retain(|(_, handle)| handle.update(cx, |_, _, _| ()).is_ok());
    });
}

fn forget(id: u64) {
    PREVIEW_WINDOWS.with(|slot| slot.borrow_mut().retain(|(open, _)| *open != id));
}

/// Scale that makes the whole width visible inside `viewport_w`, never
/// magnifying past 1:1 — blowing up a small stitch by default wastes space.
fn fit_zoom(viewport_w: f32, image_w: f32) -> f32 {
    if image_w <= 0.0 {
        return 1.0;
    }
    let available = (viewport_w - PAD * 2.0).max(64.0);
    (available / image_w).clamp(MIN_ZOOM, 1.0)
}

pub struct LongPreviewView {
    focus_handle: FocusHandle,
    image: Arc<RenderImage>,
    /// Ledger reservation for the RenderImage copy, held for the view's life.
    /// The field is intentionally never read — its Drop releases the bytes.
    #[allow(dead_code)]
    permit: crate::image_budget::MemoryPermit,
    pixels: Arc<Vec<u8>>,
    pixel_size: (u32, u32),
    zoom: f32,
    saved: bool,
    copied: bool,
}

impl LongPreviewView {
    fn new(
        cx: &mut Context<Self>,
        image: Arc<RenderImage>,
        pixels: Arc<Vec<u8>>,
        pixel_size: (u32, u32),
        zoom: f32,
        permit: crate::image_budget::MemoryPermit,
    ) -> Self {
        Self {
            focus_handle: cx.focus_handle(),
            image,
            permit,
            pixels,
            pixel_size,
            zoom,
            saved: false,
            copied: false,
        }
    }

    fn set_zoom(&mut self, zoom: f32, cx: &mut Context<Self>) {
        let next = zoom.clamp(MIN_ZOOM, MAX_ZOOM);
        if (next - self.zoom).abs() < f32::EPSILON {
            return;
        }
        self.zoom = next;
        cx.notify();
    }

    /// Hands the shared buffer to the export thread. `write_file` and `copy`
    /// are independent so the three buttons differ only by these two flags.
    fn export(&mut self, write_file: bool, copy: bool, cx: &mut Context<Self>) {
        let (w, h) = self.pixel_size;
        let result =
            crate::app::export_pixels_shared(self.pixels.clone(), w, h, w * 4, write_file, copy);
        match result {
            Ok(()) => {
                self.saved |= write_file;
                self.copied |= copy;
                cx.notify();
            }
            Err(e) => crate::perf::log_app(&format!("long preview export: {e}")),
        }
    }

    fn on_save(&mut self, _: &SaveLongPreview, _: &mut Window, cx: &mut Context<Self>) {
        self.export(true, false, cx);
    }

    fn on_copy(&mut self, _: &CopyLongPreview, _: &mut Window, cx: &mut Context<Self>) {
        self.export(false, true, cx);
    }

    fn on_save_and_copy(
        &mut self,
        _: &SaveAndCopyLongPreview,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // The file write runs on a worker thread, so `Ok` here only means the
        // job started. Keep the window open: closing on spawn success would
        // hide a later write failure and leave the user without a file.
        self.export(true, true, cx);
    }

    fn on_close(&mut self, _: &CloseLongPreview, window: &mut Window, _: &mut Context<Self>) {
        window.remove_window();
    }

    /// Hands the shared stitch buffer straight to the splitter — no re-encode,
    /// so a tall screenshot can be cut into printable tiles immediately.
    fn on_split(&mut self, _: &SplitLongPreview, _window: &mut Window, cx: &mut Context<Self>) {
        crate::imagesplit_window::show_split_pixels(
            self.pixels.clone(),
            self.pixel_size.0,
            self.pixel_size.1,
            cx,
        );
    }

    /// Opens the collage window with the stitched image, ready for multi-image
    /// layout and PDF export.
    fn on_collage(&mut self, _: &CollageLongPreview, _window: &mut Window, cx: &mut Context<Self>) {
        crate::collage_window::show_collage_pixels(
            self.pixels.clone(),
            self.pixel_size.0,
            self.pixel_size.1,
            cx,
        );
    }

    fn on_zoom_in(&mut self, _: &ZoomInLongPreview, _: &mut Window, cx: &mut Context<Self>) {
        self.set_zoom(self.zoom * ZOOM_STEP, cx);
    }

    fn on_zoom_out(&mut self, _: &ZoomOutLongPreview, _: &mut Window, cx: &mut Context<Self>) {
        self.set_zoom(self.zoom / ZOOM_STEP, cx);
    }

    fn on_fit(&mut self, _: &FitLongPreview, window: &mut Window, cx: &mut Context<Self>) {
        let fit = fit_zoom(
            window.viewport_size().width.into(),
            self.pixel_size.0 as f32,
        );
        self.set_zoom(fit, cx);
    }

    fn titlebar(&self) -> impl IntoElement {
        gpui_kit::component::TitleBar::new().pl(px(14.)).child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(9.))
                .child(
                    div()
                        .text_size(px(13.))
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(theme::primary_fg())
                        .child("长截图预览"),
                )
                .child(
                    div()
                        .text_size(px(11.))
                        .text_color(theme::secondary_fg())
                        .child(format!("{} × {} px", self.pixel_size.0, self.pixel_size.1)),
                ),
        )
    }

    /// One status sentence on the left, the export actions on the right.
    fn footer(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let percent = (self.zoom * 100.0).round() as i32;
        let status = match (self.saved, self.copied) {
            (true, true) => "已保存并复制",
            (true, false) => "已保存",
            (false, true) => "已复制",
            (false, false) => "关闭将丢弃此截图",
        };
        div()
            .flex_none()
            .h(px(52.))
            .px(px(16.))
            .flex()
            .flex_row()
            .items_center()
            .justify_between()
            .gap(px(12.))
            .bg(theme::surface_solid())
            .border_t_1()
            .border_color(theme::border())
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(10.))
                    .child(zoom_button("zoom-out", "−").on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, _, window, cx| {
                            this.on_zoom_out(&ZoomOutLongPreview, window, cx)
                        }),
                    ))
                    .child(
                        div()
                            .min_w(px(44.))
                            .text_size(px(11.))
                            .text_color(theme::secondary_fg())
                            .child(format!("{percent}%")),
                    )
                    .child(zoom_button("zoom-in", "+").on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, _, window, cx| {
                            this.on_zoom_in(&ZoomInLongPreview, window, cx)
                        }),
                    ))
                    .child(zoom_button("zoom-fit", "适应").on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, _, window, cx| this.on_fit(&FitLongPreview, window, cx)),
                    ))
                    .child(
                        div()
                            .id("preview-split")
                            .ml(px(6.))
                            .h(px(26.))
                            .px(px(10.))
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded(px(5.))
                            .bg(theme::elevated_surface())
                            .border_1()
                            .border_color(theme::border())
                            .text_size(px(11.))
                            .cursor(CursorStyle::PointingHand)
                            .hover(|s| s.bg(theme::selection_wash()))
                            .child("切图")
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(|this, _, window, cx| {
                                    this.on_split(&SplitLongPreview, window, cx)
                                }),
                            ),
                    )
                    .child(
                        div()
                            .id("preview-collage")
                            .ml(px(6.))
                            .h(px(26.))
                            .px(px(10.))
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded(px(5.))
                            .bg(theme::elevated_surface())
                            .border_1()
                            .border_color(theme::border())
                            .text_size(px(11.))
                            .cursor(CursorStyle::PointingHand)
                            .hover(|s| s.bg(theme::selection_wash()))
                            .child("拼图")
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(|this, _, window, cx| {
                                    this.on_collage(&CollageLongPreview, window, cx)
                                }),
                            ),
                    )
                    .child(
                        div()
                            .ml(px(6.))
                            .text_size(px(11.))
                            .text_color(theme::secondary_fg())
                            .child(status),
                    ),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(8.))
                    .child(action_button("preview-close", "关闭", false).on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, _, window, cx| {
                            this.on_close(&CloseLongPreview, window, cx)
                        }),
                    ))
                    .child(action_button("preview-copy", "复制", false).on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, _, window, cx| {
                            this.on_copy(&CopyLongPreview, window, cx)
                        }),
                    ))
                    .child(action_button("preview-save", "保存", false).on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, _, window, cx| {
                            this.on_save(&SaveLongPreview, window, cx)
                        }),
                    ))
                    .child(
                        action_button("preview-save-copy", "保存并复制", true).on_mouse_down(
                            MouseButton::Left,
                            cx.listener(|this, _, window, cx| {
                                this.on_save_and_copy(&SaveAndCopyLongPreview, window, cx)
                            }),
                        ),
                    ),
            )
    }

    fn canvas(&self) -> impl IntoElement {
        let (w, h) = (self.pixel_size.0 as f32, self.pixel_size.1 as f32);
        let dw = (w * self.zoom).max(1.0);
        let dh = (h * self.zoom).max(1.0);
        div()
            .id("long-preview-scroll")
            .flex_1()
            .min_h(px(0.))
            .w_full()
            .overflow_scroll()
            .bg(theme::muted())
            .child(
                div()
                    .w_full()
                    .p(px(PAD))
                    .flex()
                    .flex_col()
                    .items_center()
                    .child(
                        img(ImageSource::Render(self.image.clone()))
                            .w(px(dw))
                            .h(px(dh)),
                    ),
            )
    }
}

impl Focusable for LongPreviewView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for LongPreviewView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(theme::surface_solid())
            .text_color(theme::primary_fg())
            .key_context(KEY_CONTEXT)
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::on_save))
            .on_action(cx.listener(Self::on_copy))
            .on_action(cx.listener(Self::on_save_and_copy))
            .on_action(cx.listener(Self::on_close))
            .on_action(cx.listener(Self::on_zoom_in))
            .on_action(cx.listener(Self::on_zoom_out))
            .on_action(cx.listener(Self::on_fit))
            .on_action(cx.listener(Self::on_split))
            .on_action(cx.listener(Self::on_collage))
            .child(self.titlebar())
            .child(self.canvas())
            .child(self.footer(cx))
    }
}

/// Footer export button. `primary` paints the accent fill, mirroring the
/// settings window's filled button.
fn action_button(
    id: &'static str,
    label: &'static str,
    primary: bool,
) -> gpui::Stateful<gpui::Div> {
    let base = div()
        .id(id)
        .flex_none()
        .h(px(30.))
        .px(px(14.))
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(6.))
        .text_size(px(12.))
        .cursor(CursorStyle::PointingHand)
        .child(label);
    if primary {
        base.bg(theme::accent())
            .text_color(theme::on_accent())
            .hover(|s| s.bg(theme::accent_hover()))
    } else {
        base.bg(theme::elevated_surface())
            .text_color(theme::primary_fg())
            .border_1()
            .border_color(theme::border())
            .hover(|s| s.bg(theme::selection_wash()))
    }
}

fn zoom_button(id: &'static str, label: &'static str) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .flex_none()
        .h(px(26.))
        .min_w(px(26.))
        .px(px(8.))
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(5.))
        .text_size(px(12.))
        .text_color(theme::primary_fg())
        .cursor(CursorStyle::PointingHand)
        .hover(|s| s.bg(theme::elevated_surface()))
        .child(label)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_wide_stitch_fits_the_viewport() {
        // 1600 px of content in a 1000 px pane: scale down so the width, and
        // with it the whole picture, is visible without scrolling sideways.
        let zoom = fit_zoom(1000.0, 1600.0);
        assert!(zoom < 1.0);
        assert!((1600.0 * zoom) <= 1000.0);
    }

    #[test]
    fn a_narrow_stitch_is_not_magnified() {
        // Blowing a small capture up to fill the window only softens it.
        assert_eq!(fit_zoom(1000.0, 400.0), 1.0);
    }

    #[test]
    fn fit_zoom_survives_degenerate_sizes() {
        assert_eq!(fit_zoom(1000.0, 0.0), 1.0);
        // A pane narrower than the gutters still yields a usable scale rather
        // than a zero or negative one.
        let zoom = fit_zoom(0.0, 4000.0);
        assert!(zoom >= MIN_ZOOM);
        assert!(4000.0 * zoom >= 64.0);
    }

    #[test]
    fn zoom_requests_are_clamped_to_the_supported_range() {
        // The clamps are what keep a 16384 px stitch from asking the renderer
        // for an impossible layout size, and what stop a fit-to-width on a
        // narrow window from scaling the image down to nothing.
        let widest = crate::image_budget::MAX_SIDE as f32;
        let clamped_high = (widest * 4.0).clamp(MIN_ZOOM, MAX_ZOOM);
        assert_eq!(clamped_high, MAX_ZOOM);
        let clamped_low = (1.0_f32 / widest).clamp(MIN_ZOOM, MAX_ZOOM);
        assert_eq!(clamped_low, MIN_ZOOM);
        // At the smallest allowed zoom the tallest stitch still occupies at
        // least one layout pixel, so it is neither invisible nor degenerate.
        assert!((widest * MIN_ZOOM) >= 1.0);
    }
}
