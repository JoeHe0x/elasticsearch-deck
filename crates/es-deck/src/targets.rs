//! The connection's tree: saved items and cluster objects as filterable, collapsible folders.

use std::collections::{HashMap, HashSet};

use gpui_kit::assets::IconName;
use gpui_kit::component::list::{ListDelegate, ListItem, ListState};
use gpui_kit::component::menu::{ContextMenuExt as _, PopupMenuItem};
use gpui_kit::component::{ActiveTheme as _, Icon, IndexPath, Sizable as _};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    App, Context, FontWeight, InteractiveElement as _, ParentElement as _, Role, SharedString,
    StatefulInteractiveElement as _, Styled as _, Task, TestSupportExt as _, Window, div, px, rems,
};

use gpui_kit::actions;

use crate::connect::row_id;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Folder {
    Consoles,
    Indices,
    Aliases,
    Nodes,
    Templates,
    Tasks,
    Pipelines,
}

/// Display order.
const FOLDERS: [Folder; 7] = [
    Folder::Consoles,
    Folder::Indices,
    Folder::Aliases,
    Folder::Nodes,
    Folder::Templates,
    Folder::Tasks,
    Folder::Pipelines,
];

impl Folder {
    fn slot(self) -> usize {
        FOLDERS.iter().position(|f| *f == self).unwrap_or(0)
    }

    pub fn title(self) -> &'static str {
        match self {
            Self::Consoles => "Consoles",
            Self::Indices => "Indices",
            Self::Aliases => "Aliases",
            Self::Nodes => "Nodes",
            Self::Templates => "Templates",
            Self::Tasks => "Tasks",
            Self::Pipelines => "Ingest pipelines",
        }
    }

    /// The icon of a row in this folder.
    pub fn icon(self) -> IconName {
        match self {
            Self::Consoles => IconName::SquareTerminal,
            Self::Indices => IconName::Table2,
            Self::Aliases => IconName::Link2,
            Self::Nodes => IconName::Server,
            Self::Tasks => IconName::Clock,
            Self::Pipelines => IconName::ListTree,
            Self::Templates => IconName::FileText,
        }
    }

    /// The user's own bucket stays listed while empty, so it is visible where saved items go.
    fn always_shown(self) -> bool {
        matches!(self, Self::Consoles)
    }
}

actions!(targets, [OpenInDataBrowser, OpenInRestConsole]);

/// What the right-click menu of a row asks for, and the one command a click on a chevron sends.
#[derive(Debug, Clone, Copy)]
pub enum ConsoleOp {
    Rename,
    Delete,
    /// Index and alias rows only.
    OpenBrowser,
    OpenConsole,
    /// Index and alias rows only: show or hide the row's mapping fields.
    ToggleFields,
}

/// Called with the operation and the console's name; the owner of the tree performs it.
pub type ConsoleMenu = std::rc::Rc<dyn Fn(ConsoleOp, String, &mut Window, &mut App)>;

/// One entry in a folder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    pub label: String,
    /// What a click puts in the editor.
    pub text: String,
    /// A closed index: listed, but drawn dimmed.
    pub closed: bool,
}

/// One mapping field of an index or alias, as an expanded row lists it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FieldRow {
    pub name: String,
    /// The mapping type (`text`, `keyword`, `long`, ...), shown beside the name.
    pub ty: String,
    /// What the type holds, which picks the row's icon.
    pub kind: es_dsl_core::FieldKind,
}

impl FieldRow {
    /// Builds a row from a mapping type name, classified the same way completion classifies it.
    pub fn new(name: impl Into<String>, ty: impl Into<String>) -> Self {
        let ty = ty.into();
        Self {
            kind: es_dsl_core::FieldKind::of(&ty),
            name: name.into(),
            ty,
        }
    }
}

/// A field of an expanded row as the list nests it: an object holds the fields below it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct FieldNode {
    /// The last path segment, shown in the row (`realm`).
    name: String,
    /// The full dotted path (`access_token.realm`); the expansion key and the breadcrumb name.
    path: String,
    ty: String,
    kind: es_dsl_core::FieldKind,
    children: Vec<FieldNode>,
}

/// One visible row of a field tree, flattened for the list.
#[derive(Debug, Clone, PartialEq, Eq)]
struct FieldLine {
    /// The text shown: a leaf's last segment, or the whole dotted name for a row that stays flat.
    name: String,
    /// The full dotted path.
    path: String,
    ty: String,
    kind: es_dsl_core::FieldKind,
    depth: usize,
    /// An object or nested field that holds the rows below it.
    expandable: bool,
    open: bool,
}

