//! Connection settings, shown in a dialog: the saved connections on the left, the selected one on the
//! right. The saved list holds no secrets; the secret goes to the OS keychain and is read back from
//! there on later connects.

use std::path::PathBuf;
use std::sync::Arc;

use es_client::connection::{self, AuthKind, Connection, Keychain, Secrets};
use es_client::{Client, ClusterMeta};
use es_dsl_core::Version;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::dialog::AlertDialog;
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::radio::{Radio, RadioGroup};
use gpui_kit::component::{ActiveTheme as _, Selectable as _, WindowExt as _};
use gpui_kit::{
    AnyElement, AppContext as _, Context, Entity, EventEmitter, IntoElement, ParentElement as _,
    Render, SharedString, Styled as _, Window, div,
};

/// A cluster that answered `GET /`; the owner starts completion against it.
pub struct Connected {
    pub client: Arc<Client>,
    pub version: Version,
    pub meta: ClusterMeta,
    pub name: String,
}

impl EventEmitter<Connected> for ConnectPanel {}

const AUTH: [(AuthKind, &str); 4] = [
    (AuthKind::None, "None"),
    (AuthKind::Basic, "Basic"),
    (AuthKind::ApiKey, "API key"),
    (AuthKind::Bearer, "Bearer token"),
];

pub struct ConnectPanel {
    path: PathBuf,
    name: Entity<InputState>,
    url: Entity<InputState>,
    user: Entity<InputState>,
    secret: Entity<InputState>,
    ca_file: Entity<InputState>,
    timeout: Entity<InputState>,
    insecure: bool,
    auth: AuthKind,
    /// The last failure, shown next to the actions; cleared by the next attempt.
    failure: Option<SharedString>,
    busy: bool,
    saved: Vec<Connection>,
}

impl ConnectPanel {
    pub fn new(path: PathBuf, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let (saved, unreadable) = match connection::load(&path) {
            Ok(saved) => (saved, None),
            Err(e) => (Vec::new(), Some(SharedString::from(e.to_string()))),
        };
        let input = |hint: &str, window: &mut Window, cx: &mut Context<Self>| {
            let hint = hint.to_string();
            cx.new(|cx| InputState::new(window, cx).placeholder(hint))
        };
        let secret = cx.new(|cx| InputState::new(window, cx).masked(true));
        let mut panel = Self {
            name: input("local", window, cx),
            url: input("https://localhost:9200", window, cx),
            user: input("elastic", window, cx),
            secret,
            ca_file: input("Optional PEM file", window, cx),
            timeout: input("30", window, cx),
            insecure: false,
            auth: AuthKind::None,
            failure: None,
            busy: false,
            saved,
            path,
        };
        match panel.saved.first().cloned() {
            Some(first) => panel.fill(&first, window, cx),
            None => panel.fill(&Connection::default(), window, cx),
        }
        panel.failure = unreadable;
        panel
    }

    /// Fills the form. The secret box stays empty, so connecting reuses the keychain entry.
    fn fill(&mut self, c: &Connection, window: &mut Window, cx: &mut Context<Self>) {
        let set = |s: &Entity<InputState>, v: &str, window: &mut Window, cx: &mut Context<Self>| {
            s.update(cx, |s, cx| s.set_value(v.to_string(), window, cx));
        };
        set(&self.name, &c.name, window, cx);
        set(&self.url, &c.url, window, cx);
        set(&self.user, &c.user, window, cx);
        set(&self.secret, "", window, cx);
        set(&self.ca_file, &c.ca_file, window, cx);
        let secs = if c.timeout_secs > 0 {
            c.timeout_secs.to_string()
        } else {
            String::new()
        };
        set(&self.timeout, &secs, window, cx);
        (self.auth, self.insecure, self.failure) = (c.auth, c.insecure, None);
        cx.notify();
    }

    /// Clears the form for a new connection, or keeps it as a copy under a new name. A copy has no
    /// secret: the keychain entry belongs to the original name.
    pub fn start_new(&mut self, copy: bool, window: &mut Window, cx: &mut Context<Self>) {
        if !copy {
            self.fill(&Connection::default(), window, cx);
            return;
        }
        let name = format!("{} copy", self.name(cx));
        self.name.update(cx, |s, cx| s.set_value(name, window, cx));
        self.secret.update(cx, |s, cx| s.set_value("", window, cx));
        self.failure = None;
        cx.notify();
    }

    fn name(&self, cx: &Context<Self>) -> String {
        self.name.read(cx).value().trim().to_string()
    }

    fn is_saved(&self, cx: &Context<Self>) -> bool {
        let name = self.name(cx);
        self.saved.iter().any(|c| c.name == name)
    }

    /// Saved connection names, most recently used first.
    pub fn names(&self) -> Vec<String> {
        self.saved.iter().map(|c| c.name.clone()).collect()
    }

