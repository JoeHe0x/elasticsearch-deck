//! The hits of a search response as a virtualized table.

use std::rc::Rc;

use es_client::{CellKind, Hits};
use gpui_kit::component::ActiveTheme as _;
use gpui_kit::component::Sizable as _;
use gpui_kit::component::menu::{PopupMenu, PopupMenuItem};
use gpui_kit::component::table::{Column, ColumnSort, DataTable, TableDelegate, TableState};
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    App, ClipboardItem, Context, Entity, FontWeight, InteractiveElement as _, IntoElement,
    ParentElement as _, SharedString, StatefulInteractiveElement as _, Styled as _, Window, div,
    px,
};

/// The table as the Console result and the Data Browser both show it.
pub fn view(table: &Entity<TableState<HitsTable>>) -> impl IntoElement {
    div().size_full().text_sm().child(
        DataTable::new(table)
            .small()
            .header_row_height(px(28.))
            .stripe(true),
    )
}

/// Called with a column's name and the sort a header click asked for.
pub type OnSort = Rc<dyn Fn(&str, ColumnSort, &mut Window, &mut App)>;

pub struct HitsTable {
    names: Vec<String>,
    columns: Vec<Column>,
    rows: Vec<Vec<SharedString>>,
    kinds: Vec<Vec<CellKind>>,
    docs: Vec<String>,
    /// The mapping type of each column, where the mapping knows it.
    types: Vec<Option<SharedString>>,
    on_sort: Option<OnSort>,
}

impl HitsTable {
    pub fn new(hits: Hits) -> Self {
        let names = hits.columns;
        let columns = names
            .iter()
            .enumerate()
            .map(|(ix, name)| {
                let column = Column::new(name.clone(), name.clone())
                    .resizable(true)
                    .movable(true)
                    .width(width_for(name, ix, &hits.rows));
                if is_numeric(ix, &hits.kinds) {
                    column.text_right()
                } else {
                    column
                }
            })
            .collect();
        let kinds = hits.kinds;
        let docs = hits.docs;
        let rows = hits
            .rows
            .into_iter()
            .map(|r| r.into_iter().map(SharedString::from).collect())
            .collect();
        Self {
            columns,
            rows,
            kinds,
            types: vec![None; names.len()],
            names,
            docs,
            on_sort: None,
        }
    }

    /// Names each column's mapping type in its header tooltip.
    pub fn types<'a>(mut self, fields: impl IntoIterator<Item = &'a es_dsl_core::Field>) -> Self {
        for f in fields {
            if let Some(ix) = self.names.iter().position(|n| *n == f.name) {
                self.types[ix] = Some(f.ty.clone().into());
            }
        }
        self
    }

    /// The whole hit of a row as pretty JSON.
    pub fn doc(&self, row_ix: usize) -> Option<&str> {
        self.docs.get(row_ix).map(String::as_str)
    }

    /// Makes the columns `sortable` accepts clickable and marks `current` (a column and its
    /// direction). Sorting is the caller's: the table only reports the click through `on_sort`.
    pub fn sorting(
        mut self,
        sortable: impl Fn(&str) -> bool,
        current: Option<(&str, ColumnSort)>,
        on_sort: OnSort,
    ) -> Self {
        for (name, column) in self.names.iter().zip(&mut self.columns) {
            if !sortable(name) {
                continue;
            }
            *column = match current {
                Some((c, sort)) if c == name => column.clone().sort(sort),
                _ => column.clone().sortable(),
            };
        }
        self.on_sort = Some(on_sort);
        self
    }
}

/// Characters of the longest value among the first rows, so the cost stays flat on a big page.
const WIDTH_SAMPLE: usize = 50;
/// Rough width of one character at the table's font size, plus the cell padding and the sort icon.
const CHAR_PX: f32 = 8.0;
const PADDING_PX: f32 = 36.0;

fn width_for(name: &str, ix: usize, rows: &[Vec<String>]) -> gpui_kit::Pixels {
    let longest = rows
        .iter()
        .take(WIDTH_SAMPLE)
        .map(|r| r[ix].chars().count())
        .max()
        .unwrap_or(0)
        .max(name.chars().count());
    px((longest as f32 * CHAR_PX + PADDING_PX).clamp(80.0, 360.0))
}