/// Groups a mapping's flat, dotted fields into the tree the list shows. A field nests under its
/// parent only when that parent is itself a listed object or nested field; a multi-field such as
/// `title.keyword` and a field whose parent is not listed keep their whole dotted name at the top.
fn field_tree(rows: &[FieldRow]) -> Vec<FieldNode> {
    use es_dsl_core::FieldKind;
    let containers: HashSet<&str> = rows
        .iter()
        .filter(|row| matches!(row.kind, FieldKind::Object | FieldKind::Nested))
        .map(|row| row.name.as_str())
        .collect();
    let mut children: HashMap<&str, Vec<&FieldRow>> = HashMap::new();
    let mut roots: Vec<&FieldRow> = Vec::new();
    for row in rows {
        match row.name.rsplit_once('.') {
            Some((parent, _)) if containers.contains(parent) => {
                children.entry(parent).or_default().push(row);
            }
            _ => roots.push(row),
        }
    }
    fn node(row: &FieldRow, name: String, children: &HashMap<&str, Vec<&FieldRow>>) -> FieldNode {
        let kids = children.get(row.name.as_str());
        FieldNode {
            name,
            path: row.name.clone(),
            ty: row.ty.clone(),
            kind: row.kind,
            children: kids
                .map(|kids| {
                    kids.iter()
                        .map(|kid| {
                            let name = kid.name.rsplit('.').next().unwrap_or(&kid.name);
                            node(kid, name.to_string(), children)
                        })
                        .collect()
                })
                .unwrap_or_default(),
        }
    }
    roots
        .iter()
        .map(|row| node(row, row.name.clone(), &children))
        .collect()
}

/// Flattens the open part of a field tree into the rows the list draws.
fn flatten_fields(
    nodes: &[FieldNode],
    key: &(usize, String),
    expanded: &HashSet<(usize, String, String)>,
    depth: usize,
    out: &mut Vec<FieldLine>,
) {
    for node in nodes {
        let expandable = !node.children.is_empty();
        let open = expandable && expanded.contains(&(key.0, key.1.clone(), node.path.clone()));
        out.push(FieldLine {
            name: node.name.clone(),
            path: node.path.clone(),
            ty: node.ty.clone(),
            kind: node.kind,
            depth,
            expandable,
            open,
        });
        if open {
            flatten_fields(&node.children, key, expanded, depth + 1, out);
        }
    }
}

/// Whether the node at `path` holds rows to open.
fn holds_fields(nodes: &[FieldNode], path: &str) -> bool {
    nodes.iter().any(|node| {
        (node.path == path && !node.children.is_empty()) || holds_fields(&node.children, path)
    })
}

/// The selected line as the title bar names it: its folder, the row's label, and for a field line
/// the field's name and kind.
pub type SelectedPath<'a> = (
    Folder,
    Option<&'a str>,
    Option<(&'a str, es_dsl_core::FieldKind)>,
);

/// The icon for a field's type: the shape of the value at a glance, not just its name.
pub(crate) fn field_icon(kind: es_dsl_core::FieldKind) -> IconName {
    use es_dsl_core::FieldKind;
    match kind {
        FieldKind::Text => IconName::Type,
        FieldKind::Keyword => IconName::Tag,
        FieldKind::Numeric => IconName::Hash,
        FieldKind::Date => IconName::Calendar,
        FieldKind::Boolean => IconName::ToggleLeft,
        FieldKind::Ip => IconName::Globe,
        FieldKind::Geo => IconName::MapPin,
        FieldKind::Object | FieldKind::Nested => IconName::Boxes,
        FieldKind::Version | FieldKind::Other => IconName::CircleSmall,
    }
}

/// The request a click on an index or alias inserts.
pub fn search_text(name: &str) -> String {
    format!("GET {name}/_search\n{{\n  \"query\": {{\n    \"match_all\": {{}}\n  }}\n}}\n")
}

/// One visible line of the tree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Line {
    /// A folder, by slot.
    Folder(usize),
    /// A row of a folder, by slot and position in `shown`.
    Item(usize, usize),
    /// An expanded row whose mapping has not arrived yet, by slot and row.
    Loading(usize, usize),
    /// A mapping field of an expanded row: slot, row, field.
    Field(usize, usize, usize),
}

/// The tree is one flat list: a folder line followed by its rows while it is open. The kit's list
/// draws no header for an empty section, so real sections would make a collapsed folder vanish.
pub struct Targets {
    all: [Vec<Row>; 7],
    query: String,
    shown: [Vec<Row>; 7],
    lines: Vec<Line>,
    selected: Option<IndexPath>,
    /// Folders the user collapsed, by slot; everything but the indices starts closed.
    collapsed: [bool; 7],
    /// Index and alias rows whose fields are shown, by slot and label.
    expanded: HashSet<(usize, String)>,
    /// The mapping fields of rows that have been read, as trees. A missing entry under an
    /// expanded row means the mapping is still on its way.
    trees: HashMap<(usize, String), Vec<FieldNode>>,
    /// The open part of those trees, flattened for the list. Rebuilt on every relayout.
    fields: HashMap<(usize, String), Vec<FieldLine>>,
    /// Object fields opened inside an expanded row, by slot, row label and full path.
    expanded_fields: HashSet<(usize, String, String)>,
    /// The last activated line and when, to tell a double click from two clicks.
    last_click: Option<(usize, std::time::Instant)>,
    menu: Option<ConsoleMenu>,
}

