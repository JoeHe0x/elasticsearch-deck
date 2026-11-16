//! The connection's tree: saved items and cluster objects as filterable, collapsible folders.

use gpui_kit::assets::IconName;
use gpui_kit::component::list::{ListDelegate, ListItem, ListState};
use gpui_kit::component::{ActiveTheme as _, Icon, IndexPath, Sizable as _};
use gpui_kit::{App, Context, ParentElement as _, SharedString, Styled as _, Task, Window, div};

use crate::connect::row_id;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Folder {
    Consoles,
    Requests,
    Queries,
    Indices,
    Aliases,
    Nodes,
    Templates,
    Tasks,
    Pipelines,
}

/// Display order.
const FOLDERS: [Folder; 9] = [
    Folder::Consoles,
    Folder::Requests,
    Folder::Queries,
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

    fn title(self) -> &'static str {
        match self {
            Self::Consoles => "Consoles",
            Self::Requests => "Requests",
            Self::Queries => "Queries",
            Self::Indices => "Indices",
            Self::Aliases => "Aliases",
            Self::Nodes => "Nodes",
            Self::Templates => "Templates",
            Self::Tasks => "Tasks",
            Self::Pipelines => "Ingest pipelines",
        }
    }

    /// The icon of a row in this folder.
    fn icon(self) -> IconName {
        match self {
            Self::Consoles => IconName::SquareTerminal,
            Self::Requests => IconName::Braces,
            Self::Indices => IconName::Table2,
            Self::Aliases => IconName::Link2,
            Self::Nodes => IconName::Server,
            Self::Tasks => IconName::Clock,
            Self::Pipelines => IconName::ListTree,
            Self::Queries | Self::Templates => IconName::FileText,
        }
    }

    /// The user's own buckets stay listed while empty, so it is visible where saved items go.
    fn always_shown(self) -> bool {
        matches!(self, Self::Consoles | Self::Requests | Self::Queries)
    }
}

/// One entry in a folder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    pub label: String,
    /// What a click puts in the editor.
    pub text: String,
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
    all: [Vec<Row>; 9],
    query: String,
    shown: [Vec<Row>; 9],
    lines: Vec<Line>,
    selected: Option<IndexPath>,
    /// Folders the user collapsed, by slot; everything but the indices starts closed.
    collapsed: [bool; 9],
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
        let muted = cx.theme().muted_foreground;
        let line = *self.lines.get(ix.row)?;
        let row = div().flex().items_center().gap_1p5().min_w_0();
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
                ListItem::new(("tree-folder", slot)).child(
                    row.child(chevron)
                        .child(Icon::new(icon).small().text_color(muted))
                        .child(
                            div()
                                .flex_1()
                                .truncate()
                                .text_sm()
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
                let label = self.shown[slot].get(i)?.label.clone();
                let id = row_id(&format!("{slot}/{label}"));
                ListItem::new(("tree-item", id))
                    .selected(self.selected == Some(ix))
                    .child(
                        // Indented under the folder's icon, past its chevron.
                        row.pl_9()
                            .child(Icon::new(FOLDERS[slot].icon()).small().text_color(muted))
                            .child(div().truncate().child(SharedString::from(label))),
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
        // Only the user's own buckets are listed while empty.
        assert_eq!(t.lines, [Line::Folder(0), Line::Folder(1), Line::Folder(2)]);
        assert!(t.set(Folder::Indices, vec![row("a"), row("b")]));
        assert!(!t.set(Folder::Indices, vec![row("a"), row("b")]));
        assert!(t.set(Folder::Nodes, vec![row("n1")]));
        // Three buckets, indices open with two rows, nodes closed.
        assert_eq!(t.lines.len(), 7);
        assert_eq!(t.activate(at(6)), None, "a folder line toggles");
        assert_eq!(t.lines.len(), 8, "nodes opened");
        assert_eq!(t.activate(at(6)), None);
        assert_eq!(t.lines.len(), 7, "nodes closed again");
        assert_eq!(t.activate(at(4)), Some((Folder::Indices, "GET a".into())));
    }

    #[test]
    fn a_filter_opens_folders_and_hides_the_ones_without_matches() {
        let mut t = Targets::default();
        t.set(Folder::Nodes, vec![row("node-1"), row("other")]);
        t.query = "node".into();
        t.refilter();
        assert_eq!(t.lines, [Line::Folder(5), Line::Item(5, 0)]);
        t.query.clear();
        t.refilter();
        assert_eq!(
            t.lines,
            [
                Line::Folder(0),
                Line::Folder(1),
                Line::Folder(2),
                Line::Folder(5)
            ]
        );
    }
}
