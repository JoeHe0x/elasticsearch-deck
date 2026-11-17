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
use gpui_kit::component::{ActiveTheme as _, Disableable as _, Selectable as _, WindowExt as _};
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
    /// The saved connection the form was filled from; Save renames it instead of adding a new one.
    editing: Option<String>,
    /// The last success of Test or Save, shown where the failure would be.
    notice: Option<SharedString>,
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
            notice: None,
            editing: None,
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
        self.editing = self
            .saved
            .iter()
            .any(|s| s.name == c.name)
            .then(|| c.name.clone());
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
        (self.failure, self.editing) = (None, None);
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

    /// Reconnects to the last used connection (the first saved one); a no-op with none saved.
    pub fn reconnect_last(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(name) = self.saved.first().map(|c| c.name.clone()) {
            self.switch_to(&name, window, cx);
        }
    }

    /// The form as a connection, plus the secret typed into it (empty keeps the keychain entry).
    fn read_form(&self, cx: &Context<Self>) -> (Connection, String) {
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
        (conn, self.secret.read(cx).value().to_string())
    }

    fn connect(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (conn, typed) = self.read_form(cx);
        self.start(conn, typed, false, window, cx);
    }

    /// Checks that the cluster answers with the form's settings; nothing is saved or switched.
    fn test(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (conn, typed) = self.read_form(cx);
        self.busy = true;
        (self.failure, self.notice) = (None, None);
        cx.notify();
        cx.spawn_in(window, async move |this, cx| {
            // The keychain may block on the platform service, so it stays off the UI thread.
            let built = cx
                .background_spawn(async move { probe(&conn, &typed) })
                .await;
            let outcome = match built {
                Ok(client) => client.version().await.map_err(|e| e.to_string()),
                Err(e) => Err(e),
            };
            this.update(cx, |this, cx| {
                this.busy = false;
                match outcome {
                    Ok(v) => {
                        this.notice = Some(
                            format!("Connection works. Elasticsearch {}.{}.{}", v[0], v[1], v[2])
                                .into(),
                        )
                    }
                    Err(e) => this.failure = Some(format!("Could not connect. {e}").into()),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Writes the form to the saved list and the keychain without connecting.
    fn save(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (conn, typed) = self.read_form(cx);
        (self.failure, self.notice) = (None, None);
        if conn.name.is_empty() {
            self.failure = Some("Name is required.".into());
            cx.notify();
            return;
        }
        let path = self.path.clone();
        let (old, name) = (self.editing.clone(), conn.name.clone());
        self.busy = true;
        cx.notify();
        cx.spawn_in(window, async move |this, cx| {
            let done = cx
                .background_spawn(async move { persist(&path, &conn, &typed, old.as_deref()) })
                .await;
            this.update(cx, |this, cx| {
                this.busy = false;
                match done {
                    Ok(saved) => {
                        this.saved = saved;
                        this.editing = Some(name);
                        this.notice = Some("Saved.".into());
                    }
                    Err(e) => this.failure = Some(format!("Could not save. {e}").into()),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
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
        (self.failure, self.notice) = (None, None);
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
                .on_ok(move |_, window, cx| {
                    this.update(cx, |this, cx| this.delete(name.clone(), window, cx));
                    true
                })
        });
    }

    /// Removes the connection, then shows the next saved one (or a blank form) so the dialog visibly
    /// changes; a failure is also pushed as a notification, since the form's own message is easy to miss.
    fn delete(&mut self, name: String, window: &mut Window, cx: &mut Context<Self>) {
        let path = self.path.clone();
        cx.spawn_in(window, async move |this, cx| {
            let n = name.clone();
            let done = cx.background_spawn(async move { remove(&path, &n) }).await;
            this.update_in(cx, |this, window, cx| {
                match done {
                    Ok(saved) => {
                        this.saved = saved;
                        let next = this.saved.first().cloned().unwrap_or_default();
                        this.fill(&next, window, cx);
                        window.push_notification(format!("Deleted connection {name}."), cx);
                    }
                    Err(e) => {
                        let msg = format!("Could not delete. {e}");
                        window.push_notification(msg.clone(), cx);
                        this.failure = Some(msg.into());
                    }
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
            .children(self.notice.clone().map(|n| {
                div()
                    .flex_1()
                    .min_w_0()
                    .text_sm()
                    .text_color(cx.theme().success)
                    .child(n)
            }))
            .child(div().flex_1())
            .children(self.is_saved(cx).then(|| {
                Button::new("delete")
                    .outline()
                    .label("Delete…")
                    .on_click(cx.listener(|this, _, window, cx| this.confirm_delete(window, cx)))
            }))
            .child(
                Button::new("test")
                    .outline()
                    .label("Test")
                    .disabled(self.busy)
                    .on_click(cx.listener(|this, _, window, cx| this.test(window, cx))),
            )
            .child(
                Button::new("save")
                    .outline()
                    .label("Save")
                    .disabled(self.busy)
                    .on_click(cx.listener(|this, _, window, cx| this.save(window, cx))),
            )
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

/// Builds a client from the form without touching the keychain or the saved list; a typed secret
/// wins over the stored one.
fn probe(conn: &Connection, typed: &str) -> Result<Client, String> {
    let show = |e: es_client::Error| e.to_string();
    let secret = if conn.auth == AuthKind::None {
        None
    } else if !typed.is_empty() {
        Some(typed.to_string())
    } else {
        Keychain.get(&conn.name).map_err(show)?
    };
    Client::new(conn.config(secret).map_err(show)?).map_err(show)
}

/// Stores the typed secret (if any) and writes the connection in place of `old`, the entry the form
/// was filled from; a new name moves the keychain secret along. Without `old` it is a new entry.
fn persist(
    path: &std::path::Path,
    conn: &Connection,
    typed: &str,
    old: Option<&str>,
) -> Result<Vec<Connection>, String> {
    let show = |e: es_client::Error| e.to_string();
    let mut saved = connection::load(path).map_err(show)?;
    let old = old.filter(|o| *o != conn.name);
    if old.is_some() && saved.iter().any(|c| c.name == conn.name) {
        return Err(format!(
            "A connection named \"{}\" already exists.",
            conn.name
        ));
    }
    if conn.auth != AuthKind::None {
        let secret = match (typed.is_empty(), old) {
            (false, _) => Some(typed.to_string()),
            (true, Some(old)) => Keychain.get(old).map_err(show)?,
            (true, None) => None,
        };
        if let Some(secret) = secret {
            Keychain.set(&conn.name, &secret).map_err(show)?;
        }
    }
    let key = old.unwrap_or(&conn.name);
    match saved.iter_mut().find(|c| c.name == key) {
        Some(slot) => *slot = conn.clone(),
        None => saved.insert(0, conn.clone()),
    }
    connection::save(path, &saved).map_err(show)?;
    if let Some(old) = old {
        Keychain.delete(old).map_err(show)?;
    }
    Ok(saved)
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
