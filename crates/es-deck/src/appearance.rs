//! The window appearance: light, dark, or whatever the system uses. The choice is remembered in
//! `appearance.json` next to the other config files.

use std::path::Path;

use gpui_kit::component::scroll::ScrollbarMode;
use gpui_kit::component::{Theme, ThemeMode};
use gpui_kit::{App, Window, px};
use serde::{Deserialize, Serialize};

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
/// theme load, and scrollbars that show a long response has more.
pub fn apply(pref: Pref, window: Option<&mut Window>, cx: &mut App) {
    match pref {
        Pref::System => Theme::sync_system_appearance(window, cx),
        Pref::Light => Theme::change(ThemeMode::Light, window, cx),
        Pref::Dark => Theme::change(ThemeMode::Dark, window, cx),
    }
    Theme::set_scrollbar_mode(ScrollbarMode::Always, cx);
    Theme::update(cx, |theme| {
        theme.radius = px(8.0);
        theme.radius_lg = px(12.0);
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
}