impl Default for Targets {
    fn default() -> Self {
        let mut t = Self {
            all: Default::default(),
            query: String::new(),
            shown: Default::default(),
            lines: Vec::new(),
            selected: None,
            collapsed: FOLDERS.map(|f| f != Folder::Indices),
            expanded: HashSet::new(),
            trees: HashMap::new(),
            fields: HashMap::new(),
            expanded_fields: HashSet::new(),
            last_click: None,
            menu: None,
        };
        t.relayout();
        t
    }
}

impl Targets {
    /// Replaces a folder's rows. Returns whether anything changed, so the caller can skip a redraw.
    pub fn set(&mut self, folder: Folder, rows: Vec<Row>) -> bool {
        let slot = folder.slot();
        if self.all[slot] == rows {
            return false;
        }
        self.all[slot] = rows;
        // A row that is gone takes its expansion and cached mapping with it.
        let labels: Vec<&str> = self.all[slot].iter().map(|r| r.label.as_str()).collect();
        self.expanded
            .retain(|(s, label)| *s != slot || labels.contains(&label.as_str()));
        self.trees
            .retain(|(s, label), _| *s != slot || labels.contains(&label.as_str()));
        self.fields
            .retain(|(s, label), _| *s != slot || labels.contains(&label.as_str()));
        self.expanded_fields
            .retain(|(s, label, _)| *s != slot || labels.contains(&label.as_str()));
        self.refilter();
        true
    }

    /// Gives the console rows a right-click menu and the index rows their expand control.
    pub fn set_menu(&mut self, menu: ConsoleMenu) {
        self.menu = Some(menu);
    }

    /// Drops every row, e.g. on disconnect.
    pub fn clear(&mut self) {
        self.all = Default::default();
        self.expanded.clear();
        self.trees.clear();
        self.fields.clear();
        self.expanded_fields.clear();
        self.refilter();
    }

    /// Opens or closes an index or alias row's field list. `fields` is the mapping when it is
    /// already known; `None` leaves a loading line until [`Targets::set_fields`] supplies one.
    /// Returns whether the row is now expanded.
    pub fn toggle_fields(&mut self, label: &str, fields: Option<Vec<FieldRow>>) -> bool {
        let slot = (0..FOLDERS.len()).find(|slot| {
            matches!(FOLDERS[*slot], Folder::Indices | Folder::Aliases)
                && self.all[*slot].iter().any(|r| r.label == label)
        });
        let Some(slot) = slot else { return false };
        let key = (slot, label.to_string());
        if self.expanded.remove(&key) {
            self.relayout();
            return false;
        }
        if let Some(fields) = fields {
            self.trees.insert(key.clone(), field_tree(&fields));
        }
        self.expanded.insert(key);
        self.relayout();
        true
    }

    /// Replaces the fields of an expanded row. `None` means the mapping is still missing, which
    /// keeps the loading line. Returns whether the tree changed.
    pub fn set_fields(&mut self, target: &str, fields: Option<Vec<FieldRow>>) -> bool {
        let Some(fields) = fields else { return false };
        let Some(key) = self
            .expanded
            .iter()
            .find(|(_, label)| label == target)
            .cloned()
        else {
            return false;
        };
        let tree = field_tree(&fields);
        if self.trees.get(&key) == Some(&tree) {
            return false;
        }
        self.trees.insert(key, tree);
        self.relayout();
        true
    }

    /// Opens or closes an object field's children inside an expanded row. Returns whether it is
    /// now open; false when the path does not hold fields.
    pub fn toggle_field(&mut self, label: &str, path: &str) -> bool {
        let Some(slot) = self
            .expanded
            .iter()
            .find(|(_, row)| row == label)
            .map(|(slot, _)| *slot)
        else {
            return false;
        };
        let nodes = self.trees.get(&(slot, label.to_string()));
        if !nodes.is_some_and(|nodes| holds_fields(nodes, path)) {
            return false;
        }
        let key = (slot, label.to_string(), path.to_string());
        let open = if self.expanded_fields.remove(&key) {
            false
        } else {
            self.expanded_fields.insert(key);
            true
        };
        self.relayout();
        open
    }

