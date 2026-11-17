//! The request editor: a GPUI Kit code editor whose completions come from the in-process LSP.

use std::ops::Range;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use es_client::catalog::{Catalog, Item};
use es_client::history::Entry;
use es_client::saved::{self, Kind, Saved};
use es_client::{Client, Request};
use es_dsl_core::Index;
use gpui_kit::assets::IconName;
use gpui_kit::component::Icon;
use gpui_kit::component::TitleBar;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::dialog::AlertDialog;
use gpui_kit::component::input::{Editor, EditorState, Enter, Input, InputEvent, InputState};
use gpui_kit::component::list::ListItem;
use gpui_kit::component::list::{List, ListEvent, ListState};
use gpui_kit::component::menu::{AppMenuBar, DropdownMenu as _, PopupMenuItem};
use gpui_kit::component::resizable::{h_resizable, resizable_panel};
use gpui_kit::component::status_bar::StatusBar;
use gpui_kit::component::tab::{Tab, TabBar};
use gpui_kit::component::table::{DataTable, TableState};
use gpui_kit::component::tree::{TreeItem, TreeState, tree};
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Selectable as _, Sizable as _, Theme, WindowExt as _,
    scroll::ScrollbarMode,
};
use gpui_kit::{
    App, AppContext as _, ClipboardItem, Context, Entity, Focusable as _, FontWeight, Hsla,
    InteractiveElement as _, IntoElement, KeyBinding, Menu, MenuItem, MouseButton, MouseDownEvent,
    ParentElement as _, Render, SharedString, Styled as _, Task, Window, actions, div, px, rems,
};

use crate::completion::{LspCompletion, LspHover};
use crate::connect::{ConnectPanel, Connected};
use crate::history_list::HistoryList;
use crate::hits_table::HitsTable;
use crate::library::{Changed, Library};
use crate::lsp_client::LspClient;
use crate::paint::Paint;
use crate::targets::{Folder, Row, Targets, search_text};

/// How often a connected console re-reads cluster metadata for targets it has not seen yet.
const REFRESH_EVERY: Duration = Duration::from_secs(2);

/// Nodes, templates, tasks and pipelines are re-read every this many metadata refreshes.
const CATALOG_EVERY: u32 = 15;

/// Quiet time after the last keystroke before the document is checked.
const LINT_DELAY: Duration = Duration::from_millis(400);

/// The response viewer shows at most this much; the status line says when it cut the body.
const MAX_SHOWN: usize = 2 * 1024 * 1024;

actions!(
    console,
    [
        Send,
        Format,
        Newline,
        CancelRequest,
        ToggleTree,
        ToggleReadonly,
        ShowHistory,
        ShowShortcuts
    ]
);

/// The application menu: one Action per command, shared with the key bindings. Native on macOS,
/// drawn by `AppMenuBar` in the title bar elsewhere.
pub fn app_menus() -> Vec<Menu> {
    vec![
        Menu::new("Request").items([
            MenuItem::action("Send", Send),
            MenuItem::action("Cancel", CancelRequest),
            MenuItem::separator(),
            MenuItem::action("Format body", Format),
        ]),
        Menu::new("View").items([
            MenuItem::action("Toggle tree", ToggleTree),
            MenuItem::action("History…", ShowHistory),
            MenuItem::action("Toggle readonly", ToggleReadonly),
        ]),
        Menu::new("Help").items([MenuItem::action("Shortcuts", ShowShortcuts)]),
    ]
}

/// Binds Ctrl/Cmd+Enter to [`Send`] and Ctrl/Cmd+Shift+F to [`Format`] inside editors. It must run
/// after `gpui_kit::init`: a later binding wins over the editor's own Ctrl/Cmd+Enter, which would
/// insert a newline.
pub fn bind_keys(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new(SEND_KEY, Send, Some("Input")),
        KeyBinding::new(FORMAT_KEY, Format, Some("Input")),
        KeyBinding::new("enter", Newline, Some("Input")),
    ]);
}

#[cfg(target_os = "macos")]
const SEND_KEY: &str = "cmd-enter";
#[cfg(not(target_os = "macos"))]
const SEND_KEY: &str = "ctrl-enter";

#[cfg(target_os = "macos")]
const FORMAT_KEY: &str = "cmd-shift-f";
#[cfg(not(target_os = "macos"))]
const FORMAT_KEY: &str = "ctrl-shift-f";

/// How a search response is shown; other responses are always JSON.
#[derive(Clone, Copy, PartialEq)]
enum View {
    Table,
    Tree,
    Json,
}

/// Where the last request stands. The body itself lives in the response editor.
enum Run {
    Idle,
    Sending,
    Done {
        status: u16,
        took: Duration,
        bytes: usize,
        cut: bool,
        /// e.g. `50 of 10000+ hits`, for a search response.
        found: Option<String>,
    },
    Failed(SharedString),
}

fn tree_item(node: &es_client::Node, open: bool) -> TreeItem {
    TreeItem::new(node.path.clone(), node.label.clone())
        .expanded(open)
        .children(node.children.iter().map(|c| tree_item(c, false)))
}

/// Colors of the response tree, by kind of text.
#[derive(Clone, Copy)]
struct TreePalette {
    key: Hsla,
    string: Hsla,
    number: Hsla,
    keyword: Hsla,
    muted: Hsla,
    guide: Hsla,
}

/// What the toolbar shows about the cluster the console is talking to.
struct Summary {
    name: SharedString,
    version: SharedString,
}

