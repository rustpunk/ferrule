//! The central TUI application state.
//!
//! [`App`] owns the live connection, the schema-tree model, the query
//! input buffer, the results model, the focused pane, and a status /
//! error line. The state transitions that do not touch the database
//! (focus cycling, status/error edits, quit) are pure and unit-tested
//! here; [`App::run_query`] is the one method that performs I/O and is
//! exercised by the smoke path rather than a unit test (it needs a live
//! connection).

use super::input::InputBuffer;
use super::results::ResultsModel;
use super::schema_tree::{ConnectionSchemaSource, SchemaTree};
use ferrule_core::OutputFormat;
use ferrule_sql::Connection;

/// Which pane currently has keyboard focus.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    /// The schema-tree navigation pane (left).
    SchemaTree,
    /// The query editor pane (top-right).
    Input,
    /// The results pane (bottom-right).
    Results,
}

impl Focus {
    /// The next pane in the cycle: SchemaTree -> Input -> Results -> SchemaTree.
    #[must_use]
    pub fn next(self) -> Self {
        match self {
            Focus::SchemaTree => Focus::Input,
            Focus::Input => Focus::Results,
            Focus::Results => Focus::SchemaTree,
        }
    }
}

/// The status line content: either a neutral message or an error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Status {
    /// A neutral informational message.
    Info(String),
    /// An error from the last query or action.
    Error(String),
}

impl Status {
    /// The message text, regardless of kind.
    #[must_use]
    pub fn text(&self) -> &str {
        match self {
            Status::Info(s) | Status::Error(s) => s,
        }
    }

    /// `true` when this status represents an error.
    #[must_use]
    pub fn is_error(&self) -> bool {
        matches!(self, Status::Error(_))
    }
}

/// The full TUI application state.
pub struct App {
    /// The live database connection. `query`/`list_*` block the event
    /// loop while they run (synchronous execution is a documented
    /// limitation of this increment).
    conn: Box<dyn Connection>,
    /// Redacted connection label shown in the header. Never holds a raw
    /// password — built from `DatabaseUrl::redacted`.
    conn_label: String,
    /// The pane with keyboard focus.
    focus: Focus,
    /// The query editor buffer.
    input: InputBuffer,
    /// The schema-tree navigation model.
    tree: SchemaTree,
    /// The results pane model.
    results: ResultsModel,
    /// The status / error line.
    status: Status,
    /// Output format used to render results (table for the TUI).
    format: OutputFormat,
    /// Cleared to stop the event loop.
    running: bool,
}

impl App {
    /// Create the application state from an established connection and a
    /// built schema tree.
    pub fn new(conn: Box<dyn Connection>, conn_label: String, tree: SchemaTree) -> Self {
        Self {
            conn,
            conn_label,
            focus: Focus::Input,
            input: InputBuffer::new(),
            tree,
            results: ResultsModel::empty(),
            status: Status::Info(
                "Tab to switch panes · Ctrl-Enter to run · Ctrl-Q to quit".to_string(),
            ),
            format: OutputFormat::Table,
            running: true,
        }
    }

    /// The redacted connection label for the header.
    #[must_use]
    pub fn conn_label(&self) -> &str {
        &self.conn_label
    }

    /// The currently-focused pane.
    #[must_use]
    pub fn focus(&self) -> Focus {
        self.focus
    }

    /// The query editor buffer.
    #[must_use]
    pub fn input(&self) -> &InputBuffer {
        &self.input
    }

    /// Mutable access to the query editor buffer (for the event loop's
    /// character-insert / cursor-move handling).
    pub fn input_mut(&mut self) -> &mut InputBuffer {
        &mut self.input
    }

    /// The schema-tree model.
    #[must_use]
    pub fn tree(&self) -> &SchemaTree {
        &self.tree
    }

    /// Mutable access to the schema tree (for selection navigation).
    pub fn tree_mut(&mut self) -> &mut SchemaTree {
        &mut self.tree
    }

    /// The results model.
    #[must_use]
    pub fn results(&self) -> &ResultsModel {
        &self.results
    }

    /// Mutable access to the results model (for scroll handling).
    pub fn results_mut(&mut self) -> &mut ResultsModel {
        &mut self.results
    }

    /// The current status / error line.
    #[must_use]
    pub fn status(&self) -> &Status {
        &self.status
    }