/// Right-aligned when every cell is a number (or empty) and at least one is a number.
fn is_numeric(ix: usize, kinds: &[Vec<CellKind>]) -> bool {
    let mut any = false;
    for row in kinds {
        match row[ix] {
            CellKind::Number => any = true,
            CellKind::Null | CellKind::Missing => {}
            _ => return false,
        }
    }
    any
}

impl TableDelegate for HitsTable {
    fn columns_count(&self, _: &App) -> usize {
        self.columns.len()
    }

    fn rows_count(&self, _: &App) -> usize {
        self.rows.len()
    }

    fn column(&self, col_ix: usize, _: &App) -> Column {
        self.columns[col_ix].clone()
    }

    fn perform_sort(
        &mut self,
        col_ix: usize,
        sort: ColumnSort,
        window: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) {
        if let (Some(on_sort), Some(name)) = (&self.on_sort, self.names.get(col_ix)) {
            on_sort(name, sort, window, cx);
        }
    }

    fn context_menu(
        &mut self,
        row_ix: usize,
        menu: PopupMenu,
        _: &mut Window,
        _: &mut Context<TableState<Self>>,
    ) -> PopupMenu {
        let Some(doc) = self.docs.get(row_ix).cloned() else {
            return menu;
        };
        menu.item(
            PopupMenuItem::new("Copy row as JSON").on_click(move |_, _, cx| {
                cx.write_to_clipboard(ClipboardItem::new_string(doc.clone()))
            }),
        )
    }

    fn render_th(
        &mut self,
        col_ix: usize,
        _: &mut Window,
        _: &mut Context<TableState<Self>>,
    ) -> impl IntoElement {
        let name = SharedString::from(self.names[col_ix].clone());
        let tip = self.types[col_ix]
            .as_ref()
            .map(|ty| SharedString::from(format!("{name} · {ty}")));
        div()
            .id(("th", col_ix))
            .w_full()
            .overflow_hidden()
            .text_ellipsis()
            .whitespace_nowrap()
            .font_weight(FontWeight::SEMIBOLD)
            .child(name)
            .when_some(tip, |d, tip| {
                d.tooltip(move |window, cx| Tooltip::new(tip.clone()).build(window, cx))
            })
    }

    fn render_td(
        &mut self,
        row_ix: usize,
        col_ix: usize,
        _: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> impl IntoElement {
        let text = self.rows[row_ix][col_ix].clone();
        let kind = self.kinds[row_ix][col_ix];
        let theme = cx.theme();
        // Values that line up by character (ids, numbers, structures) read better in mono.
        let mono = matches!(
            kind,
            CellKind::Number | CellKind::Bool | CellKind::Array(_) | CellKind::Object
        ) || matches!(self.names[col_ix].as_str(), "_id" | "_index");
        let cell = div()
            .id(("cell", row_ix * self.columns.len() + col_ix))
            .w_full()
            .min_w_0()
            .overflow_hidden()
            .text_ellipsis()
            .whitespace_nowrap()
            .when(mono, |c| c.font_family(theme.mono_font_family.clone()));
        let cell = match kind {
            CellKind::Null => cell.text_color(theme.muted_foreground).child("null"),
            CellKind::Missing => cell,
            CellKind::Array(n) => cell.child(
                div()
                    .flex()
                    .gap_1p5()
                    .child(
                        div()
                            .flex_none()
                            .text_color(theme.muted_foreground)
                            .child(SharedString::from(n.to_string())),
                    )
                    .child(
                        div()
                            .min_w_0()
                            .overflow_hidden()
                            .text_ellipsis()
                            .child(text.clone()),
                    ),
            ),
            _ => cell.child(text.clone()),
        };
        // A hover shows the whole value, but only where it can have been cut.
        let cut = text.chars().count() as f32 * CHAR_PX > 120.0;
        cell.when(cut, |c| {
            c.tooltip(move |window, cx| Tooltip::new(text.clone()).build(window, cx))
        })
    }

    fn cell_text(&self, row_ix: usize, col_ix: usize, _: &App) -> String {
        self.rows[row_ix][col_ix].to_string()
    }
}
