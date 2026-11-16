//! The hits of a search response as a virtualized table.

use es_client::Hits;
use gpui_kit::component::table::{Column, TableDelegate, TableState};
use gpui_kit::{App, Context, IntoElement, ParentElement as _, SharedString, Window, div};

pub struct HitsTable {
    columns: Vec<Column>,
    rows: Vec<Vec<SharedString>>,
}

impl HitsTable {
    pub fn new(hits: Hits) -> Self {
        let columns = hits
            .columns
            .into_iter()
            .map(|name| Column::new(name.clone(), name).resizable(true))
            .collect();
        let rows = hits
            .rows
            .into_iter()
            .map(|r| r.into_iter().map(SharedString::from).collect())
            .collect();
        Self { columns, rows }
    }
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

    fn render_td(
        &mut self,
        row_ix: usize,
        col_ix: usize,
        _: &mut Window,
        _: &mut Context<TableState<Self>>,
    ) -> impl IntoElement {
        div().child(self.rows[row_ix][col_ix].clone())
    }

    fn cell_text(&self, row_ix: usize, col_ix: usize, _: &App) -> String {
        self.rows[row_ix][col_ix].to_string()
    }
}