pub struct Console {
    editor: Entity<EditorState>,
    panel: Entity<ConnectPanel>,
    summary: Option<Summary>,
    /// The connected cluster; `None` until a connection succeeds.
    http: Option<Arc<Client>>,
    response: Entity<EditorState>,
    /// Set when the last response was a search; it is what the Table view shows.
    hits: Option<Entity<TableState<HitsTable>>>,
    /// The last response as a collapsible tree; `None` when it was not JSON.
    tree: Option<Entity<TreeState>>,
    /// The whole last response body, for Save; the viewers may show less.
    full_body: Option<String>,
    targets: Entity<ListState<Targets>>,
    /// Saved items and history, shared by every tab; the tree and the history list show the current connection's.
    library: Entity<Library>,
    /// The in-flight catalog read; replaced by the next one.
    catalog: Option<Task<()>>,
    history_list: Entity<ListState<HistoryList>>,
    view: View,
    /// Blocks every request that would change the cluster; per session, off at start.
    readonly: bool,
    /// Diagnostics in the editor text as of the last check; `None` before the first one.
    problems: Option<usize>,
    /// JSON path at the cursor, as shown in the status bar; empty outside a body.
    crumbs: SharedString,
    /// The request under the cursor, as framed in the editor.
    active_block: Option<Range<usize>>,
    run: Run,
    /// The in-flight request; dropping it aborts the call.
    sending: Option<Task<()>>,
    index: Index,
    client: Arc<LspClient>,
    /// Whether the last metadata refresh reached the cluster; drives the tree's status dot.
    online: bool,
    /// Whether the tree panel is shown.
    sidebar_open: bool,
    /// Whether the tree's filter input is shown; hidden, it filters nothing.
    filter_open: bool,
    /// The connected cluster's completion cache, kept for the tree's refresh button.
    meta: Option<es_client::ClusterMeta>,
    /// Dropping it stops refreshing for the previous connection.
    refresh: Option<Task<()>>,
    /// A pending check; replaced (so cancelled) by the next edit.
    lint: Option<Task<()>>,
    paint: Paint,
    menu_bar: Entity<AppMenuBar>,
}

impl Console {
    pub fn new(
        text: &str,
        index: Index,
        client: Arc<LspClient>,
        library: Entity<Library>,
        connections: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let editor = cx.new(|cx| {
            let mut state = EditorState::new(window, cx)
                .language("json")
                .soft_wrap(false)
                .default_value(text.to_string());
            install(&mut state, &client);
            state
        });
        cx.subscribe(&editor, |this, _, ev: &InputEvent, cx| {
            if matches!(ev, InputEvent::Change) {
                this.lint(cx);
            }
        })
        .detach();
        let paint = Paint::new(&editor, cx);
        paint.refresh(text, cx);
        // Any editor notification may be a cursor move, so what depends on the cursor is recomputed
        // and compared; setting a decoration notifies the editor again.
        cx.observe(&editor, |this, editor, cx| {
            let state = editor.read(cx);
            let (text, cursor) = (state.value(), state.cursor());
            let crumbs = breadcrumb(&es_dsl_core::path_at(&text, cursor));
            let block = es_dsl_core::block_spans(&text)
                .into_iter()
                .find(|b| (b.start..=b.end).contains(&cursor));
            if block != this.active_block {
                this.paint.set_active(block.clone(), cx);
                this.active_block = block;
            }
            if crumbs != this.crumbs {
                this.crumbs = crumbs;
                cx.notify();
            }
        })
        .detach();
        let response = cx.new(|cx| {
            EditorState::new(window, cx)
                .language("json")
                .soft_wrap(false)
        });
        let targets = cx.new(|cx| ListState::new(Targets::default(), window, cx));
        cx.subscribe(&targets, |this, list, ev: &ListEvent, cx| {
            if let ListEvent::Confirm(ix) = ev {
                let picked = list.update(cx, |list, cx| {
                    let picked = list.delegate_mut().activate(*ix);
                    cx.notify();
                    picked
                });
                if let Some((folder, text)) = picked {
                    this.open_row(folder, text, cx);
                }
            }
        })
        .detach();
        // The kit does not follow the system appearance by itself: apply it now and on every change.
        restyle(Some(window), cx);
        cx.observe_window_appearance(window, |_, window, cx| restyle(Some(window), cx))
            .detach();
        cx.subscribe(&library, |this, _, ev: &Changed, cx| match ev {
            Changed::Saved => this.refresh_saved(cx),
            Changed::History => this.refresh_history(cx),
        })
        .detach();
        let history_list =
            cx.new(|cx| ListState::new(HistoryList::default(), window, cx).searchable(true));
        cx.subscribe(&history_list, |this, list, ev: &ListEvent, cx| {
            if let ListEvent::Confirm(ix) = ev {
                let entry = list.read(cx).delegate().entry(*ix).cloned();
                if let Some(e) = entry {
                    let body = e.body.unwrap_or_default();
                    this.append_request(format!("{} {}\n{body}", e.method, e.path), true, cx);
                }
            }
        })
        .detach();
        let panel = cx.new(|cx| ConnectPanel::new(connections, window, cx));
        cx.subscribe(&panel, |this, _, done: &Connected, cx| {
            this.connected(done, cx)
        })
        .detach();
        Self {
            editor,
            panel,
            summary: None,
            http: None,
            response,
            hits: None,
            tree: None,
            full_body: None,
            targets,
            library,
            catalog: None,
            history_list,
            view: View::Table,
            readonly: false,
            problems: None,
            crumbs: SharedString::default(),
            active_block: None,
            run: Run::Idle,
            sending: None,
            index,
            client,
            online: false,
            sidebar_open: true,
            filter_open: false,
            meta: None,
            refresh: None,
            lint: None,
            paint,
            menu_bar: AppMenuBar::new(cx),
        }
    }

    /// The open completion menu of the focused editor, if any. Other inputs under the console (the
    /// tree filter, dialogs) keep their own Enter, so callers propagate when this is `None`.
    fn open_menu(&self, window: &Window, cx: &App) -> Option<bool> {
        let editor = self.editor.read(cx);
        editor
            .focus_handle(cx)
            .is_focused(window)
            .then(|| editor.completion_menu_state().open)
    }

    /// Enter takes the highlighted suggestion; with no menu open it is a plain newline.
    fn newline(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.open_menu(window, cx) == Some(true) {
            let enter = Enter {
                secondary: false,
                shift: false,
            };
            let taken = self.editor.update(cx, |state, cx| {
                state.route_overlay_action(Box::new(enter), window, cx)
            });
            if taken {
                return;
            }
        }
        cx.propagate();
    }

