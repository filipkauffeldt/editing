//! Icon assets: the kit's default set plus the few extra Lucide icons the
//! app uses (ISC licensed, same source as the kit's own icons).

use std::borrow::Cow;

use gpui_kit::{AssetSource, Result, SharedString};

/// Path of the lock icon, for `Icon::empty().path(LOCK_ICON)`.
pub const LOCK_ICON: &str = "icons/lock.svg";

/// Paths of the rotate icons used by the crop tool.
pub const ROTATE_LEFT_ICON: &str = "icons/rotate-ccw.svg";
pub const ROTATE_RIGHT_ICON: &str = "icons/rotate-cw.svg";

const LOCK_SVG: &str = include_str!("../assets/icons/lock.svg");
const ROTATE_LEFT_SVG: &str = include_str!("../assets/icons/rotate-ccw.svg");
const ROTATE_RIGHT_SVG: &str = include_str!("../assets/icons/rotate-cw.svg");

/// The kit's assets with the app's extra icons in front.
pub struct AppAssets;

impl AssetSource for AppAssets {
    fn load(&self, path: &str) -> Result<Option<Cow<'static, [u8]>>> {
        let svg = match path {
            LOCK_ICON => Some(LOCK_SVG),
            ROTATE_LEFT_ICON => Some(ROTATE_LEFT_SVG),
            ROTATE_RIGHT_ICON => Some(ROTATE_RIGHT_SVG),
            _ => None,
        };
        if let Some(svg) = svg {
            return Ok(Some(Cow::Borrowed(svg.as_bytes())));
        }
        gpui_kit::assets::Assets.load(path)
    }

    fn list(&self, path: &str) -> Result<Vec<SharedString>> {
        gpui_kit::assets::Assets.list(path)
    }
}

#[cfg(test)]
mod tests {
    use super::{AppAssets, LOCK_ICON, ROTATE_LEFT_ICON, ROTATE_RIGHT_ICON};
    use gpui_kit::AssetSource as _;

    #[test]
    fn the_app_icons_load_and_the_kit_icons_still_do() {
        for icon in [LOCK_ICON, ROTATE_LEFT_ICON, ROTATE_RIGHT_ICON] {
            let svg = AppAssets.load(icon).unwrap().unwrap();
            assert!(
                std::str::from_utf8(&svg).unwrap().contains("<svg"),
                "{icon}"
            );
        }
        assert!(AppAssets.load("icons/check.svg").unwrap().is_some());
    }
}
