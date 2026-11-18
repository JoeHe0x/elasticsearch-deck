//! Data Browser: a tab that shows one index or alias as a table, with paging, header sorting and
//! a filter box. It only reads; every request is a `_search` built by `es_client::browse`.

use std::collections::HashSet;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use es_client::browse::{self, Order, Page, Span};
use es_client::{Client, ClusterMeta, Hits};
use es_dsl_core::{Fields as _, Token};
use gpui_kit::HighlightStyle;
use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{
    Editor, EditorState, InputEvent, TextDecoration, TextDecorationCollection,
};
use gpui_kit::component::menu::{DropdownMenu as _, PopupMenuItem};
use gpui_kit::component::table::{ColumnSort, TableEvent, TableState};
use gpui_kit::component::{ActiveTheme as _, Disableable as _, Sizable as _};
use gpui_kit::{
    AppContext as _, ClipboardItem, Context, Entity, InteractiveElement as _, IntoElement,
    ParentElement as _, Render, SharedString, StatefulInteractiveElement as _, Styled as _, Task,
    Window, div, px,
};

use crate::completion::{FieldCompletion, highlight_subsequence};
use crate::hits_table::{self, HitsTable};

/// The choices of the page-size button, which cycles through them.
const SIZES: [u64; 4] = [25, 50, 100, 200];
const DEFAULT_SIZE: u64 = 50;

enum State {
    Loading,
    Ready {
        rows: usize,
        took: Duration,
        server_ms: Option<u64>,
    },
    Failed(SharedString),
}

pub struct DataBrowser {
    client: Arc<Client>,
    meta: Option<ClusterMeta>,
    target: String,
    filter: Entity<EditorState>,
    /// The filter in effect; the box may hold text that was not applied yet.
    applied: String,
    /// Zero-based.
    page: u64,
    size: u64,
    /// The clicked column and its direction. The column, not the field sorted on: a text column
    /// sorts on its `.keyword` sub-field but the arrow belongs to the column.
    sort: Option<(String, Order)>,
    window_limit: u64,
    total: Option<u64>,
    table: Option<Entity<TableState<HitsTable>>>,
    /// The page as parsed, so hiding a column rebuilds the table without a request.
    hits: Option<Hits>,
    /// Columns the user hid; kept across pages and reloads of this tab.
    hidden: HashSet<String>,
    /// The row whose whole document the side panel shows.
    detail: Option<usize>,
    state: State,
    /// Dropping a task cancels its request, so a newer one replaces the older.
    load: Option<Task<()>>,
    _window: Option<Task<()>>,
}

/// Colors the filter's fields, operators and values with the console's palette.
fn paint_filter(colors: &TextDecorationCollection, text: &str, cx: &mut gpui_kit::App) {
    let dark = cx.theme().is_dark();
    let spans = browse::kql_spans(text).into_iter().map(|(range, span)| {
        let token = match span {
            Span::Field => Token::Property,
            Span::Operator => Token::Target,
            Span::Number => Token::Number,
            Span::Value => Token::Field,
        };
        let style = HighlightStyle {
            color: Some(crate::paint::color(token, dark)),
            ..Default::default()
        };
        TextDecoration::new(range, style)
    });
    colors.set(spans.collect(), cx);
}

impl DataBrowser {
    pub fn new(
        target: String,
        client: Arc<Client>,
        meta: Option<ClusterMeta>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let known = meta.clone().zip(Some(target.clone()));
        // The mapping loads on first use and that first call answers empty; ask now so it is there
        // by the time the user types.
        if let Some(meta) = &meta {
            meta.fields(&target);
        }
        let filter = cx.new(|cx| {
            // The editor widget is the one that takes completion; one row that submits on Enter
            // makes it a single-line box.
            let mut state = EditorState::new(window, cx)
                .line_number(false)
                .soft_wrap(false)
                .submit_on_enter(true)
                .placeholder("Filter, e.g. Title : S* and Rating > 7.5");
            state.lsp_mut().completion_provider =
                Some(Rc::new(FieldCompletion(Rc::new(move || match &known {
                    Some((meta, target)) => meta.fields(target),
                    None => Arc::from([]),
                }))));
            state
                .lsp_mut()
                .set_completion_highlighter(Rc::new(highlight_subsequence));
            // A row that never scrolls vertically should not show a scrollbar.
            state.set_scrollbar_renderer(Some(Rc::new(|_, _, _| div().into_any_element())));
            state
        });
        let colors = filter.update(cx, |state, cx| {
            state.create_decorations_collection(Vec::new(), cx)
        });
        cx.subscribe_in(
            &filter,
            window,
            move |this, filter, ev: &InputEvent, window, cx| match ev {
                InputEvent::PressEnter { .. } => this.apply(window, cx),
                InputEvent::Change => {
                    let text = filter.read(cx).value().to_string();
                    paint_filter(&colors, &text, cx);
                }
                _ => {}
            },
        )
        .detach();
        let call = client.send(browse::window_request(&target));
        let reading = cx.spawn(async move |this, cx| {
            let limit = match call.await {
                Ok(r) if r.status < 300 => browse::parse_window(&r.body),
                _ => browse::DEFAULT_WINDOW,
            };
            let _ = this.update(cx, |this, cx| {
                this.window_limit = limit;
                cx.notify();
            });
        });
        let mut this = Self {
            client,
            meta,
            target,
            filter,
            applied: String::new(),
            page: 0,
            size: DEFAULT_SIZE,
            sort: None,
            window_limit: browse::DEFAULT_WINDOW,
            total: None,
            table: None,
            detail: None,
            hits: None,
            hidden: HashSet::new(),
            state: State::Loading,
            load: None,
            _window: Some(reading),
        };
        this.load(window, cx);
        this
    }

