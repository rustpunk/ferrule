//! The schema-tree navigation model for the left pane.
//!
//! A two-level tree: each schema node holds a lazily-loaded list of
//! table names. Schema nodes expand and collapse; the model keeps a
//! selection cursor over the *flattened visible rows* (the rows the UI
//! actually draws). All of this is pure state — the only side effect is
//! the [`SchemaSource`] the model is built from, which is injected so
//! the tree is testable without a database and forward-compatible with
//! however the connection surfaces schemas/tables.
//!
//! Track #4 added [`ferrule_sql::Connection::list_schemas`], so the
//! production source ([`ConnectionSchemaSource`]) populates a real
//! schema level and fetches each schema's tables via
//! `list_tables(Some(schema))`. Tests build an in-memory source.

use ferrule_sql::{Connection, SqlError};

/// The data the schema tree needs from a connection, abstracted so the
/// model is unit-testable without a live database.
pub trait SchemaSource {
    /// All schema names, in display order, with the default schema (if
    /// any) marked. The default schema is expanded on initial build.
    fn schemas(&mut self) -> Result<Vec<SchemaEntry>, SqlError>;

    /// Tables within `schema`, in display order.
    fn tables(&mut self, schema: &str) -> Result<Vec<String>, SqlError>;
}

/// One schema returned by [`SchemaSource::schemas`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SchemaEntry {
    /// The schema / database / owner name.
    pub name: String,
    /// `true` for the schema unqualified objects resolve against; the
    /// tree expands this one on initial build.
    pub is_default: bool,
}

/// A [`SchemaSource`] backed by a live [`Connection`]. Schemas come from
/// [`Connection::list_schemas`]; each schema's tables from
/// `list_tables(Some(schema))`.
pub struct ConnectionSchemaSource<'a> {
    conn: &'a mut dyn Connection,
}

impl<'a> ConnectionSchemaSource<'a> {
    /// Wrap a connection as a schema source.
    pub fn new(conn: &'a mut dyn Connection) -> Self {
        Self { conn }
    }
}

impl SchemaSource for ConnectionSchemaSource<'_> {
    fn schemas(&mut self) -> Result<Vec<SchemaEntry>, SqlError> {
        Ok(self
            .conn
            .list_schemas()?
            .into_iter()
            .map(|s| SchemaEntry {
                name: s.name,
                is_default: s.is_default,
            })
            .collect())
    }

    fn tables(&mut self, schema: &str) -> Result<Vec<String>, SqlError> {
        self.conn.list_tables(Some(schema))
    }
}

/// A single schema node and its (lazily-loaded) tables.
#[derive(Debug, Clone)]
struct SchemaNode {
    name: String,
    expanded: bool,
    /// `None` until the schema is first expanded, then the cached table
    /// list. Empty `Vec` means "loaded, no tables".
    tables: Option<Vec<String>>,
}

/// One row of the flattened, drawable view of the tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VisibleRow {
    /// A schema header row. `expanded` drives the disclosure marker.
    Schema { name: String, expanded: bool },
    /// A table row nested under `schema`.
    Table { schema: String, name: String },
}

/// The schema-tree model: schema nodes plus a selection cursor over the
/// flattened visible rows.
pub struct SchemaTree {
    schemas: Vec<SchemaNode>,
    /// Selection index into [`SchemaTree::visible_rows`].
    selected: usize,
}

impl SchemaTree {
    /// Build the tree from `source`. Schemas are listed eagerly; the
    /// default schema is expanded (its tables fetched) on build so the
    /// user sees a populated pane immediately. A failure to fetch the
    /// default schema's tables is swallowed into a collapsed-but-loaded
    /// empty list rather than failing the whole build — the rest of the
    /// tree is still useful.
    pub fn build(source: &mut dyn SchemaSource) -> Result<Self, SqlError> {
        let entries = source.schemas()?;
        // The schema to auto-expand: the one flagged default, else the
        // first schema so the pane is never entirely collapsed on open.
        let default_idx = entries
            .iter()
            .position(|e| e.is_default)
            .or(if entries.is_empty() { None } else { Some(0) });

        let mut schemas: Vec<SchemaNode> = entries
            .into_iter()
            .map(|e| SchemaNode {
                name: e.name,
                expanded: false,
                tables: None,
            })
            .collect();

        // Fetch the default schema's tables up front so the user sees a
        // populated pane immediately. A fetch failure degrades to an
        // empty (loaded) list rather than failing the whole build.
        if let Some(idx) = default_idx {
            if let Some(node) = schemas.get_mut(idx) {
                node.tables = Some(source.tables(&node.name).unwrap_or_default());
                node.expanded = true;
            }
        }

        Ok(Self {
            schemas,
            selected: 0,
        })
    }