    /// Connects to a saved connection with its keychain secret, without the dialog; a failure is
    /// shown as a notification.
    pub fn switch_to(&mut self, name: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some(conn) = self.saved.iter().find(|c| c.name == name).cloned() else {
            return;
        };
        self.fill(&conn, window, cx);
        self.start(conn, String::new(), true, window, cx);
    }

    fn connect(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let text =
            |s: &Entity<InputState>, cx: &Context<Self>| s.read(cx).value().trim().to_string();
        let conn = Connection {
            name: text(&self.name, cx),
            url: text(&self.url, cx),
            auth: self.auth,
            user: text(&self.user, cx),
            ca_file: text(&self.ca_file, cx),
            insecure: self.insecure,
            // Anything that is not a whole number of seconds keeps the default.
            timeout_secs: text(&self.timeout, cx).parse().unwrap_or(0),
        };
        let typed = self.secret.read(cx).value().to_string();
        self.start(conn, typed, false, window, cx);
    }

    /// Connects in the background. `quiet` means no dialog is open, so a failure is pushed as a
    /// notification instead of shown in the form.
    fn start(
        &mut self,
        conn: Connection,
        typed: String,
        quiet: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let path = self.path.clone();
        self.busy = true;
        self.failure = None;
        cx.notify();
        cx.spawn_in(window, async move |this, cx| {
            // The keychain may block on the platform service, so it stays off the UI thread.
            let c = conn.clone();
            let prepared = cx
                .background_spawn(async move { prepare(&path, &c, &typed) })
                .await;
            let outcome = match prepared {
                Ok((client, saved)) => {
                    let client = Arc::new(client);
                    client
                        .version()
                        .await
                        .map(|version| {
                            (
                                Connected {
                                    client,
                                    version,
                                    meta: ClusterMeta::default(),
                                    name: conn.name.clone(),
                                },
                                saved,
                            )
                        })
                        .map_err(|e| e.to_string())
                }
                Err(e) => Err(e),
            };
            this.update_in(cx, |this, window, cx| {
                this.busy = false;
                match outcome {
                    Ok((done, saved)) => {
                        this.saved = saved;
                        cx.emit(done);
                        if !quiet {
                            window.close_dialog(cx);
                        }
                    }
                    Err(e) if quiet => {
                        window.push_notification(format!("Could not connect. {e}"), cx)
                    }
                    Err(e) => this.failure = Some(format!("Could not connect. {e}").into()),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Asks before removing the connection named in the form; a saved password cannot be recovered.
    fn confirm_delete(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let name = self.name(cx);
        let this = cx.entity();
        window.open_alert_dialog(cx, move |alert: AlertDialog, _, _| {
            let (this, name) = (this.clone(), name.clone());
            alert
                .title(format!("Delete connection \"{name}\"?"))
                .description("Its saved password, API key or token is removed from the keychain.")
                .ok_text("Delete connection")
                .show_cancel(true)
                .on_ok(move |_, _, cx| {
                    this.update(cx, |this, cx| this.delete(name.clone(), cx));
                    true
                })
        });
    }

    fn delete(&mut self, name: String, cx: &mut Context<Self>) {
        let path = self.path.clone();
        cx.spawn(async move |this, cx| {
            let n = name.clone();
            let done = cx.background_spawn(async move { remove(&path, &n) }).await;
            this.update(cx, |this, cx| {
                match done {
                    Ok(saved) => this.saved = saved,
                    Err(e) => this.failure = Some(format!("Could not delete. {e}").into()),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// One labelled form row.
    fn field(label: &str, input: impl IntoElement, cx: &Context<Self>) -> AnyElement {
        div()
            .flex()
            .flex_col()
            .gap_1()
            .child(
                div()
                    .text_sm()
                    .text_color(cx.theme().muted_foreground)
                    .child(label.to_string()),
            )
            .child(input)
            .into_any_element()
    }

    fn render_saved(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let current = self.name(cx);
        div()
            .flex()
            .flex_col()
            .gap_1()
            .w_48()
            .flex_none()
            .child(
                Button::new("new-connection")
                    .outline()
                    .label("New connection")
                    .on_click(cx.listener(|this, _, window, cx| {
                        let blank = Connection {
                            url: "http://localhost:9200".into(),
                            ..Connection::default()
                        };
                        this.fill(&blank, window, cx);
                    })),
            )
            .children(self.saved.iter().map(|c| {
                let target = c.clone();
                Button::new(("saved", row_id(&c.name)))
                    .ghost()
                    .selected(c.name == current)
                    .w_full()
                    .child(
                        div()
                            .w_full()
                            .truncate()
                            .text_left()
                            .child(SharedString::from(c.name.clone())),
                    )
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.fill(&target, window, cx);
                    }))
            }))
    }

    fn render_form(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let this = cx.entity();
        let auth_ix = AUTH.iter().position(|(k, _)| *k == self.auth);
        let on_auth = {
            let this = this.clone();
            move |ix: &usize, _: &mut Window, cx: &mut gpui_kit::App| {
                this.update(cx, |t, cx| {
                    t.auth = AUTH[*ix].0;
                    cx.notify();
                })
            }
        };
        let secret_label = match self.auth {
            AuthKind::Basic => "Password",
            AuthKind::ApiKey => "API key",
            _ => "Token",
        };
        div()
            .flex()
            .flex_col()
            .gap_3()
            .flex_1()
            .min_w_0()
            .child(Self::field(
                "Name",
                Input::new(&self.name).aria_label("Name"),
                cx,
            ))
            .child(Self::field(
                "URL",
                Input::new(&self.url).aria_label("URL"),
                cx,
            ))
            .child(Self::field(
                "Authentication",
                RadioGroup::horizontal("auth")
                    .children(
                        AUTH.iter()
                            .enumerate()
                            .map(|(i, (_, l))| Radio::new(("auth", i)).label(*l)),
                    )
                    .selected_index(auth_ix)
                    .on_click(on_auth),
                cx,
            ))
            .children(
                (self.auth == AuthKind::Basic)
                    .then(|| Self::field("User", Input::new(&self.user).aria_label("User"), cx)),
            )
            .children((self.auth != AuthKind::None).then(|| {
                Self::field(
                    secret_label,
                    Input::new(&self.secret)
                        .aria_label(secret_label)
                        .mask_toggle(),
                    cx,
                )
            }))
            .child(Self::field(
                "CA certificate",
                Input::new(&self.ca_file).aria_label("CA certificate"),
                cx,
            ))
            .child(Self::field(
                "Timeout (seconds)",
                Input::new(&self.timeout).aria_label("Timeout in seconds"),
                cx,
            ))
            .child(
                Checkbox::new("insecure")
                    .label("Skip certificate verification")
                    .checked(self.insecure)
                    .on_click(move |checked: &bool, _, cx| {
                        this.update(cx, |t, cx| {
                            t.insecure = *checked;
                            cx.notify();
                        })
                    }),
            )
            .children(self.insecure.then(|| {
                div().text_sm().text_color(cx.theme().warning).child(
                    "The server's identity is not checked. Use this only on a network you trust.",
                )
            }))
    }

    fn render_actions(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let hint = self.auth != AuthKind::None;
        div()
            .flex()
            .items_center()
            .gap_2()
            .children(self.failure.clone().map(|f| {
                div()
                    .flex_1()
                    .min_w_0()
                    .text_sm()
                    .text_color(cx.theme().danger)
                    .child(f)
            }))
            .child(div().flex_1())
            .children(self.is_saved(cx).then(|| {
                Button::new("delete")
                    .outline()
                    .label("Delete…")
                    .on_click(cx.listener(|this, _, window, cx| this.confirm_delete(window, cx)))
            }))
            .child(
                Button::new("connect")
                    .primary()
                    .label("Connect")
                    .loading(self.busy)
                    .tooltip(if hint {
                        "An empty secret keeps the one in the keychain"
                    } else {
                        "Connect"
                    })
                    .on_click(cx.listener(|this, _, window, cx| this.connect(window, cx))),
            )
    }
}

/// Stores the typed secret (if any), then builds a client from what the keychain holds.
fn prepare(
    path: &std::path::Path,
    conn: &Connection,
    typed: &str,
) -> Result<(Client, Vec<Connection>), String> {
    let show = |e: es_client::Error| e.to_string();
    if conn.auth != AuthKind::None && !typed.is_empty() {
        Keychain.set(&conn.name, typed).map_err(show)?;
    }
    let secret = if conn.auth == AuthKind::None {
        None
    } else {
        Keychain.get(&conn.name).map_err(show)?
    };
    let config = conn.config(secret).map_err(show)?;
    let client = Client::new(config).map_err(show)?;
    let mut saved = connection::load(path).map_err(show)?;
    // The newest connection goes first: the form and the switcher default to the last one used.
    saved.retain(|c| c.name != conn.name);
    saved.insert(0, conn.clone());
    connection::save(path, &saved).map_err(show)?;
    Ok((client, saved))
}

/// Drops `name` from the saved list and the keychain; returns the remaining list.
fn remove(path: &std::path::Path, name: &str) -> Result<Vec<Connection>, String> {
    let show = |e: es_client::Error| e.to_string();
    let mut saved = connection::load(path).map_err(show)?;
    saved.retain(|c| c.name != name);
    connection::save(path, &saved).map_err(show)?;
    Keychain.delete(name).map_err(show)?;
    Ok(saved)
}

/// A stable element id for a saved connection, so rows keep their identity when the list changes.
pub(crate) fn row_id(name: &str) -> u64 {
    use std::hash::{DefaultHasher, Hash as _, Hasher as _};
    let mut h = DefaultHasher::new();
    name.hash(&mut h);
    h.finish()
}

impl Render for ConnectPanel {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .flex()
            .flex_col()
            .gap_4()
            .child(
                div()
                    .flex()
                    .items_start()
                    .gap_4()
                    .child(self.render_saved(cx))
                    .child(self.render_form(cx)),
            )
            .child(self.render_actions(cx))
    }
}
