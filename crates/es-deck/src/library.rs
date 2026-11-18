//! Saved items and request history, shared by every tab: both live in files, so one copy in memory
//! keeps two tabs from overwriting each other. Tabs observe it and refresh their own lists.

use std::path::PathBuf;

use es_client::history::{self, Entry};
use es_client::saved::{self, Kind, Saved};
use gpui_kit::{AppContext as _, Context, EventEmitter};

pub struct Library {
    saved: Vec<Saved>,
    saved_path: PathBuf,
    history: Vec<Entry>,
    history_path: PathBuf,
    draft_path: PathBuf,
}

/// What changed, so a tab refreshes only the list that depends on it.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Changed {
    Saved,
    History,
}

impl EventEmitter<Changed> for Library {}

impl Library {
    /// `dir_file` is any file in the config directory; the two files live next to it.
    pub fn load(dir_file: &std::path::Path) -> Self {
        let saved_path = dir_file.with_file_name("saved.json");
        let history_path = dir_file.with_file_name("history.json");
        // An unreadable file was moved aside by `load`; starting empty cannot overwrite it.
        let mut saved: Vec<Saved> = history::load(&saved_path).unwrap_or_default();
        saved::rename_auto_console(&mut saved);
        Self {
            saved,
            history: history::load(&history_path).unwrap_or_default(),
            saved_path,
            history_path,
            draft_path: dir_file.with_file_name("draft.txt"),
        }
    }

    /// The editor text left by the previous run, empty when there is none.
    pub fn draft(&self) -> String {
        std::fs::read_to_string(&self.draft_path).unwrap_or_default()
    }

    /// Writes the editor text off the UI thread so the next run can restore it.
    pub fn save_draft(&self, text: String, cx: &mut Context<Self>) {
        let path = self.draft_path.clone();
        // A failed write only loses this session's draft.
        cx.background_spawn(async move {
            let _ = std::fs::write(path, text);
        })
        .detach();
    }

    /// Empty, with files under the temp directory, so a test never reads or writes real ones.
    #[cfg(test)]
    pub fn blank() -> Self {
        let dir = std::env::temp_dir().join("esdeck-blank-library");
        Self {
            saved: Vec::new(),
            saved_path: dir.join("saved.json"),
            history: Vec::new(),
            history_path: dir.join("history.json"),
            draft_path: dir.join("draft.txt"),
        }
    }

    /// The saved items of one connection and kind, in saving order.
    /// The text of one connection's console called `name`.
    pub fn console(&self, connection: &str, name: &str) -> Option<String> {
        self.saved(connection, Kind::Console)
            .find(|s| s.name == name)
            .map(|s| s.text.clone())
    }

    pub fn saved(&self, connection: &str, kind: Kind) -> impl Iterator<Item = &Saved> {
        let connection = connection.to_string();
        self.saved
            .iter()
            .filter(move |s| s.connection == connection && s.kind == kind)
    }

    /// The history of one connection, in recording order.
    pub fn history(&self, connection: &str) -> Vec<Entry> {
        self.history
            .iter()
            .filter(|e| e.connection == connection)
            .cloned()
            .collect()
    }

    /// Returns false when `item` was refused: empty, or it may hold credentials.
    pub fn add_saved(&mut self, item: Saved, cx: &mut Context<Self>) -> bool {
        let added = saved::add(&mut self.saved, item);
        if added {
            self.persist_saved(cx);
        }
        added
    }

    /// Keeps the connection's console called `name` equal to the editor text.
    pub fn set_console(
        &mut self,
        connection: &str,
        name: &str,
        text: &str,
        cx: &mut Context<Self>,
    ) {
        let at = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        if saved::set_console(&mut self.saved, connection, name, text, at) {
            self.persist_saved(cx);
        }
    }

    pub fn remove_saved(
        &mut self,
        connection: &str,
        kind: Kind,
        name: &str,
        cx: &mut Context<Self>,
    ) {
        if saved::remove(&mut self.saved, connection, kind, name) {
            self.persist_saved(cx);
        }
    }

    pub fn rename_saved(
        &mut self,
        connection: &str,
        kind: Kind,
        old: &str,
        name: &str,
        cx: &mut Context<Self>,
    ) -> bool {
        let renamed = saved::rename(&mut self.saved, connection, kind, old, name);
        if renamed {
            self.persist_saved(cx);
        }
        renamed
    }

