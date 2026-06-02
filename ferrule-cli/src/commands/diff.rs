use super::DiffArgs;
use crate::error::CliError;
use ferrule_config::profile::GlobalConfig;
use ferrule_core::backend::Backend;
use ferrule_core::connection::{ConnectOptions, Connection, QueryResult};
use ferrule_core::formatter::OutputFormat;
use ferrule_core::value::Value;
use std::collections::BTreeMap;

/// One column extracted from a `describe_table` result, normalised across
/// backends to just (name, data_type) for diff comparison.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ColumnSpec {
    name: String,
    data_type: String,
}

/// Diff for a single table that exists on both sides.
#[derive(Debug)]
struct TableDiff {
    table: String,
    only_in_a: Vec<ColumnSpec>,
    only_in_b: Vec<ColumnSpec>,
    type_changes: Vec<TypeChange>,
}

#[derive(Debug)]
struct TypeChange {
    column: String,
    a_type: String,
    b_type: String,
}

impl TableDiff {
    fn is_empty(&self) -> bool {
        self.only_in_a.is_empty() && self.only_in_b.is_empty() && self.type_changes.is_empty()
    }
}

/// Top-level diff result across two databases.
#[derive(Debug, Default)]
struct SchemaDiff {
    only_in_a: Vec<String>,
    only_in_b: Vec<String>,
    table_diffs: Vec<TableDiff>,
}

impl SchemaDiff {
    fn is_empty(&self) -> bool {
        self.only_in_a.is_empty()
            && self.only_in_b.is_empty()
            && self.table_diffs.iter().all(TableDiff::is_empty)
    }
}

/// Backends store their describe metadata in different shapes:
/// - SQLite uses `PRAGMA table_info`: (cid, name, type, notnull, dflt_value, pk)
/// - Postgres / MySQL / MSSQL / Oracle query `information_schema.columns`,
///   which by ferrule's convention returns (column_name, data_type, ...).
fn extract_columns(result: &QueryResult, backend: Backend) -> Vec<ColumnSpec> {
    let (name_idx, type_idx) = match backend {
        Backend::Sqlite => (1usize, 2usize),
        // information_schema.columns variant — same shape across the other
        // backends because ferrule's describe_table normalises them.
        _ => (0usize, 1usize),
    };

    result
        .rows
        .iter()
        .filter_map(|row| {
            let name = match row.get(name_idx) {
                Some(Value::String(s)) => s.clone(),
                _ => return None,
            };
            let data_type = match row.get(type_idx) {
                Some(Value::String(s)) => s.clone(),
                _ => String::new(),
            };
            Some(ColumnSpec { name, data_type })
        })
        .collect()
}

fn diff_tables(
    a: &[ColumnSpec],
    b: &[ColumnSpec],
) -> (Vec<ColumnSpec>, Vec<ColumnSpec>, Vec<TypeChange>) {
    let a_map: BTreeMap<&str, &ColumnSpec> = a.iter().map(|c| (c.name.as_str(), c)).collect();
    let b_map: BTreeMap<&str, &ColumnSpec> = b.iter().map(|c| (c.name.as_str(), c)).collect();

    let only_in_a = a
        .iter()
        .filter(|c| !b_map.contains_key(c.name.as_str()))
        .cloned()
        .collect();
    let only_in_b = b
        .iter()
        .filter(|c| !a_map.contains_key(c.name.as_str()))
        .cloned()
        .collect();
    let type_changes = a
        .iter()
        .filter_map(|ca| {
            b_map.get(ca.name.as_str()).and_then(|cb| {
                if ca.data_type.eq_ignore_ascii_case(&cb.data_type) {
                    None
                } else {
                    Some(TypeChange {
                        column: ca.name.clone(),
                        a_type: ca.data_type.clone(),
                        b_type: cb.data_type.clone(),
                    })
                }
            })
        })
        .collect();

    (only_in_a, only_in_b, type_changes)
}

async fn collect_table_set(
    conn: &mut dyn Connection,
    pinned: Option<&str>,
) -> Result<Vec<String>, CliError> {
    if let Some(name) = pinned {
        return Ok(vec![name.to_string()]);
    }
    conn.list_tables(None).await.map_err(CliError::query)
}