    /// The labels whose mapping the caller should supply to [`Targets::set_fields`].
    pub fn expanded_targets(&self) -> Vec<String> {
        self.expanded
            .iter()
            .map(|(_, label)| label.clone())
            .collect()
    }

    /// The mapping field shown at `slot`/`i`/`f`, when its row is expanded and loaded.
    fn field_line(&self, slot: usize, i: usize, f: usize) -> Option<&FieldLine> {
        let label = &self.shown[slot].get(i)?.label;
        self.fields.get(&(slot, label.clone()))?.get(f)
    }

    /// Whether rows of this folder expand to show fields.
    fn expandable(slot: usize) -> bool {
        matches!(FOLDERS[slot], Folder::Indices | Folder::Aliases)
    }

    /// The selected row of a folder; a selected folder line or field gives `None`.
    pub fn selected(&self) -> Option<(Folder, &Row)> {
        match self.lines.get(self.selected?.row)? {
            Line::Item(slot, i) => Some((FOLDERS[*slot], self.shown[*slot].get(*i)?)),
            Line::Folder(_) | Line::Loading(_, _) | Line::Field(_, _, _) => None,
        }
    }

    /// The selected line as the title bar names it: its folder, the row's label, and for a field
    /// line the field's name and type. A folder line has neither name, so selecting `Nodes` reads
    /// as the folder alone.
    pub fn selected_path(&self) -> Option<SelectedPath<'_>> {
        match self.lines.get(self.selected?.row)? {
            Line::Folder(slot) => Some((FOLDERS[*slot], None, None)),
            Line::Item(slot, i) => {
                let row = self.shown[*slot].get(*i)?;
                Some((FOLDERS[*slot], Some(row.label.as_str()), None))
            }
            Line::Field(slot, i, j) => {
                let row = self.shown[*slot].get(*i)?;
                let field = self.field_line(*slot, *i, *j)?;
                Some((
                    FOLDERS[*slot],
                    Some(row.label.as_str()),
                    Some((field.path.as_str(), field.kind)),
                ))
            }
            Line::Loading(slot, i) => {
                let row = self.shown[*slot].get(*i)?;
                Some((FOLDERS[*slot], Some(row.label.as_str()), None))
            }
        }
    }

    /// Whether this activation of the line follows another one on it within the double-click time.
    pub fn double_click(&mut self, ix: IndexPath) -> bool {
        let now = std::time::Instant::now();
        let double = self.last_click.is_some_and(|(row, at)| {
            row == ix.row && now.duration_since(at) < std::time::Duration::from_millis(400)
        });
        self.last_click = (!double).then_some((ix.row, now));
        double
    }

    /// Enter or click: a folder line opens or closes, a row returns its folder and text.
    pub fn activate(&mut self, ix: IndexPath) -> Option<(Folder, String)> {
        match *self.lines.get(ix.row)? {
            Line::Folder(slot) => {
                self.collapsed[slot] = !self.collapsed[slot];
                self.relayout();
                None
            }
            Line::Item(slot, i) => Some((FOLDERS[slot], self.shown[slot].get(i)?.text.clone())),
            Line::Loading(_, _) | Line::Field(_, _, _) => None,
        }
    }

    /// A filter opens folders so matches are not hidden behind a collapsed one.
    fn is_open(&self, slot: usize) -> bool {
        !self.collapsed[slot] || !self.query.trim().is_empty()
    }

    fn refilter(&mut self) {
        self.shown = self.all.clone().map(|rows| matching(rows, &self.query));
        self.relayout();
    }

    fn relayout(&mut self) {
        self.lines.clear();
        // Re-flatten the open part of every expanded mapping; rows without a tree keep loading.
        let keys: Vec<(usize, String)> = self.expanded.iter().cloned().collect();
        for key in keys {
            match self.trees.get(&key) {
                Some(nodes) => {
                    let mut fields = Vec::new();
                    flatten_fields(nodes, &key, &self.expanded_fields, 0, &mut fields);
                    self.fields.insert(key, fields);
                }
                None => {
                    self.fields.remove(&key);
                }
            }
        }
        for (slot, folder) in FOLDERS.iter().enumerate() {
            let rows = self.shown[slot].len();
            // Other empty folders are noise; the tree only lists what exists. A filter hides all of them.
            if rows == 0 && !(folder.always_shown() && self.query.trim().is_empty()) {
                continue;
            }
            self.lines.push(Line::Folder(slot));
            if !self.is_open(slot) {
                continue;
            }
            for row in 0..rows {
                self.lines.push(Line::Item(slot, row));
                if !Self::expandable(slot) {
                    continue;
                }
                let Some(label) = self.shown[slot].get(row).map(|r| r.label.clone()) else {
                    continue;
                };
                if !self.expanded.contains(&(slot, label.clone())) {
                    continue;
                }
                match self.fields.get(&(slot, label)).map(|fields| fields.len()) {
                    Some(count) => self
                        .lines
                        .extend((0..count).map(|f| Line::Field(slot, row, f))),
                    None => self.lines.push(Line::Loading(slot, row)),
                }
            }
        }
        self.selected = None;
    }
}