    /// The flattened list of rows the UI draws, top to bottom.
    #[must_use]
    pub fn visible_rows(&self) -> Vec<VisibleRow> {
        let mut rows = Vec::new();
        for node in &self.schemas {
            rows.push(VisibleRow::Schema {
                name: node.name.clone(),
                expanded: node.expanded,
            });
            if node.expanded {
                if let Some(tables) = &node.tables {
                    for t in tables {
                        rows.push(VisibleRow::Table {
                            schema: node.name.clone(),
                            name: t.clone(),
                        });
                    }
                }
            }
        }
        rows
    }

    /// Number of currently-visible rows.
    #[must_use]
    pub fn visible_len(&self) -> usize {
        // Counted without allocating the full Vec.
        self.schemas
            .iter()
            .map(|n| {
                1 + if n.expanded {
                    n.tables.as_ref().map_or(0, Vec::len)
                } else {
                    0
                }
            })
            .sum()
    }

    /// The selection cursor index into [`SchemaTree::visible_rows`].
    #[must_use]
    pub fn selected(&self) -> usize {
        self.selected
    }

    /// The currently-selected row, if any (the tree may be empty).
    #[must_use]
    pub fn selected_row(&self) -> Option<VisibleRow> {
        self.visible_rows().into_iter().nth(self.selected)
    }

    /// Move the selection up one visible row, clamped at the top.
    pub fn select_up(&mut self) {
        self.selected = self.selected.saturating_sub(1);
    }

    /// Move the selection down one visible row, clamped at the bottom.
    pub fn select_down(&mut self) {
        let max = self.visible_len().saturating_sub(1);
        if self.selected < max {
            self.selected += 1;
        }
    }

    /// Expand or collapse the schema under the selection. Selecting a
    /// table row toggles its parent schema. Loads tables on first
    /// expand via `source`. A `None` source skips lazy loading (tables
    /// already cached stay; an unloaded schema expands empty).
    pub fn toggle_selected(&mut self, source: Option<&mut dyn SchemaSource>) {
        let Some(row) = self.selected_row() else {
            return;
        };
        let schema_name = match row {
            VisibleRow::Schema { name, .. } => name,
            VisibleRow::Table { schema, .. } => schema,
        };
        let Some(idx) = self.schemas.iter().position(|n| n.name == schema_name) else {
            return;
        };
        let node = &mut self.schemas[idx];
        if node.expanded {
            node.expanded = false;
        } else {
            if node.tables.is_none() {
                if let Some(src) = source {
                    node.tables = Some(src.tables(&node.name).unwrap_or_default());
                } else {
                    node.tables = Some(Vec::new());
                }
            }
            node.expanded = true;
        }
        // Collapsing can drop the selected row below the new bottom;
        // re-clamp so the cursor never dangles past the end.
        self.clamp_selection();
    }

