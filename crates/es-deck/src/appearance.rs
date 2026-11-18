//! The window appearance: light, dark, or whatever the system uses. The choice is remembered in
//! `appearance.json` next to the other config files.

use std::path::Path;
use std::sync::Arc;

use gpui_kit::component::scroll::ScrollbarMode;
use gpui_kit::component::{Theme, ThemeMode};
use gpui_kit::{App, Window, px};
use serde::{Deserialize, Serialize};

use crate::surface::Surfaces;

/// What the user chose. `System` follows the OS now and whenever it changes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Pref {
    #[default]
    System,
    Light,
    Dark,
}

impl Pref {
    /// The value the choice is stored and offered under, in the file and in Settings.
    pub fn key(self) -> &'static str {
        match self {
            Self::System => "system",
            Self::Light => "light",
            Self::Dark => "dark",
        }
    }

    /// The choice `key` names, or `None` for anything else.
    pub fn from_key(key: &str) -> Option<Self> {
        match key {
            "system" => Some(Self::System),
            "light" => Some(Self::Light),
            "dark" => Some(Self::Dark),
            _ => None,
        }
    }

    /// How the choice reads to the user.
    pub fn label(self) -> &'static str {
        match self {
            Self::System => "Follow system",
            Self::Light => "Light",
            Self::Dark => "Dark",
        }
    }

    /// A missing or unreadable file leaves the default; the file holds only this one choice, so a
    /// failed read must not stop the application.
    pub fn load(path: &Path) -> Self {
        std::fs::read_to_string(path)
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default()
    }

    /// Writes through a temporary file so a crash cannot leave half a choice behind. Callers run
    /// this off the UI thread; a failed write only loses the choice for the next run.
    pub fn save(self, path: &Path) {
        let Ok(text) = serde_json::to_string(&self) else {
            return;
        };
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let tmp = path.with_extension("json.tmp");
        if std::fs::write(&tmp, text).is_ok() {
            let _ = std::fs::rename(&tmp, path);
        }
    }
}

/// Applies `pref`, then the look the rest of the app expects: the radii the kit resets on every
/// theme load, the island surfaces, and scrollbars that show a long response has more.
pub fn apply(pref: Pref, window: Option<&mut Window>, cx: &mut App) {
    match pref {
        Pref::System => Theme::sync_system_appearance(window, cx),
        Pref::Light => Theme::change(ThemeMode::Light, window, cx),
        Pref::Dark => Theme::change(ThemeMode::Dark, window, cx),
    }
    Theme::set_scrollbar_mode(ScrollbarMode::Always, cx);
    Theme::update(cx, |theme| {
        // Controls use the small radius; islands and dialogs use the large one. Sharing a single
        // 8px radius made a row, a tab and a whole panel read as the same kind of object.
        theme.radius = px(6.0);
        theme.radius_lg = px(10.0);
        let surfaces = Surfaces::new(theme);
        // The kit paints the title bar, status bar, table and popovers from these tokens. Point
        // them at the same surfaces the hand-built islands use, or the chrome drifts from the
        // cards it sits next to.
        theme.tokens.title_bar = surfaces.ground.into();
        theme.tokens.status_bar = surfaces.ground.into();
        theme.tokens.status_bar_border = surfaces.ground.into();
        theme.tokens.table = surfaces.panel.into();
        theme.tokens.table_head = surfaces.header.into();
        theme.tokens.table_head_foreground = theme.muted_foreground.into();
        theme.tokens.popover = surfaces.elevated.into();
        // The kit paints a list row's hover and selection itself. Point both at the island tones
        // so a selected tree row is one fill rather than the kit's blue plus the tree's own.
        theme.list_active = surfaces.selected;
        theme.list_hover = surfaces.hover;
        theme.tokens.list_active = surfaces.selected.into();
        theme.tokens.list_hover = surfaces.hover.into();
        // The editor paints its own background from the highlight theme. If it keeps the theme
        // background, the editor becomes a dark well inside a lighter panel instead of sharing
        // the panel's surface.
        let highlight = Arc::make_mut(&mut theme.highlight_theme);
        highlight.style.editor_background = Some(surfaces.panel);
        highlight.style.editor_gutter_background = Some(surfaces.panel);
        highlight.style.editor_active_line = Some(surfaces.header);
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir() -> std::path::PathBuf {
        std::env::temp_dir().join(format!("esdeck-appearance-{}", std::process::id()))
    }

    #[test]
    fn the_choice_round_trips_and_a_bad_file_means_system() {
        let dir = dir();
        let path = dir.join("appearance.json");
        assert_eq!(Pref::load(&path), Pref::System, "missing file");
        for pref in [Pref::Light, Pref::Dark, Pref::System] {
            pref.save(&path);
            assert_eq!(Pref::load(&path), pref);
        }
        std::fs::write(&path, "blue").unwrap();
        assert_eq!(Pref::load(&path), Pref::System, "unreadable choice");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn the_settings_key_round_trips() {
        for pref in [Pref::System, Pref::Light, Pref::Dark] {
            assert_eq!(Pref::from_key(pref.key()), Some(pref));
        }
        assert_eq!(Pref::from_key("blue"), None);
    }
}
