//! The request history of the current connection, newest first, as a searchable list.

use es_client::history::Entry;
use gpui_kit::component::IndexPath;
use gpui_kit::component::list::{ListDelegate, ListItem, ListState};
use gpui_kit::{App, Context, ParentElement as _, SharedString, Styled as _, Task, Window, div};

use crate::connect::row_id;

#[derive(Default)]
pub struct HistoryList {
    all: Vec<Entry>,
    query: String,
    shown: Vec<Entry>,
    selected: Option<IndexPath>,
}

impl HistoryList {
    /// `entries` are in recording order; the list shows them newest first.
    pub fn set(&mut self, entries: impl Iterator<Item = Entry>) {
        self.all = entries.collect();
        self.all.reverse();
        self.refilter();
    }

    pub fn entry(&self, ix: IndexPath) -> Option<&Entry> {
        self.shown.get(ix.row)
    }

    fn refilter(&mut self) {
        let q = self.query.trim().to_lowercase();
        self.shown = self
            .all
            .iter()
            .filter(|e| {
                format!("{} {}", e.method, e.path)
                    .to_lowercase()
                    .contains(&q)
            })
            .cloned()
            .collect();
        self.selected = None;
    }
}

impl ListDelegate for HistoryList {
    type Item = ListItem;

    fn perform_search(
        &mut self,
        query: &str,
        _: &mut Window,
        cx: &mut Context<ListState<Self>>,
    ) -> Task<()> {
        self.query = query.to_string();
        self.refilter();
        cx.notify();
        Task::ready(())
    }

    fn items_count(&self, _: usize, _: &App) -> usize {
        self.shown.len()
    }

    fn render_item(
        &mut self,
        ix: IndexPath,
        _: &mut Window,
        _: &mut Context<ListState<Self>>,
    ) -> Option<Self::Item> {
        let e = self.shown.get(ix.row)?;
        let key = format!("{}{}{}{:?}", e.at, e.method, e.path, e.body);
        Some(
            ListItem::new(("history", row_id(&key)))
                .selected(self.selected == Some(ix))
                .child(
                    div()
                        .w_full()
                        .truncate()
                        .child(SharedString::from(format!("{} {}", e.method, e.path))),
                ),
        )
    }

    fn set_selected_index(
        &mut self,
        ix: Option<IndexPath>,
        _: &mut Window,
        cx: &mut Context<ListState<Self>>,
    ) {
        self.selected = ix;
        cx.notify();
    }
}
