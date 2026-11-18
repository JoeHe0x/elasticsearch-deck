//! The connection's tree: saved items and cluster objects as filterable, collapsible folders.

use gpui_kit::assets::IconName;
use gpui_kit::component::list::{ListDelegate, ListItem, ListState};
use gpui_kit::component::menu::{ContextMenuExt as _, PopupMenuItem};
use gpui_kit::component::{ActiveTheme as _, Icon, IndexPath, Sizable as _};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    App, Context, FontWeight, InteractiveElement as _, ParentElement as _, SharedString,
    Styled as _, Task, Window, div, rems,
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

/// What the right-click menu of a row asks for.
#[derive(Debug, Clone, Copy)]
pub enum ConsoleOp {
    Rename,
    Delete,
    /// Index and alias rows only.
    OpenBrowser,
    OpenConsole,
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
        self.refilter();
        true
    }

    /// Gives the console rows a right-click menu.
    pub fn set_menu(&mut self, menu: ConsoleMenu) {
        self.menu = Some(menu);
    }

    /// Drops every row, e.g. on disconnect.
    pub fn clear(&mut self) {
        self.all = Default::default();
        self.refilter();
    }

    /// The selected row of a folder; a selected folder line gives `None`.
    pub fn selected(&self) -> Option<(Folder, &Row)> {
        match self.lines.get(self.selected?.row)? {
            Line::Item(slot, i) => Some((FOLDERS[*slot], self.shown[*slot].get(*i)?)),
            Line::Folder(_) => None,
        }
    }

    /// The selected line as the title bar names it: its folder and, for a row, the row's label.
    /// A folder line has no label, so selecting `Nodes` reads as the folder alone.
    pub fn selected_path(&self) -> Option<(Folder, Option<&str>)> {
        match self.lines.get(self.selected?.row)? {
            Line::Folder(slot) => Some((FOLDERS[*slot], None)),
            Line::Item(slot, i) => {
                let row = self.shown[*slot].get(*i)?;
                Some((FOLDERS[*slot], Some(row.label.as_str())))
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
        for (slot, folder) in FOLDERS.iter().enumerate() {
            let rows = self.shown[slot].len();
            // Other empty folders are noise; the tree only lists what exists. A filter hides all of them.
            if rows == 0 && !(folder.always_shown() && self.query.trim().is_empty()) {
                continue;
            }
            self.lines.push(Line::Folder(slot));
            if self.is_open(slot) {
                self.lines.extend((0..rows).map(|i| Line::Item(slot, i)));
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
        let line = *self.lines.get(ix.row)?;
        // The kit paints hover and selection on the item, from the list tokens the appearance
        // setup points at the island surfaces. Painting a second fill here would double the tint.
        let row = move || {
            div()
                .w_full()
                .h(rems(1.75))
                .px_2()
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
                let inner = row()
                    // One full level past the folder icon, so a child never shares its parent's
                    // icon column. The group already names the type, so the icon stays neutral.
                    .pl_12()
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
        // Selecting the folder line names the folder alone; its row adds the name.
        t.selected = Some(at(1));
        assert_eq!(t.selected_path(), Some((Folder::Nodes, None)));
        t.activate(at(1));
        t.selected = Some(at(2));
        assert_eq!(t.selected_path(), Some((Folder::Nodes, Some("n1"))));
        t.selected = None;
        assert_eq!(t.selected_path(), None);
    }
}