    fn fields(&self) -> Arc<[es_dsl_core::Field]> {
        match &self.meta {
            Some(meta) => meta.fields(&self.target),
            None => Arc::from([]),
        }
    }

    fn paging(&self) -> Page {
        Page {
            target: self.target.clone(),
            from: self.page * self.size,
            size: self.size,
            filter: self.applied.clone(),
            sort: None,
        }
    }

    /// The request for the page on screen.
    fn current(&self) -> Page {
        let fields = self.fields();
        let sort = self
            .sort
            .as_ref()
            .and_then(|(column, order)| Some((browse::sort_field(column, &fields)?, *order)));
        Page {
            sort,
            ..self.paging()
        }
    }

    fn load(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let call = self.client.send(self.current().request());
        let started = Instant::now();
        self.state = State::Loading;
        self.load = Some(cx.spawn_in(window, async move |this, cx| {
            let result = call.await;
            let took = started.elapsed();
            // Parsing a big page is not UI work.
            let parsed = match &result {
                Ok(r) if r.status < 300 => {
                    let body = r.body.clone();
                    cx.background_spawn(async move { es_client::parse_hits(&body) })
                        .await
                }
                _ => None,
            };
            let _ = this.update_in(cx, |this, window, cx| {
                this.load = None;
                match (result, parsed) {
                    (Ok(r), Some(hits)) if r.status < 300 => {
                        this.total = hits
                            .total
                            .as_deref()
                            .and_then(|t| t.trim_end_matches('+').parse().ok());
                        let rows = hits.rows.len();
                        let server_ms = hits.took_ms;
                        this.detail = None;
                        this.hits = Some(hits);
                        this.rebuild(window, cx);
                        this.state = State::Ready {
                            rows,
                            took,
                            server_ms,
                        };
                    }
                    (Ok(r), _) => {
                        let reason = browse::error_reason(&r.body)
                            .unwrap_or_else(|| r.body.chars().take(300).collect());
                        this.state = State::Failed(format!("{}: {reason}", r.status).into());
                    }
                    (Err(e), _) => {
                        this.state = State::Failed(format!("Request failed. {e}").into())
                    }
                }
                cx.notify();
            });
        }));
        cx.notify();
    }

    /// Builds the table from the last page, leaving out the hidden columns.
    fn rebuild(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(hits) = &self.hits else { return };
        let keep: Vec<usize> = (0..hits.columns.len())
            .filter(|ix| !self.hidden.contains(&hits.columns[*ix]))
            .collect();
        fn pick<T: Clone>(row: &[T], keep: &[usize]) -> Vec<T> {
            keep.iter().map(|ix| row[*ix].clone()).collect()
        }
        let shown = Hits {
            columns: pick(&hits.columns, &keep),
            rows: hits.rows.iter().map(|r| pick(r, &keep)).collect(),
            kinds: hits.kinds.iter().map(|r| pick(r, &keep)).collect(),
            ..hits.clone()
        };
        let this_entity = cx.entity().downgrade();
        let fields = self.fields();
        let current = self
            .sort
            .as_ref()
            .map(|(c, o)| (c.clone(), column_sort(*o)));
        let table = HitsTable::new(shown).types(fields.iter()).sorting(
            |name| browse::sort_field(name, &fields).is_some(),
            current.as_ref().map(|(c, s)| (c.as_str(), *s)),
            Rc::new(move |name, sort, window, cx| {
                let _ = this_entity.update(cx, |this, cx| this.sort_by(name, sort, window, cx));
            }),
        );
        let table = cx.new(|cx| TableState::new(table, window, cx));
        cx.subscribe(&table, |this, _, ev: &TableEvent, cx| {
            if let TableEvent::SelectRow(ix) = ev {
                this.detail = Some(*ix);
                cx.notify();
            }
        })
        .detach();
        self.table = Some(table);
        cx.notify();
    }

