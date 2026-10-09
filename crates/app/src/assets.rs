//! Icon assets: the kit's default set plus the few extra Lucide icons the
//! app uses (ISC licensed, same source as the kit's own icons).

use std::borrow::Cow;

use gpui_kit::{AssetSource, Result, SharedString};

/// Path of the lock icon, for `Icon::empty().path(LOCK_ICON)`.
pub const LOCK_ICON: &str = "icons/lock.svg";

const LOCK_SVG: &str = include_str!("../assets/icons/lock.svg");

/// The kit's assets with the app's extra icons in front.
pub struct AppAssets;

impl AssetSource for AppAssets {
    fn load(&self, path: &str) -> Result<Option<Cow<'static, [u8]>>> {
        if path == LOCK_ICON {
            return Ok(Some(Cow::Borrowed(LOCK_SVG.as_bytes())));
        }
        gpui_kit::assets::Assets.load(path)
    }

    fn list(&self, path: &str) -> Result<Vec<SharedString>> {
        gpui_kit::assets::Assets.list(path)
    }
}

#[cfg(test)]
mod tests {
    use super::{AppAssets, LOCK_ICON};
    use gpui_kit::AssetSource as _;

    #[test]
    fn the_lock_icon_loads_and_the_kit_icons_still_do() {
        let lock = AppAssets.load(LOCK_ICON).unwrap().unwrap();
        assert!(std::str::from_utf8(&lock).unwrap().contains("<svg"));
        assert!(AppAssets.load("icons/check.svg").unwrap().is_some());
    }
}
