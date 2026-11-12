use std::borrow::Cow;

use gpui::{AssetSource, SharedString};

pub struct ToolbarAssets;

impl AssetSource for ToolbarAssets {
    fn load(&self, path: &str) -> gpui::Result<Option<Cow<'static, [u8]>>> {
        Ok(match path {
            "toolbar/select.svg" => Some(Cow::Borrowed(include_bytes!(
                "../../../../assets/toolbar/select.svg"
            ))),
            "toolbar/rect.svg" => Some(Cow::Borrowed(include_bytes!(
                "../../../../assets/toolbar/rect.svg"
            ))),
            "toolbar/ellipse.svg" => Some(Cow::Borrowed(include_bytes!(
                "../../../../assets/toolbar/ellipse.svg"
            ))),
            "toolbar/shape.svg" => Some(Cow::Borrowed(include_bytes!(
                "../../../../assets/toolbar/shape.svg"
            ))),
            "toolbar/arrowGroup.svg" => Some(Cow::Borrowed(include_bytes!(
                "../../../../assets/toolbar/arrowGroup.svg"
            ))),
            "toolbar/number.svg" => Some(Cow::Borrowed(include_bytes!(
                "../../../../assets/toolbar/number.svg"
            ))),
            "toolbar/lineSolid.svg" => Some(Cow::Borrowed(include_bytes!(
                "../../../../assets/toolbar/lineSolid.svg"
            ))),
            "toolbar/lineDashed.svg" => Some(Cow::Borrowed(include_bytes!(
                "../../../../assets/toolbar/lineDashed.svg"
            ))),
            "toolbar/lineCloud.svg" => Some(Cow::Borrowed(include_bytes!(
                "../../../../assets/toolbar/lineCloud.svg"
            ))),
            "toolbar/polylineArrow.svg" => Some(Cow::Borrowed(include_bytes!(
                "../../../../assets/toolbar/polylineArrow.svg"
            ))),
            "toolbar/curveArrow.svg" => Some(Cow::Borrowed(include_bytes!(
                "../../../../assets/toolbar/curveArrow.svg"
            ))),
            "toolbar/arrow.svg" => Some(Cow::Borrowed(include_bytes!(
                "../../../../assets/toolbar/arrow.svg"
            ))),
            "toolbar/pen.svg" => Some(Cow::Borrowed(include_bytes!(
                "../../../../assets/toolbar/pen.svg"
            ))),
            "toolbar/mosaic.svg" => Some(Cow::Borrowed(include_bytes!(
                "../../../../assets/toolbar/mosaic.svg"
            ))),
            "toolbar/text.svg" => Some(Cow::Borrowed(include_bytes!(
                "../../../../assets/toolbar/text.svg"
            ))),
            "toolbar/undo.svg" => Some(Cow::Borrowed(include_bytes!(
                "../../../../assets/toolbar/undo.svg"
            ))),
            "toolbar/redo.svg" => Some(Cow::Borrowed(include_bytes!(
                "../../../../assets/toolbar/redo.svg"
            ))),
            "toolbar/clear.svg" => Some(Cow::Borrowed(include_bytes!(
                "../../../../assets/toolbar/clear.svg"
            ))),
            "toolbar/ocr.svg" => Some(Cow::Borrowed(include_bytes!(
                "../../../../assets/toolbar/ocr.svg"
            ))),
            "toolbar/translate.svg" => Some(Cow::Borrowed(include_bytes!(
                "../../../../assets/toolbar/translate.svg"
            ))),
            "toolbar/longCapture.svg" => Some(Cow::Borrowed(include_bytes!(
                "../../../../assets/toolbar/longCapture.svg"
            ))),
            "toolbar/pin.svg" => Some(Cow::Borrowed(include_bytes!(
                "../../../../assets/toolbar/pin.svg"
            ))),
            "toolbar/save.svg" => Some(Cow::Borrowed(include_bytes!(
                "../../../../assets/toolbar/save.svg"
            ))),
            "toolbar/complete.svg" => Some(Cow::Borrowed(include_bytes!(
                "../../../../assets/toolbar/complete.svg"
            ))),
            "toolbar/cancel.svg" => Some(Cow::Borrowed(include_bytes!(
                "../../../../assets/toolbar/cancel.svg"
            ))),
            "toolbar/propStroke.svg" => Some(Cow::Borrowed(include_bytes!(
                "../../../../assets/toolbar/propStroke.svg"
            ))),
            "toolbar/propFill.svg" => Some(Cow::Borrowed(include_bytes!(
                "../../../../assets/toolbar/propFill.svg"
            ))),
            "toolbar/propWidth1.svg" => Some(Cow::Borrowed(include_bytes!(
                "../../../../assets/toolbar/propWidth1.svg"
            ))),
            "toolbar/propWidth2.svg" => Some(Cow::Borrowed(include_bytes!(
                "../../../../assets/toolbar/propWidth2.svg"
            ))),
            "toolbar/propWidth4.svg" => Some(Cow::Borrowed(include_bytes!(
                "../../../../assets/toolbar/propWidth4.svg"
            ))),
            "toolbar/propWidth8.svg" => Some(Cow::Borrowed(include_bytes!(
                "../../../../assets/toolbar/propWidth8.svg"
            ))),
            "toolbar/propBadgeFilled.svg" => Some(Cow::Borrowed(include_bytes!(
                "../../../../assets/toolbar/propBadgeFilled.svg"
            ))),
            "toolbar/propBadgeHollow.svg" => Some(Cow::Borrowed(include_bytes!(
                "../../../../assets/toolbar/propBadgeHollow.svg"
            ))),
            "toolbar/propBadgeSquare.svg" => Some(Cow::Borrowed(include_bytes!(
                "../../../../assets/toolbar/propBadgeSquare.svg"
            ))),
            "toolbar/propArrowStart.svg" => Some(Cow::Borrowed(include_bytes!(
                "../../../../assets/toolbar/propArrowStart.svg"
            ))),
            "toolbar/propArrowEnd.svg" => Some(Cow::Borrowed(include_bytes!(
                "../../../../assets/toolbar/propArrowEnd.svg"
            ))),
            "toolbar/propArrowBoth.svg" => Some(Cow::Borrowed(include_bytes!(
                "../../../../assets/toolbar/propArrowBoth.svg"
            ))),
            "toolbar/propArrowNone.svg" => Some(Cow::Borrowed(include_bytes!(
                "../../../../assets/toolbar/propArrowNone.svg"
            ))),
            "toolbar/propHeadFilled.svg" => Some(Cow::Borrowed(include_bytes!(
                "../../../../assets/toolbar/propHeadFilled.svg"
            ))),
            "toolbar/propHeadOpen.svg" => Some(Cow::Borrowed(include_bytes!(
                "../../../../assets/toolbar/propHeadOpen.svg"
            ))),
            "toolbar/propHeadBar.svg" => Some(Cow::Borrowed(include_bytes!(
                "../../../../assets/toolbar/propHeadBar.svg"
            ))),
            "toolbar/propAlphaMinus.svg" => Some(Cow::Borrowed(include_bytes!(
                "../../../../assets/toolbar/propAlphaMinus.svg"
            ))),
            "toolbar/propAlphaPlus.svg" => Some(Cow::Borrowed(include_bytes!(
                "../../../../assets/toolbar/propAlphaPlus.svg"
            ))),
            "settings/screenshot.svg" => Some(Cow::Borrowed(include_bytes!(
                "../../../../assets/settings/screenshot.svg"
            ))),
            "settings/ocr.svg" => Some(Cow::Borrowed(include_bytes!(
                "../../../../assets/settings/ocr.svg"
            ))),
            "settings/hotkey.svg" => Some(Cow::Borrowed(include_bytes!(
                "../../../../assets/settings/hotkey.svg"
            ))),
            "settings/general.svg" => Some(Cow::Borrowed(include_bytes!(
                "../../../../assets/settings/general.svg"
            ))),
            "settings/copy.svg" => Some(Cow::Borrowed(include_bytes!(
                "../../../../assets/settings/copy.svg"
            ))),
            "settings/monitor.svg" => Some(Cow::Borrowed(include_bytes!(
                "../../../../assets/settings/monitor.svg"
            ))),
            "settings/check.svg" => Some(Cow::Borrowed(include_bytes!(
                "../../../../assets/settings/check.svg"
            ))),
            "settings/info.svg" => Some(Cow::Borrowed(include_bytes!(
                "../../../../assets/settings/info.svg"
            ))),
            "settings/folder.svg" => Some(Cow::Borrowed(include_bytes!(
                "../../../../assets/settings/folder.svg"
            ))),
            "settings/chevron.svg" => Some(Cow::Borrowed(include_bytes!(
                "../../../../assets/settings/chevron.svg"
            ))),
            "settings/close.svg" => Some(Cow::Borrowed(include_bytes!(
                "../../../../assets/settings/close.svg"
            ))),
            "settings/record.svg" => Some(Cow::Borrowed(include_bytes!(
                "../../../../assets/settings/record.svg"
            ))),
            "settings/history.svg" => Some(Cow::Borrowed(include_bytes!(
                "../../../../assets/settings/history.svg"
            ))),
            "settings/output.svg" => Some(Cow::Borrowed(include_bytes!(
                "../../../../assets/settings/output.svg"
            ))),
            "settings/appearance.svg" => Some(Cow::Borrowed(include_bytes!(
                "../../../../assets/settings/appearance.svg"
            ))),
            "settings/resources.svg" => Some(Cow::Borrowed(include_bytes!(
                "../../../../assets/settings/resources.svg"
            ))),
            "settings/data.svg" => Some(Cow::Borrowed(include_bytes!(
                "../../../../assets/settings/data.svg"
            ))),
            "settings/search.svg" => Some(Cow::Borrowed(include_bytes!(
                "../../../../assets/settings/search.svg"
            ))),
            _ => None,
        })
    }

    fn list(&self, path: &str) -> gpui::Result<Vec<SharedString>> {
        if path.trim_end_matches('/') == "toolbar" {
            Ok(super::toolbar::TOOLBAR_ICONS
                .iter()
                .map(|p| SharedString::from(*p))
                .collect())
        } else {
            Ok(Vec::new())
        }
    }
}