async fn build_schema_diff(
    a_conn: &mut dyn Connection,
    a_backend: Backend,
    b_conn: &mut dyn Connection,
    b_backend: Backend,
    pinned_table: Option<&str>,
) -> Result<SchemaDiff, CliError> {
    let a_tables = collect_table_set(a_conn, pinned_table).await?;
    let b_tables = collect_table_set(b_conn, pinned_table).await?;

    let a_set: BTreeMap<String, ()> = a_tables.iter().cloned().map(|t| (t, ())).collect();
    let b_set: BTreeMap<String, ()> = b_tables.iter().cloned().map(|t| (t, ())).collect();

    let mut diff = SchemaDiff::default();

    for t in &a_tables {
        if !b_set.contains_key(t) {
            diff.only_in_a.push(t.clone());
        }
    }
    for t in &b_tables {
        if !a_set.contains_key(t) {
            diff.only_in_b.push(t.clone());
        }
    }

    // For tables present on both sides, compare columns.
    let mut common: Vec<&String> = a_tables.iter().filter(|t| b_set.contains_key(*t)).collect();
    common.sort();
    for t in common {
        let a_desc = a_conn
            .describe_table(None, t)
            .await
            .map_err(CliError::query)?;
        let b_desc = b_conn
            .describe_table(None, t)
            .await
            .map_err(CliError::query)?;
        let a_cols = extract_columns(&a_desc, a_backend);
        let b_cols = extract_columns(&b_desc, b_backend);
        let (only_in_a, only_in_b, type_changes) = diff_tables(&a_cols, &b_cols);
        diff.table_diffs.push(TableDiff {
            table: t.clone(),
            only_in_a,
            only_in_b,
            type_changes,
        });
    }

    Ok(diff)
}

fn render_diff(diff: &SchemaDiff, format: OutputFormat) -> Result<String, CliError> {
    match format {
        OutputFormat::Json => render_json(diff),
        // For non-JSON output we render a human-readable plain-text summary.
        // Tabled's column model assumes uniform columns, but a diff has
        // sections of different shape — the textual section format is more
        // useful here than a forced two-column table.
        //
        // The wildcard arm intentionally absorbs every non-JSON variant —
        // including row-shaped formats like Markdown/JSONL/HTML (added in
        // #12). Schema diffs are structural, not row-shaped, so a single
        // text summary is the right rendering for all of them.
        _ => Ok(render_text(diff)),
    }
}

fn render_json(diff: &SchemaDiff) -> Result<String, CliError> {
    let json = serde_json::json!({
        "tables_only_in_a": diff.only_in_a,
        "tables_only_in_b": diff.only_in_b,
        "table_diffs": diff.table_diffs.iter().map(|td| {
            serde_json::json!({
                "table": td.table,
                "columns_only_in_a": td.only_in_a.iter().map(|c| {
                    serde_json::json!({"name": c.name, "data_type": c.data_type})
                }).collect::<Vec<_>>(),
                "columns_only_in_b": td.only_in_b.iter().map(|c| {
                    serde_json::json!({"name": c.name, "data_type": c.data_type})
                }).collect::<Vec<_>>(),
                "type_changes": td.type_changes.iter().map(|tc| {
                    serde_json::json!({
                        "column": tc.column,
                        "a_type": tc.a_type,
                        "b_type": tc.b_type,
                    })
                }).collect::<Vec<_>>(),
            })
        }).collect::<Vec<_>>(),
    });
    serde_json::to_string_pretty(&json)
        .map_err(|e| CliError::query(ferrule_core::CoreError::QueryFailed(e.to_string())))
}

fn render_text(diff: &SchemaDiff) -> String {
    let mut out = String::new();
    if diff.is_empty() {
        out.push_str("No schema differences.\n");
        return out;
    }

    if !diff.only_in_a.is_empty() {
        out.push_str("Tables only in A:\n");
        for t in &diff.only_in_a {
            out.push_str(&format!("  - {t}\n"));
        }
        out.push('\n');
    }
    if !diff.only_in_b.is_empty() {
        out.push_str("Tables only in B:\n");
        for t in &diff.only_in_b {
            out.push_str(&format!("  + {t}\n"));
        }
        out.push('\n');
    }
    for td in &diff.table_diffs {
        if td.is_empty() {
            continue;
        }
        out.push_str(&format!("Table {}:\n", td.table));
        for c in &td.only_in_a {
            out.push_str(&format!("  - {} {}\n", c.name, c.data_type));
        }
        for c in &td.only_in_b {
            out.push_str(&format!("  + {} {}\n", c.name, c.data_type));
        }
        for tc in &td.type_changes {
            out.push_str(&format!(
                "  ~ {}: {} -> {}\n",
                tc.column, tc.a_type, tc.b_type
            ));
        }
        out.push('\n');
    }
    out
}

