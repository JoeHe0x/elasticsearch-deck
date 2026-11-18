//! Icon assets: the kit's default component icons plus the few extra Lucide icons the app uses.

use std::borrow::Cow;

use gpui_kit::assets::{Assets, icon_assets};
use gpui_kit::{AssetSource, SharedString};

icon_assets!(
    ExtraIcons,
    [
        Bookmark,
        Braces,
        Brackets,
        CircleDot,
        CircleQuestionMark,
        Clock,
        Copy,
        Database,
        Download,
        Hash,
        FileText,
        Folder,
        FolderOpen,
        Link2,
        ListTree,
        Lock,
        LockOpen,
        PanelLeft,
        Pencil,
        Plus,
        Quote,
        RefreshCw,
        Save,
        Server,
        SquareTerminal,
        Table2,
        ToggleLeft,
        Trash,
        Unplug
    ]
);

/// Extras first, then the kit's defaults, as the kit's `extra_assets` example composes them.
pub struct AppAssets;

impl AssetSource for AppAssets {
    fn load(&self, path: &str) -> gpui_kit::Result<Option<Cow<'static, [u8]>>> {
        match ExtraIcons.load(path)? {
            Some(bytes) => Ok(Some(bytes)),
            None => Assets.load(path),
        }
    }

    fn list(&self, path: &str) -> gpui_kit::Result<Vec<SharedString>> {
        let mut paths = Assets.list(path)?;
        paths.extend(ExtraIcons.list(path)?);
        paths.sort();
        paths.dedup();
        Ok(paths)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui_kit::assets::IconName;

    /// An icon the app draws itself has to be either a kit default or listed in `ExtraIcons`;
    /// otherwise the asset source returns nothing and the control renders as a blank button.
    #[test]
    fn the_icons_the_console_draws_itself_are_bundled() {
        for name in [
            IconName::ChevronDown,
            IconName::ChevronUp,
            IconName::Maximize,
            IconName::Minimize,
        ] {
            let path = name.path();
            assert!(
                AppAssets.load(path.as_ref()).unwrap().is_some(),
                "{path} is not in the asset source"
            );
        }
    }
}