/// Case-insensitive substring match on the label; an empty query keeps everything.
fn matching(rows: Vec<Row>, query: &str) -> Vec<Row> {
    let q = query.trim().to_lowercase();
    rows.into_iter()
        .filter(|r| r.label.to_lowercase().contains(&q))
        .collect()
}

impl ListDelegate for Targets {
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

    fn sections_count(&self, _: &App) -> usize {
        1
    }

    fn items_count(&self, _: usize, _: &App) -> usize {
        self.lines.len()
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
        let line = *self.lines.get(ix.row)?;
        // The kit paints hover and selection on the item, from the list tokens the appearance
        // setup points at the island surfaces. Painting a second fill here would double the tint.
        let row = move || {
            div()
                .w_full()
                .h(rems(1.75))
                .pl_2()
                // The list's scrollbar overlays the right edge; keep the row's text clear of it.
                .pr_4()
                .rounded(radius)
                .flex()
                .items_center()
                .gap_1p5()
                .min_w_0()
        };
        let item = |id: (&'static str, usize)| ListItem::new(id).p_0().rounded(radius).text_sm();
        Some(match line {
            Line::Folder(slot) => {
                let open = self.is_open(slot);
                let empty = self.shown[slot].is_empty();
                let icon = if open && !empty {
                    IconName::FolderOpen
                } else {
                    IconName::Folder
                };
                // An empty folder keeps the chevron's width so titles line up.
                let chevron = div().size_4().flex_none().children((!empty).then(|| {
                    Icon::new(if open {
                        IconName::ChevronDown
                    } else {
                        IconName::ChevronRight
                    })
                    .small()
                    .text_color(muted)
                }));
                item(("tree-folder", slot)).child(
                    row()
                        .child(chevron)
                        .child(Icon::new(icon).size_4().text_color(muted))
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .truncate()
                                .text_sm()
                                .font_weight(FontWeight::MEDIUM)
                                .child(FOLDERS[slot].title()),
                        )
                        .children((!empty).then(|| {
                            div()
                                .text_xs()
                                .text_color(muted)
                                .child(self.shown[slot].len().to_string())
                        })),
                )
            }
            Line::Item(slot, i) => {
                let (label, closed) = {
                    let r = self.shown[slot].get(i)?;
                    (r.label.clone(), r.closed)
                };
                let id = row_id(&format!("{slot}/{label}"));
                // A closed index has no readable mapping, so it does not offer to expand.
                let expandable = Self::expandable(slot) && !closed;
                let open = self.expanded.contains(&(slot, label.clone()));
                let toggle = self.menu.clone();
                let chevron = div()
                    .size_4()
                    .flex_none()
                    .flex()
                    .items_center()
                    .justify_center()
                    .children(expandable.then(|| {
                        let (menu, name, open) = (toggle, label.clone(), open);
                        div()
                            .id(("tree-expand", id as usize))
                            .test_support()
                            .role(Role::Button)
                            .aria_label(format!(
                                "{} fields of {name}",
                                if open { "Hide" } else { "Show" }
                            ))
                            .size_full()
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded(radius)
                            .cursor_pointer()
                            .hover(|hover| hover.bg(muted.opacity(0.15)))
                            .on_click(move |_, window, cx| {
                                if let Some(menu) = &menu {
                                    menu(ConsoleOp::ToggleFields, name.clone(), window, cx);
                                }
                            })
                            .child(
                                Icon::new(if open {
                                    IconName::ChevronDown
                                } else {
                                    IconName::ChevronRight
                                })
                                .xsmall()
                                .text_color(muted),
                            )
                    }));
                let inner = row()
                    // The chevron column keeps every row's icon in one line whether or not the
                    // row can expand.
                    .pl_6()
                    .child(chevron)
                    .child(
                        Icon::new(if closed {
                            IconName::EyeOff
                        } else {
                            FOLDERS[slot].icon()
                        })
                        .size_4()
                        .text_color(muted),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .when(closed, |d| d.text_color(muted))
                            .child(SharedString::from(label.clone())),
                    );
                let entry = item(("tree-item", id as usize));
                match (&self.menu, FOLDERS[slot]) {
                    (Some(menu), Folder::Indices | Folder::Aliases) => {
                        let menu = menu.clone();
                        entry.child(inner.id(("tree-target", id as usize)).context_menu(
                            move |popup, _, _| {
                                let (browser, console) = (menu.clone(), menu.clone());
                                let (browser_name, console_name) = (label.clone(), label.clone());
                                popup
                                    .item(
                                        PopupMenuItem::new("Open in Data Browser")
                                            .icon(IconName::Table2)
                                            .action(Box::new(OpenInDataBrowser))
                                            .on_click(move |_, window, cx| {
                                                browser(
                                                    ConsoleOp::OpenBrowser,
                                                    browser_name.clone(),
                                                    window,
                                                    cx,
                                                )
                                            }),
                                    )
                                    .item(
                                        PopupMenuItem::new("Open in REST Console")
                                            .icon(IconName::SquareTerminal)
                                            .action(Box::new(OpenInRestConsole))
                                            .on_click(move |_, window, cx| {
                                                console(
                                                    ConsoleOp::OpenConsole,
                                                    console_name.clone(),
                                                    window,
                                                    cx,
                                                )
                                            }),
                                    )
                            },
                        ))
                    }
                    (Some(menu), Folder::Consoles) if label != es_client::saved::AUTO_CONSOLE => {
                        let menu = menu.clone();
                        entry.child(inner.id(("tree-console", id as usize)).context_menu(
                            move |popup, _, _| {
                                let (rename, delete) = (menu.clone(), menu.clone());
                                let (rename_name, delete_name) = (label.clone(), label.clone());
                                popup
                                    .item(PopupMenuItem::new("Rename…").on_click(
                                        move |_, window, cx| {
                                            rename(
                                                ConsoleOp::Rename,
                                                rename_name.clone(),
                                                window,
                                                cx,
                                            )
                                        },
                                    ))
                                    .item(PopupMenuItem::new("Delete").on_click(
                                        move |_, window, cx| {
                                            delete(
                                                ConsoleOp::Delete,
                                                delete_name.clone(),
                                                window,
                                                cx,
                                            )
                                        },
                                    ))
                            },
                        ))
                    }
                    _ => entry.child(inner),
                }
            }
            Line::Loading(slot, i) => {
                let label = self.shown[slot].get(i)?.label.clone();
                item(("tree-loading", row_id(&format!("{slot}/{label}")) as usize)).child(
                    div()
                        .w_full()
                        .h(rems(1.5))
                        .pl_16()
                        .pr_4()
                        .flex()
                        .items_center()
                        .gap_1p5()
                        .text_xs()
                        .text_color(muted)
                        .child("Loading fields…"),
                )
            }
            Line::Field(slot, i, f) => {
                let label = self.shown[slot].get(i)?.label.clone();
                let field = self.field_line(slot, i, f)?.clone();
                let list = cx.entity();
                let mut chevron = div().size_4().flex_none().flex().items_center();
                if field.expandable {
                    let (list, row, path, open) =
                        (list, label.clone(), field.path.clone(), field.open);
                    chevron = chevron.justify_center().child(
                        div()
                            .id(("tree-field-expand", row_id(&path) as usize))
                            .test_support()
                            .role(Role::Button)
                            .aria_label(format!(
                                "{} fields of {path}",
                                if open { "Hide" } else { "Show" }
                            ))
                            .size_full()
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded(radius)
                            .cursor_pointer()
                            .hover(|hover| hover.bg(muted.opacity(0.15)))
                            .on_click(move |_, _, cx| {
                                list.update(cx, |list, cx| {
                                    list.delegate_mut().toggle_field(&row, &path);
                                    cx.notify();
                                });
                            })
                            .child(
                                Icon::new(if open {
                                    IconName::ChevronDown
                                } else {
                                    IconName::ChevronRight
                                })
                                .xsmall()
                                .text_color(muted),
                            ),
                    );
                }
                item((
                    "tree-field",
                    row_id(&format!("{slot}/{label}/{}", field.path)) as usize,
                ))
                .child(
                    div()
                        .relative()
                        .w_full()
                        .h(rems(1.5))
                        // Each level of an object steps right by the chevron's width.
                        .pl(rems(4.0 + field.depth as f32))
                        .pr_4()
                        .flex()
                        .items_center()
                        .gap_1p5()
                        .min_w_0()
                        // A hairline per enclosing object, under that object's chevron, so rows at
                        // the same level line up across a long mapping. An open container also
                        // draws its own level, otherwise the line would start one row too late
                        // and break at the parent. The list item pads 4px above and below this
                        // row, so each segment reaches past the padding to meet its neighbours
                        // instead of leaving a gap at every row boundary.
                        .children((0..field.depth + usize::from(field.open)).map(|level| {
                            div()
                                .absolute()
                                .top(rems(-0.25))
                                .bottom(rems(-0.25))
                                .left(rems(4.5 + level as f32))
                                .w(px(1.))
                                .bg(muted.opacity(0.2))
                        }))
                        .child(chevron)
                        .child(Icon::new(field_icon(field.kind)).xsmall().text_color(muted))
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .truncate()
                                .font_family(mono.clone())
                                .text_xs()
                                .child(SharedString::from(field.name)),
                        )
                        .child(
                            div()
                                .flex_none()
                                .font_family(mono)
                                .text_xs()
                                .text_color(muted)
                                .child(SharedString::from(field.ty)),
                        ),
                )
            }
        })
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

#[cfg(test)]
mod tests {
    use super::*;