    fn show_all_columns(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.hidden.clear();
        self.rebuild(window, cx);
    }

    fn toggle_column(&mut self, name: &str, window: &mut Window, cx: &mut Context<Self>) {
        if !self.hidden.remove(name) {
            self.hidden.insert(name.to_string());
        }
        self.rebuild(window, cx);
    }

    fn apply(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.applied = self.filter.read(cx).value().to_string();
        self.page = 0;
        self.load(window, cx);
    }

    fn sort_by(
        &mut self,
        column: &str,
        sort: ColumnSort,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.sort = match sort {
            ColumnSort::Ascending => Some((column.to_string(), Order::Asc)),
            ColumnSort::Descending => Some((column.to_string(), Order::Desc)),
            ColumnSort::Default => None,
        };
        self.page = 0;
        self.load(window, cx);
    }

    fn go(&mut self, page: u64, window: &mut Window, cx: &mut Context<Self>) {
        self.page = page;
        self.load(window, cx);
    }

    fn resize(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let at = SIZES.iter().position(|s| *s == self.size).unwrap_or(0);
        self.size = SIZES[(at + 1) % SIZES.len()];
        self.page = 0;
        self.load(window, cx);
    }
}

fn column_sort(order: Order) -> ColumnSort {
    match order {
        Order::Asc => ColumnSort::Ascending,
        Order::Desc => ColumnSort::Descending,
    }
}

