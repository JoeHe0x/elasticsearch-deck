//! The request history of the current connection, newest first, as a searchable list.

use std::rc::Rc;
use std::time::{SystemTime, UNIX_EPOCH};

use es_client::history::Entry;
use es_dsl_core::Token;
use gpui_kit::assets::IconName;
use gpui_kit::component::ActiveTheme as _;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::empty::{
    Empty, EmptyDescription, EmptyHeader, EmptyMedia, EmptyMediaVariant, EmptyTitle,
};
use gpui_kit::component::list::{ListDelegate, ListItem, ListState};
use gpui_kit::component::{Icon, IndexPath, Sizable as _};
// Available in every build: it observes the row for UI tests when the kit's `test-support`
// feature is on, and returns the element unchanged otherwise.
use gpui_kit::base::TestSupportExt as _;
use gpui_kit::{
    App, Context, FontWeight, InteractiveElement as _, IntoElement, ParentElement as _,
    SharedString, StatefulInteractiveElement as _, Styled as _, Task, Window, div, rems,
};

use crate::connect::row_id;
use crate::paint;

/// What pressing a row's delete button does with the entry.
type DeleteHandler = Rc<dyn Fn(&Entry, &mut App)>;

#[derive(Default)]
pub struct HistoryList {
    all: Vec<Entry>,
    query: String,
    shown: Vec<Entry>,
    selected: Option<IndexPath>,
    /// The row the pointer is over, which is the only one that shows its delete button.
    hovered: Option<IndexPath>,
    on_delete: Option<DeleteHandler>,
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

