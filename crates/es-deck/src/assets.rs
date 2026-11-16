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
