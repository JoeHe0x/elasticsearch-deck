//! Connection settings, shown in a dialog: the saved connections on the left, the selected one on the
//! right. The saved list holds no secrets; the secret goes to the OS keychain and is read back from
//! there on later connects.

use std::path::PathBuf;
use std::sync::Arc;

use es_client::connection::{self, AuthKind, Connection, Keychain, Secrets};
use es_client::{Client, ClusterMeta};
use es_dsl_core::Version;
use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::dialog::AlertDialog;
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::radio::{Radio, RadioGroup};
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Icon, Selectable as _, Sizable as _, WindowExt as _,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AnyElement, AppContext as _, Context, Entity, EventEmitter, InteractiveElement as _,
    IntoElement, ParentElement as _, Render, SharedString, Styled as _, Window, div, rems,
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
            None => panel.fill(&blank(), window, cx),
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
            self.fill(&blank(), window, cx);
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
                        // Connecting also saves the form's connection, so the list lights it up.
                        this.editing = Some(done.name.clone());
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
                        let next = this.saved.first().cloned().unwrap_or_else(blank);
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
        // The row stays lit while the form is edited: `editing` survives a rename in progress,
        // where the name field no longer matches the saved entry.
        let current = self.editing.clone();
        let muted = cx.theme().muted_foreground;
        div()
            .flex()
            .flex_col()
            .gap_2()
            .w_48()
            .flex_none()
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .text_xs()
                    .text_color(muted)
                    .child("Saved connections")
                    .children((!self.saved.is_empty()).then(|| self.saved.len().to_string())),
            )
            .child(
                Button::new("new-connection")
                    .outline()
                    .w_full()
                    .icon(IconName::Plus)
                    .label("New connection")
                    .on_click(cx.listener(|this, _, window, cx| {
                        let blank = blank();
                        this.fill(&blank, window, cx);
                    })),
            )
            .child(
                div()
                    .id("saved-connections")
                    .flex()
                    .flex_col()
                    .gap_1()
                    // Scrolling keeps a long list from pushing the actions out of the dialog.
                    .max_h(rems(22.))
                    .overflow_y_scrollbar()
                    .children(self.saved.is_empty().then(|| {
                        div()
                            .flex()
                            .flex_col()
                            .gap_1()
                            .px_2()
                            .py_1()
                            .child(
                                div()
                                    .text_sm()
                                    .text_color(muted)
                                    .child("No saved connections"),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(muted)
                                    .child("Connections you save appear here."),
                            )
                    }))
                    .children(self.saved.iter().map(|c| {
                        let target = c.clone();
                        Button::new(("saved", row_id(&c.name)))
                            .ghost()
                            .selected(current.as_deref() == Some(c.name.as_str()))
                            .w_full()
                            .icon(IconName::Database)
                            .accessibility_label(c.name.clone())
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .truncate()
                                    .text_left()
                                    .child(SharedString::from(c.name.clone())),
                            )
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.fill(&target, window, cx);
                            }))
                    })),
            )
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
                let warning = cx.theme().warning;
                div()
                    .flex()
                    .items_start()
                    .gap_2()
                    .text_sm()
                    .text_color(warning)
                    .child(Icon::new(IconName::TriangleAlert).small().text_color(warning))
                    .child(
                        "The server's identity is not checked. Use this only on a network you trust.",
                    )
            }))
    }

    /// The result of the last Test, Save or Connect, on its own line so a long error never squeezes
    /// the buttons. A failure and a notice never coexist: every attempt clears both first.
    fn render_status(&self, cx: &Context<Self>) -> Option<AnyElement> {
        let (icon, color, text) = match (&self.failure, &self.notice) {
            (Some(failure), _) => (IconName::TriangleAlert, cx.theme().danger, failure),
            (None, Some(notice)) => (IconName::CircleCheck, cx.theme().success, notice),
            (None, None) => return None,
        };
        Some(
            div()
                .flex()
                .items_start()
                .gap_2()
                .text_sm()
                .text_color(color)
                .child(Icon::new(icon).small().text_color(color))
                .child(div().min_w_0().child(text.clone()))
                .into_any_element(),
        )
    }

    fn render_actions(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let hint = self.auth != AuthKind::None;
        let status = self.render_status(cx);
        div().flex().flex_col().gap_3().children(status).child(
            div()
                .flex()
                .items_center()
                .gap_2()
                .children(self.is_saved(cx).then(|| {
                    Button::new("delete")
                        .outline()
                        .danger()
                        .icon(IconName::Trash)
                        .label("Delete…")
                        .tooltip("Remove this connection and its saved secret")
                        .on_click(
                            cx.listener(|this, _, window, cx| this.confirm_delete(window, cx)),
                        )
                }))
                .child(div().flex_1())
                .child(
                    Button::new("test")
                        .outline()
                        .icon(IconName::Play)
                        .label("Test connection")
                        .disabled(self.busy)
                        .on_click(cx.listener(|this, _, window, cx| this.test(window, cx))),
                )
                .child(
                    Button::new("save")
                        .outline()
                        .icon(IconName::Save)
                        .label("Save")
                        .disabled(self.busy)
                        .on_click(cx.listener(|this, _, window, cx| this.save(window, cx))),
                )
                .child(
                    Button::new("connect")
                        .primary()
                        .icon(IconName::Plug)
                        .label("Connect")
                        .loading(self.busy)
                        // Only worth a tooltip when it says something the label does not.
                        .when(hint, |button| {
                            button.tooltip("An empty secret keeps the one in the keychain")
                        })
                        .on_click(cx.listener(|this, _, window, cx| this.connect(window, cx))),
                ),
        )
    }
}

