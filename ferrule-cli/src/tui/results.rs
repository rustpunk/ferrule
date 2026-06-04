//! The results pane model.
//!
//! Holds the rendered output of the last query — produced by the
//! *existing* formatter ([`ferrule_core::formatter::format_result`]) so
//! the TUI reuses the box-drawn table renderer rather than growing a
//! second one — split into lines, plus a vertical scroll offset. Scroll
//! operations clamp against the line count; the actual drawing lives in
//! [`crate::tui::ui`].

use ferrule_core::formatter::format_result;
use ferrule_core::OutputFormat;
use ferrule_sql::{QueryResult, SqlError};

/// The scrollable, pre-formatted view of a query result.
#[derive(Debug, Default, Clone)]
pub struct ResultsModel {
    /// The formatted output, one entry per visual line.
    lines: Vec<String>,
    /// Index of the first visible line (vertical scroll offset).
    scroll: usize,
    /// Row count of the underlying result, for the status bar.
    row_count: usize,
}

impl ResultsModel {
    /// An empty results model (no query run yet).
    #[must_use]
    pub fn empty() -> Self {
        Self::default()
    }

    /// Build the model by formatting `result` with `format` (the TUI
    /// uses [`OutputFormat::Table`]). The formatted string is split into
    /// lines for line-wise scrolling; the scroll offset resets to the
    /// top.
    pub fn from_query_result(result: &QueryResult, format: OutputFormat) -> Result<Self, SqlError> {
        let formatted = format_result(result, format)?;
        let lines: Vec<String> = formatted.lines().map(str::to_string).collect();
        Ok(Self {
            lines,
            scroll: 0,
            row_count: result.rows.len(),
        })
    }

    /// The formatted lines.
    #[must_use]
    pub fn lines(&self) -> &[String] {
        &self.lines
    }

    /// The current vertical scroll offset.
    #[must_use]
    pub fn scroll(&self) -> usize {
        self.scroll
    }

    /// Number of formatted lines.
    #[must_use]
    pub fn line_count(&self) -> usize {
        self.lines.len()
    }

    /// Row count of the underlying result set.
    #[must_use]
    pub fn row_count(&self) -> usize {
        self.row_count
    }

    /// Largest valid scroll offset: keeps at least one line on screen.
    fn max_scroll(&self) -> usize {
        self.lines.len().saturating_sub(1)
    }

    /// Scroll up one line, clamped at the top.
    pub fn scroll_up(&mut self) {
        self.scroll = self.scroll.saturating_sub(1);
    }

    /// Scroll down one line, clamped so the last line stays reachable.
    pub fn scroll_down(&mut self) {
        if self.scroll < self.max_scroll() {
            self.scroll += 1;
        }
    }

    /// Scroll up by `page` lines, clamped at the top.
    pub fn page_up(&mut self, page: usize) {
        self.scroll = self.scroll.saturating_sub(page);
    }

    /// Scroll down by `page` lines, clamped at the bottom.
    pub fn page_down(&mut self, page: usize) {
        self.scroll = (self.scroll + page).min(self.max_scroll());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ferrule_sql::value::{ColumnInfo, TypeHint, Value};

    fn sample_result() -> QueryResult {
        QueryResult {
            columns: vec![
                ColumnInfo {
                    name: "id".into(),
                    type_hint: TypeHint::Int64,
                    nullable: false,
                },
                ColumnInfo {
                    name: "name".into(),
                    type_hint: TypeHint::String,
                    nullable: true,
                },
            ],
            rows: vec![
                vec![Value::Int64(1), Value::String("Alice".into())],
                vec![Value::Int64(2), Value::String("Bob".into())],
            ],
        }
    }

    #[test]
    fn from_query_result_produces_header_and_row_lines() {
        let model = ResultsModel::from_query_result(&sample_result(), OutputFormat::Table).unwrap();
        // A box-drawn table has more lines than data rows (borders +
        // header). At minimum: header + 2 data rows are represented.
        assert!(model.line_count() >= 3, "got {} lines", model.line_count());
        assert_eq!(model.row_count(), 2);
        let joined = model.lines().join("\n");
        assert!(joined.contains("Alice"));
        assert!(joined.contains("Bob"));
        assert!(joined.contains("id"));
        assert!(joined.contains("name"));
    }

    #[test]
    fn scroll_down_past_last_line_clamps() {
        let mut model =
            ResultsModel::from_query_result(&sample_result(), OutputFormat::Table).unwrap();
        for _ in 0..1000 {
            model.scroll_down();
        }
        assert_eq!(model.scroll(), model.line_count() - 1);
    }

    #[test]
    fn scroll_up_at_offset_zero_stays_at_zero() {
        let mut model =
            ResultsModel::from_query_result(&sample_result(), OutputFormat::Table).unwrap();
        assert_eq!(model.scroll(), 0);
        model.scroll_up();
        assert_eq!(model.scroll(), 0);
    }

    #[test]
    fn page_down_then_page_up_returns_to_top() {
        let mut model =
            ResultsModel::from_query_result(&sample_result(), OutputFormat::Table).unwrap();
        model.page_down(10);
        assert!(model.scroll() <= model.line_count().saturating_sub(1));
        model.page_up(10);
        assert_eq!(model.scroll(), 0);
    }

    #[test]
    fn empty_model_scrolls_without_panicking() {
        let mut model = ResultsModel::empty();
        assert_eq!(model.line_count(), 0);
        model.scroll_down();
        model.scroll_up();
        model.page_down(5);
        model.page_up(5);
        assert_eq!(model.scroll(), 0);
    }
}
