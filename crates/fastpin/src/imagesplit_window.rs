//! Image-splitting window — the FastPin front end for [`crate::imagesplit`].
//!
//! Mirrors snow-shot's `ImageSplitWindow` at a deliberately small scale: one
//! image, a control column for the split parameters, a live grid preview, and
//! an export action that calls the pure `export_split_images` core. The image
//! can arrive from the long-capture preview (the common case: cut a tall shot
//! into printable pages) or be picked from disk.

use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use gpui::{
    actions, div, img, prelude::*, px, size, App, Bounds, Context, CursorStyle, FocusHandle,
    Focusable, FontWeight, ImageSource, KeyBinding, MouseButton, Render, RenderImage, SharedString,
    Window, WindowBounds, WindowHandle, WindowOptions,
};

use crate::config::ImageFormat;
use crate::imagesplit::{
    calculate_layout, export_split_images, SplitDirection, SplitExportResult, SplitLayout,
    SplitSettings, SplitType, SplitUnit,
};
use crate::overlay::theme;

actions!(imagesplit_window, [CloseSplit, ExportSplit, OpenSplitImage]);

const KEY_CONTEXT: &str = "ImageSplitWindow";
/// Preview is fit to this width (never magnified past 1:1), matching the long
/// preview so a tall capture stays legible while the whole grid is visible.
const PREVIEW_MAX_W: f32 = 680.0;

thread_local! {
    static SPLIT_WINDOWS: RefCell<Vec<(u64, WindowHandle<SplitWindowView>)>> =
        const { RefCell::new(Vec::new()) };
}
static NEXT_ID: AtomicU64 = AtomicU64::new(1);
thread_local! {
    static KEYS_BOUND: Cell<bool> = const { Cell::new(false) };
}

/// Opens the splitter with no image; the user picks one from the control column.
pub fn show_split_window(cx: &mut App) {
    open(cx, None);
}

/// Opens the splitter preloaded with a captured buffer (BGRA, tight stride).
pub fn show_split_pixels(pixels: Arc<Vec<u8>>, width: u32, height: u32, cx: &mut App) {
    if width == 0 || height == 0 {
        return;
    }
    open(cx, Some((pixels, width, height, "screenshot".to_string())));
}

fn open(cx: &mut App, initial: Option<(Arc<Vec<u8>>, u32, u32, String)>) {
    bind_keys(cx);
    prune_closed(cx);
    // 已打开的窗口直接激活，重复点击入口不再叠加新窗口。
    let last = SPLIT_WINDOWS.with(|slot| slot.borrow().last().map(|(_, handle)| *handle));
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

    let bounds = Bounds::centered(None, size(px(1020.), px(720.)), cx);
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    let opened = cx.open_window(
        WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            focus: true,
            show: true,
            is_movable: true,
            is_resizable: true,
            is_minimizable: true,
            window_min_size: Some(size(px(760.), px(520.))),
            ..gpui_kit::component::TitleBar::window_options()
        },
        |_, cx| cx.new(|cx| SplitWindowView::new(cx, initial)),
    );
    let handle = match opened {
        Ok(h) => h,
        Err(e) => {
            crate::perf::log_app(&format!("image split window failed: {e}"));
            return;
        }
    };
    if handle
        .update(cx, |view, window, cx| {
            window.focus(&view.focus_handle, cx);
            window.on_window_should_close(cx, move |_, cx| {
                forget(id);
                cx.refresh_windows();
                true
            });
        })
        .is_err()
    {
        return;
    }
    SPLIT_WINDOWS.with(|slot| slot.borrow_mut().push((id, handle)));
}

fn bind_keys(cx: &mut App) {
    if KEYS_BOUND.get() {
        return;
    }
    KEYS_BOUND.set(true);
    let context = Some(KEY_CONTEXT);
    cx.bind_keys(vec![
        KeyBinding::new("escape", CloseSplit, context),
        KeyBinding::new("ctrl-e", ExportSplit, context),
        KeyBinding::new("ctrl-o", OpenSplitImage, context),
    ]);
}