/// The application's registered asset source.
///
/// `gpui-component` renders its `Icon`s from `icons/*.svg` in the bundled
/// `gpui-kit-assets` bundle, while FastPin ships its own toolbar and settings
/// SVGs in `ToolbarAssets`. This source asks the bundle first and falls back
/// to the app assets, so both path spaces resolve without renaming either.
pub struct AppAssets;

impl AssetSource for AppAssets {
    fn load(&self, path: &str) -> gpui::Result<Option<Cow<'static, [u8]>>> {
        // The bundle reports a miss as `Err`, not `Ok(None)`; either way the
        // app's own paths are the fallback.
        if let Ok(Some(bytes)) = gpui_kit::assets::Assets.load(path) {
            return Ok(Some(bytes));
        }
        ToolbarAssets.load(path)
    }

    fn list(&self, path: &str) -> gpui::Result<Vec<SharedString>> {
        let mut names = gpui_kit::assets::Assets.list(path).unwrap_or_default();
        names.extend(ToolbarAssets.list(path)?);
        Ok(names)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui_kit::assets::IconName;

    #[test]
    fn app_assets_serve_both_bundles() {
        // gpui-component's icons resolve through the kit bundle.
        for name in [
            IconName::Search,
            IconName::WindowMinimize,
            IconName::WindowMaximize,
            IconName::WindowRestore,
            IconName::WindowClose,
        ] {
            let icon = name.path();
            assert!(
                AppAssets.load(&icon).ok().flatten().is_some(),
                "missing component icon {icon}"
            );
        }
        // FastPin's own toolbar SVG still resolves through the fallback.
        assert!(AppAssets
            .load("toolbar/select.svg")
            .ok()
            .flatten()
            .is_some());
        assert!(AppAssets
            .load("settings/close.svg")
            .ok()
            .flatten()
            .is_some());
    }

    #[test]
    fn app_assets_list_both_path_spaces() {
        let icons = AppAssets.list("icons/").unwrap();
        assert!(!icons.is_empty());
        assert!(icons.iter().all(|p| p.starts_with("icons/")));
        let toolbar = AppAssets.list("toolbar").unwrap();
        assert_eq!(toolbar.len(), super::super::toolbar::TOOLBAR_ICONS.len());
    }

    #[test]
    fn app_assets_miss_returns_none() {
        assert!(AppAssets.load("no/such/asset.svg").ok().flatten().is_none());
    }
}