    fn row(label: &str) -> Row {
        Row {
            label: label.into(),
            text: format!("GET {label}"),
            closed: false,
        }
    }

    fn at(row: usize) -> IndexPath {
        IndexPath::new(row)
    }

    #[test]
    fn filters_by_case_insensitive_substring() {
        let rows = vec![row("Logs-1"), row("orders"), row(".kibana")];
        assert_eq!(matching(rows.clone(), "log"), [row("Logs-1")]);
        assert_eq!(matching(rows.clone(), "  "), rows);
        assert!(matching(rows, "zzz").is_empty());
    }

    #[test]
    fn only_folders_with_rows_show_and_only_the_indices_start_open() {
        let mut t = Targets::default();
        // Only the user's own bucket is listed while empty.
        assert_eq!(t.lines, [Line::Folder(0)]);
        assert!(t.set(Folder::Indices, vec![row("a"), row("b")]));
        assert!(!t.set(Folder::Indices, vec![row("a"), row("b")]));
        assert!(t.set(Folder::Nodes, vec![row("n1")]));
        // Consoles, indices open with two rows, nodes closed.
        assert_eq!(t.lines.len(), 5);
        assert_eq!(t.activate(at(4)), None, "a folder line toggles");
        assert_eq!(t.lines.len(), 6, "nodes opened");
        assert_eq!(t.activate(at(4)), None);
        assert_eq!(t.lines.len(), 5, "nodes closed again");
        assert_eq!(t.activate(at(2)), Some((Folder::Indices, "GET a".into())));
    }