pub async fn run(args: DiffArgs, global_config: &GlobalConfig) -> Result<(), CliError> {
    let format = args.output.resolve_format(global_config);

    // Both sides share the same `--ssh-tunnel` / `--ssh-key`: in
    // practice users diffing against a bastion-isolated DB tunnel
    // both ends through the same bastion. Cross-bastion diffs would
    // need per-side SSH flags; out of scope for now.
    let resolved_a = super::resolve_connection(
        &args.connection_a,
        args.password_a,
        args.conn_flags.ssh_tunnel.as_deref(),
        args.conn_flags.ssh_key.as_deref(),
        args.conn_flags.proxy_url.as_deref(),
        global_config,
    )
    .await?;
    let resolved_b = super::resolve_connection(
        &args.connection_b,
        args.password_b,
        args.conn_flags.ssh_tunnel.as_deref(),
        args.conn_flags.ssh_key.as_deref(),
        args.conn_flags.proxy_url.as_deref(),
        global_config,
    )
    .await?;
    super::check_daemon_ssh_compat(args.conn_flags.daemon, &resolved_a)?;
    super::check_daemon_ssh_compat(args.conn_flags.daemon, &resolved_b)?;

    let backend_a = Backend::from_scheme(resolved_a.url.scheme()).ok_or_else(|| {
        CliError::usage(format!("Unsupported scheme A: {}", resolved_a.url.scheme()))
    })?;
    let backend_b = Backend::from_scheme(resolved_b.url.scheme()).ok_or_else(|| {
        CliError::usage(format!("Unsupported scheme B: {}", resolved_b.url.scheme()))
    })?;

    if args.output.verbose {
        eprintln!(
            "[ferrule] Diff: {} ({}) vs {} ({})",
            resolved_a.url.redacted(),
            backend_a.name(),
            resolved_b.url.redacted(),
            backend_b.name()
        );
    }

    let opts = ConnectOptions {
        insecure: args.conn_flags.insecure,
    };
    if opts.insecure {
        eprintln!("Warning: --insecure disables TLS certificate verification.");
    }

    let mut conn_a = super::connect_resolved(resolved_a, &opts).await?;
    let mut conn_b = super::connect_resolved(resolved_b, &opts).await?;

    let diff = build_schema_diff(
        conn_a.as_mut(),
        backend_a,
        conn_b.as_mut(),
        backend_b,
        args.table.as_deref(),
    )
    .await?;

    let rendered = render_diff(&diff, format)?;
    println!("{}", rendered);

    // GNU diff convention: exit 0 when no differences, exit 1 when any are
    // found. Code 1 is `exit::RESULT_NOTABLE` — reserved for diff-class
    // commands that succeed with a caller-gateable result.
    if !diff.is_empty() {
        std::process::exit(crate::error::exit::RESULT_NOTABLE);
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ferrule_core::connection::QueryResult;
    use ferrule_core::value::ColumnInfo;

    fn col(name: &str, data_type: &str) -> ColumnSpec {
        ColumnSpec {
            name: name.to_string(),
            data_type: data_type.to_string(),
        }
    }

    #[test]
    fn diff_columns_added_removed_changed() {
        let a = vec![
            col("id", "INTEGER"),
            col("name", "TEXT"),
            col("age", "INTEGER"),
        ];
        let b = vec![
            col("id", "INTEGER"),
            col("name", "VARCHAR"),
            col("email", "TEXT"),
        ];
        let (only_a, only_b, type_changes) = diff_tables(&a, &b);
        assert_eq!(only_a, vec![col("age", "INTEGER")]);
        assert_eq!(only_b, vec![col("email", "TEXT")]);
        assert_eq!(type_changes.len(), 1);
        assert_eq!(type_changes[0].column, "name");
        assert_eq!(type_changes[0].a_type, "TEXT");
        assert_eq!(type_changes[0].b_type, "VARCHAR");
    }

    #[test]
    fn diff_columns_no_diff_when_identical() {
        let cols = vec![col("id", "INTEGER"), col("name", "TEXT")];
        let (only_a, only_b, type_changes) = diff_tables(&cols, &cols);
        assert!(only_a.is_empty());
        assert!(only_b.is_empty());
        assert!(type_changes.is_empty());
    }

    #[test]
    fn diff_columns_case_insensitive_type_match() {
        let a = vec![col("id", "Integer")];
        let b = vec![col("id", "INTEGER")];
        let (_, _, type_changes) = diff_tables(&a, &b);
        assert!(
            type_changes.is_empty(),
            "type comparison should be case-insensitive"
        );
    }

    #[test]
    fn extract_columns_sqlite_pragma_shape() {
        // PRAGMA table_info row: (cid, name, type, notnull, dflt_value, pk)
        let qr = QueryResult {
            columns: vec![
                ColumnInfo {
                    name: "cid".into(),
                    type_hint: ferrule_core::value::TypeHint::Int64,
                    nullable: false,
                },
                ColumnInfo {
                    name: "name".into(),
                    type_hint: ferrule_core::value::TypeHint::String,
                    nullable: false,
                },
                ColumnInfo {
                    name: "type".into(),
                    type_hint: ferrule_core::value::TypeHint::String,
                    nullable: false,
                },
                ColumnInfo {
                    name: "notnull".into(),
                    type_hint: ferrule_core::value::TypeHint::Int64,
                    nullable: false,
                },
                ColumnInfo {
                    name: "dflt_value".into(),
                    type_hint: ferrule_core::value::TypeHint::String,
                    nullable: true,
                },
                ColumnInfo {
                    name: "pk".into(),
                    type_hint: ferrule_core::value::TypeHint::Int64,
                    nullable: false,
                },
            ],
            rows: vec![
                vec![
                    Value::Int64(0),
                    Value::String("id".into()),
                    Value::String("INTEGER".into()),
                    Value::Int64(1),
                    Value::Null,
                    Value::Int64(1),
                ],
                vec![
                    Value::Int64(1),
                    Value::String("name".into()),
                    Value::String("TEXT".into()),
                    Value::Int64(0),
                    Value::Null,
                    Value::Int64(0),
                ],
            ],
        };
        let cols = extract_columns(&qr, Backend::Sqlite);
        assert_eq!(cols, vec![col("id", "INTEGER"), col("name", "TEXT")]);
    }

    #[tokio::test]
    async fn end_to_end_sqlite_drift_detected() {
        use ferrule_core::url::DatabaseUrl;
        use std::sync::atomic::{AtomicU64, Ordering};

        static N: AtomicU64 = AtomicU64::new(0);
        let pid = std::process::id();
        let n = N.fetch_add(1, Ordering::SeqCst);
        let path_a = std::env::temp_dir().join(format!("ferrule-diff-test-{pid}-{n}-a.db"));
        let n = N.fetch_add(1, Ordering::SeqCst);
        let path_b = std::env::temp_dir().join(format!("ferrule-diff-test-{pid}-{n}-b.db"));
        let _ = std::fs::remove_file(&path_a);
        let _ = std::fs::remove_file(&path_b);

        let url_a = DatabaseUrl::parse(&format!("sqlite://{}", path_a.display())).unwrap();
        let url_b = DatabaseUrl::parse(&format!("sqlite://{}", path_b.display())).unwrap();
        let mut a = ferrule_core::connect(&url_a, &ConnectOptions::default(), None)
            .await
            .unwrap();
        let mut b = ferrule_core::connect(&url_b, &ConnectOptions::default(), None)
            .await
            .unwrap();

        a.execute("CREATE TABLE t (id INTEGER, name TEXT)")
            .await
            .unwrap();
        b.execute("CREATE TABLE t (id INTEGER, name TEXT, age INTEGER)")
            .await
            .unwrap();
        a.execute("CREATE TABLE only_a (id INTEGER)").await.unwrap();
        b.execute("CREATE TABLE only_b (id INTEGER)").await.unwrap();

        let diff = build_schema_diff(
            a.as_mut(),
            Backend::Sqlite,
            b.as_mut(),
            Backend::Sqlite,
            None,
        )
        .await
        .expect("diff");

        assert!(
            diff.only_in_a.contains(&"only_a".to_string()),
            "only_in_a: {:?}",
            diff.only_in_a
        );
        assert!(
            diff.only_in_b.contains(&"only_b".to_string()),
            "only_in_b: {:?}",
            diff.only_in_b
        );
        let t_diff = diff
            .table_diffs
            .iter()
            .find(|td| td.table == "t")
            .expect("t should be in table_diffs");
        assert_eq!(t_diff.only_in_b, vec![col("age", "INTEGER")]);
        assert!(t_diff.only_in_a.is_empty());
        assert!(t_diff.type_changes.is_empty());

        assert!(!diff.is_empty(), "diff should be non-empty");

        let _ = std::fs::remove_file(&path_a);
        let _ = std::fs::remove_file(&path_b);
    }
}