fn prune_closed(cx: &mut App) {
    SPLIT_WINDOWS.with(|slot| {
        slot.borrow_mut()
            .retain(|(_, handle)| handle.update(cx, |_, _, _| ()).is_ok());
    });
}

fn forget(id: u64) {
    SPLIT_WINDOWS.with(|slot| slot.borrow_mut().retain(|(open, _)| *open != id));
}

/// BGRA buffer → GPUI image. Same conversion as `longpreview::render_image`:
/// the buffer is handed over as-is and GPUI's upload handles channel order.
fn render_image(
    pixels: &Arc<Vec<u8>>,
    w: u32,
    h: u32,
) -> Option<(Arc<RenderImage>, crate::image_budget::MemoryPermit)> {
    // `from_raw` copies the whole buffer into the RenderImage. Reserve that copy
    // against the process-wide ledger and keep the permit on the view alongside
    // the image/pixels so several windows cannot sum to an OOM.
    let bytes = (w as u64).checked_mul(h as u64)?.checked_mul(4)?;
    let permit = crate::image_budget::try_reserve(bytes).ok()?;
    let rgba = image::RgbaImage::from_raw(w, h, pixels.as_ref().clone())?;
    Some((
        Arc::new(RenderImage::new(vec![image::Frame::new(rgba)])),
        permit,
    ))
}

fn placeholder_image() -> Arc<RenderImage> {
    let rgba = image::RgbaImage::from_raw(1, 1, vec![0, 0, 0, 0]).unwrap();
    Arc::new(RenderImage::new(vec![image::Frame::new(rgba)]))
}

pub struct SplitWindowView {
    focus_handle: FocusHandle,
    pixels: Arc<Vec<u8>>,
    width: u32,
    height: u32,
    image: Arc<RenderImage>,
    /// Ledger reservation for the RenderImage copy; replaced with the next
    /// image, dropped when the window closes.
    permit: Option<crate::image_budget::MemoryPermit>,
    image_name: String,
    settings: SplitSettings,
    layout: SplitLayout,
    format: ImageFormat,
    quality: u8,
    export_result: Option<SplitExportResult>,
    status: SharedString,
}

impl SplitWindowView {
    fn new(cx: &mut Context<Self>, initial: Option<(Arc<Vec<u8>>, u32, u32, String)>) -> Self {
        let config = crate::config::get();
        let split = config.screenshot.split.clone();
        let export = config.screenshot.export.clone();
        let mut this = Self {
            focus_handle: cx.focus_handle(),
            pixels: Arc::new(Vec::new()),
            width: 0,
            height: 0,
            image: placeholder_image(),
            permit: None,
            image_name: "split".to_string(),
            settings: split.clone(),
            layout: calculate_layout(0, 0, &split),
            format: export.format,
            quality: export.clamped_quality(),
            export_result: None,
            status: "请选择一张图片开始切图。".into(),
        };
        if let Some((pixels, w, h, name)) = initial {
            this.set_image(pixels, w, h, name);
        }
        this
    }

    fn set_image(&mut self, pixels: Arc<Vec<u8>>, w: u32, h: u32, name: String) {
        match render_image(&pixels, w, h) {
            Some((image, permit)) => {
                self.image = image;
                self.permit = Some(permit); // releases the previous reservation
            }
            None => {
                self.image = placeholder_image();
                self.permit = None;
            }
        }
        self.pixels = pixels;
        self.width = w;
        self.height = h;
        self.image_name = name;
        self.export_result = None;
        self.recalc();
        self.status = format!("已载入 {} × {} 图片。", w, h).into();
    }

    fn recalc(&mut self) {
        self.layout = calculate_layout(self.width, self.height, &self.settings);
        self.export_result = None;
    }