    /// Whether the event loop should keep running.
    #[must_use]
    pub fn running(&self) -> bool {
        self.running
    }

    /// Advance focus to the next pane.
    pub fn focus_next(&mut self) {
        self.focus = self.focus.next();
    }

    /// Set the status line to a neutral message.
    pub fn set_info(&mut self, msg: impl Into<String>) {
        self.status = Status::Info(msg.into());
    }

    /// Set the status line to an error.
    pub fn set_error(&mut self, msg: impl Into<String>) {
        self.status = Status::Error(msg.into());
    }

    /// Reset the status line to the default hint, clearing any error.
    pub fn clear_error(&mut self) {
        self.status = Status::Info("Ready".to_string());
    }

    /// Stop the event loop.
    pub fn quit(&mut self) {
        self.running = false;
    }

    /// Toggle the selected schema-tree node, loading tables on first
    /// expand through the live connection.
    pub fn toggle_tree_node(&mut self) {
        let mut source = ConnectionSchemaSource::new(self.conn.as_mut());
        self.tree.toggle_selected(Some(&mut source));
    }

    /// Handle an "activate" on the selected schema-tree row:
    ///
    /// - On a **schema** row, expand/collapse it (loading tables on
    ///   first expand).
    /// - On a **table** row, fill the query editor with a starter
    ///   `SELECT * FROM <schema>.<table>` and focus the editor, so the
    ///   tree doubles as a query launcher.
    pub fn activate_tree_selection(&mut self) {
        use super::schema_tree::VisibleRow;
        match self.tree.selected_row() {
            Some(VisibleRow::Table { name, .. }) => {
                // A dialect-neutral starter the user can refine. The
                // table is referenced unqualified — qualify by schema or
                // add quoting by hand if the backend needs it.
                self.input.set_text(format!("SELECT * FROM {name}"));
                self.focus = Focus::Input;
                self.set_info("Loaded SELECT into the editor — Ctrl-Enter to run.");
            }
            Some(VisibleRow::Schema { .. }) | None => self.toggle_tree_node(),
        }
    }

    /// Clear an error from the status line when the user resumes editing,
    /// so a stale error does not linger across a fresh attempt. A no-op
    /// when the status is already neutral.
    pub fn clear_error_on_edit(&mut self) {
        if self.status.is_error() {
            self.clear_error();
        }
    }