    /// Re-checks the document shortly after the last edit and shows the result as underlines.
    fn lint(&mut self, cx: &mut Context<Self>) {
        // Every text change, typed or set by code, ends up here, so the colors are redone too.
        let text = self.editor.read(cx).value().to_string();
        self.paint.refresh(&text, cx);
        self.lint = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(LINT_DELAY).await;
            let Ok((text, client)) = this.update(cx, |this, cx| {
                (
                    this.editor.read(cx).value().to_string(),
                    this.client.clone(),
                )
            }) else {
                return;
            };
            let sent = text.clone();
            // The client blocks on the server's answer, so keep it off the UI thread.
            let Ok(found) = cx
                .background_spawn(async move { client.diagnose(sent) })
                .await
            else {
                return;
            };
            let _ = this.update(cx, |this, cx| {
                let count = found.len();
                let current = this.editor.update(cx, |state, cx| {
                    // The text moved on while the server worked; the next edit schedules a fresh check.
                    if state.value() != text {
                        return false;
                    }
                    let rope = state.text().clone();
                    if let Some(set) = state.diagnostics_mut() {
                        set.reset(&rope);
                        set.extend(found);
                    }
                    cx.notify();
                    true
                });
                if current {
                    this.problems = Some(count);
                    cx.notify();
                }
            });
        }));
    }

    /// Runs the request under the cursor; a request already in flight is cancelled first.
    fn send(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(http) = self.http.clone() else {
            self.fail("Not connected. Choose a connection first.", cx);
            return;
        };
        let editor = self.editor.read(cx);
        let Some(req) = Request::at(&editor.value(), editor.cursor()) else {
            self.fail(
                "No request at the cursor. Put the cursor in a request and try again.",
                cx,
            );
            return;
        };
        if self.readonly && es_dsl_core::is_write(&req.method, &req.path) {
            self.fail("Readonly is on. This request would change the cluster.", cx);
            return;
        }
        match es_dsl_core::danger(&req.method, &req.path, req.body.as_deref()) {
            Some(danger) => self.confirm(req, danger, window, cx),
            None => self.dispatch(http, req, window, cx),
        }
    }

    /// Asks before a destructive request runs; the request is sent only on the confirm button.
    fn confirm(
        &mut self,
        req: Request,
        danger: es_dsl_core::Danger,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let this = cx.entity();
        let line = format!("{} {}", req.method, req.path);
        window.open_alert_dialog(cx, move |alert: AlertDialog, _, _| {
            let (this, req) = (this.clone(), req.clone());
            alert
                .title(danger.title.clone())
                .description(line.clone())
                .ok_text(danger.action)
                .show_cancel(true)
                .on_ok(move |_, window, cx| {
                    let req = req.clone();
                    this.update(cx, |this, cx| {
                        // The connection may have been dropped or switched while the dialog was open.
                        if let Some(http) = this.http.clone() {
                            this.dispatch(http, req, window, cx);
                        }
                    });
                    true
                })
        });
    }

    fn dispatch(
        &mut self,
        http: Arc<Client>,
        mut req: Request,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Let the server indent the JSON so the viewer never reorders keys.
        if !req.path.contains("pretty") {
            let sep = if req.path.contains('?') { '&' } else { '?' };
            req.path.push_str(&format!("{sep}pretty"));
        }
        self.remember(&req, cx);
        self.run = Run::Sending;
        let started = Instant::now();
        let call = http.send(req);
        self.sending = Some(cx.spawn_in(window, async move |this, cx| {
            let result = call.await;
            let took = started.elapsed();
            let (parsed, nodes) = match &result {
                Ok(resp) => {
                    let body = resp.body.clone();
                    cx.background_spawn(async move {
                        (es_client::parse_hits(&body), es_client::json_tree(&body))
                    })
                    .await
                }
                Err(_) => (None, None),
            };
            let _ = this.update_in(cx, |this, window, cx| {
                this.sending = None;
                match result {
                    Ok(resp) => {
                        let bytes = resp.body.len();
                        let mut shown = resp.body.clone();
                        let cut = bytes > MAX_SHOWN;
                        if cut {
                            let mut end = MAX_SHOWN;
                            while !shown.is_char_boundary(end) {
                                end -= 1;
                            }
                            shown.truncate(end);
                        }
                        this.full_body = Some(resp.body);
                        this.tree = nodes.map(|root| {
                            cx.new(|cx| {
                                let mut state = TreeState::new(cx);
                                state.set_items(vec![tree_item(&root, true)], cx);
                                state
                            })
                        });
                        this.response
                            .update(cx, |state, cx| state.set_value(shown, window, cx));
                        let found = parsed.as_ref().map(|h| match &h.total {
                            Some(t) => format!("{} of {t} hits", h.rows.len()),
                            None => format!("{} hits", h.rows.len()),
                        });
                        this.hits = parsed
                            .map(|h| cx.new(|cx| TableState::new(HitsTable::new(h), window, cx)));
                        this.view = if this.hits.is_some() {
                            View::Table
                        } else {
                            View::Json
                        };
                        this.run = Run::Done {
                            status: resp.status,
                            took,
                            bytes,
                            cut,
                            found,
                        };
                    }
                    Err(e) => this.run = Run::Failed(format!("Request failed. {e}").into()),
                }
                cx.notify();
            });
        }));
        cx.notify();
    }

    /// Re-indents the JSON body of the request under the cursor; one undo step reverts it.
    fn format(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let editor = self.editor.clone();
        editor.update(cx, |state, cx| {
            let text = state.value().to_string();
            let Some(edit) = es_dsl_core::format_body(&text, state.cursor()) else {
                return;
            };
            let mut out = text;
            out.replace_range(edit.range.clone(), &edit.text);
            state.replace_all(out, window, cx);
            state.set_selected_range(edit.range.start..edit.range.start, cx);
        });
        self.lint(cx);
    }

    /// Asks for a file and writes the whole last response body to it.
    fn save_response(&mut self, cx: &mut Context<Self>) {
        let Some(body) = self.full_body.clone() else {
            return;
        };
        let dir = dirs::download_dir()
            .or_else(dirs::home_dir)
            .unwrap_or_default();
        cx.spawn(async move |this, cx| {
            let asked = cx.update(|cx| cx.prompt_for_new_path(&dir, Some("response.json")));
            // Cancelled, or no file dialog on this platform: nothing to write.
            let Ok(Ok(Some(path))) = asked.await else {
                return;
            };
            let written = cx
                .background_spawn(async move { std::fs::write(&path, body) })
                .await;
            if let Err(e) = written {
                let _ = this.update(cx, |this, cx| {
                    this.run = Run::Failed(format!("Could not save the response. {e}").into());
                    cx.notify();
                });
            }
        })
        .detach();
    }

    fn copy_path(&mut self, cx: &mut Context<Self>) {
        let path = self
            .tree
            .as_ref()
            .and_then(|t| t.read(cx).selected_item().map(|i| i.id.to_string()));
        if let Some(path) = path {
            cx.write_to_clipboard(ClipboardItem::new_string(path));
        }
    }

    fn cancel(&mut self, cx: &mut Context<Self>) {
        self.sending = None;
        self.run = Run::Idle;
        cx.notify();
    }

    fn fail(&mut self, message: &'static str, cx: &mut Context<Self>) {
        self.run = Run::Failed(message.into());
        cx.notify();
    }

    /// A click in the tree: consoles replace the editor text, everything else is appended.
    fn open_row(&mut self, folder: Folder, text: String, cx: &mut Context<Self>) {
        if folder != Folder::Consoles {
            self.append_request(text, false, cx);
            return;
        }
        let editor = self.editor.clone();
        let Some(window) = cx.active_window() else {
            return;
        };
        let _ = window.update(cx, |_, window, cx| {
            editor.update(cx, |state, cx| {
                state.set_value(text, window, cx);
                state.focus(window, cx);
            });
        });
        self.lint(cx);
    }

    /// Appends a request to the editor, puts the cursor on its first line and checks the document.
    /// `close_dialog` also closes the dialog the request was picked from.
    fn append_request(&mut self, request: String, close_dialog: bool, cx: &mut Context<Self>) {
        let editor = self.editor.clone();
        let Some(window) = cx.active_window() else {
            return;
        };
        let _ = window.update(cx, |_, window, cx| {
            if close_dialog {
                window.close_dialog(cx);
            }
            editor.update(cx, |state, cx| {
                let mut text = state.value().to_string();
                if !text.is_empty() && !text.ends_with("\n\n") {
                    text.push_str(if text.ends_with('\n') { "\n" } else { "\n\n" });
                }
                let start = text.len();
                text.push_str(&request);
                if !request.ends_with('\n') {
                    text.push('\n');
                }
                state.set_value(text, window, cx);
                state.set_selected_range(start..start, cx);
                state.focus(window, cx);
            });
        });
        // `set_value` emits no change event, so ask for the check by hand.
        self.lint(cx);
    }

    fn connection_name(&self) -> String {
        self.summary
            .as_ref()
            .map_or(String::new(), |s| s.name.to_string())
    }

    /// Pushes the current connection's history into the list.
    fn refresh_history(&mut self, cx: &mut Context<Self>) {
        let name = self.connection_name();
        let mine = self.library.read(cx).history(&name);
        self.history_list.update(cx, |list, cx| {
            list.delegate_mut().set(mine.into_iter());
            cx.notify();
        });
    }

    fn remember(&mut self, req: &Request, cx: &mut Context<Self>) {
        let at = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        let entry = Entry {
            connection: self.connection_name(),
            method: req.method.clone(),
            path: req.path.clone(),
            body: req.body.clone(),
            at,
        };
        self.library
            .update(cx, |library, cx| library.record(entry, cx));
    }

    fn clear_history(&mut self, cx: &mut Context<Self>) {
        let name = self.connection_name();
        self.library
            .update(cx, |library, cx| library.clear_history(&name, cx));
    }

    /// Pushes the current connection's saved items into the tree.
    fn refresh_saved(&mut self, cx: &mut Context<Self>) {
        let name = self.connection_name();
        let library = self.library.read(cx);
        let rows = |kind: Kind| -> Vec<Row> {
            library
                .saved(&name, kind)
                .map(|s| Row {
                    label: s.name.clone(),
                    text: s.text.clone(),
                })
                .collect()
        };
        let by_kind = [(Folder::Consoles, rows(Kind::Console))];
        self.targets.update(cx, |list, cx| {
            for (folder, rows) in by_kind {
                list.delegate_mut().set(folder, rows);
            }
            cx.notify();
        });
    }

    fn store(&mut self, kind: Kind, text: String, cx: &mut Context<Self>) {
        let item = Saved {
            connection: self.connection_name(),
            kind,
            name: saved::name_of(&text),
            text,
            at: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_secs()),
        };
        if !self
            .library
            .update(cx, |library, cx| library.add_saved(item, cx))
        {
            self.fail("Nothing to save, or it touches the security APIs.", cx);
        }
    }

    fn save_console(&mut self, cx: &mut Context<Self>) {
        let text = self.editor.read(cx).value().to_string();
        self.store(Kind::Console, text, cx);
    }

    /// The saved item selected in the tree, as its kind, text and label.
    fn selected_saved(&self, cx: &Context<Self>) -> Option<(Kind, String, String)> {
        let targets = self.targets.read(cx);
        let (folder, row) = targets.delegate().selected()?;
        let kind = match folder {
            Folder::Consoles => Kind::Console,
            _ => return None,
        };
        Some((kind, row.text.clone(), row.label.clone()))
    }

    /// Removes the saved item selected in the tree; other rows are not deletable.
    fn delete_saved(&mut self, cx: &mut Context<Self>) {
        let Some((kind, text, _)) = self.selected_saved(cx) else {
            return;
        };
        let name = self.connection_name();
        self.library.update(cx, |library, cx| {
            library.remove_saved(&name, kind, &text, cx)
        });
    }

    /// Asks for a new name for the selected saved item.
    fn rename_saved(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some((kind, text, label)) = self.selected_saved(cx) else {
            return;
        };
        let input = cx.new(|cx| InputState::new(window, cx));
        input.update(cx, |s, cx| s.set_value(label, window, cx));
        let console = cx.entity();
        window.open_dialog(cx, move |dialog, _, _| {
            let (input, console, text) = (input.clone(), console.clone(), text.clone());
            dialog.title("Rename").child(
                div()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .child(Input::new(&input).aria_label("Name"))
                    .child(
                        div().flex().justify_end().child(
                            Button::new("rename-ok")
                                .primary()
                                .small()
                                .label("Rename")
                                .on_click({
                                    let input = input.clone();
                                    move |_, window, cx| {
                                        let name = input.read(cx).value().to_string();
                                        let (console, text) = (console.clone(), text.clone());
                                        console.update(cx, |this, cx| {
                                            let conn = this.connection_name();
                                            this.library.update(cx, |library, cx| {
                                                library.rename_saved(&conn, kind, &text, &name, cx)
                                            });
                                        });
                                        window.close_dialog(cx);
                                    }
                                }),
                        ),
                    ),
            )
        });
    }

    /// Opens the connections dialog; `start` first resets the form to a blank or copied connection.
    fn open_connections(
        &mut self,
        start: Option<bool>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let panel = self.panel.clone();
        if let Some(copy) = start {
            panel.update(cx, |p, cx| p.start_new(copy, window, cx));
        }
        window.open_dialog(cx, move |dialog, _, _| {
            // The list (12rem) plus the form need more than the default 448px, or the two squeeze together.
            dialog.title("Connections").w(px(720.)).child(panel.clone())
        });
    }

    /// Drops the connection: stops refreshing and cancels whatever is in flight.
    fn disconnect(&mut self, cx: &mut Context<Self>) {
        (self.http, self.summary, self.meta) = (None, None, None);
        (self.refresh, self.catalog, self.sending) = (None, None, None);
        (self.online, self.run) = (false, Run::Idle);
        self.targets.update(cx, |list, cx| {
            list.delegate_mut().clear();
            cx.notify();
        });
        self.refresh_history(cx);
        cx.notify();
    }

    fn show_history(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let list = self.history_list.clone();
        let console = cx.entity();
        window.open_dialog(cx, move |dialog, _, _| {
            let console = console.clone();
            dialog.title("History").w(px(640.)).child(
                div()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .child(div().h_80().child(List::new(&list)))
                    .child(
                        div().flex().justify_end().child(
                            Button::new("clear-history")
                                .ghost()
                                .small()
                                .label("Clear history")
                                .on_click(move |_, _, cx| {
                                    console.update(cx, |this, cx| this.clear_history(cx))
                                }),
                        ),
                    ),
            )
        });
    }

    fn show_help(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        window.open_dialog(cx, move |dialog, _, _| {
            let line = |keys: String, what: &'static str| {
                div()
                    .flex()
                    .gap_4()
                    .child(div().w_40().child(keys))
                    .child(what)
            };
            dialog.title("Shortcuts").child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(line(SEND_SHORTCUT.into(), "Send the request at the cursor"))
                    .child(line(FORMAT_SHORTCUT.into(), "Re-indent the request body")),
            )
        });
    }

    /// Re-reads the nodes, templates, tasks and pipelines now.
    fn load_catalog(&mut self, cx: &mut Context<Self>) {
        let Some(http) = self.http.clone() else {
            return;
        };
        self.catalog = Some(cx.spawn(async move |this, cx| {
            let catalog = Catalog::fetch(&http).await;
            let _ = this.update(cx, |this, cx| {
                let rows = |items: Vec<Item>| -> Vec<Row> {
                    items
                        .into_iter()
                        .map(|i| Row {
                            label: i.label,
                            text: i.request,
                        })
                        .collect()
                };
                this.targets.update(cx, |list, cx| {
                    let d = list.delegate_mut();
                    d.set(Folder::Nodes, rows(catalog.nodes));
                    d.set(Folder::Templates, rows(catalog.templates));
                    d.set(Folder::Tasks, rows(catalog.tasks));
                    d.set(Folder::Pipelines, rows(catalog.pipelines));
                    cx.notify();
                });
            });
        }));
    }

    /// Reloads everything the tree lists from the cluster.
    fn refresh_tree(&mut self, meta: &es_client::ClusterMeta, cx: &mut Context<Self>) {
        meta.invalidate();
        self.load_catalog(cx);
    }

    /// Restarts completion against the cluster that just answered, and keeps its metadata fresh.
    fn connected(&mut self, done: &Connected, cx: &mut Context<Self>) {
        let started = LspClient::start(self.index.clone(), Some(done.version), done.meta.clone());
        let Ok(client) = started else { return };
        let [a, b, c] = done.version;
        self.summary = Some(Summary {
            name: done.name.clone().into(),
            version: format!("Elasticsearch {a}.{b}.{c}").into(),
        });
        self.client = client.clone();
        self.http = Some(done.client.clone());
        self.online = true;
        self.refresh_history(cx);
        self.refresh_saved(cx);
        self.load_catalog(cx);
        self.editor.update(cx, |state, _| install(state, &client));
        self.lint(cx);
        let (http, meta) = (done.client.clone(), done.meta.clone());
        self.meta = Some(meta.clone());
        self.refresh = Some(cx.spawn(async move |this, cx| {
            let mut tick = 0u32;
            loop {
                // A failed refresh keeps the previous data; the next tick retries.
                let reached = meta.refresh(&http).await.is_ok();
                let names = es_dsl_core::Fields::names(&meta);
                let aliases = es_dsl_core::Fields::aliases(&meta);
                let indices = names
                    .into_iter()
                    .filter(|n| aliases.binary_search(n).is_err());
                let rows = |names: Vec<String>| -> Vec<Row> {
                    names
                        .into_iter()
                        .map(|n| Row {
                            text: search_text(&n),
                            label: n,
                        })
                        .collect()
                };
                let (indices, aliases) = (rows(indices.collect()), rows(aliases));
                let alive = this.update(cx, |this, cx| {
                    if this.online != reached {
                        this.online = reached;
                        cx.notify();
                    }
                    this.targets.update(cx, |list, cx| {
                        let d = list.delegate_mut();
                        let changed =
                            d.set(Folder::Indices, indices) | d.set(Folder::Aliases, aliases);
                        if changed {
                            cx.notify();
                        }
                    })
                });
                if alive.is_err() {
                    return;
                }
                tick += 1;
                if tick.is_multiple_of(CATALOG_EVERY) {
                    let _ = this.update(cx, |this, cx| this.load_catalog(cx));
                }
                cx.background_executor().timer(REFRESH_EVERY).await;
            }
        }));
    }
}