    #[test]
    fn a_second_activation_of_the_same_line_is_a_double_click() {
        let mut t = Targets::default();
        assert!(!t.double_click(at(1)));
        assert!(t.double_click(at(1)));
        assert!(!t.double_click(at(1)), "a third click starts over");
        assert!(!t.double_click(at(2)));
        assert!(!t.double_click(at(1)), "another line resets it");
    }

    #[test]
    fn a_filter_opens_folders_and_hides_the_ones_without_matches() {
        let mut t = Targets::default();
        t.set(Folder::Nodes, vec![row("node-1"), row("other")]);
        t.query = "node".into();
        t.refilter();
        assert_eq!(t.lines, [Line::Folder(3), Line::Item(3, 0)]);
        t.query.clear();
        t.refilter();
        assert_eq!(t.lines, [Line::Folder(0), Line::Folder(3)]);
    }

    #[test]
    fn the_selected_line_reads_as_a_folder_then_a_row() {
        let mut t = Targets::default();
        t.set(Folder::Nodes, vec![row("n1")]);
        // Selecting the folder line names the folder alone; its row adds the name, and a field
        // line adds the field below both.
        t.selected = Some(at(1));
        assert_eq!(t.selected_path(), Some((Folder::Nodes, None, None)));
        t.activate(at(1));
        t.selected = Some(at(2));
        assert_eq!(t.selected_path(), Some((Folder::Nodes, Some("n1"), None)));
        t.selected = None;
        assert_eq!(t.selected_path(), None);
    }

    fn field(name: &str, ty: &str) -> FieldRow {
        FieldRow::new(name, ty)
    }

    #[test]
    fn fields_take_an_icon_from_their_type() {
        use es_dsl_core::FieldKind;
        assert_eq!(field_icon(FieldKind::of("text")), IconName::Type);
        assert_eq!(field_icon(FieldKind::of("date_nanos")), IconName::Calendar);
        assert_eq!(field_icon(FieldKind::of("long")), IconName::Hash);
        assert_eq!(field_icon(FieldKind::of("geo_point")), IconName::MapPin);
        // A type this build does not know still gets a row.
        assert_eq!(
            field_icon(FieldKind::of("unheard_of")),
            IconName::CircleSmall
        );
    }