/// A new connection form pointed at the default local cluster; the name stays empty until typed.
fn blank() -> Connection {
    Connection {
        url: "http://localhost:9200".into(),
        ..Connection::default()
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
                    .gap_6()
                    .child(self.render_saved(cx))
                    .child(self.render_form(cx)),
            )
            .child(self.render_actions(cx))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui_kit::TestAppContext;
    use gpui_kit::test::{TestQueryExt as _, TestWindowExt as _};

    fn conn(name: &str, url: &str) -> Connection {
        Connection {
            name: name.into(),
            url: url.into(),
            ..Connection::default()
        }
    }

    /// Opening a saved connection from the list fills the form with it, and New connection clears
    /// the form back to the defaults; both are reached through the dialog's own rows and buttons.
    #[gpui_kit::test]
    fn the_saved_list_fills_the_form_and_new_connection_clears_it(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        let (window, _panel) = cx
            .update(|cx| {
                gpui_kit::open_window(Default::default(), cx, |window, cx| {
                    cx.new(|cx| {
                        // A missing file loads as an empty list; the test supplies its own.
                        let mut panel = ConnectPanel::new(PathBuf::from("unused.json"), window, cx);
                        panel.saved = vec![
                            conn("local", "http://localhost:9200"),
                            conn("prod", "https://prod.example:9243"),
                        ];
                        let first = panel.saved[0].clone();
                        panel.fill(&first, window, cx);
                        panel
                    })
                })
            })
            .unwrap();

        cx.update_window(window, |_, window, cx| {
            window.activate_window();
            window.render_frame(cx);
            assert!(window.try_find_by_label("New connection").is_some());
            assert_eq!(window.find_by_label("Name").value(), Some("local"));
            window.click(("saved", row_id("prod")), cx);
        })
        .unwrap();
        cx.update_window(window, |_, window, cx| {
            window.render_frame(cx);
            assert_eq!(window.find_by_label("Name").value(), Some("prod"));
            assert_eq!(
                window.find_by_label("URL").value(),
                Some("https://prod.example:9243")
            );
            window.click("new-connection", cx);
        })
        .unwrap();
        cx.update_window(window, |_, window, cx| {
            window.render_frame(cx);
            assert_eq!(window.find_by_label("Name").value(), Some(""));
            assert_eq!(
                window.find_by_label("URL").value(),
                Some("http://localhost:9200")
            );
        })
        .unwrap();
    }
}