/// Title-bar controls must not start a window drag, which would swallow their click.
fn no_drag(_: &MouseDownEvent, window: &mut Window, cx: &mut App) {
    window.prevent_default();
    cx.stop_propagation();
}

fn install(state: &mut EditorState, client: &Arc<LspClient>) {
    let lsp = state.lsp_mut();
    lsp.completion_provider = Some(Rc::new(LspCompletion(client.clone())));
    lsp.hover_provider = Some(Rc::new(LspHover(client.clone())));
    lsp.set_completion_highlighter(Rc::new(crate::completion::highlight_subsequence));
}

/// Follows the system appearance, then rounds the theme: a theme load resets the radii.
fn restyle(window: Option<&mut Window>, cx: &mut App) {
    Theme::sync_system_appearance(window, cx);
    // The sync picks hover-only scrollbars off macOS; a table or a long request should show that there is more.
    Theme::set_scrollbar_mode(ScrollbarMode::Always, cx);
    Theme::update(cx, |theme| {
        theme.radius = px(8.0);
        theme.radius_lg = px(12.0);
    });
}

impl Console {
    fn render_titlebar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let label = self
            .summary
            .as_ref()
            .map_or("Connect…".into(), |s| s.name.clone());
        let muted = cx.theme().muted_foreground;
        let console = cx.entity();
        let version = self
            .summary
            .as_ref()
            .map_or(SharedString::default(), |s| s.version.clone());
        // Flat, in the window ground colour, so the title bar and the islands read as one surface.
        let ground = cx
            .theme()
            .background
            .blend(cx.theme().foreground.opacity(0.09));
        TitleBar::new()
            .bg(ground)
            .border_0()
            .child(self.menu_bar.clone())
            .child(self.render_toolbar(cx))
            .child(
                div()
                    .flex_1()
                    .flex()
                    .justify_end()
                    .text_sm()
                    .text_color(muted)
                    .child(version),
            )
            .child(div().on_mouse_down(MouseButton::Left, no_drag).child({
                let (panel, current) = (self.panel.clone(), label.to_string());
                Button::new("connections")
                    .ghost()
                    .icon(IconName::Database)
                    .small()
                    .label(label)
                    .dropdown_caret(true)
                    .dropdown_menu(move |menu, _, cx| {
                        let names = panel.read(cx).names();
                        let menu = names.into_iter().fold(menu, |menu, name| {
                            let panel = panel.clone();
                            let target = name.clone();
                            menu.item(
                                PopupMenuItem::new(name.clone())
                                    .checked(name == current)
                                    .on_click(move |_, window, cx| {
                                        panel.update(cx, |p, cx| p.switch_to(&target, window, cx))
                                    }),
                            )
                        });
                        let console = console.clone();
                        menu.separator()
                            .item(PopupMenuItem::new("Manage connections…").on_click(
                                move |_, window, cx| {
                                    console.update(cx, |this, cx| {
                                        this.open_connections(None, window, cx)
                                    })
                                },
                            ))
                    })
            }))
    }

    /// Always-visible commands, in the title bar next to the menus; the same ones are in the menus.
    fn render_toolbar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let sending = matches!(self.run, Run::Sending);
        div()
            .flex()
            .items_center()
            .gap_0p5()
            .ml_2()
            .on_mouse_down(MouseButton::Left, no_drag)
            .child(
                Button::new("send")
                    .small()
                    .icon(IconName::Play)
                    .primary()
                    .disabled(sending)
                    .tooltip(format!("Send the request at the cursor ({SEND_SHORTCUT})"))
                    .on_click(cx.listener(|this, _, window, cx| this.send(window, cx))),
            )
            .child(
                Button::new("cancel")
                    .ghost()
                    .small()
                    .icon(IconName::Square)
                    .disabled(!sending)
                    .tooltip("Cancel the request in flight")
                    .on_click(cx.listener(|this, _, _, cx| this.cancel(cx))),
            )
            .child(
                Button::new("format")
                    .ghost()
                    .small()
                    .icon(IconName::Braces)
                    .tooltip(format!("Re-indent the request body ({FORMAT_SHORTCUT})"))
                    .on_click(cx.listener(|this, _, window, cx| this.format(window, cx))),
            )
            .child(
                Button::new("toggle-tree")
                    .ghost()
                    .small()
                    .icon(IconName::PanelLeft)
                    .selected(self.sidebar_open)
                    .tooltip("Show or hide the tree")
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.sidebar_open = !this.sidebar_open;
                        cx.notify();
                    })),
            )
            .child(
                Button::new("history")
                    .ghost()
                    .small()
                    .icon(IconName::Clock)
                    .tooltip("History")
                    .on_click(cx.listener(|this, _, window, cx| this.show_history(window, cx))),
            )
    }

    /// Bottom bar: the outcome of the last request on the left, the document check on the right.
    fn render_status(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let (muted, danger, warning) = (theme.muted_foreground, theme.danger, theme.warning);
        let colored = |color, text: SharedString| div().text_color(color).child(text);
        let run = match &self.run {
            Run::Idle => None,
            Run::Sending => Some(colored(muted, "Sending".into())),
            Run::Failed(why) => Some(colored(danger, why.clone())),
            Run::Done {
                status,
                took,
                bytes,
                cut,
                found,
            } => {
                let mut line = format!(
                    "{status} · {} ms · {}",
                    took.as_millis(),
                    human_size(*bytes)
                );
                if let Some(found) = found {
                    line.push_str(&format!(" · {found}"));
                }
                if *cut {
                    line.push_str(" · JSON shows the first 2 MB");
                }
                Some(colored(
                    if *status >= 400 { danger } else { muted },
                    line.into(),
                ))
            }
        };
        let problems = self.problems.map(|n| {
            let text = match n {
                0 => "No problems".to_string(),
                1 => "1 problem".to_string(),
                n => format!("{n} problems"),
            };
            colored(if n == 0 { muted } else { warning }, text.into())
        });
        let mut bar = StatusBar::new();
        if !self.crumbs.is_empty() {
            bar = bar.left(colored(muted, self.crumbs.clone()));
        }
        if let Some(run) = run {
            bar = bar.left(run);
        }
        if let Some(problems) = problems {
            bar = bar.right(problems);
        }
        bar
    }

    /// The response viewer: view tabs and actions above the body. Only built once a response exists.
    fn render_response(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let palette = TreePalette {
            key: theme.blue,
            string: theme.green,
            number: theme.yellow,
            keyword: theme.magenta,
            muted: theme.muted_foreground,
            guide: theme.border,
        };
        let mut views = Vec::new();
        if self.hits.is_some() {
            views.push((View::Table, "Table", IconName::Table2));
        }
        if self.tree.is_some() {
            views.push((View::Tree, "Tree", IconName::ListTree));
        }
        views.push((View::Json, "JSON", IconName::Braces));
        let selected = views
            .iter()
            .position(|(v, ..)| *v == self.view)
            .unwrap_or(0);
        let order: Vec<View> = views.iter().map(|(v, ..)| *v).collect();
        let tabs = TabBar::new("response-views")
            .segmented()
            .small()
            .selected_index(selected)
            .on_click(cx.listener(move |this, ix: &usize, _, cx| {
                if let Some(view) = order.get(*ix) {
                    this.view = *view;
                    cx.notify();
                }
            }))
            .children(
                views
                    .iter()
                    .map(|(_, label, icon)| Tab::new().icon(*icon).label(*label)),
            );
        let table = self.hits.as_ref().filter(|_| self.view == View::Table);
        let outline = self.tree.as_ref().filter(|_| self.view == View::Tree);
        div()
            .size_full()
            .flex()
            .flex_col()
            .gap_2()
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(tabs)
                    .child(
                        div()
                            .flex()
                            .gap_1()
                            .children((self.view == View::Tree).then(|| {
                                Button::new("copy-path")
                                    .ghost()
                                    .icon(IconName::Copy)
                                    .small()
                                    .label("Copy path")
                                    .on_click(cx.listener(|this, _, _, cx| this.copy_path(cx)))
                            }))
                            .child(
                                Button::new("save-response")
                                    .ghost()
                                    .icon(IconName::Download)
                                    .small()
                                    .label("Save…")
                                    .on_click(cx.listener(|this, _, _, cx| this.save_response(cx))),
                            ),
                    ),
            )
            .child(
                div().flex_1().min_h_0().child(match (table, outline) {
                    (Some(t), _) => DataTable::new(t).stripe(true).into_any_element(),
                    (None, Some(t)) => tree(t, move |_, entry, selected, _, _| {
                        let chevron = if !entry.is_folder() {
                            None
                        } else if entry.is_expanded() {
                            Some(IconName::ChevronDown)
                        } else {
                            Some(IconName::ChevronRight)
                        };
                        let label = entry.item().label.to_string();
                        // shortcut: a key containing ": " is split early, only its colors are off.
                        let (key, value) = match label.split_once(": ") {
                            Some((k, v)) => (Some(k.to_string()), v.to_string()),
                            None => (None, label),
                        };
                        let (kind, value_color) = match value.as_bytes().first() {
                            Some(b'"') => (IconName::Quote, palette.string),
                            Some(b'{') => (IconName::Braces, palette.muted),
                            Some(b'[') => (IconName::Brackets, palette.muted),
                            _ if value == "null" => (IconName::CircleDot, palette.keyword),
                            _ if value == "true" || value == "false" => {
                                (IconName::ToggleLeft, palette.keyword)
                            }
                            _ => (IconName::Hash, palette.number),
                        };
                        ListItem::new(SharedString::from(entry.item().id.to_string()))
                            .selected(selected)
                            .text_sm()
                            .p_0()
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .h(rems(1.75))
                                    .gap_1()
                                    // One guide line per ancestor level, like an IDE's indent guides.
                                    .children((0..entry.depth()).map(|_| {
                                        div()
                                            .w(rems(1.))
                                            .h_full()
                                            .border_l_1()
                                            .border_color(palette.guide)
                                    }))
                                    .child(div().w_1())
                                    .child(match chevron {
                                        Some(i) => Icon::new(i)
                                            .size_3()
                                            .text_color(palette.muted)
                                            .into_any_element(),
                                        None => div().size_3().into_any_element(),
                                    })
                                    .child(Icon::new(kind).size_3().text_color(value_color))
                                    .children(key.map(|k| {
                                        div()
                                            .font_weight(FontWeight::MEDIUM)
                                            .text_color(palette.key)
                                            .child(format!("{k}:"))
                                    }))
                                    .child(div().text_color(value_color).child(value)),
                            )
                    })
                    .into_any_element(),
                    (None, None) => Editor::new(&self.response)
                        .bordered(false)
                        .readonly(true)
                        .h_full()
                        .into_any_element(),
                }),
            )
    }
}