    /// Sets what the row's delete button does. The list does not touch the history file itself:
    /// the file belongs to the library, and only the console knows which connection is shown.
    pub fn set_on_delete(&mut self, f: impl Fn(&Entry, &mut App) + 'static) {
        self.on_delete = Some(Rc::new(f));
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
        self.hovered = None;
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
        cx: &mut Context<ListState<Self>>,
    ) -> Option<Self::Item> {
        let theme = cx.theme();
        let (muted, radius) = (theme.muted_foreground, theme.radius);
        let mono = theme.mono_font_family.clone();
        // The same accent the editor gives a method, so a row reads as the request it reopens.
        let method_color = paint::color(Token::Method, theme.is_dark());
        let e = self.shown.get(ix.row)?;
        let key = format!("{}{}{}{:?}", e.at, e.method, e.path, e.body);
        let (method, path) = (e.method.clone(), e.path.clone());
        let age = age_label(e.at, now_secs());
        let label = format!("{method} {path}");
        let row = row_id(&key);
        let hovered = self.hovered == Some(ix);
        // The slot below is kept whether or not the button is in it, so entering or leaving a row
        // does not move the age and the path under the pointer.
        let delete = (hovered && self.on_delete.is_some()).then(|| {
            let entry = e.clone();
            let on_delete = self.on_delete.clone();
            Button::new(("history-delete", row))
                .ghost()
                .small()
                .icon(IconName::Trash)
                .tooltip("Delete this request")
                .accessibility_label("Delete this request")
                .on_click(move |_, _, cx| {
                    // The row underneath takes any click as "open this request".
                    cx.stop_propagation();
                    if let Some(f) = &on_delete {
                        f(&entry, cx);
                    }
                })
                .into_any_element()
        });
        let list = cx.entity();
        Some(
            ListItem::new(("history", row))
                .selected(self.selected == Some(ix))
                .p_0()
                .rounded(radius)
                .accessibility_label(label)
                .child(
                    div()
                        .id(("history-row", row))
                        .test_support()
                        .w_full()
                        .h(rems(1.75))
                        .px_2()
                        .flex()
                        .items_center()
                        .gap_2()
                        .min_w_0()
                        .on_hover(move |&is_hovered, _, cx| {
                            list.update(cx, |state, cx| {
                                let delegate = state.delegate_mut();
                                // The row being left and the row being entered arrive in either
                                // order, so a leave only clears the state this row owns.
                                let next = if is_hovered {
                                    Some(ix)
                                } else if delegate.hovered == Some(ix) {
                                    None
                                } else {
                                    return;
                                };
                                if delegate.hovered != next {
                                    delegate.hovered = next;
                                    cx.notify();
                                }
                            });
                        })
                        .child(
                            div()
                                .flex_none()
                                .w(rems(3.5))
                                .text_xs()
                                .font_weight(FontWeight::BOLD)
                                .font_family(mono.clone())
                                .text_color(method_color)
                                .child(SharedString::from(method)),
                        )
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .truncate()
                                .text_sm()
                                .font_family(mono)
                                .child(SharedString::from(path)),
                        )
                        .child(
                            div()
                                .flex_none()
                                .flex()
                                .items_center()
                                .gap_1()
                                .child(
                                    div()
                                        .text_xs()
                                        .text_color(muted)
                                        .child(SharedString::from(age)),
                                )
                                .child(
                                    div()
                                        .w_6()
                                        .flex()
                                        .items_center()
                                        .justify_center()
                                        .children(delete),
                                ),
                        ),
                ),
        )
    }

    fn render_empty(
        &mut self,
        _: &mut Window,
        cx: &mut Context<ListState<Self>>,
    ) -> impl IntoElement {
        let muted = cx.theme().muted_foreground;
        let (title, hint) = if self.all.is_empty() {
            (
                "No requests yet",
                "Requests you send from the console appear here.",
            )
        } else {
            (
                "No matches",
                "No request matches this filter. Try a method, a path or part of one.",
            )
        };
        Empty::new().border_0().header(
            EmptyHeader::new()
                .media(
                    EmptyMedia::new()
                        .with_variant(EmptyMediaVariant::Icon)
                        .child(Icon::new(IconName::Clock).text_color(muted)),
                )
                .title(EmptyTitle::new().child(title))
                .description(EmptyDescription::new().child(hint)),
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

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// How long ago `at` was, in the compact form a row's right edge has room for.
fn age_label(at: u64, now: u64) -> String {
    let s = now.saturating_sub(at);
    match s {
        0..60 => "now".to_string(),
        60..3_600 => format!("{}m", s / 60),
        3_600..86_400 => format!("{}h", s / 3_600),
        86_400..2_592_000 => format!("{}d", s / 86_400),
        2_592_000..31_536_000 => format!("{}mo", s / 2_592_000),
        _ => format!("{}y", s / 31_536_000),
    }
}

#[cfg(test)]
mod tests {
    use std::cell::{Cell, RefCell};

    use gpui_kit::component::list::{List, ListEvent};
    use gpui_kit::test::TestWindowExt as _;
    use gpui_kit::{AppContext as _, Entity, Render, TestAppContext, px};

    use super::*;

    #[test]
    fn age_is_compact_and_never_negative() {
        let now = 1_700_000_000;
        let cases = [
            (now, "now"),
            (now - 10, "now"),
            (now - 59, "now"),
            (now - 60, "1m"),
            (now - 3_599, "59m"),
            (now - 3_600, "1h"),
            (now - 86_399, "23h"),
            (now - 86_400, "1d"),
            (now - 2_592_000, "1mo"),
            (now - 31_536_000, "1y"),
            (now + 60, "now"),
        ];
        for (at, want) in cases {
            assert_eq!(age_label(at, now), want, "at: {at}");
        }
    }

    fn entry(path: &str, at: u64) -> Entry {
        Entry {
            connection: "c".into(),
            method: "GET".into(),
            path: path.into(),
            body: None,
            at,
        }
    }

    /// The row id the list derives from an entry, so a test can address the row it wants.
    fn row_of(e: &Entry) -> u64 {
        row_id(&format!("{}{}{}{:?}", e.at, e.method, e.path, e.body))
    }

    struct Page {
        list: Entity<ListState<HistoryList>>,
    }

    impl Page {
        fn new(
            list: Entity<ListState<HistoryList>>,
            confirms: Rc<Cell<usize>>,
            cx: &mut Context<Self>,
        ) -> Self {
            cx.subscribe(&list, move |_, _, ev: &ListEvent, _| {
                if let ListEvent::Confirm(_) = ev {
                    confirms.set(confirms.get() + 1);
                }
            })
            .detach();
            Self { list }
        }
    }

    impl Render for Page {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div().size(px(600.)).p_4().child(List::new(&self.list))
        }
    }

    /// Entering a row reveals its delete button; pressing it alone asks for the deletion and
    /// does not also open the request, which is what clicking anywhere else on the row does.
    #[gpui_kit::test]
    fn a_row_is_deleted_from_its_button_without_opening_the_request(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        let (old, new) = (entry("a/_search", 10), entry("b/_search", 20));
        let (deleted, confirms) = (Rc::new(RefCell::new(None)), Rc::new(Cell::new(0)));
        let (window, page) = cx
            .update(|cx| {
                gpui_kit::open_window(Default::default(), cx, |window, cx| {
                    let list = cx.new(|cx| {
                        let mut delegate = HistoryList::default();
                        delegate.set([old.clone(), new.clone()].into_iter());
                        let deleted = deleted.clone();
                        delegate.set_on_delete(move |entry, _| {
                            *deleted.borrow_mut() = Some(entry.clone())
                        });
                        ListState::new(delegate, window, cx)
                    });
                    let confirms = confirms.clone();
                    cx.new(|cx| Page::new(list, confirms, cx))
                })
            })
            .unwrap();
        let list = page.read_with(cx, |page, _| page.list.clone());
        // Newest first, so the row under the test is row 0.
        let (row, button) = (
            ("history-row", row_of(&new)),
            ("history-delete", row_of(&new)),
        );

        cx.update_window(window, |_, window, cx| {
            window.render_frame(cx);
            assert!(
                window.try_find(button).is_none(),
                "the button is hidden until the row is entered"
            );
            window.hover(row, cx);
            window.render_frame(cx);
        })
        .unwrap();
        cx.update(|cx| {
            assert_eq!(list.read(cx).delegate().hovered, Some(IndexPath::new(0)));
        });

        cx.update_window(window, |_, window, cx| window.click(button, cx))
            .unwrap();
        assert_eq!(deleted.borrow().as_ref(), Some(&new));
        assert_eq!(
            confirms.get(),
            0,
            "deleting a row must not open its request"
        );

        // The rest of the row still opens the request.
        cx.update_window(window, |_, window, cx| window.click(row, cx))
            .unwrap();
        assert_eq!(confirms.get(), 1);
        assert!(
            deleted.borrow().as_ref() == Some(&new),
            "still the same entry"
        );
    }
}
