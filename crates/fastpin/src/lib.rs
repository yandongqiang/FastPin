// The gpui-kit facade re-exports GPUI verbatim (`pub use ::gpui::*`), and the
// gpui-kit docs sanction aliasing the kit as `gpui` so existing `gpui::` paths
// keep resolving. This is a crate alias, not a glob import.
extern crate gpui_kit as gpui;

pub mod app;
pub mod black;
pub mod brand_icon;
pub mod capture;
pub mod codec;
pub mod config;
pub mod error;
pub mod exclusive;
pub mod geom;
pub mod gpui_bridge;
pub mod hdr;
pub mod hotkey;
pub mod longcapture;
pub mod longworker;
mod migrate;
pub mod ocr;
pub mod overlay;
pub mod perf;
pub mod pin;
pub mod pin_edit;
pub mod pin_snap;
pub mod platform;
pub mod settings;
pub mod translate;
pub mod update;

pub use app::{once_capture, run};
pub use error::{Error, Result};

/// Product name shown to users (window titles, brand, about page, installer).
/// Paths and storage identifiers use the same name; `migrate` moves the data a
/// pre-rebrand build left behind on first start. Every internal identifier —
/// crate name, window classes, `WM_FASTPIN_*` messages, the settings text-input
/// element id and the GitHub repository — now uses the FastPin name too.
pub const APP_NAME: &str = "FastPin";