impl Render for DataBrowser {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let (muted, danger) = (theme.muted_foreground, theme.danger);
        let toolbar = div()
            .flex()
            .items_center()
            .gap_1()
            .pb_1p5()
            .child(div().flex_1().child(Editor::new(&self.filter).h(px(32.))))
            .child(
                Button::new("browse-apply")
                    .ghost()
                    .small()
                    .icon(IconName::Search)
                    .tooltip("Apply the filter")
                    .on_click(cx.listener(|this, _, window, cx| this.apply(window, cx))),
            )
            .child({
                let columns = self
                    .hits
                    .as_ref()
                    .map(|h| h.columns.clone())
                    .unwrap_or_default();
                let hidden = self.hidden.clone();
                let this = cx.entity();
                Button::new("browse-columns")
                    .ghost()
                    .small()
                    .icon(IconName::Columns3)
                    .tooltip("Show or hide columns")
                    .disabled(columns.is_empty())
                    .dropdown_menu(move |menu, _, _| {
                        // Wide indexes have up to 60 columns; the list must scroll.
                        let menu = menu.scrollable(true).max_h(px(360.));
                        let menu = if hidden.is_empty() {
                            menu
                        } else {
                            let this = this.clone();
                            menu.item(PopupMenuItem::new("Show all").on_click(
                                move |_, window, cx| {
                                    this.update(cx, |t, cx| t.show_all_columns(window, cx))
                                },
                            ))
                            .separator()
                        };
                        columns.iter().fold(menu, |menu, name| {
                            let (this, n) = (this.clone(), name.clone());
                            menu.item(
                                PopupMenuItem::new(name.clone())
                                    .checked(!hidden.contains(name))
                                    .on_click(move |_, window, cx| {
                                        this.update(cx, |t, cx| t.toggle_column(&n, window, cx))
                                    }),
                            )
                        })
                    })
            })
            .child(
                Button::new("browse-reload")
                    .ghost()
                    .small()
                    .icon(IconName::RefreshCw)
                    .tooltip("Reload the page")
                    .on_click(cx.listener(|this, _, window, cx| this.load(window, cx))),
            );
        let body = match (&self.state, &self.table) {
            (State::Failed(message), _) => div()
                .p_2()
                .text_color(danger)
                .child(message.clone())
                .into_any_element(),
            (State::Ready { rows: 0, .. }, _) => div()
                .p_2()
                .text_color(muted)
                .child("No matching documents.")
                .into_any_element(),
            (_, Some(table)) => {
                let doc = self
                    .detail
                    .and_then(|ix| table.read(cx).delegate().doc(ix))
                    .map(|d| SharedString::from(d.to_string()));
                let (border, mono) = (theme.border, theme.mono_font_family.clone());
                div()
                    .size_full()
                    .flex()
                    .child(div().flex_1().min_w_0().child(hits_table::view(table)))
                    .children(doc.map(|doc| {
                        let copied = doc.clone();
                        div()
                            .w(px(380.))
                            .flex_none()
                            .flex()
                            .flex_col()
                            .border_l_1()
                            .border_color(border)
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap_1()
                                    .px_2()
                                    .child(
                                        div()
                                            .flex_1()
                                            .text_xs()
                                            .text_color(muted)
                                            .child("Document"),
                                    )
                                    .child(
                                        Button::new("detail-copy")
                                            .ghost()
                                            .small()
                                            .icon(IconName::Copy)
                                            .tooltip("Copy the document")
                                            .on_click(move |_, _, cx| {
                                                cx.write_to_clipboard(ClipboardItem::new_string(
                                                    copied.to_string(),
                                                ))
                                            }),
                                    )
                                    .child(
                                        Button::new("detail-close")
                                            .ghost()
                                            .small()
                                            .icon(IconName::Close)
                                            .tooltip("Close")
                                            .on_click(cx.listener(|this, _, _, cx| {
                                                this.detail = None;
                                                cx.notify();
                                            })),
                                    ),
                            )
                            .child(
                                div()
                                    .id("detail-body")
                                    .flex_1()
                                    .min_h_0()
                                    .overflow_y_scroll()
                                    .p_2()
                                    .text_xs()
                                    .font_family(mono)
                                    .child(doc),
                            )
                    }))
                    .into_any_element()
            }
            (State::Loading, None) => div()
                .p_2()
                .text_color(muted)
                .child("Loading…")
                .into_any_element(),
            _ => div().into_any_element(),
        };
        let paging = self.paging();
        let has_prev = self.page > 0;
        let has_next = paging.has_next(self.total, self.window_limit);
        // Stopped by the window rather than by the hits: say why, rather than just disabling.
        let capped = !has_next && self.total.is_none_or(|t| paging.from + paging.size < t);
        let summary = match (&self.state, self.total) {
            (
                State::Ready {
                    rows,
                    took,
                    server_ms,
                },
                total,
            ) if *rows > 0 => {
                let first = paging.from + 1;
                let last = paging.from + *rows as u64;
                let of = total.map(|t| format!(" of {t}")).unwrap_or_default();
                let server = server_ms
                    .map(|ms| format!(" (cluster {ms} ms)"))
                    .unwrap_or_default();
                format!("{first}–{last}{of} · {} ms{server}", took.as_millis())
            }
            (State::Loading, _) => "Loading…".to_string(),
            _ => String::new(),
        };
        let footer = div()
            .flex()
            .items_center()
            .gap_2()
            .pt_1p5()
            .child(
                Button::new("browse-prev")
                    .ghost()
                    .small()
                    .icon(IconName::ChevronLeft)
                    .tooltip("Previous page")
                    .disabled(!has_prev)
                    .on_click(cx.listener(|this, _, window, cx| {
                        let page = this.page.saturating_sub(1);
                        this.go(page, window, cx)
                    })),
            )
            .child(
                Button::new("browse-next")
                    .ghost()
                    .small()
                    .icon(IconName::ChevronRight)
                    .tooltip("Next page")
                    .disabled(!has_next)
                    .on_click(cx.listener(|this, _, window, cx| {
                        let page = this.page + 1;
                        this.go(page, window, cx)
                    })),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(muted)
                    .child(SharedString::from(summary)),
            )
            .children(capped.then(|| {
                div().text_color(muted).child(SharedString::from(format!(
                    "Paging stops at index.max_result_window ({}).",
                    self.window_limit
                )))
            }))
            .child(div().flex_1())
            .child(
                Button::new("browse-size")
                    .ghost()
                    .small()
                    .label(format!("{} / page", self.size))
                    .tooltip("Change the page size")
                    .on_click(cx.listener(|this, _, window, cx| this.resize(window, cx))),
            );
        div()
            .size_full()
            .flex()
            .flex_col()
            .child(toolbar)
            .child(div().flex_1().min_h_0().child(body))
            .child(footer)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui_kit::TestAppContext;
    use gpui_kit::component::table::TableDelegate as _;
    use httpmock::prelude::*;

    const HITS: &str = r#"{"hits":{"total":{"value":500,"relation":"eq"},"hits":[
        {"_id":"1","_index":"logs","_score":1.0,"_source":{"msg":"a"}},
        {"_id":"2","_index":"logs","_score":1.0,"_source":{"msg":"b"}}]}}"#;

    /// Lets the client's own runtime answer while the test executor waits for it.
    fn settle(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        for _ in 0..20 {
            std::thread::sleep(Duration::from_millis(25));
            cx.run_until_parked();
        }
    }

    /// Opens a browser on `logs` against a mock cluster with a result window of 100.
    #[gpui_kit::test]
    fn paging_and_filtering_send_the_request_and_stop_at_the_window(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(GET).path("/logs/_settings");
            then.status(200)
                .body(r#"{"logs":{"settings":{"index":{"max_result_window":"100"}}}}"#);
        });
        let first = server.mock(|when, then| {
            when.method(POST)
                .path("/logs/_search")
                .body_includes("\"from\":0");
            then.status(200).body(HITS);
        });
        let second = server.mock(|when, then| {
            when.method(POST)
                .path("/logs/_search")
                .body_includes("\"from\":50")
                .body_includes("\"status\":\"active\"");
            then.status(200).body(HITS);
        });
        let client = Arc::new(Client::new(es_client::Config::new(server.base_url())).unwrap());
        let (window, browser) = cx
            .update(|cx| {
                gpui_kit::open_window(Default::default(), cx, |window, cx| {
                    cx.new(|cx| DataBrowser::new("logs".into(), client, None, window, cx))
                })
            })
            .unwrap();
        settle(cx);
        first.assert();
        cx.update(|cx| {
            let b = browser.read(cx);
            assert!(matches!(b.state, State::Ready { rows: 2, .. }));
            assert_eq!((b.total, b.window_limit), (Some(500), 100));
            assert!(b.table.is_some());
            assert_eq!(b.detail, None);
            // Rows 50..100 are the last page the window of 100 allows.
            assert!(b.paging().has_next(b.total, b.window_limit));
        });
        cx.update(|cx| {
            let table = browser.read(cx).table.clone().unwrap();
            table.update(cx, |t, cx| t.set_selected_row(1, cx));
        });
        cx.run_until_parked();
        cx.update(|cx| {
            let b = browser.read(cx);
            assert_eq!(b.detail, Some(1));
            assert!(
                b.table
                    .as_ref()
                    .unwrap()
                    .read(cx)
                    .delegate()
                    .doc(1)
                    .is_some()
            );
        });
        cx.update_window(window, |_, window, cx| {
            browser.update(cx, |b, cx| {
                let all = b
                    .table
                    .as_ref()
                    .unwrap()
                    .read(cx)
                    .delegate()
                    .columns_count(cx);
                b.toggle_column("_index", window, cx);
                let table = b.table.as_ref().unwrap().read(cx).delegate();
                assert_eq!(table.columns_count(cx), all - 1);
                b.toggle_column("_index", window, cx);
                let table = b.table.as_ref().unwrap().read(cx).delegate();
                assert_eq!(table.columns_count(cx), all);
                b.toggle_column("_index", window, cx);
                b.show_all_columns(window, cx);
                let table = b.table.as_ref().unwrap().read(cx).delegate();
                assert_eq!(table.columns_count(cx), all);
            })
        })
        .unwrap();
        cx.update_window(window, |_, window, cx| {
            browser.update(cx, |b, cx| {
                b.filter
                    .update(cx, |f, cx| f.set_value("status:active", window, cx));
                b.applied = "status:active".into();
                b.go(1, window, cx)
            })
        })
        .unwrap();
        settle(cx);
        second.assert();
        cx.update(|cx| {
            let b = browser.read(cx);
            assert!(!b.paging().has_next(b.total, b.window_limit));
        });
    }

    /// An error answer shows the server's reason instead of a table.
    #[gpui_kit::test]
    fn a_rejected_filter_shows_the_servers_reason(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(GET).path("/logs/_settings");
            then.status(200).body("{}");
        });
        server.mock(|when, then| {
            when.method(POST).path("/logs/_search");
            then.status(400).body(
                r#"{"error":{"root_cause":[{"type":"x","reason":"Failed to parse query [a:]"}]},"status":400}"#,
            );
        });
        let client = Arc::new(Client::new(es_client::Config::new(server.base_url())).unwrap());
        let (_, browser) = cx
            .update(|cx| {
                gpui_kit::open_window(Default::default(), cx, |window, cx| {
                    cx.new(|cx| DataBrowser::new("logs".into(), client, None, window, cx))
                })
            })
            .unwrap();
        settle(cx);
        cx.update(|cx| match &browser.read(cx).state {
            State::Failed(m) => assert_eq!(m.as_ref(), "400: Failed to parse query [a:]"),
            _ => panic!("expected a failure"),
        });
    }
}