    pub fn record(&mut self, entry: Entry, cx: &mut Context<Self>) {
        if history::record(&mut self.history, entry) {
            self.persist_history(cx);
        }
    }

    pub fn clear_history(&mut self, connection: &str, cx: &mut Context<Self>) {
        self.history.retain(|e| e.connection != connection);
        self.persist_history(cx);
    }

    /// Drops one recorded request and persists the file.
    pub fn remove_history(&mut self, connection: &str, entry: &Entry, cx: &mut Context<Self>) {
        // The entry carries its connection, but the check keeps a stale row from another
        // connection's history from deleting a matching request here.
        self.history
            .retain(|e| !(e.connection == connection && e == entry));
        self.persist_history(cx);
    }

    fn persist_saved(&mut self, cx: &mut Context<Self>) {
        let (path, items) = (self.saved_path.clone(), self.saved.clone());
        // A failed write only loses this session's saves.
        cx.background_spawn(async move {
            let _ = history::save(&path, &items);
        })
        .detach();
        cx.emit(Changed::Saved);
    }

    fn persist_history(&mut self, cx: &mut Context<Self>) {
        let (path, entries) = (self.history_path.clone(), self.history.clone());
        // A failed write only loses this session's history; the request itself already ran.
        cx.background_spawn(async move {
            let _ = history::save(&path, &entries);
        })
        .detach();
        cx.emit(Changed::History);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui_kit::TestAppContext;

    fn item(connection: &str, text: &str) -> Saved {
        Saved {
            connection: connection.into(),
            kind: Kind::Query,
            name: saved::name_of(text),
            text: text.into(),
            at: 0,
        }
    }

    #[gpui_kit::test]
    fn saved_items_are_scoped_by_connection_and_refuse_credentials(cx: &mut TestAppContext) {
        let library = cx.new(|_| Library::blank());
        let changes = std::rc::Rc::new(std::cell::Cell::new(0));
        let seen = changes.clone();
        let _sub = cx
            .update(|cx| cx.subscribe(&library, move |_, _: &Changed, _| seen.set(seen.get() + 1)));
        let added = |cx: &mut TestAppContext, c: &str, t: &str| {
            library.update(cx, |l, cx| l.add_saved(item(c, t), cx))
        };
        assert!(added(cx, "a", "GET x/_search"));
        assert!(added(cx, "b", "GET y/_search"));
        assert!(
            !added(cx, "a", "PUT _security/user/u"),
            "credentials are never kept"
        );
        library.read_with(cx, |l, _| {
            assert_eq!(l.saved("a", Kind::Query).count(), 1);
            assert_eq!(l.saved("b", Kind::Query).count(), 1);
        });
        // Two accepted saves emitted; the refused one did not.
        assert_eq!(changes.get(), 2);
    }

    fn entry(connection: &str, path: &str) -> Entry {
        Entry {
            connection: connection.into(),
            method: "GET".into(),
            path: path.into(),
            body: None,
            at: 0,
        }
    }

    #[gpui_kit::test]
    fn one_history_entry_can_be_removed(cx: &mut TestAppContext) {
        let library = cx.new(|_| Library::blank());
        let (keep, drop, other) = (
            entry("a", "a/_search"),
            entry("a", "b/_doc/1"),
            entry("b", "x/_search"),
        );
        for e in [keep.clone(), drop.clone(), other.clone()] {
            library.update(cx, |l, cx| l.record(e, cx));
        }
        library.update(cx, |l, cx| l.remove_history("a", &drop, cx));
        library.read_with(cx, |l, _| {
            let kept = l.history("a");
            assert_eq!(kept.len(), 1);
            assert_eq!(&kept[0], &keep);
            // Removing from one connection leaves every other connection's history alone.
            let untouched = l.history("b");
            assert_eq!(untouched.len(), 1);
            assert_eq!(&untouched[0], &other);
        });
        // The stale row of another connection matches nothing.
        library.update(cx, |l, cx| l.remove_history("b", &keep, cx));
        library.read_with(cx, |l, _| assert_eq!(&l.history("a")[0], &keep));
    }
}