    #[test]
    fn an_expanded_index_lists_its_fields_after_a_loading_line() {
        let mut t = Targets::default();
        t.set(Folder::Indices, vec![row("logs")]);
        // Consoles (always shown), Indices with its row.
        assert_eq!(
            t.lines,
            [Line::Folder(0), Line::Folder(1), Line::Item(1, 0)]
        );
        assert!(
            t.toggle_fields("logs", None),
            "a miss expands with a placeholder"
        );
        assert_eq!(
            t.lines,
            [
                Line::Folder(0),
                Line::Folder(1),
                Line::Item(1, 0),
                Line::Loading(1, 0)
            ]
        );
        assert_eq!(t.expanded_targets(), ["logs"]);
        assert!(t.set_fields("logs", Some(vec![field("msg", "text")])));
        assert_eq!(t.lines.last(), Some(&Line::Field(1, 0, 0)));
        assert!(!t.set_fields("logs", Some(vec![field("msg", "text")])));
        assert!(!t.toggle_fields("logs", None), "a second click collapses");
        assert!(!t.lines.contains(&Line::Field(1, 0, 0)));
        // Reopening shows the mapping that is already cached instead of loading again.
        assert!(t.toggle_fields("logs", Some(vec![field("msg", "text")])));
        assert_eq!(t.lines.last(), Some(&Line::Field(1, 0, 0)));
    }

    /// The rows shown under an expanded index, as (label, depth, open).
    fn shown_fields(t: &Targets, label: &str) -> Vec<(String, usize, bool)> {
        let key = (Folder::Indices.slot(), label.to_string());
        t.fields
            .get(&key)
            .map(|fields| {
                fields
                    .iter()
                    .map(|field| (field.name.clone(), field.depth, field.open))
                    .collect()
            })
            .unwrap_or_default()
    }

    #[test]
    fn objects_nest_and_open_level_by_level() {
        let mut t = Targets::default();
        t.set(Folder::Indices, vec![row("logs")]);
        t.toggle_fields(
            "logs",
            Some(vec![
                field("access", "object"),
                field("access.token", "object"),
                field("access.token.realm", "keyword"),
                field("name", "text"),
                // A multi-field address is not a child of the field it names.
                field("name.keyword", "keyword"),
            ]),
        );
        // Objects start closed, so an index shows its top-level fields only.
        assert_eq!(
            shown_fields(&t, "logs"),
            [
                ("access".into(), 0, false),
                ("name".into(), 0, false),
                ("name.keyword".into(), 0, false),
            ]
        );
        assert!(t.toggle_field("logs", "access"));
        assert_eq!(
            shown_fields(&t, "logs"),
            [
                ("access".into(), 0, true),
                ("token".into(), 1, false),
                ("name".into(), 0, false),
                ("name.keyword".into(), 0, false),
            ]
        );
        assert!(t.toggle_field("logs", "access.token"));
        assert_eq!(
            shown_fields(&t, "logs")[..3],
            [
                ("access".into(), 0, true),
                ("token".into(), 1, true),
                ("realm".into(), 2, false),
            ]
        );
        assert!(
            !t.toggle_field("logs", "name"),
            "a leaf has nothing to open"
        );
        assert!(!t.toggle_field("logs", "other"), "a row that is not there");
        // Closing an outer object hides its children but remembers which ones were open.
        assert!(!t.toggle_field("logs", "access"));
        assert_eq!(shown_fields(&t, "logs").len(), 3);
        assert!(t.toggle_field("logs", "access"));
        assert_eq!(shown_fields(&t, "logs")[2], ("realm".into(), 2, false));
    }

    #[test]
    fn only_index_and_alias_rows_expand() {
        let mut t = Targets::default();
        t.set(Folder::Nodes, vec![row("n1")]);
        assert!(!t.toggle_fields("n1", None));
        t.set(Folder::Indices, vec![row("logs")]);
        t.selected = Some(at(2));
        assert_eq!(
            t.selected_path(),
            Some((Folder::Indices, Some("logs"), None))
        );
        // A field line keeps its index and names the field under it, rather than replacing it.
        t.toggle_fields("logs", Some(vec![field("msg", "text")]));
        t.selected = Some(at(3));
        assert_eq!(
            t.selected_path(),
            Some((
                Folder::Indices,
                Some("logs"),
                Some(("msg", es_dsl_core::FieldKind::of("text")))
            ))
        );
        assert_eq!(t.selected(), None, "a field is not a row that opens");
        assert_eq!(t.activate(at(3)), None, "activating a field does nothing");
    }

    #[test]
    fn a_refreshed_folder_forgets_rows_that_are_gone() {
        let mut t = Targets::default();
        t.set(Folder::Indices, vec![row("logs")]);
        t.toggle_fields("logs", Some(vec![field("msg", "text")]));
        assert_eq!(t.expanded_targets(), ["logs"]);
        t.set(Folder::Indices, vec![row("other")]);
        assert!(t.expanded_targets().is_empty());
        assert!(!t.set_fields("logs", Some(vec![field("msg", "text")])));
    }
}