#[cfg(target_os = "macos")]
const FORMAT_SHORTCUT: &str = "Cmd+Shift+F";
#[cfg(not(target_os = "macos"))]
const FORMAT_SHORTCUT: &str = "Ctrl+Shift+F";

#[cfg(target_os = "macos")]
const SEND_SHORTCUT: &str = "Cmd+Enter";
#[cfg(not(target_os = "macos"))]
const SEND_SHORTCUT: &str = "Ctrl+Enter";

fn human_size(bytes: usize) -> String {
    match bytes {
        0..1024 => format!("{bytes} B"),
        1024..1_048_576 => format!("{:.1} KB", bytes as f64 / 1024.0),
        _ => format!("{:.1} MB", bytes as f64 / 1_048_576.0),
    }
}

impl Render for Console {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Panel sizes are pixels; derive them from rems so they follow the theme's scale.
        let rem = window.rem_size();
        let size = |r: f32| rems(r).to_pixels(rem);
        let sidebar = self.http.is_some() && self.sidebar_open;
        let response = matches!(self.run, Run::Done { .. });
        // Editor and response start at 4:6 of the width the tree leaves over (minus the side margins).
        let sidebar_width = size(20.0);
        let open_tree = if sidebar { sidebar_width } else { px(0.) };
        let response_width = (window.viewport_size().width - open_tree - size(1.0)) * 0.6;
        let tool = |id: &'static str, icon: IconName, tip: &'static str| {
            Button::new(id).ghost().small().icon(icon).tooltip(tip)
        };
        let meta = self.meta.clone();
        let theme = cx.theme();
        let dot = if self.online {
            theme.success
        } else {
            theme.danger
        };
        let (radius, surface) = (theme.radius_lg, theme.background);
        // Islands: panels float as raised cards on a slightly darker window ground.
        let ground = theme.background.blend(theme.foreground.opacity(0.09));
        let card = move || {
            div()
                .size_full()
                .overflow_hidden()
                .rounded(radius)
                .bg(surface)
        };
        // Like an IDEA tool window: a title row, the actions as one quiet icon row under it, then the tree.
        let header = div()
            .flex()
            .items_center()
            .gap_2()
            .px_2()
            .pt_1()
            .child(
                div()
                    .flex_1()
                    .truncate()
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(SharedString::from(self.connection_name())),
            )
            .child(div().size_1p5().flex_none().rounded_full().bg(dot));
        let actions = div()
            .flex()
            .items_center()
            .gap_0p5()
            .px_1()
            .child(
                tool(
                    "save-console",
                    IconName::Save,
                    "Save the whole editor as a console",
                )
                .on_click(cx.listener(|this, _, _, cx| this.save_console(cx))),
            )
            .child(
                tool(
                    "rename-saved",
                    IconName::Pencil,
                    "Rename the selected saved item",
                )
                .on_click(cx.listener(|this, _, window, cx| this.rename_saved(window, cx))),
            )
            .child(
                tool(
                    "delete-saved",
                    IconName::Trash,
                    "Delete the selected saved item",
                )
                .on_click(cx.listener(|this, _, _, cx| this.delete_saved(cx))),
            )
            .child(div().flex_1())
            .child(
                tool("tree-filter", IconName::Search, "Filter the tree")
                    .selected(self.filter_open)
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.filter_open = !this.filter_open;
                        let open = this.filter_open;
                        this.targets.update(cx, |list, cx| {
                            if !open {
                                list.set_query("", window, cx);
                            }
                            list.set_searchable(open, cx);
                            if open {
                                list.focus(window, cx);
                            }
                        });
                        cx.notify();
                    })),
            )
            .child(
                tool("tree-refresh", IconName::RefreshCw, "Reload the tree").on_click(cx.listener(
                    move |this, _, _, cx| {
                        if let Some(meta) = &meta {
                            this.refresh_tree(meta, cx);
                        }
                    },
                )),
            )
            .child(
                tool("disconnect", IconName::Unplug, "Disconnect")
                    .on_click(cx.listener(|this, _, _, cx| this.disconnect(cx))),
            );
        let targets = card()
            .bg(surface)
            .p_1p5()
            .flex()
            .flex_col()
            .gap_1()
            .child(header)
            .child(actions)
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .child(List::new(&self.targets).search_placeholder("Filter")),
            );
        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(ground)
            .on_action(cx.listener(|this, _: &Send, window, cx| this.send(window, cx)))
            .on_action(cx.listener(|this, _: &Format, window, cx| this.format(window, cx)))
            .on_action(cx.listener(|this, _: &Newline, window, cx| this.newline(window, cx)))
            .on_action(cx.listener(|this, _: &CancelRequest, _, cx| this.cancel(cx)))
            .on_action(cx.listener(|this, _: &ToggleTree, _, cx| {
                this.sidebar_open = !this.sidebar_open;
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &ToggleReadonly, _, cx| {
                this.readonly = !this.readonly;
                cx.notify();
            }))
            .on_action(
                cx.listener(|this, _: &ShowHistory, window, cx| this.show_history(window, cx)),
            )
            .on_action(
                cx.listener(|this, _: &ShowShortcuts, window, cx| this.show_help(window, cx)),
            )
            .child(self.render_titlebar(cx))
            .child(
                // The tree keeps its width: a drag on another handle never takes space from it. Panels clip their content and the editors do not wrap, so a narrower panel
                // hides text instead of reflowing it.
                div().flex_1().min_h_0().mx_2().mb_2().child(
                    h_resizable("console-split")
                        .child(
                            resizable_panel()
                                .visible(sidebar)
                                .size(sidebar_width)
                                .fixed()
                                .size_range(size(16.0)..size(40.0))
                                .child(div().size_full().pr_2().child(targets)),
                        )
                        .child(
                            resizable_panel().child(
                                card()
                                    .p_1p5()
                                    .child(Editor::new(&self.editor).bordered(false).h_full()),
                            ),
                        )
                        .child(
                            resizable_panel()
                                .visible(response)
                                .size(response_width)
                                .size_range(size(16.0)..size(120.0))
                                .child(
                                    div()
                                        .size_full()
                                        .pl_2()
                                        .child(card().p_2().child(self.render_response(cx))),
                                ),
                        ),
                ),
            )
            .child(self.render_status(cx))
    }
}