    /// 切图参数落盘。每次都从最新配置出发，只覆盖本窗口管辖的 `split` 字段，
    /// 这样设置页或其它窗口在这期间保存的修改不会被本地副本整体冲掉。
    fn persist_split(&mut self, cx: &mut Context<Self>) {
        let mut config = crate::config::get();
        config.screenshot.split = self.settings.clone();
        if let Err(e) = crate::config::save(&config) {
            self.status = format!("设置保存失败：{e}").into();
            cx.notify();
        }
    }

    /// 格式 / 质量属于 `screenshot.export`，与 split 分开写回，但同样基于最新配置。
    fn persist_export(&mut self, cx: &mut Context<Self>) {
        let mut config = crate::config::get();
        config.screenshot.export.format = self.format;
        config.screenshot.export.quality = self.quality;
        if let Err(e) = crate::config::save(&config) {
            self.status = format!("设置保存失败：{e}").into();
            cx.notify();
        }
    }

    fn has_image(&self) -> bool {
        self.width > 0 && self.height > 0
    }

    fn on_open_image(&mut self, _: &OpenSplitImage, _window: &mut Window, cx: &mut Context<Self>) {
        let start = crate::config::screenshot_dir().unwrap_or_else(|_| PathBuf::from("."));
        let picked = match crate::settings::system::pick_image(&start) {
            Ok(Some(p)) => p,
            Ok(None) => return,
            Err(e) => {
                self.status = format!("选择图片失败：{e}").into();
                cx.notify();
                return;
            }
        };
        let name = picked
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "split".to_string());
        match crate::codec::decode_bgra(&picked) {
            Some((pixels, w, h)) => {
                self.set_image(Arc::new(pixels), w, h, name);
            }
            None => {
                self.status = "无法解码该图片。".into();
            }
        }
        cx.notify();
    }

    fn on_export(&mut self, _: &ExportSplit, _window: &mut Window, cx: &mut Context<Self>) {
        if !self.has_image() {
            self.status = "请先选择一张图片。".into();
            cx.notify();
            return;
        }
        if !self.layout.is_valid() {
            self.status = "切图参数有误，请检查切分数量。".into();
            cx.notify();
            return;
        }
        let parent = match crate::config::screenshot_dir() {
            Ok(d) => d,
            Err(e) => {
                self.status = format!("无法取得保存目录：{e}").into();
                cx.notify();
                return;
            }
        };
        // 克隆所需输入后把 WIC 编码与写盘移到后台线程，避免块数多时冻结 UI。
        let pixels = self.pixels.clone();
        let layout = self.layout.clone();
        let format = self.format;
        let quality = self.quality;
        let image_name = self.image_name.clone();
        self.status = "导出中…".into();
        self.export_result = None;
        cx.notify();

        let task = cx.background_spawn(async move {
            export_split_images(&pixels, &layout, format, quality, &parent, &image_name)
        });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                if result.succeeded() {
                    this.status = format!(
                        "已导出 {} 张到 {}",
                        result.saved_count,
                        result.directory.display()
                    )
                    .into();
                } else {
                    this.status = format!("导出失败：{}", result.error).into();
                }
                this.export_result = Some(result);
                cx.notify();
            });
        })
        .detach();
    }

    fn on_open_folder(&mut self, _cx: &mut Context<Self>) {
        if let Some(result) = &self.export_result {
            if result.succeeded() {
                let _ = crate::settings::system::open_folder(&result.directory);
            }
        }
    }

    fn on_close(&mut self, _: &CloseSplit, window: &mut Window, _cx: &mut Context<Self>) {
        window.remove_window();
    }

    // ---- parameter mutations --------------------------------------------

    fn cycle_direction(&mut self, cx: &mut Context<Self>) {
        self.settings.direction = match self.settings.direction {
            SplitDirection::Vertical => SplitDirection::Horizontal,
            SplitDirection::Horizontal => SplitDirection::Grid,
            SplitDirection::Grid => SplitDirection::Vertical,
        };
        self.recalc();
        self.persist_split(cx);
    }

    fn cycle_type(&mut self, cx: &mut Context<Self>) {
        self.settings.split_type = match self.settings.split_type {
            SplitType::Equal => SplitType::Precise,
            SplitType::Precise => SplitType::Smart,
            SplitType::Smart => SplitType::Equal,
        };
        self.recalc();
        self.persist_split(cx);
    }

    fn cycle_unit(&mut self, cx: &mut Context<Self>) {
        self.settings.unit = match self.settings.unit {
            SplitUnit::Pixels => SplitUnit::Percentage,
            SplitUnit::Percentage => SplitUnit::Pixels,
        };
        self.recalc();
        self.persist_split(cx);
    }

    fn cycle_format(&mut self, cx: &mut Context<Self>) {
        // Skip formats this machine's WIC cannot encode; offering WebP on a
        // build without it only produces a failed export.
        let all = ImageFormat::ALL;
        let start = all
            .iter()
            .position(|f| *f == self.format)
            .unwrap_or_default();
        for step in 1..=all.len() {
            let candidate = all[(start + step) % all.len()];
            if crate::codec::format_available(candidate) {
                self.format = candidate;
                break;
            }
        }
        self.format = crate::codec::effective_format(self.format);
        self.recalc();
        // 格式 / 质量归属 export，只更新这两个字段，不影响其它配置。
        self.persist_export(cx);
    }

    fn step_u32(field: &mut u32, delta: i32, min: u32, max: u32) {
        let next = *field as i64 + delta as i64;
        *field = next.clamp(min as i64, max as i64) as u32;
    }

    fn step_count(&mut self, delta: i32, cx: &mut Context<Self>) {
        Self::step_u32(&mut self.settings.count, delta, 1, 20);
        self.recalc();
        self.persist_split(cx);
    }

    fn step_rows(&mut self, delta: i32, cx: &mut Context<Self>) {
        Self::step_u32(&mut self.settings.rows, delta, 1, 20);
        self.recalc();
        self.persist_split(cx);
    }

    fn step_columns(&mut self, delta: i32, cx: &mut Context<Self>) {
        Self::step_u32(&mut self.settings.columns, delta, 1, 20);
        self.recalc();
        self.persist_split(cx);
    }

    fn step_pixels(&mut self, delta: i32, cx: &mut Context<Self>) {
        Self::step_u32(&mut self.settings.pixels, delta, 16, 4096);
        self.recalc();
        self.persist_split(cx);
    }

    fn step_percentage(&mut self, delta: i32, cx: &mut Context<Self>) {
        let value = self.settings.percentage.parse::<i32>().unwrap_or(50);
        let next = (value + delta * 5).clamp(5, 95);
        self.settings.percentage = next.to_string();
        self.recalc();
        self.persist_split(cx);
    }

    fn step_overlap(&mut self, delta: i32, cx: &mut Context<Self>) {
        Self::step_u32(&mut self.settings.overlap_width, delta, 0, 256);
        self.recalc();
        self.persist_split(cx);
    }

    fn step_quality(&mut self, delta: i32, cx: &mut Context<Self>) {
        let next = self.quality as i32 + delta * 5;
        self.quality = next.clamp(1, 100) as u8;
        self.recalc();
        self.persist_export(cx);
    }

    // ---- rendering -------------------------------------------------------

    fn titlebar(&self) -> impl IntoElement {
        div()
            .flex_none()
            .h(px(44.))
            .px(px(16.))
            .flex()
            .flex_row()
            .items_center()
            .justify_between()
            .bg(theme::elevated_surface())
            .border_b_1()
            .border_color(theme::border())
            .child(
                div()
                    .text_size(px(14.))
                    .font_weight(FontWeight::SEMIBOLD)
                    .child("图片切图"),
            )
            .child(
                div()
                    .text_size(px(11.))
                    .text_color(theme::secondary_fg())
                    .child(format!(
                        "{} 块 · {} × {}",
                        self.layout.output_count(),
                        self.width,
                        self.height
                    )),
            )
    }

    fn control_row(
        &self,
        id: &'static str,
        label: &'static str,
        value: String,
        cx: &mut Context<Self>,
        on_click: impl Fn(&mut Self, &mut Context<Self>) + 'static,
    ) -> impl IntoElement {
        div()
            .flex()
            .flex_row()
            .items_center()
            .justify_between()
            .gap(px(8.))
            .child(div().text_size(px(12.)).child(label))
            .child(
                div()
                    .id(id)
                    .min_w(px(96.))
                    .h(px(26.))
                    .px(px(10.))
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(px(5.))
                    .bg(theme::elevated_surface())
                    .border_1()
                    .border_color(theme::border())
                    .text_size(px(12.))
                    .cursor(CursorStyle::PointingHand)
                    .hover(|s| s.bg(theme::selection_wash()))
                    .child(value)
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _, _, cx| on_click(this, cx)),
                    ),
            )
    }

    fn stepper_row(
        &self,
        id: &'static str,
        label: &'static str,
        value: String,
        cx: &mut Context<Self>,
        dec: impl Fn(&mut Self, &mut Context<Self>) + 'static,
        inc: impl Fn(&mut Self, &mut Context<Self>) + 'static,
    ) -> impl IntoElement {
        let minus_id: SharedString = format!("{id}-dec").into();
        let plus_id: SharedString = format!("{id}-inc").into();
        div()
            .flex()
            .flex_row()
            .items_center()
            .justify_between()
            .gap(px(8.))
            .child(div().text_size(px(12.)).child(label))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(6.))
                    .child(
                        div()
                            .id(minus_id)
                            .w(px(26.))
                            .h(px(26.))
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded(px(5.))
                            .bg(theme::elevated_surface())
                            .border_1()
                            .border_color(theme::border())
                            .text_size(px(13.))
                            .cursor(CursorStyle::PointingHand)
                            .hover(|s| s.bg(theme::selection_wash()))
                            .child("−")
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(move |this, _, _, cx| dec(this, cx)),
                            ),
                    )
                    .child(
                        div()
                            .min_w(px(40.))
                            .text_center()
                            .text_size(px(12.))
                            .child(value),
                    )
                    .child(
                        div()
                            .id(plus_id)
                            .w(px(26.))
                            .h(px(26.))
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded(px(5.))
                            .bg(theme::elevated_surface())
                            .border_1()
                            .border_color(theme::border())
                            .text_size(px(13.))
                            .cursor(CursorStyle::PointingHand)
                            .hover(|s| s.bg(theme::selection_wash()))
                            .child("+")
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(move |this, _, _, cx| inc(this, cx)),
                            ),
                    ),
            )
    }

    fn controls(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let dir = match self.settings.direction {
            SplitDirection::Vertical => "竖向",
            SplitDirection::Horizontal => "横向",
            SplitDirection::Grid => "网格",
        };
        let kind = match self.settings.split_type {
            SplitType::Equal => "等分",
            SplitType::Precise => "精确",
            SplitType::Smart => "智能",
        };
        let unit = match self.settings.unit {
            SplitUnit::Pixels => "像素",
            SplitUnit::Percentage => "百分比",
        };
        let fmt = match self.format {
            ImageFormat::Png => "PNG",
            ImageFormat::Jpeg => "JPEG",
            ImageFormat::Webp => "WebP",
        };
        let precise = self.settings.split_type == SplitType::Precise;
        let grid = self.settings.direction == SplitDirection::Grid;

        let mut el = div()
            .id("split-controls")
            .flex_none()
            .w(px(300.))
            .h_full()
            .overflow_scroll()
            .bg(theme::surface_solid())
            .border_r_1()
            .border_color(theme::border())
            .p(px(14.))
            .flex()
            .flex_col()
            .gap(px(10.));

        el = el.child(
            div()
                .text_size(px(11.))
                .text_color(theme::secondary_fg())
                .child(if self.has_image() {
                    format!("{} · {} × {}", self.image_name, self.width, self.height)
                } else {
                    "尚未载入图片".to_string()
                }),
        );

        el = el.child(
            div()
                .id("split-open")
                .h(px(30.))
                .flex()
                .items_center()
                .justify_center()
                .rounded(px(5.))
                .bg(theme::elevated_surface())
                .border_1()
                .border_color(theme::border())
                .text_size(px(12.))
                .cursor(CursorStyle::PointingHand)
                .hover(|s| s.bg(theme::selection_wash()))
                .child("选择图片…")
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _, window, cx| {
                        this.on_open_image(&OpenSplitImage, window, cx)
                    }),
                ),
        );

        el = el.child(
            self.control_row("split-dir", "方向", dir.into(), cx, |this, cx| {
                this.cycle_direction(cx);
            }),
        );
        el = el.child(
            self.control_row("split-type", "类型", kind.into(), cx, |this, cx| {
                this.cycle_type(cx);
            }),
        );

        if precise {
            el = el.child(
                self.control_row("split-unit", "单位", unit.into(), cx, |this, cx| {
                    this.cycle_unit(cx);
                }),
            );
            if self.settings.unit == SplitUnit::Pixels {
                el = el.child(self.stepper_row(
                    "split-pixels",
                    "像素步长",
                    self.settings.pixels.to_string(),
                    cx,
                    |this, cx| this.step_pixels(-16, cx),
                    |this, cx| this.step_pixels(16, cx),
                ));
            } else {
                el = el.child(self.stepper_row(
                    "split-percent",
                    "百分比",
                    format!("{}%", self.settings.percentage),
                    cx,
                    |this, cx| this.step_percentage(-1, cx),
                    |this, cx| this.step_percentage(1, cx),
                ));
            }
        } else if grid {
            el = el.child(self.stepper_row(
                "split-rows",
                "行数",
                self.settings.rows.to_string(),
                cx,
                |this, cx| this.step_rows(-1, cx),
                |this, cx| this.step_rows(1, cx),
            ));
            el = el.child(self.stepper_row(
                "split-cols",
                "列数",
                self.settings.columns.to_string(),
                cx,
                |this, cx| this.step_columns(-1, cx),
                |this, cx| this.step_columns(1, cx),
            ));
        } else {
            el = el.child(self.stepper_row(
                "split-count",
                "数量",
                self.settings.count.to_string(),
                cx,
                |this, cx| this.step_count(-1, cx),
                |this, cx| this.step_count(1, cx),
            ));
        }

        el = el.child(self.stepper_row(
            "split-overlap",
            "重叠像素",
            self.settings.overlap_width.to_string(),
            cx,
            |this, cx| this.step_overlap(-2, cx),
            |this, cx| this.step_overlap(2, cx),
        ));

        el = el.child(
            self.control_row("split-format", "格式", fmt.into(), cx, |this, cx| {
                this.cycle_format(cx);
            }),
        );
        if self.format != ImageFormat::Png {
            el = el.child(self.stepper_row(
                "split-quality",
                "质量",
                self.quality.to_string(),
                cx,
                |this, cx| this.step_quality(-1, cx),
                |this, cx| this.step_quality(1, cx),
            ));
        }

        for (field, message) in self.layout.errors.iter() {
            el = el.child(
                div()
                    .text_size(px(11.))
                    .text_color(theme::danger())
                    .child(format!("{field}: {message}")),
            );
        }
        el
    }

    fn preview(&self) -> impl IntoElement {
        let mut wrap = div()
            .id("split-preview-scroll")
            .flex_1()
            .min_w(px(0.))
            .overflow_scroll()
            .bg(theme::muted())
            .p(px(16.))
            .flex()
            .items_center()
            .justify_center();

        if !self.has_image() {
            return wrap.child(
                div()
                    .py(px(80.))
                    .text_size(px(13.))
                    .text_color(theme::secondary_fg())
                    .child("选择一张图片后，这里会显示切图网格预览。"),
            );
        }

        let scale = (PREVIEW_MAX_W / self.width as f32).min(1.0);
        let dw = (self.width as f32 * scale).max(1.0);
        let dh = (self.height as f32 * scale).max(1.0);

        let mut canvas = div()
            .relative()
            .w(px(dw))
            .h(px(dh))
            .border_1()
            .border_color(theme::border())
            .child(
                img(ImageSource::Render(self.image.clone()))
                    .w(px(dw))
                    .h(px(dh)),
            );

        // Cut guides.
        for &x in &self.layout.vertical_lines {
            let lx = (x as f32 * scale).min(dw - 1.0);
            canvas = canvas.child(
                div()
                    .absolute()
                    .left(px(lx))
                    .top(px(0.))
                    .w(px(1.5))
                    .h(px(dh))
                    .bg(theme::accent()),
            );
        }
        for &y in &self.layout.horizontal_lines {
            let ly = (y as f32 * scale).min(dh - 1.0);
            canvas = canvas.child(
                div()
                    .absolute()
                    .top(px(ly))
                    .left(px(0.))
                    .h(px(1.5))
                    .w(px(dw))
                    .bg(theme::accent()),
            );
        }

        // Tile numbers show the export order (1-based, row-major).
        let count = self.layout.output_count();
        for i in 0..count {
            if let Some(region) = self.layout.base_region_at(i) {
                if region.width == 0 || region.height == 0 {
                    continue;
                }
                let lx = region.x as f32 * scale + 4.0;
                let ly = region.y as f32 * scale + 4.0;
                canvas = canvas.child(
                    div()
                        .absolute()
                        .left(px(lx))
                        .top(px(ly))
                        .px(px(4.))
                        .rounded(px(3.))
                        .bg(theme::surface())
                        .text_size(px(10.))
                        .text_color(theme::primary_fg())
                        .child((i + 1).to_string()),
                );
            }
        }

        wrap = wrap.child(canvas);
        wrap
    }

    fn footer(&self, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .flex_none()
            .h(px(56.))
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
                    .min_w(px(0.))
                    .text_size(px(11.))
                    .text_color(theme::secondary_fg())
                    .child(self.status.clone()),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(8.))
                    .child(split_button("split-folder", "打开文件夹").on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, _, _, cx| this.on_open_folder(cx)),
                    ))
                    .child(split_button("split-close", "关闭").on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, _, window, cx| this.on_close(&CloseSplit, window, cx)),
                    ))
                    .child(
                        split_primary_button("split-export")
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(|this, _, window, cx| {
                                    this.on_export(&ExportSplit, window, cx)
                                }),
                            )
                            .child("导出切图"),
                    ),
            )
    }
}

