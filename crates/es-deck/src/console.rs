//! The request editor: a GPUI Kit code editor whose completions come from the in-process LSP.

use std::collections::HashMap;
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
use gpui_kit::component::input::{
    Editor, EditorState, Enter, Input, InputEditsEvent, InputEvent, InputState, ShowCompletions,
};
use gpui_kit::component::list::ListItem;
use gpui_kit::component::list::{List, ListEvent, ListState};
use gpui_kit::component::menu::{
    AppMenuBar, ContextMenuExt as _, DropdownMenu as _, PopupMenuItem,
};
use gpui_kit::component::resizable::{ResizableState, h_resizable, resizable_panel, v_resizable};
use gpui_kit::component::status_bar::StatusBar;
use gpui_kit::component::tab::{Tab, TabBar};
use gpui_kit::component::table::TableState;
use gpui_kit::component::tree::{TreeItem, TreeState, tree};
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, GlobalState, Selectable as _, Sizable as _, WindowExt as _,
};
use gpui_kit::{
    App, AppContext as _, ClipboardItem, Context, Entity, Focusable as _, FontWeight, Hsla,
    InteractiveElement as _, IntoElement, KeyBinding, Menu, MenuItem, MouseButton, MouseDownEvent,
    ParentElement as _, Render, Role, SharedString, StatefulInteractiveElement as _, Styled as _,
    Task, TestSupportExt as _, Window, actions, div, rems,
};

use crate::appearance::{self, Pref};
use crate::completion::{LspCompletion, LspHover};
use crate::connect::{ConnectPanel, Connected, row_id};
use crate::data_browser::DataBrowser;
use crate::history_list::HistoryList;
use crate::hits_table::{self, HitsTable};
use crate::library::{Changed, Library};
use crate::lsp_client::LspClient;
use crate::paint::Paint;
use crate::schema_source;
use crate::targets::{
    ConsoleOp, Folder, OpenInDataBrowser, OpenInRestConsole, Row, Targets, search_text,
};
use es_dsl_core::Token;

/// How often a connected console re-reads cluster metadata for targets it has not seen yet.
const REFRESH_EVERY: Duration = Duration::from_secs(2);

/// Nodes, templates, tasks and pipelines are re-read every this many metadata refreshes.
const CATALOG_EVERY: u32 = 30;

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
        ShowShortcuts,
        NextTab,
        PreviousTab,
        CloseTab,
        UseSystemAppearance,
        UseLightAppearance,
        UseDarkAppearance
    ]
);

/// The application menu: one Action per command, shared with the key bindings. Native on macOS,
/// drawn by `AppMenuBar` in the title bar elsewhere. The two toggles carry a check mark so the
/// menu itself says which state is on.
pub fn app_menus(pref: Pref, readonly: bool) -> Vec<Menu> {
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
            MenuItem::action("Readonly", ToggleReadonly).checked(readonly),
            MenuItem::separator(),
            MenuItem::action("Follow system appearance", UseSystemAppearance)
                .checked(pref == Pref::System),
            MenuItem::action("Light appearance", UseLightAppearance).checked(pref == Pref::Light),
            MenuItem::action("Dark appearance", UseDarkAppearance).checked(pref == Pref::Dark),
            MenuItem::separator(),
            MenuItem::action("Next tab", NextTab),
            MenuItem::action("Previous tab", PreviousTab),
            MenuItem::action("Close tab", CloseTab),
        ]),
        Menu::new("Help").items([MenuItem::action("Shortcuts", ShowShortcuts)]),
    ]
}

/// Installs the menus in both places they appear: the platform menu bar and the `AppMenuBar` the
/// title bar draws off macOS. Every toggle that carries a check mark rebuilds them.
pub fn set_menus(cx: &mut App, pref: Pref, readonly: bool) {
    cx.set_menus(app_menus(pref, readonly));
    GlobalState::global_mut(cx).set_app_menus(
        app_menus(pref, readonly)
            .into_iter()
            .map(|m| m.owned())
            .collect(),
    );
}

/// Binds Ctrl/Cmd+Enter to [`Send`] and Ctrl/Cmd+Shift+F to [`Format`] inside editors. It must run
/// after `gpui_kit::init`: a later binding wins over the editor's own Ctrl/Cmd+Enter, which would
/// insert a newline. Ctrl+Space asks for completions where no character has been typed yet.
pub fn bind_keys(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new(SEND_KEY, Send, Some("Input")),
        KeyBinding::new(FORMAT_KEY, Format, Some("Input")),
        KeyBinding::new("enter", Newline, Some("Input")),
        KeyBinding::new(
            "ctrl-space",
            ShowCompletions,
            Some("Input && mode == editor"),
        ),
        KeyBinding::new(OPEN_BROWSER_KEY, OpenInDataBrowser, Some("List")),
        KeyBinding::new(SEND_KEY, OpenInRestConsole, Some("List")),
        KeyBinding::new("ctrl-tab", NextTab, None),
        KeyBinding::new("ctrl-shift-tab", PreviousTab, None),
        KeyBinding::new(CLOSE_TAB_KEY, CloseTab, None),
    ]);
}

#[cfg(target_os = "macos")]
const OPEN_BROWSER_KEY: &str = "cmd-down";
#[cfg(not(target_os = "macos"))]
const OPEN_BROWSER_KEY: &str = "ctrl-down";

#[cfg(target_os = "macos")]
const SEND_KEY: &str = "cmd-enter";
#[cfg(not(target_os = "macos"))]
const SEND_KEY: &str = "ctrl-enter";

#[cfg(target_os = "macos")]
const FORMAT_KEY: &str = "cmd-shift-f";
#[cfg(not(target_os = "macos"))]
const FORMAT_KEY: &str = "ctrl-shift-f";

#[cfg(target_os = "macos")]
const CLOSE_TAB_KEY: &str = "cmd-w";
#[cfg(not(target_os = "macos"))]
const CLOSE_TAB_KEY: &str = "ctrl-w";

/// How a search response is shown; other responses are always JSON.
#[derive(Clone, Copy, PartialEq)]
enum View {
    Table,
    Tree,
    Json,
}

/// How the editor and the response share their height. Collapsed keeps the response's header
/// row so the way back stays one click away; maximized hides the editor while data is read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ResultLayout {
    Split,
    Collapsed,
    Maximized,
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

/// Marks a tab key as a Data Browser's, so it cannot equal the name of a saved console.
const BROWSER_PREFIX: &str = "table:";

fn browser_key(target: &str) -> String {
    format!("{BROWSER_PREFIX}{target}")
}