/// `query › bool › must › [0]`, the way the status bar shows a JSON path.
fn breadcrumb(path: &[es_dsl_core::Seg]) -> SharedString {
    path.iter()
        .map(|seg| match seg {
            es_dsl_core::Seg::Key(k) => k.clone(),
            es_dsl_core::Seg::Index(i) => format!("[{i}]"),
        })
        .collect::<Vec<_>>()
        .join(" › ")
        .into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lsp_client::sample_index;
    use gpui_kit::TestAppContext;
    use gpui_kit::test::TestWindowExt as _;

    /// Real editor, real provider, real server: typing a key prefix opens the menu and Enter inserts the key.
    #[gpui_kit::test]
    fn typing_a_key_prefix_completes_through_the_lsp(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        let client = LspClient::start(sample_index(), None, es_dsl_core::NoFields).unwrap();
        let (window, console) = cx
            .update(|cx| {
                gpui_kit::open_window(Default::default(), cx, |window, cx| {
                    let library = cx.new(|_| Library::blank());
                    cx.new(|cx| {
                        Console::new(
                            "",
                            sample_index(),
                            client,
                            library,
                            PathBuf::from("unused.json"),
                            window,
                            cx,
                        )
                    })
                })
            })
            .unwrap();
        let editor = cx.update(|cx| console.read(cx).editor.clone());
        let doc = "POST idx/_search\n{\n  \"";

        cx.update_window(window, |_, window, cx| {
            window.activate_window();
            editor.update(cx, |state, cx| {
                state.focus(window, cx);
                state.set_value(doc, window, cx);
                state.set_selected_range(doc.len()..doc.len(), cx);
            });
        })
        .unwrap();
        cx.run_until_parked();
        cx.update_window(window, |_, window, cx| window.input("q", cx))
            .unwrap();
        // The provider runs on a background task and answers asynchronously.
        cx.run_until_parked();
        cx.update_window(window, |_, window, cx| {
            window.render_frame(cx);
            assert!(
                window.try_find("completion-menu").is_some(),
                "menu did not open"
            );
        })
        .unwrap();

        cx.update_window(window, |_, window, cx| window.press("enter", cx))
            .unwrap();
        cx.run_until_parked();
        let value = cx.update(|cx| editor.read(cx).value().to_string());
        // The typed `"q` is replaced by the whole quoted key.
        assert_eq!(value, "POST idx/_search\n{\n  \"query\": ");
    }

    /// Real editor and server: an unknown key is underlined once the typing pause has passed.
    #[gpui_kit::test]
    fn an_unknown_key_is_underlined_after_a_pause(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        let client = LspClient::start(sample_index(), None, es_dsl_core::NoFields).unwrap();
        let (window, console) = cx
            .update(|cx| {
                gpui_kit::open_window(Default::default(), cx, |window, cx| {
                    let library = cx.new(|_| Library::blank());
                    cx.new(|cx| {
                        Console::new(
                            "",
                            sample_index(),
                            client,
                            library,
                            PathBuf::from("unused.json"),
                            window,
                            cx,
                        )
                    })
                })
            })
            .unwrap();
        let editor = cx.update(|cx| console.read(cx).editor.clone());
        let doc = "POST idx/_search\n{\"nope\": 1";
        cx.update_window(window, |_, window, cx| {
            window.activate_window();
            editor.update(cx, |state, cx| {
                state.focus(window, cx);
                // `set_value` emits no change event, so finish the document by typing.
                state.set_value(doc, window, cx);
                state.set_selected_range(doc.len()..doc.len(), cx);
            });
        })
        .unwrap();
        cx.run_until_parked();
        cx.update_window(window, |_, window, cx| window.input("}", cx))
            .unwrap();
        cx.executor().advance_clock(LINT_DELAY * 2);
        cx.run_until_parked();
        let shown = cx.update(|cx| editor.read(cx).diagnostics().map(|d| d.len()));
        assert_eq!(shown, Some(1));
        assert_eq!(cx.update(|cx| console.read(cx).problems), Some(1));
    }
}