impl Focusable for SplitWindowView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for SplitWindowView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(theme::surface_solid())
            .text_color(theme::primary_fg())
            .key_context(KEY_CONTEXT)
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::on_open_image))
            .on_action(cx.listener(Self::on_export))
            .on_action(cx.listener(Self::on_close))
            .child(self.titlebar())
            .child(
                div()
                    .flex_1()
                    .min_h(px(0.))
                    .flex()
                    .flex_row()
                    .child(self.controls(cx))
                    .child(self.preview()),
            )
            .child(self.footer(cx))
    }
}

fn split_button(id: &'static str, label: &'static str) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .flex_none()
        .h(px(32.))
        .px(px(14.))
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(6.))
        .bg(theme::elevated_surface())
        .border_1()
        .border_color(theme::border())
        .text_size(px(12.))
        .cursor(CursorStyle::PointingHand)
        .hover(|s| s.bg(theme::selection_wash()))
        .child(label)
}

fn split_primary_button(id: &'static str) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .flex_none()
        .h(px(32.))
        .px(px(16.))
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(6.))
        .bg(theme::accent())
        .text_color(theme::primary_fg())
        .text_size(px(12.))
        .cursor(CursorStyle::PointingHand)
        .hover(|s| s.bg(theme::accent_hover()))
}