/// The response area of a tab that is not shown, kept so switching back finds it as it was.
struct Pane {
    run: Run,
    hits: Option<Entity<TableState<HitsTable>>>,
    tree: Option<Entity<TreeState>>,
    full_body: Option<String>,
    view: View,
    /// The text of the response editor.
    shown: String,
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
    /// How the editor and the response share their height.
    result_layout: ResultLayout,
    /// The editor/response split, owned here so its sizes survive the other layouts.
    split: Entity<ResizableState>,
    /// Blocks every request that would change the cluster; per session, off at start.
    readonly: bool,
    /// The appearance the user chose; only `System` lets later OS changes through.
    theme: Pref,
    /// Where the choice is remembered, next to the other config files.
    appearance_file: PathBuf,
    /// Diagnostics in the editor text as of the last check; `None` before the first one.
    problems: Option<usize>,
    /// JSON path at the cursor, as shown in the status bar; empty outside a body.
    crumbs: SharedString,
    /// The request under the cursor, as framed in the editor.
    active_block: Option<Range<usize>>,
    run: Run,
    /// The in-flight request; dropping it aborts the call.
    sending: Option<Task<()>>,
    index: Arc<Index>,
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
    /// The config directory, where downloaded indexes are cached.
    config_dir: PathBuf,
    /// Fetching the index of the connected version; replaced, so cancelled, by the next connection.
    schema_load: Option<Task<()>>,
    /// The console the editor shows, which the autosave writes to.
    active: String,
    /// The response area of every tab but the active one, by console name.
    panes: HashMap<String, Pane>,
    /// The Data Browser tabs, by tab key (see [`browser_key`]); each is also in `tabs`.
    browsers: HashMap<String, Entity<DataBrowser>>,
    /// The consoles shown as tabs above the editor, in opening order; `active` is one of them.
    tabs: Vec<String>,
    paint: Paint,
    /// Colors of the response editor's JSON.
    response_paint: Paint,
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
        // The kit inserts a completion's text verbatim, so a completion that supplies `{}` or `[]`
        // leaves the caret after the closing bracket. Move it between the brackets, where the user
        // continues. Only the completion's own shape (`": {}"` / `": []"`) qualifies; typing `{`
        // already auto-closes with the caret inside and never produces this suffix.
        cx.subscribe(&editor, |this, _, ev: &InputEditsEvent, cx| {
            let Some((edit, after)) = ev.edits().last() else {
                return;
            };
            let inserted = after.slice(edit.start_byte..edit.new_end_byte).to_string();
            if edit.old_end_byte == edit.start_byte
                || !(inserted.ends_with(": {}") || inserted.ends_with(": []"))
            {
                return;
            }
            if this.editor.read(cx).cursor() != edit.new_end_byte {
                return;
            }
            let inside = edit.new_end_byte - 1;
            let editor = this.editor.clone();
            cx.spawn(async move |_, cx| {
                editor.update(cx, |state, cx| state.set_selected_range(inside..inside, cx));
            })
            .detach();
        })
        .detach();
        let paint = Paint::new(&editor, false, cx);
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
        let response_paint = Paint::new(&response, true, cx);
        cx.subscribe(&response, |this, response, ev: &InputEvent, cx| {
            if matches!(ev, InputEvent::Change) {
                let text = response.read(cx).value().to_string();
                this.response_paint.refresh(&text, cx);
            }
        })
        .detach();
        let targets = cx.new(|cx| ListState::new(Targets::default(), window, cx));
        let owner = cx.weak_entity();
        targets.update(cx, |list, _| {
            list.delegate_mut()
                .set_menu(std::rc::Rc::new(move |op, name, window, cx| {
                    let _ = owner.update(cx, |this, cx| match op {
                        ConsoleOp::Rename => this.rename_console(name, window, cx),
                        ConsoleOp::Delete => this.delete_console(name, cx),
                        ConsoleOp::OpenBrowser => this.open_browser(name, cx),
                        ConsoleOp::OpenConsole => this.open_named(name, cx),
                    });
                }));
        });
        cx.subscribe(&targets, |this, list, ev: &ListEvent, cx| {
            if let ListEvent::Confirm(ix) = ev {
                let mut double = false;
                let picked = list.update(cx, |list, cx| {
                    double = list.delegate_mut().double_click(*ix);
                    let picked = list.delegate_mut().activate(*ix);
                    cx.notify();
                    picked
                });
                let label = list
                    .read(cx)
                    .delegate()
                    .selected()
                    .map(|(_, row)| row.label.clone());
                if let (Some((folder, text)), Some(label)) = (picked, label) {
                    match folder {
                        Folder::Consoles => this.open_console(label, text, cx),
                        // A single click only selects; the second one opens the object's console.
                        Folder::Indices | Folder::Aliases => {
                            if double {
                                this.open_browser(label, cx);
                            }
                        }
                        _ => this.append_request(text, false, cx),
                    }
                }
            }
        })
        .detach();
        // The kit does not follow the system appearance by itself: apply the remembered choice now
        // and, while it is `System`, on every change.
        let appearance_file = connections.with_file_name("appearance.json");
        let theme = Pref::load(&appearance_file);
        appearance::apply(theme, Some(window), cx);
        cx.observe_window_appearance(window, |this, window, cx| {
            if this.theme == Pref::System {
                appearance::apply(this.theme, Some(window), cx);
            }
        })
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
        let config_dir = connections.parent().map(PathBuf::from).unwrap_or_default();
        let panel = cx.new(|cx| ConnectPanel::new(connections, window, cx));
        cx.subscribe(&panel, |this, _, done: &Connected, cx| {
            this.connected(done, cx)
        })
        .detach();
        // The editor's draft is restored on startup, so the connection it ran against is too.
        panel.update(cx, |p, cx| p.reconnect_last(window, cx));
        let split = cx.new(|_| ResizableState::default());
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
            result_layout: ResultLayout::Split,
            split,
            readonly: false,
            theme,
            appearance_file,
            problems: None,
            crumbs: SharedString::default(),
            active_block: None,
            run: Run::Idle,
            sending: None,
            index: Arc::new(index),
            client,
            online: false,
            sidebar_open: true,
            filter_open: false,
            meta: None,
            refresh: None,
            lint: None,
            config_dir,
            schema_load: None,
            active: saved::AUTO_CONSOLE.into(),
            panes: HashMap::new(),
            browsers: HashMap::new(),
            tabs: vec![saved::AUTO_CONSOLE.into()],
            paint,
            response_paint,
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
                // The menu propagates while accepting so a key it ignores still reaches the
                // editor. Here it took the key: without this the same Enter would also run the
                // kit's newline and split the line under the inserted completion.
                cx.stop_propagation();
                return;
            }
        }
        cx.propagate();
    }

    /// Re-checks the document shortly after the last edit and shows the result as underlines.
    fn lint(&mut self, cx: &mut Context<Self>) {
        // Every text change, typed or set by code, ends up here, so the colors are redone too.
        let text = self.editor.read(cx).value().to_string();
        // Classified on the spot, not after the lint delay: keys would otherwise show the default
        // color first and change once the schema pass lands.
        let keys = es_dsl_core::key_tokens(&self.index, &text, None);
        self.paint.set_custom(&text, keys, cx);
        self.lint = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(LINT_DELAY).await;
            let Ok((text, client)) = this.update(cx, |this, cx| {
                let text = this.editor.read(cx).value().to_string();
                // This task is replaced by every edit, so the draft is saved once typing pauses.
                this.library
                    .update(cx, |library, cx| library.save_draft(text.clone(), cx));
                this.save_active(cx);
                (text, this.client.clone())
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
        // The filter box of a Data Browser shares the key context; its keys are not console keys.
        if self.browsers.contains_key(&self.active) {
            return;
        }
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
        // A new answer should be visible even when the last one was folded away.
        self.result_layout = ResultLayout::Split;
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
        if self.browsers.contains_key(&self.active) {
            return;
        }
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

    /// Shows the console called `name` in the editor and makes it the one that autosaves.
    fn open_console(&mut self, name: String, text: String, cx: &mut Context<Self>) {
        let editor = self.editor.clone();
        let Some(window) = cx.active_window() else {
            return;
        };
        // The pause before the autosave would otherwise lose the last edits of the console being left.
        self.save_active(cx);
        if !self.tabs.contains(&name) {
            self.tabs.push(name.clone());
        }
        // A check still waiting would save the old text under the new name.
        self.lint = None;
        let leaving = std::mem::replace(&mut self.active, name.clone());
        let shown = (leaving != name).then(|| self.swap_pane(leaving, &name, cx));
        let response = self.response.clone();
        // Opened from a list click, the window is busy with that event and refuses a direct update,
        // which would leave the old text in the editor; a task runs once the event is done.
        cx.spawn(async move |this, cx| {
            let _ = window.update(cx, |_, window, cx| {
                if let Some(shown) = shown {
                    response.update(cx, |state, cx| state.set_value(shown, window, cx));
                }
                editor.update(cx, |state, cx| {
                    state.set_value(text, window, cx);
                    state.focus(window, cx);
                });
            });
            let _ = this.update(cx, |this, cx| this.lint(cx));
        })
        .detach();
    }

    /// Parks the response area under `leaving` and brings back `entering`'s, or an empty one.
    /// Returns the text the response editor must show. A request still in flight is cancelled:
    /// its answer would otherwise land in the wrong tab.
    fn swap_pane(&mut self, leaving: String, entering: &str, cx: &mut Context<Self>) -> String {
        self.sending = None;
        let run = match std::mem::replace(&mut self.run, Run::Idle) {
            Run::Sending => Run::Idle,
            run => run,
        };
        let pane = Pane {
            run,
            hits: self.hits.take(),
            tree: self.tree.take(),
            full_body: self.full_body.take(),
            view: self.view,
            shown: self.response.read(cx).value().to_string(),
        };
        // A deleted console leaves nothing behind.
        if !leaving.is_empty() {
            self.panes.insert(leaving, pane);
        }
        let Some(next) = self.panes.remove(entering) else {
            self.view = View::Json;
            return String::new();
        };
        (self.run, self.hits, self.tree) = (next.run, next.hits, next.tree);
        (self.full_body, self.view) = (next.full_body, next.view);
        next.shown
    }

    /// Opens the console named after an index or alias: created when new, otherwise the search
    /// request is added at its end.
    fn open_named(&mut self, name: String, cx: &mut Context<Self>) {
        let search = search_text(&name);
        let conn = self.connection_name();
        let text = match self.library.read(cx).console(&conn, &name) {
            Some(old) if old.contains(&search) => old,
            Some(old) => format!("{}\n\n{search}", old.trim_end()),
            None => search,
        };
        self.open_console(name, text, cx);
    }

    /// The index or alias selected in the tree, for the keyboard shortcuts.
    fn selected_target(&self, cx: &Context<Self>) -> Option<String> {
        let list = self.targets.read(cx);
        match list.delegate().selected()? {
            (Folder::Indices | Folder::Aliases, row) => Some(row.label.clone()),
            _ => None,
        }
    }

    /// Opens the Data Browser of an index or alias, or switches to the one already open.
    fn open_browser(&mut self, target: String, cx: &mut Context<Self>) {
        let key = browser_key(&target);
        if self.browsers.contains_key(&key) {
            self.activate_browser(key, cx);
            return;
        }
        let (Some(http), Some(window)) = (self.http.clone(), cx.active_window()) else {
            return;
        };
        let meta = self.meta.clone();
        // Opened from a list click, the window is busy with that event; see `open_console`.
        cx.spawn(async move |this, cx| {
            let _ = window.update(cx, |_, window, cx| {
                let _ = this.update(cx, |this, cx| {
                    let browser = cx.new(|cx| DataBrowser::new(target, http, meta, window, cx));
                    this.browsers.insert(key.clone(), browser);
                    this.activate_browser(key, cx);
                });
            });
        })
        .detach();
    }

    /// Makes the Data Browser tab `key` the shown one; the console it replaces is saved and parked.
    fn activate_browser(&mut self, key: String, cx: &mut Context<Self>) {
        self.save_active(cx);
        if !self.tabs.contains(&key) {
            self.tabs.push(key.clone());
        }
        // A check still waiting would save the editor text under the new name.
        self.lint = None;
        let leaving = std::mem::replace(&mut self.active, key.clone());
        if leaving != key {
            self.swap_pane(leaving, &key, cx);
        }
        cx.notify();
    }

    /// Switches to the tab at `ix`, loading its saved text.
    fn select_tab(&mut self, ix: usize, cx: &mut Context<Self>) {
        let Some(name) = self.tabs.get(ix).cloned() else {
            return;
        };
        if self.browsers.contains_key(&name) {
            self.activate_browser(name, cx);
            return;
        }
        // Consoles are stored per connection; with none there is no text to load, and reloading
        // would clear the draft still in the editor.
        if self.summary.is_none() {
            return;
        }
        let text = self
            .library
            .read(cx)
            .console(&self.connection_name(), &name)
            .unwrap_or_default();
        self.open_console(name, text, cx);
    }

    /// Closes the tab at `ix`; the last one stays. The console itself stays saved in the tree.
    fn close_tab(&mut self, ix: usize, cx: &mut Context<Self>) {
        if self.tabs.len() < 2 || ix >= self.tabs.len() {
            return;
        }
        let closed_active = self.tabs[ix] == self.active;
        self.save_active(cx);
        let closed = self.tabs.remove(ix);
        if self.browsers.remove(&closed).is_some() {
            self.panes.remove(&closed);
        }
        if closed_active {
            self.select_tab(ix.saturating_sub(1), cx);
        } else {
            cx.notify();
        }
    }

    /// Closes the tab called `name`; the lookup keeps a button press from depending on an index
    /// that moved since the frame was drawn.
    fn close_named_tab(&mut self, name: &str, cx: &mut Context<Self>) {
        if let Some(ix) = self.tabs.iter().position(|t| t == name) {
            self.close_tab(ix, cx);
        }
    }

    /// Moves the selection `step` tabs forward or back, wrapping around the ends.
    fn cycle_tab(&mut self, step: isize, cx: &mut Context<Self>) {
        if self.tabs.len() < 2 {
            return;
        }
        let Some(ix) = self.tabs.iter().position(|t| *t == self.active) else {
            return;
        };
        let next = (ix as isize + step).rem_euclid(self.tabs.len() as isize) as usize;
        self.select_tab(next, cx);
    }

    /// Closes the active tab, if another one remains.
    fn close_active_tab(&mut self, cx: &mut Context<Self>) {
        if let Some(ix) = self.tabs.iter().position(|t| *t == self.active) {
            self.close_tab(ix, cx);
        }
    }

    /// The menu item, the toolbar button and the action all land here, so the mode has a single
    /// way to change and every surface redraws together.
    fn toggle_readonly(&mut self, cx: &mut Context<Self>) {
        self.readonly = !self.readonly;
        // The menu item carries a check mark for the mode, so it has to be rebuilt with it.
        self.refresh_menus(cx);
        cx.notify();
    }

    /// Folds the response away, leaving only its header row so the editor takes the freed height.
    fn toggle_result_collapsed(&mut self, cx: &mut Context<Self>) {
        self.result_layout = match self.result_layout {
            ResultLayout::Collapsed => ResultLayout::Split,
            _ => ResultLayout::Collapsed,
        };
        cx.notify();
    }

    /// Gives the response the whole height for reading data, or returns to the split.
    fn toggle_result_maximized(&mut self, cx: &mut Context<Self>) {
        self.result_layout = match self.result_layout {
            ResultLayout::Maximized => ResultLayout::Split,
            _ => ResultLayout::Maximized,
        };
        cx.notify();
    }

    /// Rebuilds both menu bars after anything they show as a check mark changes.
    fn refresh_menus(&self, cx: &mut Context<Self>) {
        set_menus(cx, self.theme, self.readonly);
        self.menu_bar.update(cx, |bar, cx| bar.reload(cx));
    }

    /// Applies and remembers the appearance. Both menus are rebuilt because they are where the
    /// current choice is shown, and a native menu keeps the check marks it was built with.
    fn set_theme(&mut self, pref: Pref, window: &mut Window, cx: &mut Context<Self>) {
        if self.theme == pref {
            return;
        }
        self.theme = pref;
        appearance::apply(pref, Some(window), cx);
        self.refresh_menus(cx);
        let path = self.appearance_file.clone();
        cx.background_spawn(async move { pref.save(&path) })
            .detach();
        cx.notify();
    }

    /// Replaces the draft with `text` and focuses the editor, so the next keystroke or `Send`
    /// acts on the example the user picked.
    fn fill_editor(&mut self, text: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.editor.update(cx, |state, cx| {
            state.set_value(text, window, cx);
            state.set_selected_range(text.len()..text.len(), cx);
            state.focus(window, cx);
        });
    }

    /// One tab per console: icon, name and a close button. Right-click renames or deletes a saved
    /// console; the active one is tinted, so the strip reads at a glance.
    fn render_tabs(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let conn = self.connection_name();
        let closable = self.tabs.len() > 1;
        let console = cx.entity();
        let theme = cx.theme();
        let (muted, radius, blue) = (theme.muted_foreground, theme.radius, theme.blue);
        div()
            .flex()
            .items_center()
            .gap_1()
            .px_1()
            .overflow_x_hidden()
            .children(self.tabs.iter().enumerate().map(|(ix, name)| {
                let browsing = self.browsers.contains_key(name);
                let shown = name.strip_prefix(BROWSER_PREFIX).unwrap_or(name);
                let label = if conn.is_empty() {
                    shown.to_string()
                } else {
                    format!("{shown} [{conn}]")
                };
                let tab = div()
                    .id(("console-tab", row_id(name)))
                    .flex_none()
                    .h(rems(1.75))
                    .px_2()
                    .text_sm()
                    .flex()
                    .items_center()
                    .gap_2()
                    .rounded(radius)
                    .border_1()
                    .border_color(gpui_kit::transparent_black())
                    .cursor_pointer()
                    .on_click(cx.listener(move |this, _, _, cx| this.select_tab(ix, cx)))
                    .child(
                        Icon::new(if browsing {
                            IconName::Table2
                        } else {
                            IconName::SquareTerminal
                        })
                        .small()
                        .text_color(muted),
                    )
                    .child(div().whitespace_nowrap().child(SharedString::from(label)));
                let tab = if *name == self.active {
                    tab.bg(blue.opacity(0.12)).border_color(blue.opacity(0.45))
                } else {
                    tab
                };
                let (rename, delete) = (console.clone(), console.clone());
                let (rename_name, delete_name) = (name.clone(), name.clone());
                let target = name.clone();
                let tab = tab.children(closable.then(|| {
                    div()
                        .id(("close-tab", row_id(name)))
                        .test_support()
                        .size_4()
                        .flex()
                        .items_center()
                        .justify_center()
                        .rounded_sm()
                        .text_color(muted)
                        .hover(|s| s.bg(muted.opacity(0.2)))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            cx.stop_propagation();
                            this.close_named_tab(&target, cx);
                        }))
                        .child(Icon::new(IconName::Close).size_3())
                }));
                // The default console can be neither renamed nor deleted, and a Data Browser is not
                // a saved console, so neither has a menu.
                if name == saved::AUTO_CONSOLE || browsing {
                    return tab.into_any_element();
                }
                tab.context_menu(move |menu, _, _| {
                    let (rename, rename_name) = (rename.clone(), rename_name.clone());
                    let (delete, delete_name) = (delete.clone(), delete_name.clone());
                    menu.item(
                        PopupMenuItem::new("Rename…").on_click(move |_, window, cx| {
                            let name = rename_name.clone();
                            rename.update(cx, |this, cx| this.rename_console(name, window, cx))
                        }),
                    )
                    .item(PopupMenuItem::new("Delete").on_click(move |_, _, cx| {
                        let name = delete_name.clone();
                        delete.update(cx, |this, cx| this.delete_console(name, cx))
                    }))
                })
                .into_any_element()
            }))
    }

    fn save_active(&mut self, cx: &mut Context<Self>) {
        let (name, text) = (self.connection_name(), self.editor.read(cx).value());
        // Before a connection is chosen, or after the open console was deleted, there is none to keep.
        // A Data Browser has no text; the editor still holds the console it replaced.
        if !name.is_empty() && !self.active.is_empty() && !self.browsers.contains_key(&self.active)
        {
            let active = self.active.clone();
            self.library.update(cx, |library, cx| {
                library.set_console(&name, &active, &text, cx)
            });
        }
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
                    closed: false,
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

    /// Removes the console called `name`, closing its tab.
    fn delete_console(&mut self, name: String, cx: &mut Context<Self>) {
        // The default console always exists.
        if name == saved::AUTO_CONSOLE {
            return;
        }
        let conn = self.connection_name();
        self.panes.remove(&name);
        let was_active = name == self.active;
        // Without this the autosave would write the open console straight back.
        if was_active {
            self.active.clear();
        }
        self.library.update(cx, |library, cx| {
            library.remove_saved(&conn, Kind::Console, &name, cx)
        });
        let Some(ix) = self.tabs.iter().position(|t| *t == name) else {
            return;
        };
        if self.tabs.len() > 1 {
            self.tabs.remove(ix);
            if was_active {
                self.select_tab(ix.saturating_sub(1), cx);
            }
        } else if was_active {
            // The only tab stays, empty; an empty console is never saved, so it stays deleted.
            self.open_console(name, String::new(), cx);
        }
        cx.notify();
    }

    /// Asks for a new name for the console called `old`.
    fn rename_console(&mut self, old: String, window: &mut Window, cx: &mut Context<Self>) {
        // The default console keeps its name, as the autosave and the connection's startup look for it.
        if old == saved::AUTO_CONSOLE {
            return;
        }
        let input = cx.new(|cx| InputState::new(window, cx));
        input.update(cx, |s, cx| s.set_value(old.clone(), window, cx));
        let console = cx.entity();
        window.open_dialog(cx, move |dialog, _, _| {
            let (input, console) = (input.clone(), console.clone());
            let old = old.clone();
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
                                        let new = input.read(cx).value().trim().to_string();
                                        let new: String = new.chars().take(60).collect();
                                        let (console, old) = (console.clone(), old.clone());
                                        console.update(cx, |this, cx| {
                                            let conn = this.connection_name();
                                            let done = this.library.update(cx, |library, cx| {
                                                library.rename_saved(
                                                    &conn,
                                                    Kind::Console,
                                                    &old,
                                                    &new,
                                                    cx,
                                                )
                                            });
                                            // Tabs and the autosave follow the console by name.
                                            if done {
                                                if let Some(pane) = this.panes.remove(&old) {
                                                    this.panes.insert(new.clone(), pane);
                                                }
                                                for t in
                                                    this.tabs.iter_mut().chain([&mut this.active])
                                                {
                                                    if *t == old {
                                                        *t = new.clone();
                                                    }
                                                }
                                            }
                                            cx.notify();
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
        window.open_dialog(cx, move |dialog, window, _| {
            // The list (12rem) plus the form need more than the default 448px, or the two squeeze together.
            dialog
                .title("Connections")
                .w(rems(45.).to_pixels(window.rem_size()))
                .child(panel.clone())
        });
    }

    /// Drops the connection: stops refreshing and cancels whatever is in flight.
    fn disconnect(&mut self, cx: &mut Context<Self>) {
        // Keep the text with the connection it was typed in while that name still exists.
        self.save_active(cx);
        (self.http, self.summary, self.meta) = (None, None, None);
        (self.refresh, self.catalog, self.sending) = (None, None, None);
        (self.online, self.run) = (false, Run::Idle);
        // Tabs, panes and the response belong to the connection. Without one only the default tab
        // remains, so a click cannot load an empty console over the draft; the old response must
        // not linger either.
        self.tabs = vec![saved::AUTO_CONSOLE.into()];
        self.active = saved::AUTO_CONSOLE.into();
        self.panes.clear();
        self.browsers.clear();
        (self.hits, self.tree, self.full_body) = (None, None, None);
        self.view = View::Json;
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
        window.open_dialog(cx, move |dialog, window, _| {
            let console = console.clone();
            dialog
                .title("History")
                .w(rems(40.).to_pixels(window.rem_size()))
                .child(
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

    /// Re-reads the nodes, templates, tasks and pipelines.
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
                            closed: false,
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

    /// Switches completion to `index`, the one that matches the connected version.
    fn use_index(
        &mut self,
        index: Index,
        version: es_dsl_core::Version,
        fields: es_client::ClusterMeta,
        cx: &mut Context<Self>,
    ) {
        let shared = Arc::new(index);
        let Ok(client) = LspClient::start(Index::clone(&shared), Some(version), fields) else {
            return;
        };
        self.index = shared;
        self.client = client.clone();
        self.editor.update(cx, |state, _| install(state, &client));
        self.lint(cx);
    }

    /// Takes on `done`'s identity and returns the text its default console should show. `None`
    /// means this is a reconnect to the same cluster, so the open tabs stay.
    ///
    /// Tabs, panes and the response area belong to the connection that produced them: a tab of
    /// the previous connection would resolve its console against `done.name` and open empty.
    fn switch_to(&mut self, done: &Connected, cx: &mut Context<Self>) -> Option<String> {
        // The editor text belongs to the connection being left. Save it before the summary names
        // the new connection, or a pending autosave would store it under `done.name`.
        self.save_active(cx);
        let previous = self.summary.as_ref().map(|s| s.name.to_string());
        let [a, b, c] = done.version;
        self.summary = Some(Summary {
            name: done.name.clone().into(),
            version: format!("Elasticsearch {a}.{b}.{c}").into(),
        });
        if previous.as_deref() == Some(done.name.as_str()) {
            return None;
        }
        self.tabs = vec![saved::AUTO_CONSOLE.into()];
        self.panes.clear();
        self.browsers.clear();
        // Empty until `open_console` names the tab, so it does not save the old text under the
        // new connection.
        self.active.clear();
        // The connection's own console comes back; a connection without one keeps the editor text.
        Some(
            match self
                .library
                .read(cx)
                .console(&done.name, saved::AUTO_CONSOLE)
            {
                Some(text) => text,
                None => self.editor.read(cx).value().to_string(),
            },
        )
    }

    /// Restarts completion against the cluster that just answered, and keeps its metadata fresh.
    fn connected(&mut self, done: &Connected, cx: &mut Context<Self>) {
        let started = LspClient::start(
            Index::clone(&self.index),
            Some(done.version),
            done.meta.clone(),
        );
        let Ok(client) = started else { return };
        let text = self.switch_to(done, cx);
        self.client = client.clone();
        self.http = Some(done.client.clone());
        self.online = true;
        self.refresh_history(cx);
        self.refresh_saved(cx);
        self.load_catalog(cx);
        if let Some(text) = text {
            self.open_console(saved::AUTO_CONSOLE.into(), text, cx);
        }
        // `open_console` needs a window; without one the default console is still the one to save.
        if self.active.is_empty() {
            self.active = saved::AUTO_CONSOLE.into();
        }
        self.editor.update(cx, |state, _| install(state, &client));
        self.lint(cx);
        let (dir, version, fields) = (self.config_dir.clone(), done.version, done.meta.clone());
        self.schema_load = Some(cx.spawn(async move |this, cx| {
            let loaded = cx.background_spawn(schema_source::load(dir, version)).await;
            let Some(index) = loaded else { return };
            let _ = this.update(cx, |this, cx| this.use_index(index, version, fields, cx));
        }));
        let (http, meta) = (done.client.clone(), done.meta.clone());
        self.meta = Some(meta.clone());
        self.refresh = Some(cx.spawn(async move |this, cx| {
            let mut tick = 0u32;
            loop {
                // Nothing is polled while the app is in the background; the first tick back catches up.
                if cx.update(|cx| cx.active_window().is_none()) {
                    cx.background_executor().timer(REFRESH_EVERY).await;
                    continue;
                }
                // A failed refresh keeps the previous data; the next tick retries.
                let reached = meta.refresh(&http).await.is_ok();
                let names = es_dsl_core::Fields::names(&meta);
                let aliases = es_dsl_core::Fields::aliases(&meta);
                let closed = meta.closed();
                let indices = names
                    .into_iter()
                    .filter(|n| aliases.binary_search(n).is_err());
                let rows = |names: Vec<String>| -> Vec<Row> {
                    names
                        .into_iter()
                        .map(|n| Row {
                            text: search_text(&n),
                            closed: closed.contains(&n),
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
                // Completion wakes this loop when it asks for a mapping the cache lacks, so the
                // first request against a new target waits for a round trip, not a full tick.
                let timer = Box::pin(cx.background_executor().timer(REFRESH_EVERY));
                let _ = futures::future::select(timer, Box::pin(meta.wanted())).await;
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
                    .loading(sending)
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
                Button::new("save-console")
                    .ghost()
                    .small()
                    .icon(IconName::Save)
                    .tooltip("Save a copy of the editor as a console")
                    .on_click(cx.listener(|this, _, _, cx| this.save_console(cx))),
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
            .child({
                // One of the icon commands: the lock changes shape with the mode and is pressed
                // while the mode is on, and the status bar repeats the state next to the response.
                let (icon, tip) = if self.readonly {
                    (
                        IconName::Lock,
                        "Readonly is on: requests that change the cluster are blocked",
                    )
                } else {
                    (
                        IconName::LockOpen,
                        "Readonly is off: requests can change the cluster",
                    )
                };
                Button::new("readonly")
                    .ghost()
                    .small()
                    .icon(icon)
                    .selected(self.readonly)
                    .accessibility_label(tip)
                    .tooltip(tip)
                    .on_click(cx.listener(|this, _, _, cx| this.toggle_readonly(cx)))
            })
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
        // The toolbar button shows the mode too, but the status bar sits next to the response it
        // protects: a write refused silently there would otherwise look like a bug.
        if self.readonly {
            bar = bar.left(
                div()
                    .id("readonly-badge")
                    .test_support()
                    .role(Role::Status)
                    .aria_label("Readonly is on")
                    .flex()
                    .flex_none()
                    .items_center()
                    .gap_1()
                    // A tinted chip, not just colored text: the mode has to stand out from the
                    // status text next to it at a glance.
                    .rounded(theme.radius)
                    .bg(warning.opacity(0.14))
                    .border_1()
                    .border_color(warning.opacity(0.35))
                    .px_1p5()
                    .text_color(warning)
                    .child(Icon::new(IconName::Lock).small().text_color(warning))
                    .child("Readonly"),
            );
        }
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

    /// Before the first response: a short hint and requests that show the shape without docs. A
    /// click fills the editor, so the examples also work before a connection exists; they are only
    /// offered while the editor is empty, so a typed draft is never replaced.
    fn render_empty_response(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let (muted, foreground, radius, mono) = (
            theme.muted_foreground,
            theme.foreground,
            theme.radius,
            theme.mono_font_family.clone(),
        );
        let blank = self.editor.read(cx).value().trim().is_empty();
        let (title, hint) = match (self.http.is_some(), blank) {
            (true, true) => (
                "Nothing run yet",
                format!("Write a request and press {SEND_SHORTCUT} to run it."),
            ),
            (true, false) => (
                "Nothing run yet",
                format!("Press {SEND_SHORTCUT} to run the request at the cursor."),
            ),
            (false, true) => (
                "No connection",
                "Open a connection in the title bar, or start from an example.".to_string(),
            ),
            (false, false) => (
                "No connection",
                "Open a connection in the title bar to run this request.".to_string(),
            ),
        };
        div()
            .size_full()
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap_2()
            .child(
                Icon::new(IconName::SquareTerminal)
                    .large()
                    .text_color(muted),
            )
            .child(
                div()
                    .text_base()
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(title),
            )
            .child(div().text_sm().text_color(muted).child(hint))
            .children(blank.then(|| {
                div()
                    .mt_2()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .children(SAMPLES.iter().enumerate().map(|(ix, (name, text))| {
                        let (name, text) = (*name, *text);
                        let head = text.lines().next().unwrap_or_default();
                        div()
                            .id(("sample-request", ix))
                            .test_support()
                            .role(Role::Button)
                            .aria_label(format!("Insert {text}"))
                            .flex()
                            .items_center()
                            .gap_3()
                            .px_2()
                            .py_1()
                            .rounded(radius)
                            .cursor_pointer()
                            .hover(|style| style.bg(foreground.opacity(0.06)))
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.fill_editor(text, window, cx)
                            }))
                            .child(Icon::new(IconName::Play).xsmall().text_color(muted))
                            .child(div().text_sm().child(name))
                            .child(
                                div()
                                    .font_family(mono.clone())
                                    .text_xs()
                                    .text_color(muted)
                                    .child(head.to_string()),
                            )
                    }))
            }))
    }

    /// The response viewer: view tabs and actions above the body. Only built once a response
    /// exists. `show_body` is false while the response is collapsed to its header row.
    fn render_response(&self, cx: &mut Context<Self>, show_body: bool) -> impl IntoElement {
        let theme = cx.theme();
        let palette = TreePalette {
            key: crate::paint::color(Token::Key, theme.is_dark()),
            string: crate::paint::color(Token::String, theme.is_dark()),
            number: crate::paint::color(Token::Number, theme.is_dark()),
            keyword: crate::paint::color(Token::Keyword, theme.is_dark()),
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
            .children(views.iter().map(|(_, label, icon)| {
                // `Tab::icon` draws the icon alone; the accessible name keeps the view's
                // meaning for tests and screen readers.
                Tab::new()
                    .icon(Icon::new(*icon).xsmall())
                    .aria_label(*label)
            }));
        let table = self.hits.as_ref().filter(|_| self.view == View::Table);
        let outline = self.tree.as_ref().filter(|_| self.view == View::Tree);
        let mut actions = div()
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
            );
        // Maximized, the response already has the height; the way out is to restore the split.
        actions = if self.result_layout == ResultLayout::Maximized {
            actions.child(
                Button::new("minimize-result")
                    .ghost()
                    .small()
                    .icon(IconName::Minimize)
                    .tooltip("Restore the split view")
                    .accessibility_label("Restore the split view")
                    .on_click(cx.listener(|this, _, _, cx| this.toggle_result_maximized(cx))),
            )
        } else {
            // The result lives below the editor: folding it moves the top edge down, and
            // expanding moves it back up, so the chevrons follow that direction.
            let (icon, tip) = if show_body {
                (IconName::ChevronDown, "Collapse the result")
            } else {
                (IconName::ChevronUp, "Expand the result")
            };
            actions
                .child(
                    Button::new("maximize-result")
                        .ghost()
                        .small()
                        .icon(IconName::Maximize)
                        .tooltip("Maximize the result")
                        .accessibility_label("Maximize the result")
                        .on_click(cx.listener(|this, _, _, cx| this.toggle_result_maximized(cx))),
                )
                .child(
                    Button::new("collapse-result")
                        .ghost()
                        .small()
                        .icon(icon)
                        .tooltip(tip)
                        .accessibility_label(tip)
                        .on_click(cx.listener(|this, _, _, cx| this.toggle_result_collapsed(cx))),
                )
        };
        let mut viewer = div().size_full().flex().flex_col().gap_2().child(
            div()
                .flex()
                .items_center()
                .justify_between()
                .child(tabs)
                .child(actions),
        );
        if show_body {
            viewer = viewer.child(
                div().flex_1().min_h_0().child(match (table, outline) {
                    (Some(t), _) => hits_table::view(t).into_any_element(),
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
            );
        }
        viewer
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

/// Requests offered before the first result: one shape per row, all read-only, so the examples
/// are safe on every connection.
const SAMPLES: [(&str, &str); 3] = [
    ("Cluster health", "GET _cluster/health\n"),
    ("List indices", "GET _cat/indices?v\n"),
    (
        "Search everything",
        "POST _search\n{\n  \"query\": {\n    \"match_all\": {}\n  }\n}\n",
    ),
];

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
        // Both halves are always laid out: the panel library keeps the editor's full width when a hidden panel appears later.
        let response = matches!(self.run, Run::Done { .. });
        // Idle is the only state with neither a result nor a request in flight to report.
        let empty = matches!(self.run, Run::Idle);
        let sidebar_width = size(20.0);
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
        let surface = theme.background;
        // JetBrains Islands look: flat theme-rounded panels on a darker ground, no border or shadow,
        // separated by a small gap.
        let radius = theme.radius;
        let ground = theme.background.blend(theme.foreground.opacity(0.07));
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
            .on_action(cx.listener(|this, _: &OpenInDataBrowser, _, cx| {
                if let Some(name) = this.selected_target(cx) {
                    this.open_browser(name, cx);
                }
            }))
            .on_action(cx.listener(|this, _: &OpenInRestConsole, _, cx| {
                if let Some(name) = this.selected_target(cx) {
                    this.open_named(name, cx);
                }
            }))
            .on_action(cx.listener(|this, _: &Send, window, cx| this.send(window, cx)))
            .on_action(cx.listener(|this, _: &Format, window, cx| this.format(window, cx)))
            .on_action(cx.listener(|this, _: &Newline, window, cx| this.newline(window, cx)))
            .on_action(cx.listener(|this, _: &CancelRequest, _, cx| this.cancel(cx)))
            .on_action(cx.listener(|this, _: &NextTab, _, cx| this.cycle_tab(1, cx)))
            .on_action(cx.listener(|this, _: &PreviousTab, _, cx| this.cycle_tab(-1, cx)))
            .on_action(cx.listener(|this, _: &CloseTab, _, cx| this.close_active_tab(cx)))
            .on_action(cx.listener(|this, _: &ToggleTree, _, cx| {
                this.sidebar_open = !this.sidebar_open;
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &ToggleReadonly, _, cx| this.toggle_readonly(cx)))
            .on_action(cx.listener(|this, _: &UseSystemAppearance, window, cx| {
                this.set_theme(Pref::System, window, cx)
            }))
            .on_action(cx.listener(|this, _: &UseLightAppearance, window, cx| {
                this.set_theme(Pref::Light, window, cx)
            }))
            .on_action(cx.listener(|this, _: &UseDarkAppearance, window, cx| {
                this.set_theme(Pref::Dark, window, cx)
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
                div().flex_1().min_h_0().mx_1().mb_1().child(
                    h_resizable("console-split")
                        .child(
                            resizable_panel()
                                .visible(sidebar)
                                .size(sidebar_width)
                                .fixed()
                                .size_range(size(16.0)..size(40.0))
                                .child(div().size_full().pr_0p5().child(targets)),
                        )
                        .child(
                            // The tabs belong to the editor and the response together, so they sit above both.
                            resizable_panel().child(
                                div()
                                    .size_full()
                                    .flex()
                                    .flex_col()
                                    .child(self.render_tabs(cx))
                                    .child(
                                        div().flex_1().min_h_0().child(
                                            match self.browsers.get(&self.active) {
                                                Some(browser) => card()
                                                    .id("browser-card")
                                                    .test_support()
                                                    .p_1p5()
                                                    .child(browser.clone())
                                                    .into_any_element(),
                                                None => {
                                                    // Nothing to analyze yet: the editor keeps the height.
                                                    let layout = if response {
                                                        self.result_layout
                                                    } else {
                                                        ResultLayout::Split
                                                    };
                                                    let show_body =
                                                        layout != ResultLayout::Collapsed;
                                                    let editor_card = card()
                                                        .id("editor-card")
                                                        .test_support()
                                                        .p_1p5()
                                                        .child(
                                                            Editor::new(&self.editor)
                                                                .bordered(false)
                                                                .h_full(),
                                                        );
                                                    let content = if response {
                                                        self.render_response(cx, show_body)
                                                            .into_any_element()
                                                    } else if empty {
                                                        self.render_empty_response(cx)
                                                            .into_any_element()
                                                    } else {
                                                        div().into_any_element()
                                                    };
                                                    let response_card = card()
                                                        .id("result-card")
                                                        .test_support()
                                                        .p_2()
                                                        .child(content);
                                                    match layout {
                                                        // Stacked by default: a request is wider than it is tall, and the
                                                        // response beneath it keeps the full width for tables.
                                                        ResultLayout::Split => {
                                                            v_resizable("editor-split")
                                                                .with_state(&self.split)
                                                                .child(
                                                                    resizable_panel()
                                                                        .child(editor_card),
                                                                )
                                                                .child(
                                                                    resizable_panel()
                                                                        .size_range(
                                                                            size(16.0)..size(120.0),
                                                                        )
                                                                        .child(
                                                                            div()
                                                                                .size_full()
                                                                                .pt_0p5()
                                                                                .child(
                                                                                    response_card,
                                                                                ),
                                                                        ),
                                                                )
                                                                .into_any_element()
                                                        }
                                                        // Folded away: only the response's header row stays, at the
                                                        // bottom, so the editor owns the rest of the height. Its
                                                        // card must size to that row rather than to the whole column.
                                                        ResultLayout::Collapsed => div()
                                                            .size_full()
                                                            .flex()
                                                            .flex_col()
                                                            .child(
                                                                div()
                                                                    .flex_1()
                                                                    .min_h_0()
                                                                    .child(editor_card),
                                                            )
                                                            .child(
                                                                div()
                                                                    .flex_none()
                                                                    .pt_0p5()
                                                                    .child(response_card.h_auto()),
                                                            )
                                                            .into_any_element(),
                                                        // The response alone, for reading the data.
                                                        ResultLayout::Maximized => div()
                                                            .size_full()
                                                            .flex()
                                                            .flex_col()
                                                            .child(
                                                                div()
                                                                    .flex_1()
                                                                    .min_h_0()
                                                                    .pt_0p5()
                                                                    .child(response_card),
                                                            )
                                                            .into_any_element(),
                                                    }
                                                }
                                            },
                                        ),
                                    ),
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
    use gpui_kit::component::ThemeMode;
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

    /// Accepting a key that the schema says takes an object inserts `{}` and puts the caret inside,
    /// so the next completion is already at the object's keys.
    #[gpui_kit::test]
    fn accepting_an_object_key_leaves_the_caret_inside_the_braces(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        let mut index = sample_index();
        index.endpoints[0].body = vec![es_dsl_core::Prop::new(
            "query",
            es_dsl_core::TypeRef::Named("QueryContainer".into()),
        )];
        index.types.insert(
            "QueryContainer".into(),
            vec![es_dsl_core::Prop::new(
                "bool",
                es_dsl_core::TypeRef::Named("BoolQuery".into()),
            )],
        );
        index.types.insert(
            "BoolQuery".into(),
            vec![es_dsl_core::Prop::new("must", es_dsl_core::TypeRef::Other)],
        );
        let client = LspClient::start(index.clone(), None, es_dsl_core::NoFields).unwrap();
        let (window, console) = cx
            .update(|cx| {
                gpui_kit::open_window(Default::default(), cx, |window, cx| {
                    let library = cx.new(|_| Library::blank());
                    cx.new(|cx| {
                        Console::new(
                            "",
                            index,
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
        let (value, cursor) = cx.update(|cx| {
            let state = editor.read(cx);
            (state.value().to_string(), state.cursor())
        });
        assert_eq!(value, "POST idx/_search\n{\n  \"query\": {}");
        assert_eq!(cursor, value.len() - 1, "caret is between the braces");
    }

    /// A bare quote must not open the menu: with nothing typed the kit preselects its first item,
    /// so the next Enter would insert it (and its `: `) instead of breaking the line. Typing a
    /// character still asks, and so does Ctrl+Space.
    #[gpui_kit::test]
    fn a_bare_quote_does_not_open_the_menu(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        cx.update(bind_keys);
        struct Fields;
        impl es_dsl_core::Fields for Fields {
            fn fields(&self, _target: &str) -> std::sync::Arc<[es_dsl_core::Field]> {
                std::sync::Arc::from(["sku", "title"].map(|n| es_dsl_core::Field {
                    name: n.into(),
                    ty: "keyword".into(),
                    nested: false,
                }))
            }
        }
        let mut index = sample_index();
        index.endpoints[0].body = vec![es_dsl_core::Prop::new(
            "query",
            es_dsl_core::TypeRef::Named("QueryContainer".into()),
        )];
        index.types.insert(
            "QueryContainer".into(),
            vec![es_dsl_core::Prop::new(
                "term",
                es_dsl_core::TypeRef::FieldDict(Box::new(es_dsl_core::TypeRef::Named(
                    "TermQuery".into(),
                ))),
            )],
        );
        index.types.insert(
            "TermQuery".into(),
            vec![es_dsl_core::Prop::new("value", es_dsl_core::TypeRef::Other)],
        );
        let client = LspClient::start(index.clone(), None, Fields).unwrap();
        let (window, console) = cx
            .update(|cx| {
                gpui_kit::open_window(Default::default(), cx, |window, cx| {
                    let library = cx.new(|_| Library::blank());
                    cx.new(|cx| {
                        Console::new(
                            "",
                            index,
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
        let doc = "POST idx/_search\n{\n  \n}";
        let at = doc.find("\n}").unwrap();

        cx.update_window(window, |_, window, cx| {
            window.activate_window();
            editor.update(cx, |state, cx| {
                state.focus(window, cx);
                state.set_value(doc, window, cx);
                state.set_selected_range(at..at, cx);
            });
        })
        .unwrap();
        cx.run_until_parked();

        let menu_open = |cx: &mut TestAppContext| {
            cx.update_window(window, |_, window, cx| {
                window.render_frame(cx);
                window.try_find("completion-menu").is_some()
            })
            .unwrap()
        };
        let input = |cx: &mut TestAppContext, text: &str| {
            cx.update_window(window, |_, window, cx| window.input(text, cx))
                .unwrap();
            cx.run_until_parked();
        };
        let enter = |cx: &mut TestAppContext| {
            cx.update_window(window, |_, window, cx| window.press("enter", cx))
                .unwrap();
            cx.run_until_parked();
        };

        // `query` and `term` in turn, each accepted from a typed prefix.
        for (prefix, key) in [("q", "query"), ("t", "term")] {
            input(cx, "\"");
            input(cx, prefix);
            assert!(menu_open(cx), "no menu for {key}");
            enter(cx);
        }
        // This is where the user typed a quote: the menu stays closed, so Enter breaks the line.
        input(cx, "\"");
        assert!(!menu_open(cx), "a bare quote opened the menu");

        // Ctrl+Space asks explicitly and offers the mapping fields.
        cx.update_window(window, |_, window, cx| window.press("ctrl-space", cx))
            .unwrap();
        cx.run_until_parked();
        assert!(menu_open(cx), "Ctrl+Space did not open the menu");
        enter(cx);

        let (value, cursor) = cx.update(|cx| {
            let state = editor.read(cx);
            (state.value().to_string(), state.cursor())
        });
        assert_eq!(
            value,
            "POST idx/_search\n{\n  \"query\": {\"term\": {\"sku\": }}\n}"
        );
        assert_eq!(cursor, value.find("\"sku\": ").unwrap() + "\"sku\": ".len());
    }

    /// Opening an index's console from inside a window update, as a list click does, replaces the editor text.
    #[gpui_kit::test]
    fn opening_a_new_console_shows_only_its_own_request(cx: &mut TestAppContext) {
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
        cx.update_window(window, |_, window, cx| {
            window.activate_window();
            console.update(cx, |this, _| {
                this.summary = Some(Summary {
                    name: "c".into(),
                    version: "".into(),
                });
            });
            editor.update(cx, |state, cx| {
                state.set_value("GET old/_search", window, cx)
            });
        })
        .unwrap();
        cx.run_until_parked();
        cx.update_window(window, |_, _, cx| {
            console.update(cx, |this, cx| {
                this.open_console("idx".into(), search_text("idx"), cx)
            })
        })
        .unwrap();
        cx.run_until_parked();
        let value = cx.update(|cx| editor.read(cx).value().to_string());
        assert_eq!(value, search_text("idx"));
        // Let the autosave fire: the console that was left must keep its own text.
        cx.executor().advance_clock(Duration::from_secs(2));
        cx.run_until_parked();
        let library = cx.update(|cx| console.read(cx).library.clone());
        let (left, new) = cx.update(|cx| {
            let l = library.read(cx);
            (l.console("c", saved::AUTO_CONSOLE), l.console("c", "idx"))
        });
        assert_eq!(left.as_deref(), Some("GET old/_search"));
        assert_eq!(new, Some(search_text("idx")));
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

    /// A client that answers nothing; `switch_to` is handed a `Connected` directly, so the switch
    /// itself needs no request.
    fn cluster(name: &str) -> Connected {
        let client = Client::new(es_client::Config::new("http://127.0.0.1:1")).unwrap();
        Connected {
            client: Arc::new(client),
            version: [8, 19, 0],
            meta: es_client::ClusterMeta::default(),
            name: name.into(),
        }
    }

    /// A connection switch must not leave the previous connection's tabs behind: clicking one
    /// would resolve its console against the new connection and open empty. The outgoing text
    /// must also stay with the connection it was typed in.
    #[gpui_kit::test]
    fn switching_connections_drops_the_previous_tabs(cx: &mut TestAppContext) {
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
        let library = cx.update(|cx| console.read(cx).library.clone());
        // c2 has its own default console.
        cx.update(|cx| {
            library.update(cx, |library, cx| {
                library.set_console("c2", saved::AUTO_CONSOLE, "GET c2/_search", cx)
            });
        });
        cx.update_window(window, |_, window, cx| {
            window.activate_window();
            console.update(cx, |this, _| {
                this.summary = Some(Summary {
                    name: "c1".into(),
                    version: "".into(),
                });
                this.tabs = vec![saved::AUTO_CONSOLE.into(), "logs".into()];
                this.active = "logs".into();
            });
            editor.update(cx, |state, cx| {
                state.set_value("GET logs/_search", window, cx);
                state.set_selected_range(0..0, cx);
            });
        })
        .unwrap();
        let done = cluster("c2");
        // The switch hands `open_console` c2's own console and no inherited tab state.
        let text = cx.update(|cx| console.update(cx, |this, cx| this.switch_to(&done, cx)));
        assert_eq!(text.as_deref(), Some("GET c2/_search"));
        cx.update(|cx| {
            let console = console.read(cx);
            assert_eq!(console.tabs, vec![saved::AUTO_CONSOLE.to_string()]);
            assert!(console.active.is_empty());
            assert!(console.panes.is_empty());
            // The outgoing text stayed with c1 instead of moving to c2.
            assert_eq!(
                library.read(cx).console("c1", "logs").as_deref(),
                Some("GET logs/_search")
            );
        });
        // Finishing the switch opens c2's default console.
        let text = text.unwrap();
        cx.update(|cx| {
            console.update(cx, |this, cx| {
                this.open_console(saved::AUTO_CONSOLE.into(), text, cx)
            });
        });
        cx.run_until_parked();
        cx.update(|cx| {
            let console = console.read(cx);
            assert_eq!(console.active, saved::AUTO_CONSOLE);
            assert_eq!(console.tabs, vec![saved::AUTO_CONSOLE.to_string()]);
        });
        assert_eq!(
            cx.update(|cx| editor.read(cx).value().to_string()),
            "GET c2/_search"
        );
        // Reconnecting the same cluster keeps the open tabs.
        cx.update(|cx| {
            console.update(cx, |this, _| {
                this.tabs = vec![saved::AUTO_CONSOLE.into(), "logs".into()];
                this.active = "logs".into();
            });
        });
        let again = cx.update(|cx| console.update(cx, |this, cx| this.switch_to(&done, cx)));
        assert_eq!(again, None);
        cx.update(|cx| {
            assert_eq!(
                console.read(cx).tabs,
                vec![saved::AUTO_CONSOLE.to_string(), "logs".to_string()]
            );
        });
    }

    /// Disconnecting collapses the tabs to the default console but keeps the draft; a click on the
    /// remaining tab must not load an empty console over it.
    #[gpui_kit::test]
    fn disconnecting_keeps_the_draft_and_collapses_the_tabs(cx: &mut TestAppContext) {
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
        let library = cx.update(|cx| console.read(cx).library.clone());
        cx.update_window(window, |_, window, cx| {
            window.activate_window();
            console.update(cx, |this, _| {
                this.summary = Some(Summary {
                    name: "c1".into(),
                    version: "".into(),
                });
                this.tabs = vec![saved::AUTO_CONSOLE.into(), "logs".into()];
                this.active = "logs".into();
            });
            editor.update(cx, |state, cx| {
                state.set_value("GET logs/_search", window, cx);
                state.set_selected_range(0..0, cx);
            });
        })
        .unwrap();
        cx.update(|cx| console.update(cx, |this, cx| this.disconnect(cx)));
        cx.run_until_parked();
        cx.update(|cx| {
            let console = console.read(cx);
            assert_eq!(console.tabs, vec![saved::AUTO_CONSOLE.to_string()]);
            assert_eq!(console.active, saved::AUTO_CONSOLE);
            assert!(console.panes.is_empty());
            assert_eq!(
                library.read(cx).console("c1", "logs").as_deref(),
                Some("GET logs/_search")
            );
        });
        // Without a connection the tab has nothing to load; the draft stays.
        cx.update(|cx| console.update(cx, |this, cx| this.select_tab(0, cx)));
        cx.run_until_parked();
        assert_eq!(
            cx.update(|cx| editor.read(cx).value().to_string()),
            "GET logs/_search"
        );
    }

    fn console_window(cx: &mut TestAppContext) -> (gpui_kit::AnyWindowHandle, Entity<Console>) {
        cx.update(gpui_kit::init);
        let client = LspClient::start(sample_index(), None, es_dsl_core::NoFields).unwrap();
        // Under the temp directory so the appearance choice this test saves stays out of the repo.
        let connections = std::env::temp_dir().join("esdeck-test-connections.json");
        cx.update(|cx| {
            gpui_kit::open_window(Default::default(), cx, |window, cx| {
                let library = cx.new(|_| Library::blank());
                cx.new(|cx| {
                    Console::new("", sample_index(), client, library, connections, window, cx)
                })
            })
        })
        .unwrap()
    }

    /// `Ctrl+Tab` and `Ctrl+Shift+Tab` move the selection and wrap at the ends.
    #[gpui_kit::test]
    fn cycling_tabs_wraps_around(cx: &mut TestAppContext) {
        let (window, console) = console_window(cx);
        cx.update_window(window, |_, window, cx| {
            window.activate_window();
            console.update(cx, |this, _| {
                this.summary = Some(Summary {
                    name: "c1".into(),
                    version: "".into(),
                });
                this.tabs = vec!["console".into(), "logs".into(), "metrics".into()];
                this.active = "console".into();
            });
        })
        .unwrap();
        let cycle = |cx: &mut TestAppContext, step: isize| {
            cx.update(|cx| console.update(cx, |this, cx| this.cycle_tab(step, cx)));
            cx.update(|cx| console.read(cx).active.clone())
        };
        assert_eq!(cycle(cx, 1), "logs");
        assert_eq!(cycle(cx, 1), "metrics");
        assert_eq!(cycle(cx, 1), "console", "wraps forward");
        assert_eq!(cycle(cx, -1), "metrics", "wraps back");
    }

    /// A tab's close button closes that tab without selecting it first.
    #[gpui_kit::test]
    fn a_tab_close_button_closes_without_selecting(cx: &mut TestAppContext) {
        let (window, console) = console_window(cx);
        cx.update_window(window, |_, window, cx| {
            window.activate_window();
            console.update(cx, |this, _| {
                this.summary = Some(Summary {
                    name: "c1".into(),
                    version: "".into(),
                });
                this.tabs = vec!["console".into(), "logs".into(), "metrics".into()];
                this.active = "console".into();
            });
            window.render_frame(cx);
            window.click(("close-tab", row_id("logs")), cx);
        })
        .unwrap();
        cx.run_until_parked();
        cx.update(|cx| {
            let console = console.read(cx);
            assert_eq!(
                console.tabs,
                vec!["console".to_string(), "metrics".to_string()]
            );
            assert_eq!(console.active, "console");
        });
    }

    /// The result folds to its header row and back, and takes the whole height for reading.
    #[gpui_kit::test]
    fn the_result_folds_to_its_header_and_maximizes(cx: &mut TestAppContext) {
        let (window, console) = console_window(cx);
        let (split_y, split_height, split_editor_height) = cx
            .update_window(window, |_, window, cx| {
                window.activate_window();
                console.update(cx, |this, cx| {
                    this.run = Run::Done {
                        status: 200,
                        took: Duration::from_millis(3),
                        bytes: 2,
                        cut: false,
                        found: None,
                    };
                    this.response
                        .update(cx, |state, cx| state.set_value("{}", window, cx));
                });
                window.render_frame(cx);
                let button = window
                    .try_find("collapse-result")
                    .expect("the split shows the collapse control");
                let card = window.try_find("result-card").unwrap();
                let editor = window.try_find("editor-card").unwrap();
                (
                    button.bounds().origin.y,
                    card.bounds().size.height.as_f32(),
                    editor.bounds().size.height.as_f32(),
                )
            })
            .unwrap();

        // Collapsed: the header row stays so the way back is one click away, and it sits at the
        // bottom, below the editor that now owns the rest of the height.
        cx.update_window(window, |_, window, cx| window.click("collapse-result", cx))
            .unwrap();
        cx.run_until_parked();
        cx.update(|cx| assert_eq!(console.read(cx).result_layout, ResultLayout::Collapsed));
        let (collapsed_y, collapsed_height, collapsed_editor_height) = cx
            .update_window(window, |_, window, cx| {
                window.render_frame(cx);
                assert!(window.try_find("save-response").is_some());
                let expand = window.try_find("collapse-result").unwrap();
                assert!(expand.label().unwrap().contains("Expand the result"));
                let card = window.try_find("result-card").unwrap();
                let editor = window.try_find("editor-card").unwrap();
                (
                    expand.bounds().origin.y,
                    card.bounds().size.height.as_f32(),
                    editor.bounds().size.height.as_f32(),
                )
            })
            .unwrap();
        assert!(
            collapsed_y > split_y,
            "the folded result moves below the split's response header ({collapsed_y} vs {split_y})"
        );
        assert!(
            collapsed_height < split_height,
            "the folded result keeps only its header row ({collapsed_height} vs {split_height})"
        );
        assert!(
            collapsed_height > 24.0,
            "the folded result still shows a usable header row ({collapsed_height})"
        );
        assert!(
            collapsed_editor_height > split_editor_height,
            "the editor takes the folded result's height ({collapsed_editor_height} vs {split_editor_height})"
        );

        // Clicking the same control expands the body again.
        cx.update_window(window, |_, window, cx| window.click("collapse-result", cx))
            .unwrap();
        cx.run_until_parked();
        cx.update(|cx| assert_eq!(console.read(cx).result_layout, ResultLayout::Split));

        // Maximized: the editor gives up its height; only the way back is offered.
        cx.update_window(window, |_, window, cx| {
            window.render_frame(cx);
            window.click("maximize-result", cx);
        })
        .unwrap();
        cx.run_until_parked();
        cx.update(|cx| assert_eq!(console.read(cx).result_layout, ResultLayout::Maximized));
        let (maximized_y, maximized_height) = cx
            .update_window(window, |_, window, cx| {
                window.render_frame(cx);
                assert!(window.try_find("minimize-result").is_some());
                assert!(window.try_find("collapse-result").is_none());
                assert!(
                    window.try_find("editor-card").is_none(),
                    "the maximized result hides the editor"
                );
                let y = window
                    .try_find("minimize-result")
                    .unwrap()
                    .bounds()
                    .origin
                    .y;
                let height = window
                    .try_find("result-card")
                    .unwrap()
                    .bounds()
                    .size
                    .height
                    .as_f32();
                window.click("minimize-result", cx);
                (y, height)
            })
            .unwrap();
        assert!(
            maximized_y < split_y,
            "the maximized result starts above the split's response header ({maximized_y} vs {split_y})"
        );
        assert!(
            maximized_height > split_height,
            "the maximized result takes the editor's height ({maximized_height} vs {split_height})"
        );
        cx.run_until_parked();
        cx.update(|cx| assert_eq!(console.read(cx).result_layout, ResultLayout::Split));
    }

    /// The toolbar button is the discoverable control; the status bar repeats the mode while it is
    /// on, next to the response the mode protects.
    #[gpui_kit::test]
    fn the_toolbar_turns_readonly_on_and_the_status_bar_says_so(cx: &mut TestAppContext) {
        let (window, console) = console_window(cx);
        cx.update_window(window, |_, window, cx| {
            window.activate_window();
            window.render_frame(cx);
            assert!(window.try_find("readonly-badge").is_none());
            let off = window.try_find("readonly").unwrap();
            assert!(off.label().unwrap().contains("Readonly is off"));
            window.click("readonly", cx);
        })
        .unwrap();
        cx.run_until_parked();
        cx.update(|cx| assert!(console.read(cx).readonly));
        cx.update_window(window, |_, window, cx| {
            window.render_frame(cx);
            assert!(window.try_find("readonly-badge").is_some());
            // The state is spoken as well as shown: the button carries the same wording.
            let on = window.try_find("readonly").unwrap();
            assert!(on.label().unwrap().contains("Readonly is on"));
        })
        .unwrap();
        // The native menu marks the mode with a check, which is its only icon; it is rebuilt on
        // every toggle because a platform menu keeps what it was built with.
        assert_eq!(menu_checked(cx, "Readonly"), Some(true));
        cx.update_window(window, |_, window, cx| {
            window.render_frame(cx);
            window.click("readonly", cx);
        })
        .unwrap();
        cx.run_until_parked();
        cx.update(|cx| assert!(!console.read(cx).readonly));
        assert_eq!(menu_checked(cx, "Readonly"), Some(false));
    }

    /// Whether the View menu item called `name` is checked, from the platform menu bar.
    fn menu_checked(cx: &mut TestAppContext, name: &str) -> Option<bool> {
        cx.update(|cx| {
            cx.get_menus()
                .unwrap_or_default()
                .into_iter()
                .find(|menu| menu.name == "View")
                .and_then(|menu| {
                    menu.items.into_iter().find_map(|item| match item {
                        gpui_kit::OwnedMenuItem::Action {
                            name: item,
                            checked,
                            ..
                        } if item == name => Some(checked),
                        _ => None,
                    })
                })
        })
    }

    /// The View menu actions change the theme immediately and remember the choice.
    #[gpui_kit::test]
    fn choosing_an_appearance_applies_and_remembers_it(cx: &mut TestAppContext) {
        let (window, console) = console_window(cx);
        let path = cx.update(|cx| console.read(cx).appearance_file.clone());
        let choose = |cx: &mut TestAppContext, pref: Pref| {
            cx.update_window(window, |_, window, cx| {
                console.update(cx, |this, cx| this.set_theme(pref, window, cx));
            })
            .unwrap();
            cx.run_until_parked();
            cx.update(|cx| assert_eq!(cx.theme().mode, mode(pref)));
            assert_eq!(Pref::load(&path), pref);
        };
        choose(cx, Pref::Dark);
        choose(cx, Pref::Light);
        // System follows the OS, which is light in the headless window.
        choose(cx, Pref::System);
        cx.update(|cx| assert_eq!(console.read(cx).theme, Pref::System));
    }

    /// The mode `pref` should produce in a headless window, which is light.
    fn mode(pref: Pref) -> ThemeMode {
        match pref {
            Pref::Light | Pref::System => ThemeMode::Light,
            Pref::Dark => ThemeMode::Dark,
        }
    }

    /// An example in the empty response pane fills the editor and moves the cursor to the end, so
    /// the sample can be sent straight away — even before a connection exists.
    #[gpui_kit::test]
    fn a_sample_request_fills_the_empty_editor(cx: &mut TestAppContext) {
        let (window, console) = console_window(cx);
        cx.update_window(window, |_, window, cx| {
            window.activate_window();
            window.render_frame(cx);
            window.click(("sample-request", 0usize), cx);
        })
        .unwrap();
        cx.run_until_parked();
        let editor = cx.update(|cx| console.read(cx).editor.clone());
        cx.update(|cx| {
            let state = editor.read(cx);
            assert_eq!(state.value(), SAMPLES[0].1);
            assert_eq!(state.cursor(), SAMPLES[0].1.len());
        });
    }
}