    /// Re-clamp the selection to the current visible range. Cheap to
    /// call after any structural change.
    fn clamp_selection(&mut self) {
        let max = self.visible_len().saturating_sub(1);
        if self.selected > max {
            self.selected = max;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    /// An in-memory [`SchemaSource`] for tests: a fixed schema->tables map.
    struct InMemorySchemaSource {
        schemas: Vec<SchemaEntry>,
        tables: BTreeMap<String, Vec<String>>,
    }

    impl InMemorySchemaSource {
        fn single(default: &str, tables: &[&str]) -> Self {
            let mut map = BTreeMap::new();
            map.insert(
                default.to_string(),
                tables.iter().map(|t| t.to_string()).collect(),
            );
            Self {
                schemas: vec![SchemaEntry {
                    name: default.to_string(),
                    is_default: true,
                }],
                tables: map,
            }
        }
    }

    impl SchemaSource for InMemorySchemaSource {
        fn schemas(&mut self) -> Result<Vec<SchemaEntry>, SqlError> {
            Ok(self.schemas.clone())
        }
        fn tables(&mut self, schema: &str) -> Result<Vec<String>, SqlError> {
            Ok(self.tables.get(schema).cloned().unwrap_or_default())
        }
    }

    #[test]
    fn build_from_fixed_tables_yields_expected_visible_rows() {
        let mut src = InMemorySchemaSource::single("public", &["test_users", "test_orders"]);
        let tree = SchemaTree::build(&mut src).unwrap();
        let rows = tree.visible_rows();
        // Default schema is auto-expanded: 1 schema row + 2 table rows.
        assert_eq!(rows.len(), 3);
        assert_eq!(
            rows[0],
            VisibleRow::Schema {
                name: "public".into(),
                expanded: true
            }
        );
        assert_eq!(
            rows[1],
            VisibleRow::Table {
                schema: "public".into(),
                name: "test_users".into()
            }
        );
        assert_eq!(tree.visible_len(), 3);
    }

    #[test]
    fn collapse_then_expand_toggles_child_visibility() {
        let mut src = InMemorySchemaSource::single("public", &["a", "b"]);
        let mut tree = SchemaTree::build(&mut src).unwrap();
        assert_eq!(tree.visible_len(), 3);

        // Select the schema row and collapse it.
        tree.toggle_selected(Some(&mut src));
        assert_eq!(tree.visible_len(), 1);
        assert_eq!(
            tree.visible_rows()[0],
            VisibleRow::Schema {
                name: "public".into(),
                expanded: false
            }
        );

        // Expand again — children reappear.
        tree.toggle_selected(Some(&mut src));
        assert_eq!(tree.visible_len(), 3);
    }

    #[test]
    fn selection_clamps_at_top_and_bottom() {
        let mut src = InMemorySchemaSource::single("public", &["a", "b"]);
        let mut tree = SchemaTree::build(&mut src).unwrap();

        // At the top: select_up stays at 0.
        tree.select_up();
        assert_eq!(tree.selected(), 0);

        // Walk to the bottom and past it.
        tree.select_down();
        tree.select_down();
        tree.select_down();
        tree.select_down();
        assert_eq!(tree.selected(), tree.visible_len() - 1);
    }

    #[test]
    fn empty_tree_does_not_panic_on_navigation() {
        struct Empty;
        impl SchemaSource for Empty {
            fn schemas(&mut self) -> Result<Vec<SchemaEntry>, SqlError> {
                Ok(Vec::new())
            }
            fn tables(&mut self, _schema: &str) -> Result<Vec<String>, SqlError> {
                Ok(Vec::new())
            }
        }
        let mut src = Empty;
        let mut tree = SchemaTree::build(&mut src).unwrap();
        assert_eq!(tree.visible_len(), 0);
        assert_eq!(tree.selected_row(), None);
        tree.select_up();
        tree.select_down();
        tree.toggle_selected(Some(&mut src));
        assert_eq!(tree.selected(), 0);
    }

    #[test]
    fn collapsing_reclamps_selection_below_new_bottom() {
        let mut src = InMemorySchemaSource::single("public", &["a", "b", "c"]);
        let mut tree = SchemaTree::build(&mut src).unwrap();
        // Move selection onto the last table row.
        for _ in 0..10 {
            tree.select_down();
        }
        assert_eq!(tree.selected(), 3);
        // Toggle the schema (selecting a table toggles its parent).
        tree.toggle_selected(Some(&mut src));
        // Now only the schema row is visible; selection re-clamped to 0.
        assert_eq!(tree.visible_len(), 1);
        assert_eq!(tree.selected(), 0);
    }

    #[test]
    fn toggle_without_source_expands_empty() {
        // A schema whose tables were never loaded, expanded with no
        // source, becomes an empty (loaded) list rather than panicking.
        let mut src = InMemorySchemaSource {
            schemas: vec![
                SchemaEntry {
                    name: "public".into(),
                    is_default: true,
                },
                SchemaEntry {
                    name: "other".into(),
                    is_default: false,
                },
            ],
            tables: {
                let mut m = BTreeMap::new();
                m.insert("public".to_string(), vec!["t".to_string()]);
                m.insert("other".to_string(), vec!["u".to_string()]);
                m
            },
        };
        let mut tree = SchemaTree::build(&mut src).unwrap();
        // public expanded (1+1), other collapsed (1) => 3 rows.
        assert_eq!(tree.visible_len(), 3);
        // Select the "other" schema row (index 2) and expand with no source.
        tree.select_down();
        tree.select_down();
        assert!(matches!(
            tree.selected_row(),
            Some(VisibleRow::Schema { .. })
        ));
        tree.toggle_selected(None);
        // "other" expands but loads no tables (source withheld).
        assert_eq!(tree.visible_len(), 3);
    }
}