    /// Execute the query buffer against the connection and store the
    /// result — or the error — for display.
    ///
    /// This blocks the calling (event-loop) thread for the duration of
    /// the query; a long query freezes the UI. Non-blocking execution is
    /// deferred (see [`crate::tui`]). An empty buffer is a no-op with a
    /// status hint rather than a round-trip.
    pub fn run_query(&mut self) {
        let sql = self.input.text().trim().to_string();
        if sql.is_empty() {
            self.set_info("Nothing to run — the query buffer is empty.");
            return;
        }
        self.set_info("Running…");
        match self.conn.query(&sql) {
            Ok(qr) => {
                let row_count = qr.rows.len();
                match ResultsModel::from_query_result(&qr, self.format) {
                    Ok(model) => {
                        self.results = model;
                        self.focus = Focus::Results;
                        self.set_info(format!("{row_count} row(s)"));
                    }
                    Err(e) => self.set_error(format!("formatting failed: {e}")),
                }
            }
            Err(e) => self.set_error(format!("query failed: {e}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::schema_tree::{SchemaEntry, SchemaSource};
    use ferrule_sql::SqlError;

    /// Build an `App` whose connection is never touched, for testing the
    /// pure (non-I/O) transitions. The `App::new` constructor needs a
    /// `Box<dyn Connection>`; we provide a panicking stub since these
    /// tests never call a connection method.
    fn test_app() -> App {
        struct StubSource;
        impl SchemaSource for StubSource {
            fn schemas(&mut self) -> Result<Vec<SchemaEntry>, SqlError> {
                Ok(vec![SchemaEntry {
                    name: "public".into(),
                    is_default: true,
                }])
            }
            fn tables(&mut self, _schema: &str) -> Result<Vec<String>, SqlError> {
                Ok(vec!["t".into()])
            }
        }
        let mut src = StubSource;
        let tree = SchemaTree::build(&mut src).unwrap();
        App::new(stub::boxed(), "sqlite::memory:".to_string(), tree)
    }

    // A `Connection` stub whose methods are never invoked by the pure
    // transition tests. Each method returns a structured error so an
    // accidental call surfaces loudly instead of panicking.
    mod stub {
        use ferrule_sql::connection::{
            BulkInsert, ExecutionSummary, ForeignKey, QueryResult, SchemaInfo, StatementResult,
        };
        use ferrule_sql::stream::RowCursor;
        use ferrule_sql::{Connection, SqlError};

        pub struct StubConn;

        pub fn boxed() -> Box<dyn Connection> {
            Box::new(StubConn)
        }

        fn unused() -> SqlError {
            SqlError::QueryFailed("stub connection: method not exercised in unit tests".into())
        }

        impl Connection for StubConn {
            fn execute(&mut self, _sql: &str) -> Result<ExecutionSummary, SqlError> {
                Err(unused())
            }
            fn query(&mut self, _sql: &str) -> Result<QueryResult, SqlError> {
                Err(unused())
            }
            fn query_cursor(&mut self, _sql: &str) -> Result<RowCursor<'_>, SqlError> {
                Err(unused())
            }
            fn execute_multi(&mut self, _sql: &str) -> Result<Vec<StatementResult>, SqlError> {
                Err(unused())
            }
            fn ping(&mut self) -> Result<(), SqlError> {
                Err(unused())
            }
            fn list_tables(&mut self, _schema: Option<&str>) -> Result<Vec<String>, SqlError> {
                Err(unused())
            }
            fn list_schemas(&mut self) -> Result<Vec<SchemaInfo>, SqlError> {
                Err(unused())
            }
            fn describe_table(
                &mut self,
                _schema: Option<&str>,
                _table: &str,
            ) -> Result<QueryResult, SqlError> {
                Err(unused())
            }
            fn primary_key(
                &mut self,
                _schema: Option<&str>,
                _table: &str,
            ) -> Result<Vec<String>, SqlError> {
                Err(unused())
            }
            fn list_foreign_keys(
                &mut self,
                _schema: Option<&str>,
            ) -> Result<Vec<ForeignKey>, SqlError> {
                Err(unused())
            }
            fn bulk_insert_rows(&mut self, _target: BulkInsert<'_>) -> Result<usize, SqlError> {
                Err(unused())
            }
        }
    }

    #[test]
    fn focus_next_cycles_through_all_panes() {
        let mut app = test_app();
        // Constructor starts on Input.
        assert_eq!(app.focus(), Focus::Input);
        app.focus_next();
        assert_eq!(app.focus(), Focus::Results);
        app.focus_next();
        assert_eq!(app.focus(), Focus::SchemaTree);
        app.focus_next();
        assert_eq!(app.focus(), Focus::Input);
    }

    #[test]
    fn focus_enum_next_is_a_three_cycle() {
        assert_eq!(Focus::SchemaTree.next(), Focus::Input);
        assert_eq!(Focus::Input.next(), Focus::Results);
        assert_eq!(Focus::Results.next(), Focus::SchemaTree);
    }

    #[test]
    fn set_error_stores_message_and_clear_error_wipes_it() {
        let mut app = test_app();
        app.set_error("boom");
        assert!(app.status().is_error());
        assert_eq!(app.status().text(), "boom");
        app.clear_error();
        assert!(!app.status().is_error());
    }

    #[test]
    fn set_info_overwrites_error_state() {
        let mut app = test_app();
        app.set_error("bad");
        app.set_info("fine");
        assert!(!app.status().is_error());
        assert_eq!(app.status().text(), "fine");
    }

    #[test]
    fn quit_sets_running_false() {
        let mut app = test_app();
        assert!(app.running());
        app.quit();
        assert!(!app.running());
    }

    #[test]
    fn run_query_with_empty_buffer_is_a_noop_with_hint() {
        let mut app = test_app();
        // Buffer is empty; this must not touch the (panicking) stub conn.
        app.run_query();
        assert!(!app.status().is_error());
        assert!(app.status().text().contains("empty"));
    }

    #[test]
    fn conn_label_is_preserved() {
        let app = test_app();
        assert_eq!(app.conn_label(), "sqlite::memory:");
    }
}
