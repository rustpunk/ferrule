use crate::connection::{
    AsyncConnection, BulkInsert, ConnectOptions, ExecutionSummary, ForeignKey, QueryResult,
    SchemaInfo, StatementResult,
};
use crate::error::SqlError;
use crate::stream::BoxRowStream;
use crate::url::DatabaseUrl;
use crate::value::{ColumnInfo, Row, TypeHint, Value};
use async_trait::async_trait;
use chrono::{DateTime as ChronoDateTime, FixedOffset, NaiveDate, NaiveDateTime, NaiveTime, Utc};
use secrecy::ExposeSecret;
use tiberius::{
    Client, ColumnData, ColumnType, EncryptionLevel, IntoSql, TokenRow, numeric::Numeric,
};
use tokio::net::TcpStream;
use tokio_util::compat::TokioAsyncWriteCompatExt;

pub struct MssqlConnection {
    client: Client<tokio_util::compat::Compat<TcpStream>>,
}

#[async_trait]
impl AsyncConnection for MssqlConnection {
    async fn execute(&mut self, sql: &str) -> Result<ExecutionSummary, SqlError> {
        let result = self
            .client
            .execute(sql, &[])
            .await
            .map_err(|e| SqlError::QueryFailed(e.to_string()))?;
        let affected = result.rows_affected().first().copied();
        Ok(ExecutionSummary {
            rows_affected: affected,
            command_tag: None,
        })
    }

    async fn query(&mut self, sql: &str) -> Result<QueryResult, SqlError> {
        let rows = self
            .client
            .query(sql, &[])
            .await
            .map_err(|e| SqlError::QueryFailed(e.to_string()))?
            .into_first_result()
            .await
            .map_err(|e| SqlError::QueryFailed(e.to_string()))?;

        if rows.is_empty() {
            return Ok(QueryResult {
                columns: Vec::new(),
                rows: Vec::new(),
            });
        }

        let columns: Vec<ColumnInfo> = rows[0]
            .columns()
            .iter()
            .map(|c| ColumnInfo {
                name: c.name().to_string(),
                type_hint: mssql_type_to_hint(c.column_type()),
                nullable: true,
            })
            .collect();

        let data_rows: Vec<Row> = rows
            .into_iter()
            .map(|row| {
                row.columns()
                    .iter()
                    .enumerate()
                    .map(|(i, col)| mssql_to_value(&row, i, col.column_type()))
                    .collect()
            })
            .collect();

        Ok(QueryResult {
            columns,
            rows: data_rows,
        })
    }

    /// Stream rows from a tiberius `QueryStream` at bounded memory.
    ///
    /// tiberius hands back a token stream whose first item is the
    /// `Metadata` describing the result columns, followed by one `Row`
    /// item per row, pulled from the server only as the stream is
    /// advanced. We read the leading metadata for the column shape, then
    /// drive the remaining rows through a `try_unfold` that owns the
    /// `QueryStream` (borrowing `&mut self.client`). Only the first
    /// result set is streamed, matching the eager `query`'s
    /// `into_first_result` contract; a metadata token for a later result
    /// set terminates the stream. Memory stays `O(1)` per pulled row.
    async fn query_stream(
        &mut self,
        sql: &str,
    ) -> Result<(Vec<ColumnInfo>, BoxRowStream<'_>), SqlError> {
        use futures_util::stream::{StreamExt, TryStreamExt};
        use tiberius::QueryItem;

        let mut query_stream = self
            .client
            .query(sql, &[])
            .await
            .map_err(|e| SqlError::QueryFailed(e.to_string()))?;

        // The first token of a tiberius result is the Metadata that
        // carries the column shape for the first result set.
        let (columns, col_types) = match query_stream.try_next().await {
            Ok(Some(QueryItem::Metadata(meta))) => {
                let columns: Vec<ColumnInfo> = meta
                    .columns()
                    .iter()
                    .map(|c| ColumnInfo {
                        name: c.name().to_string(),
                        type_hint: mssql_type_to_hint(c.column_type()),
                        nullable: true,
                    })
                    .collect();
                let col_types: Vec<ColumnType> =
                    meta.columns().iter().map(|c| c.column_type()).collect();
                (columns, col_types)
            }
            // A row before any metadata should not happen; treat it (and
            // an empty stream) as a zero-column result for safety.
            Ok(Some(QueryItem::Row(_))) | Ok(None) => (Vec::new(), Vec::new()),
            Err(e) => return Err(SqlError::QueryFailed(e.to_string())),
        };

        let stream = futures_util::stream::try_unfold(
            (query_stream, col_types),
            |(mut query_stream, col_types)| async move {
                match query_stream.try_next().await {
                    Ok(Some(QueryItem::Row(row))) => {
                        let values: Row = col_types
                            .iter()
                            .enumerate()
                            .map(|(i, col_type)| mssql_to_value(&row, i, *col_type))
                            .collect();
                        Ok(Some((values, (query_stream, col_types))))
                    }
                    // A second result set's metadata ends the first set;
                    // an empty stream ends iteration.
                    Ok(Some(QueryItem::Metadata(_))) | Ok(None) => Ok(None),
                    Err(e) => Err(SqlError::QueryFailed(e.to_string())),
                }
            },
        )
        .boxed();
        Ok((columns, stream))
    }

    async fn execute_multi(&mut self, sql: &str) -> Result<Vec<StatementResult>, SqlError> {
        let result_sets = self
            .client
            .query(sql, &[])
            .await
            .map_err(|e| SqlError::QueryFailed(e.to_string()))?
            .into_results()
            .await
            .map_err(|e| SqlError::QueryFailed(e.to_string()))?;

        let mut results = Vec::new();
        for rows in result_sets {
            if rows.is_empty() {
                results.push(StatementResult::Query(QueryResult {
                    columns: Vec::new(),
                    rows: Vec::new(),
                }));
                continue;
            }
            let columns: Vec<ColumnInfo> = rows[0]
                .columns()
                .iter()
                .map(|c| ColumnInfo {
                    name: c.name().to_string(),
                    type_hint: mssql_type_to_hint(c.column_type()),
                    nullable: true,
                })
                .collect();

            let data_rows: Vec<Row> = rows
                .into_iter()
                .map(|row| {
                    row.columns()
                        .iter()
                        .enumerate()
                        .map(|(i, col)| mssql_to_value(&row, i, col.column_type()))
                        .collect()
                })
                .collect();

            results.push(StatementResult::Query(QueryResult {
                columns,
                rows: data_rows,
            }));
        }

        if results.is_empty() {
            let summary = self.execute(sql).await?;
            results.push(StatementResult::Summary(summary));
        }

        Ok(results)
    }

    async fn ping(&mut self) -> Result<(), SqlError> {
        self.client
            .query("SELECT 1", &[])
            .await
            .map_err(|e| SqlError::ConnectionFailed(e.to_string()))?
            .into_first_result()
            .await
            .map_err(|e| SqlError::ConnectionFailed(e.to_string()))?;
        Ok(())
    }

    async fn list_tables(&mut self, schema: Option<&str>) -> Result<Vec<String>, SqlError> {
        let schema = schema.unwrap_or("dbo");
        let sql = format!(
            "SELECT TABLE_NAME AS table_name FROM information_schema.tables WHERE table_schema = '{}' AND table_type = 'BASE TABLE' ORDER BY table_name",
            escape_mssql_string(schema)
        );
        let result = self.query(&sql).await?;
        let names: Vec<String> = result
            .rows
            .into_iter()
            .filter_map(|row| {
                row.into_iter().next().and_then(|v| match v {
                    Value::String(s) => Some(s),
                    _ => None,
                })
            })
            .collect();
        Ok(names)
    }

    async fn list_schemas(&mut self) -> Result<Vec<SchemaInfo>, SqlError> {
        // `sys.schemas` lists the SCHEMAS within the current database
        // (mirrors list_tables, which is schema-scoped to the connected
        // database); `SCHEMA_NAME()` flags the caller's default schema,
        // typically `dbo`. The server-level database list (`sys.databases`)
        // is a different axis and is intentionally not surfaced here.
        let sql = "SELECT name, CASE WHEN name = SCHEMA_NAME() THEN 1 ELSE 0 END FROM sys.schemas ORDER BY name";
        let result = self.query(sql).await?;
        let schemas: Vec<SchemaInfo> = result
            .rows
            .into_iter()
            .filter_map(|row| {
                let name = match row.first() {
                    Some(Value::String(s)) => s.clone(),
                    _ => return None,
                };
                let is_default = crate::connection::is_default_from_value(row.get(1));
                Some(SchemaInfo { name, is_default })
            })
            .collect();
        Ok(schemas)
    }

    async fn describe_table(
        &mut self,
        schema: Option<&str>,
        table: &str,
    ) -> Result<QueryResult, SqlError> {
        let schema = schema.unwrap_or("dbo");
        let sql = format!(
            "SELECT COLUMN_NAME AS column_name, DATA_TYPE AS data_type, IS_NULLABLE AS is_nullable, COLUMN_DEFAULT AS column_default, NUMERIC_PRECISION AS numeric_precision, NUMERIC_SCALE AS numeric_scale FROM information_schema.columns WHERE table_schema = '{}' AND table_name = '{}' ORDER BY ORDINAL_POSITION",
            escape_mssql_string(schema),
            escape_mssql_string(table)
        );
        self.query(&sql).await
    }

    async fn primary_key(
        &mut self,
        schema: Option<&str>,
        table: &str,
    ) -> Result<Vec<String>, SqlError> {
        let schema = schema.unwrap_or("dbo");
        // INFORMATION_SCHEMA covers both PK columns and key order;
        // join against TABLE_CONSTRAINTS to filter to PRIMARY KEY only.
        let sql = format!(
            "SELECT k.COLUMN_NAME FROM INFORMATION_SCHEMA.KEY_COLUMN_USAGE k \
             JOIN INFORMATION_SCHEMA.TABLE_CONSTRAINTS c \
               ON c.CONSTRAINT_NAME = k.CONSTRAINT_NAME \
              AND c.TABLE_SCHEMA = k.TABLE_SCHEMA \
              AND c.TABLE_NAME = k.TABLE_NAME \
             WHERE c.CONSTRAINT_TYPE = 'PRIMARY KEY' \
               AND k.TABLE_SCHEMA = '{}' AND k.TABLE_NAME = '{}' \
             ORDER BY k.ORDINAL_POSITION",
            escape_mssql_string(schema),
            escape_mssql_string(table)
        );
        let result = self.query(&sql).await?;
        Ok(result
            .rows
            .into_iter()
            .filter_map(|row| {
                row.into_iter().next().and_then(|v| match v {
                    Value::String(s) => Some(s),
                    _ => None,
                })
            })
            .collect())
    }

    async fn list_foreign_keys(
        &mut self,
        schema: Option<&str>,
    ) -> Result<Vec<ForeignKey>, SqlError> {
        let schema = schema.unwrap_or("dbo");
        // sys.foreign_key_columns has one row per (FK, position) and
        // exposes constraint_object_id for grouping.
        let sql = format!(
            "SELECT fk.name, \
                    OBJECT_NAME(fkc.parent_object_id) AS child_table, \
                    COL_NAME(fkc.parent_object_id, fkc.parent_column_id) AS child_col, \
                    OBJECT_NAME(fkc.referenced_object_id) AS parent_table, \
                    COL_NAME(fkc.referenced_object_id, fkc.referenced_column_id) AS parent_col, \
                    fk.delete_referential_action_desc, \
                    fkc.constraint_column_id \
             FROM sys.foreign_keys fk \
             JOIN sys.foreign_key_columns fkc ON fkc.constraint_object_id = fk.object_id \
             WHERE SCHEMA_NAME(fk.schema_id) = '{}' \
             ORDER BY fk.name, fkc.constraint_column_id",
            escape_mssql_string(schema)
        );
        let result = self.query(&sql).await?;
        let mut map: indexmap::IndexMap<String, ForeignKey> = indexmap::IndexMap::new();
        for row in result.rows {
            let mut cols = row.into_iter();
            let conname = match cols.next() {
                Some(Value::String(s)) => s,
                _ => continue,
            };
            let child_table = match cols.next() {
                Some(Value::String(s)) => s,
                _ => continue,
            };
            let child_col = match cols.next() {
                Some(Value::String(s)) => s,
                _ => continue,
            };
            let parent_table = match cols.next() {
                Some(Value::String(s)) => s,
                _ => continue,
            };
            let parent_col = match cols.next() {
                Some(Value::String(s)) => s,
                _ => continue,
            };
            let on_delete = match cols.next() {
                Some(Value::String(s)) if !s.is_empty() && s != "NO_ACTION" => {
                    Some(s.replace('_', " "))
                }
                _ => None,
            };
            let entry = map.entry(conname).or_insert_with(|| ForeignKey {
                child_table: child_table.clone(),
                child_columns: Vec::new(),
                parent_table: parent_table.clone(),
                parent_columns: Vec::new(),
                on_delete,
            });
            entry.child_columns.push(child_col);
            entry.parent_columns.push(parent_col);
        }
        Ok(map.into_values().collect())
    }

    async fn bulk_insert_rows(&mut self, target: BulkInsert<'_>) -> Result<usize, SqlError> {
        if target.rows.is_empty() {
            return Ok(0);
        }

        // tiberius's `client.bulk_insert(table)` issues
        // `SELECT TOP 0 * FROM <table>` to introspect the column
        // metadata, then opens a TDS bulk-load stream against the
        // `Updateable` columns *in physical order*. Both interpolations
        // happen verbatim, so we pre-quote with the backend's
        // ANSI-quoting helper.
        let qtable = crate::copy::quote_identifier(target.table, crate::backend::Backend::MsSql);

        // C1 pre-flight: the destination's physical column order
        // (minus IDENTITY columns) MUST match `target.columns` exactly
        // — otherwise we'd push positional `TokenRow`s into the wrong
        // columns silently, or push N values into N-1 slots (IDENTITY
        // mismatch). The generic INSERT path uses named column lists
        // so it doesn't have this hazard; return `BulkUnavailable` on
        // mismatch so `--bulk-native=auto` degrades cleanly.
        let dest_cols = self.fetch_bulk_updatable_columns(target.table).await?;
        verify_bulk_column_alignment(&dest_cols, target.columns)?;

        let mut req = self
            .client
            .bulk_insert(qtable.as_str())
            .await
            .map_err(|e| classify_bulk_setup_error(&e))?;

        let hints: Vec<TypeHint> = target.columns.iter().map(|c| c.type_hint).collect();
        for row in target.rows {
            let mut token_row = TokenRow::<'static>::with_capacity(target.columns.len());
            for (idx, v) in row.iter().enumerate() {
                let hint = hints.get(idx).copied().unwrap_or(TypeHint::Other);
                token_row.push(value_to_column_data(v, hint)?);
            }
            req.send(token_row)
                .await
                .map_err(|e| SqlError::QueryFailed(format!("MSSQL bulk send: {e}")))?;
        }

        let res = req
            .finalize()
            .await
            .map_err(|e| SqlError::QueryFailed(format!("MSSQL bulk finalize: {e}")))?;
        Ok(res.total() as usize)
    }
}

impl MssqlConnection {
    /// Fetch destination column names in physical order, filtered to
    /// the same set `tiberius::Client::bulk_insert` will accept.
    ///
    /// Tiberius filters server-side using the TDS `ColumnFlag::Updateable`
    /// bit (tiberius-0.12.3 client.rs:332). Server-side, that bit is
    /// cleared for:
    /// - IDENTITY columns,
    /// - Computed (persisted or virtual) columns,
    /// - ROWVERSION / TIMESTAMP columns (auto-maintained by the server).
    ///
    /// We can't query TDS column flags via INFORMATION_SCHEMA, so we
    /// match each condition with its property/type equivalent. Missing
    /// any of these would surface as either a silent column mis-
    /// alignment (Updateable count mismatches expectations) or a
    /// hard error from tiberius mid-stream — both of which the
    /// `verify_bulk_column_alignment` step catches downstream, but
    /// filtering here keeps the auto-fallback path clean.
    async fn fetch_bulk_updatable_columns(&mut self, table: &str) -> Result<Vec<String>, SqlError> {
        // Resolve the (schema, name) pair from the user's input.
        // Accepts plain `test_users`, dot-qualified `dbo.test_users`,
        // T-SQL bracketed `[dbo].[test_users]`, and even brackets
        // wrapping embedded dots (`[my.weird].[table]`). See #41 for
        // the regression that motivated bracketed support.
        let qualified = parse_mssql_qualified_identifier(table);
        let schema_filter = match &qualified.schema {
            Some(schema) => format!(" AND c.TABLE_SCHEMA = '{}'", escape_mssql_string(schema)),
            None => String::new(),
        };
        let table_name = qualified.name;
        let sql = format!(
            "SELECT c.COLUMN_NAME, \
                    COLUMNPROPERTY(OBJECT_ID(QUOTENAME(c.TABLE_SCHEMA) + '.' + QUOTENAME(c.TABLE_NAME)), c.COLUMN_NAME, 'IsIdentity') AS is_identity, \
                    COLUMNPROPERTY(OBJECT_ID(QUOTENAME(c.TABLE_SCHEMA) + '.' + QUOTENAME(c.TABLE_NAME)), c.COLUMN_NAME, 'IsComputed') AS is_computed, \
                    c.DATA_TYPE \
             FROM INFORMATION_SCHEMA.COLUMNS c \
             WHERE c.TABLE_NAME = '{}'{} \
             ORDER BY c.ORDINAL_POSITION",
            escape_mssql_string(&table_name),
            schema_filter,
        );
        let result = self.query(&sql).await.map_err(|e| {
            SqlError::BulkUnavailable(format!(
                "MSSQL bulk pre-flight: could not introspect destination columns: {e}"
            ))
        })?;
        let mut cols = Vec::with_capacity(result.rows.len());
        for row in &result.rows {
            // Skip columns tiberius will skip via ColumnFlag::Updateable:
            // IDENTITY, computed, and ROWVERSION (DATA_TYPE = 'timestamp').
            let is_identity = column_flag_bool(&row[1]);
            let is_computed = column_flag_bool(&row[2]);
            let is_rowversion =
                matches!(&row[3], Value::String(s) if s.eq_ignore_ascii_case("timestamp"));
            if is_identity || is_computed || is_rowversion {
                continue;
            }
            if let Value::String(name) = &row[0] {
                cols.push(name.clone());
            }
        }
        Ok(cols)
    }
}

/// Interpret an `is_*` column-property value as a boolean. The MSSQL
/// driver maps the underlying INT result through `Value::Int64`,
/// `Value::Bool`, or (when the table is missing or unresolved)
/// `Value::Null`. Null is treated as `false` — for missing tables
/// the empty column list will fail downstream with a clearer error
/// than "destination has 0 non-IDENTITY columns".
fn column_flag_bool(v: &Value) -> bool {
    match v {
        Value::Bool(b) => *b,
        Value::Int64(n) => *n != 0,
        _ => false,
    }
}

/// A T-SQL identifier split into its optional schema and required
/// name halves. Both halves are *unquoted* — brackets stripped,
/// `]]` decoded to `]` — and ready to be passed to
/// `escape_mssql_string` for inclusion in an `INFORMATION_SCHEMA`
/// lookup.
#[derive(Debug, Clone, PartialEq, Eq)]
struct QualifiedIdentifier {
    schema: Option<String>,
    name: String,
}

/// Parse a target table reference in any of these forms:
///
/// - `test_users` — unqualified
/// - `dbo.test_users` — dot-qualified
/// - `[dbo].[test_users]` — T-SQL bracketed
/// - `[my.weird].[table]` — bracketed with embedded dot in the schema
/// - `dbo.[test_users]` / `[dbo].test_users` — mixed
///
/// Surrounding whitespace is trimmed. Within brackets, the T-SQL
/// escape `]]` decodes to a single `]` (matching `QUOTENAME`
/// semantics). If the bracket pair is unmatched, the input is
/// treated as a plain identifier — defensive: producing a "no
/// columns found" result with a clearer downstream error is better
/// than panicking here.
///
/// See #41 for the regression that motivated bracketed support.
fn parse_mssql_qualified_identifier(input: &str) -> QualifiedIdentifier {
    let trimmed = input.trim();
    let (first, rest) = parse_one_identifier(trimmed);
    match rest {
        Some(after_dot) => {
            // `<first>.<after_dot>` — schema-qualified.
            let (second, _) = parse_one_identifier(after_dot);
            QualifiedIdentifier {
                schema: Some(first),
                name: second,
            }
        }
        None => QualifiedIdentifier {
            schema: None,
            name: first,
        },
    }
}

/// Parse one identifier off the front of `s` and return it along
/// with the slice after a following `.` separator (or `None` if the
/// identifier exhausts the input).
///
/// Handles `[...]` quoting with `]]` as a literal `]`. Unbracketed
/// identifiers terminate at the first `.`.
fn parse_one_identifier(s: &str) -> (String, Option<&str>) {
    if let Some(after_open) = s.strip_prefix('[') {
        // Walk bytes looking for the closing `]`, treating `]]` as
        // an escaped literal `]`. ASCII `]` is a single byte and
        // can't appear inside a multi-byte UTF-8 codepoint, so a
        // byte-level scan is safe even on non-ASCII identifiers.
        let bytes = after_open.as_bytes();
        let mut i = 0;
        let mut close = None;
        while i < bytes.len() {
            if bytes[i] == b']' {
                if i + 1 < bytes.len() && bytes[i + 1] == b']' {
                    i += 2;
                    continue;
                }
                close = Some(i);
                break;
            }
            i += 1;
        }
        match close {
            Some(end) => {
                let inner = &after_open[..end];
                let unquoted = inner.replace("]]", "]");
                let after_close = &after_open[end + 1..];
                let rest = after_close.strip_prefix('.');
                (unquoted, rest)
            }
            None => {
                // Unmatched `[` — return the whole input as the
                // name (without the leading `[`). The downstream
                // pre-flight lookup will return zero rows and the
                // alignment check produces a clear diagnostic.
                (after_open.to_string(), None)
            }
        }
    } else {
        match s.find('.') {
            Some(i) => (s[..i].to_string(), Some(&s[i + 1..])),
            None => (s.to_string(), None),
        }
    }
}

/// Compare destination physical column order (already filtered to
/// non-IDENTITY) against `target.columns`. Names must match exactly,
/// case-insensitive (MSSQL identifiers default to case-insensitive
/// collation; the destination metadata may return original case but
/// the source may have produced any case). Returns
/// [`SqlError::BulkUnavailable`] on mismatch so the dispatcher
/// degrades to the generic INSERT path (which uses named column
/// lists and works regardless of physical order).
fn verify_bulk_column_alignment(
    dest_cols: &[String],
    target_cols: &[ColumnInfo],
) -> Result<(), SqlError> {
    if dest_cols.len() != target_cols.len() {
        return Err(SqlError::BulkUnavailable(format!(
            "MSSQL bulk path requires destination to have exactly the same \
             non-IDENTITY columns as the source ({} dest cols vs {} source cols). \
             The destination may have IDENTITY columns the source doesn't, or \
             columns the source doesn't write to — generic INSERT can handle \
             this with a named column list",
            dest_cols.len(),
            target_cols.len()
        )));
    }
    for (idx, (dest, src)) in dest_cols.iter().zip(target_cols).enumerate() {
        if !dest.eq_ignore_ascii_case(&src.name) {
            return Err(SqlError::BulkUnavailable(format!(
                "MSSQL bulk path requires destination column order to match source. \
                 Position {idx}: dest = {dest:?}, source = {src_name:?}. \
                 Generic INSERT uses a named column list and works regardless of order",
                src_name = src.name
            )));
        }
    }
    Ok(())
}

/// Classify a `tiberius::error::Error` raised by `client.bulk_insert`
/// setup. Returns [`SqlError::BulkUnavailable`] only for conditions
/// where falling back to a generic INSERT could *actually succeed* —
/// not for errors that would hit the generic path too (permission
/// denied, missing table). The `Auto` dispatcher's "fall back to
/// generic" only helps when the generic path is strictly more capable.
///
/// Currently bulk-recoverable on MSSQL:
/// - "Cannot bulk load" — target is a view or otherwise non-bulk-eligible.
///   Generic INSERT can still target views if INSTEAD OF triggers exist.
/// - Missing column metadata — the bulk handshake's
///   `SELECT TOP 0 *` returned no usable columns. Generic INSERT can
///   still hit a synonym or other indirection that bulk can't.
fn classify_bulk_setup_error(e: &tiberius::error::Error) -> SqlError {
    let msg = e.to_string();
    if msg.contains("Cannot bulk load") || msg.contains("expecting column metadata") {
        return SqlError::BulkUnavailable(format!("MSSQL rejected bulk_insert setup: {msg}"));
    }
    // "Invalid object name" / permission errors / network errors: the
    // generic INSERT path would fail with the same root cause, so
    // surface the error immediately rather than confusing the user
    // with a "fall back to --bulk-native=auto" hint.
    SqlError::QueryFailed(format!("MSSQL bulk_insert setup: {msg}"))
}

/// Translate a ferrule [`Value`] into tiberius `ColumnData<'static>`
/// for the bulk-load stream. The destination column's [`TypeHint`]
/// is used only to pick the right typed `None` for `Value::Null`;
/// for non-null values the variant of `Value` is authoritative.
///
/// This mirrors [`mssql_to_value`] (the read path) symmetrically.
fn value_to_column_data(v: &Value, hint: TypeHint) -> Result<ColumnData<'static>, SqlError> {
    use std::borrow::Cow;

    Ok(match v {
        Value::Null => null_for_hint(hint),
        Value::Bool(b) => ColumnData::Bit(Some(*b)),
        Value::Int64(n) => ColumnData::I64(Some(*n)),
        Value::Float64(f) => ColumnData::F64(Some(*f)),
        Value::Decimal(s) => {
            let n = parse_decimal_to_numeric(s)
                .map_err(|e| SqlError::QueryFailed(format!("MSSQL bulk: decimal {s:?}: {e}")))?;
            ColumnData::Numeric(Some(n))
        }
        Value::String(s) => ColumnData::String(Some(Cow::Owned(s.clone()))),
        Value::Bytes(b) => ColumnData::Binary(Some(Cow::Owned(b.clone()))),
        Value::Date(d) => (*d).into_sql(),
        Value::Time(t) => (*t).into_sql(),
        Value::DateTime(dt) => (*dt).into_sql(),
        Value::DateTimeTz(dt) => (*dt).into_sql(),
        Value::Json(j) => {
            // ferrule's DDL translator maps JSON → NVARCHAR(MAX) on
            // MSSQL — no native JSON column type. Serialize compact.
            let rendered = serde_json::to_string(j)
                .map_err(|e| SqlError::QueryFailed(format!("MSSQL bulk: JSON serialize: {e}")))?;
            ColumnData::String(Some(Cow::Owned(rendered)))
        }
        Value::Uuid(s) => {
            let u = tiberius::Uuid::parse_str(s)
                .map_err(|e| SqlError::QueryFailed(format!("MSSQL bulk: UUID {s:?}: {e}")))?;
            ColumnData::Guid(Some(u))
        }
        Value::Array(a) => {
            // DDL translator maps Array → NVARCHAR(MAX) (no native
            // array type). Serialize compact JSON.
            let rendered = serde_json::to_string(a)
                .map_err(|e| SqlError::QueryFailed(format!("MSSQL bulk: array serialize: {e}")))?;
            ColumnData::String(Some(Cow::Owned(rendered)))
        }
    })
}

/// Pick the typed `None` variant of `ColumnData` matching `hint`. The
/// destination column type drives the choice — sending the wrong
/// typed-NULL to a bulk column would fail with a TDS metadata
/// mismatch.
fn null_for_hint(hint: TypeHint) -> ColumnData<'static> {
    match hint {
        TypeHint::Bool => ColumnData::Bit(None),
        TypeHint::Int64 => ColumnData::I64(None),
        TypeHint::Float64 => ColumnData::F64(None),
        TypeHint::Decimal => ColumnData::Numeric(None),
        TypeHint::Bytes => ColumnData::Binary(None),
        TypeHint::Date => ColumnData::Date(None),
        TypeHint::Time => ColumnData::Time(None),
        TypeHint::DateTime => ColumnData::DateTime2(None),
        TypeHint::DateTimeTz => ColumnData::DateTimeOffset(None),
        TypeHint::Uuid => ColumnData::Guid(None),
        // NVARCHAR(MAX) is the catch-all on MSSQL — translate_type
        // sends Json/Array/String/Other/Null all here.
        _ => ColumnData::String(None),
    }
}

/// Parse a decimal string like `"99.5"` / `"-12.345"` / `"42"` into
/// a tiberius `Numeric { value: i128, scale: u8 }`. Rejects
/// scientific notation — we always render decimals as plain
/// `Display` form, so this only fails on malformed input.
fn parse_decimal_to_numeric(s: &str) -> Result<Numeric, String> {
    let trimmed = s.trim();
    if trimmed.is_empty() {
        return Err("empty string".into());
    }
    if trimmed.contains(['e', 'E']) {
        return Err("scientific notation not supported".into());
    }
    let (sign, rest) = match trimmed.as_bytes()[0] {
        b'-' => (-1i128, &trimmed[1..]),
        b'+' => (1i128, &trimmed[1..]),
        _ => (1i128, trimmed),
    };
    let (int_part, frac_part) = match rest.split_once('.') {
        Some((a, b)) => (a, b),
        None => (rest, ""),
    };
    if int_part.is_empty() && frac_part.is_empty() {
        return Err("no digits".into());
    }
    let mut digits = String::with_capacity(int_part.len() + frac_part.len());
    digits.push_str(int_part);
    digits.push_str(frac_part);
    if !digits.chars().all(|c| c.is_ascii_digit()) {
        return Err(format!("non-digit character in {s:?}"));
    }
    let raw: i128 = digits.parse().map_err(|e| format!("parse mantissa: {e}"))?;
    let scale: u8 = frac_part
        .len()
        .try_into()
        .map_err(|_| "scale exceeds u8".to_string())?;
    if scale >= 38 {
        return Err(format!("scale {scale} exceeds MSSQL max 37"));
    }
    Ok(Numeric::new_with_scale(sign * raw, scale))
}

pub(crate) async fn connect(
    url: &DatabaseUrl,
    opts: &ConnectOptions,
) -> Result<MssqlConnection, SqlError> {
    let mut config = tiberius::Config::new();
    config.host(url.host().unwrap_or("localhost"));
    config.port(url.port().unwrap_or(1433));

    if !url.username().is_empty() {
        // A caller-resolved secret takes precedence over the URL password.
        let password = opts
            .effective_password(url)
            .map(|p| p.expose_secret().to_string())
            .unwrap_or_default();
        config.authentication(tiberius::AuthMethod::sql_server(url.username(), password));
    }

    if !url.database().is_empty() {
        config.database(url.database());
    }

    if opts.insecure {
        config.trust_cert();
    }

    let params = url.params();
    if let Some(encrypt) = params.get("encrypt") {
        match encrypt.as_str() {
            "false" | "disable" | "off" => config.encryption(EncryptionLevel::Off),
            "true" | "on" | "require" => config.encryption(EncryptionLevel::Required),
            _ => {}
        }
    }
    if let Some(trust) = params
        .get("trust_server_certificate")
        .or_else(|| params.get("trustServerCertificate"))
        && (trust == "true" || trust == "yes" || trust == "1")
    {
        config.trust_cert();
    }

    let tcp = tokio::net::TcpStream::connect(config.get_addr())
        .await
        .map_err(|e| SqlError::ConnectionFailed(e.to_string()))?;
    tcp.set_nodelay(true)
        .map_err(|e| SqlError::ConnectionFailed(e.to_string()))?;

    let client = tiberius::Client::connect(config, tcp.compat_write())
        .await
        .map_err(|e| SqlError::ConnectionFailed(e.to_string()))?;

    Ok(MssqlConnection { client })
}

fn mssql_type_to_hint(col_type: ColumnType) -> TypeHint {
    match col_type {
        ColumnType::Bit | ColumnType::Bitn => TypeHint::Bool,
        ColumnType::Int1
        | ColumnType::Int2
        | ColumnType::Int4
        | ColumnType::Int8
        | ColumnType::Intn => TypeHint::Int64,
        ColumnType::Float4 | ColumnType::Float8 | ColumnType::Floatn => TypeHint::Float64,
        ColumnType::Decimaln | ColumnType::Numericn | ColumnType::Money | ColumnType::Money4 => {
            TypeHint::Decimal
        }
        ColumnType::BigVarChar
        | ColumnType::BigChar
        | ColumnType::NVarchar
        | ColumnType::NChar
        | ColumnType::Text
        | ColumnType::NText
        | ColumnType::Xml => TypeHint::String,
        ColumnType::BigVarBin | ColumnType::BigBinary | ColumnType::Image => TypeHint::Bytes,
        ColumnType::Datetime4
        | ColumnType::Datetime
        | ColumnType::Datetimen
        | ColumnType::Datetime2 => TypeHint::DateTime,
        ColumnType::Daten => TypeHint::Date,
        ColumnType::Timen => TypeHint::Time,
        ColumnType::DatetimeOffsetn => TypeHint::DateTimeTz,
        ColumnType::Guid => TypeHint::Uuid,
        ColumnType::Udt | ColumnType::SSVariant => TypeHint::Other,
        ColumnType::Null => TypeHint::Null,
    }
}

fn mssql_to_value(row: &tiberius::Row, idx: usize, col_type: ColumnType) -> Value {
    fn opt<T, E>(r: Result<Option<T>, E>) -> Option<T> {
        r.ok().flatten()
    }

    match col_type {
        ColumnType::Bit | ColumnType::Bitn => opt(row.try_get::<bool, _>(idx))
            .map(Value::Bool)
            .unwrap_or(Value::Null),
        ColumnType::Int1 => opt(row.try_get::<u8, _>(idx))
            .map(|v| Value::Int64(v as i64))
            .unwrap_or(Value::Null),
        ColumnType::Int2 => opt(row.try_get::<i16, _>(idx))
            .map(|v| Value::Int64(v as i64))
            .unwrap_or(Value::Null),
        ColumnType::Int4 => opt(row.try_get::<i32, _>(idx))
            .map(|v| Value::Int64(v as i64))
            .unwrap_or(Value::Null),
        ColumnType::Int8 => opt(row.try_get::<i64, _>(idx))
            .map(Value::Int64)
            .unwrap_or(Value::Null),
        ColumnType::Intn => opt(row.try_get::<i64, _>(idx))
            .map(Value::Int64)
            .or_else(|| opt(row.try_get::<i32, _>(idx)).map(|v| Value::Int64(v as i64)))
            .or_else(|| opt(row.try_get::<i16, _>(idx)).map(|v| Value::Int64(v as i64)))
            .or_else(|| opt(row.try_get::<u8, _>(idx)).map(|v| Value::Int64(v as i64)))
            .unwrap_or(Value::Null),
        ColumnType::Float4 => opt(row.try_get::<f32, _>(idx))
            .map(|v| Value::Float64(v as f64))
            .unwrap_or(Value::Null),
        ColumnType::Float8 => opt(row.try_get::<f64, _>(idx))
            .map(Value::Float64)
            .unwrap_or(Value::Null),
        ColumnType::Floatn => opt(row.try_get::<f64, _>(idx))
            .map(Value::Float64)
            .or_else(|| opt(row.try_get::<f32, _>(idx)).map(|v| Value::Float64(v as f64)))
            .unwrap_or(Value::Null),
        ColumnType::Money | ColumnType::Money4 => opt(row.try_get::<f64, _>(idx))
            .map(|v| Value::Decimal(format!("{:.4}", v)))
            .unwrap_or(Value::Null),
        ColumnType::Decimaln | ColumnType::Numericn => {
            opt(row.try_get::<tiberius::numeric::Numeric, _>(idx))
                .map(|v| Value::Decimal(v.to_string()))
                .unwrap_or(Value::Null)
        }
        ColumnType::BigVarChar
        | ColumnType::BigChar
        | ColumnType::NVarchar
        | ColumnType::NChar
        | ColumnType::Text
        | ColumnType::NText => opt(row.try_get::<&str, _>(idx))
            .map(|v| Value::String(v.to_string()))
            .unwrap_or(Value::Null),
        ColumnType::Xml => opt(row.try_get::<&tiberius::xml::XmlData, _>(idx))
            .map(|v| Value::String(v.to_string()))
            .unwrap_or(Value::Null),
        ColumnType::BigVarBin | ColumnType::BigBinary | ColumnType::Image => {
            opt(row.try_get::<&[u8], _>(idx))
                .map(|v| Value::Bytes(v.to_vec()))
                .unwrap_or(Value::Null)
        }
        ColumnType::Guid => opt(row.try_get::<tiberius::Uuid, _>(idx))
            .map(|v| Value::Uuid(v.to_string()))
            .unwrap_or(Value::Null),
        ColumnType::Datetime4
        | ColumnType::Datetime
        | ColumnType::Datetimen
        | ColumnType::Datetime2 => opt(row.try_get::<NaiveDateTime, _>(idx))
            .map(Value::DateTime)
            .unwrap_or(Value::Null),
        ColumnType::Daten => opt(row.try_get::<NaiveDate, _>(idx))
            .map(Value::Date)
            .unwrap_or(Value::Null),
        ColumnType::Timen => opt(row.try_get::<NaiveTime, _>(idx))
            .map(Value::Time)
            .unwrap_or(Value::Null),
        ColumnType::DatetimeOffsetn => opt(row.try_get::<ChronoDateTime<FixedOffset>, _>(idx))
            .map(|v| Value::DateTimeTz(v.with_timezone(&Utc)))
            .or_else(|| opt(row.try_get::<ChronoDateTime<Utc>, _>(idx)).map(Value::DateTimeTz))
            .unwrap_or(Value::Null),
        ColumnType::Udt | ColumnType::SSVariant => opt(row.try_get::<&str, _>(idx))
            .map(|v| Value::String(v.to_string()))
            .unwrap_or(Value::Null),
        ColumnType::Null => Value::Null,
    }
}

fn escape_mssql_string(s: &str) -> String {
    s.replace("'", "''")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::url::DatabaseUrl;

    const TEST_MSSQL_URL: &str =
        "mssql://sa:Ferrule123!@127.0.0.1:11433/ferrule?trustServerCertificate=true";

    fn try_connect() -> Option<Box<dyn crate::Connection>> {
        let url = DatabaseUrl::parse(TEST_MSSQL_URL).ok()?;
        let conn = crate::connect(&url, &ConnectOptions::default(), None).ok()?;
        Some(conn)
    }

    #[test]
    fn test_mssql_ping() {
        let Some(mut conn) = try_connect() else {
            eprintln!("MSSQL test container not available, skipping test_mssql_ping");
            return;
        };
        conn.ping().expect("ping should succeed");
    }

    #[test]
    fn test_mssql_query() {
        let Some(mut conn) = try_connect() else {
            eprintln!("MSSQL test container not available, skipping test_mssql_query");
            return;
        };
        let result = conn
            .query("SELECT * FROM test_users")
            .expect("query should succeed");
        assert!(!result.columns.is_empty(), "should have columns");
        assert!(!result.rows.is_empty(), "should have rows");
    }

    #[test]
    fn test_mssql_execute() {
        let Some(mut conn) = try_connect() else {
            eprintln!("MSSQL test container not available, skipping test_mssql_execute");
            return;
        };
        let summary = conn
            .execute("INSERT INTO test_users (name, age) VALUES ('TestUser', 99)")
            .expect("execute should succeed");
        assert!(
            summary.rows_affected.is_some_and(|n| n > 0),
            "should have affected rows"
        );
    }

    #[test]
    fn test_mssql_list_tables() {
        let Some(mut conn) = try_connect() else {
            eprintln!("MSSQL test container not available, skipping test_mssql_list_tables");
            return;
        };
        let tables = conn.list_tables(None).expect("list_tables should succeed");
        assert!(
            tables.contains(&"test_users".to_string()),
            "should contain test_users"
        );
    }

    #[test]
    fn test_mssql_list_schemas() {
        let Some(mut conn) = try_connect() else {
            eprintln!("MSSQL test container not available, skipping test_mssql_list_schemas");
            return;
        };
        let schemas = conn.list_schemas().expect("list_schemas should succeed");
        let dbo = schemas
            .iter()
            .find(|s| s.name == "dbo")
            .unwrap_or_else(|| panic!("should contain dbo, got: {schemas:?}"));
        assert!(dbo.is_default, "dbo should be the default schema");
    }

    #[test]
    fn test_mssql_describe_table() {
        let Some(mut conn) = try_connect() else {
            eprintln!("MSSQL test container not available, skipping test_mssql_describe_table");
            return;
        };
        let result = conn
            .describe_table(None, "test_users")
            .expect("describe_table should succeed");
        assert_eq!(result.columns.len(), 6, "should return 6 metadata columns");
        let col_names: Vec<String> = result.columns.iter().map(|c| c.name.clone()).collect();
        assert_eq!(
            col_names,
            vec![
                "column_name",
                "data_type",
                "is_nullable",
                "column_default",
                "numeric_precision",
                "numeric_scale"
            ]
        );
    }

    #[test]
    fn test_mssql_type_mapping() {
        let Some(mut conn) = try_connect() else {
            eprintln!("MSSQL test container not available, skipping test_mssql_type_mapping");
            return;
        };
        let result = conn
            .query("SELECT name, age, score, active, meta FROM test_users WHERE name = 'Alice'")
            .expect("query should succeed");
        assert_eq!(result.rows.len(), 1);
        let row = &result.rows[0];
        assert!(matches!(row[0], Value::String(_)), "name should be String");
        assert!(matches!(row[1], Value::Int64(_)), "age should be Int64");
        assert!(
            matches!(row[2], Value::Float64(_) | Value::Decimal(_)),
            "score should be Float64 or Decimal"
        );
        assert!(
            matches!(row[3], Value::Int64(_) | Value::Bool(_)),
            "active should be Int64 or Bool"
        );
        assert!(
            matches!(row[4], Value::Json(_) | Value::String(_)),
            "meta should be Json or String"
        );
    }

    // -------- value_to_column_data + parse_decimal_to_numeric unit tests --------

    #[test]
    fn parse_decimal_simple() {
        let n = parse_decimal_to_numeric("99.5").unwrap();
        assert_eq!(n.value(), 995);
        assert_eq!(n.scale(), 1);
    }

    #[test]
    fn parse_decimal_negative_with_explicit_plus() {
        let n = parse_decimal_to_numeric("-12.345").unwrap();
        assert_eq!(n.value(), -12345);
        assert_eq!(n.scale(), 3);
        let p = parse_decimal_to_numeric("+0.5").unwrap();
        assert_eq!(p.value(), 5);
        assert_eq!(p.scale(), 1);
    }

    #[test]
    fn parse_decimal_integer_has_zero_scale() {
        let n = parse_decimal_to_numeric("42").unwrap();
        assert_eq!(n.value(), 42);
        assert_eq!(n.scale(), 0);
    }

    // -------- C1: verify_bulk_column_alignment --------

    fn col(name: &str) -> ColumnInfo {
        ColumnInfo {
            name: name.to_string(),
            type_hint: TypeHint::String,
            nullable: true,
        }
    }

    #[test]
    fn verify_alignment_accepts_exact_match() {
        let dest = vec!["id".to_string(), "name".to_string(), "age".to_string()];
        let target = vec![col("id"), col("name"), col("age")];
        verify_bulk_column_alignment(&dest, &target).expect("matched columns should pass");
    }

    #[test]
    fn verify_alignment_is_case_insensitive() {
        // MSSQL identifiers default to case-insensitive collation —
        // the dest might come back as `ID` while the source rendered
        // `id`. Should not block.
        let dest = vec!["ID".to_string(), "Name".to_string()];
        let target = vec![col("id"), col("name")];
        verify_bulk_column_alignment(&dest, &target).expect("case-insensitive should pass");
    }

    #[test]
    fn verify_alignment_rejects_count_mismatch() {
        let dest = vec!["a".to_string(), "b".to_string()];
        let target = vec![col("a"), col("b"), col("c")];
        let err = verify_bulk_column_alignment(&dest, &target).expect_err("count mismatch");
        assert!(matches!(err, SqlError::BulkUnavailable(_)));
        let msg = err.to_string();
        assert!(
            msg.contains("2 dest cols") && msg.contains("3 source cols"),
            "useful diagnostic: {msg}"
        );
    }

    #[test]
    fn verify_alignment_rejects_order_mismatch() {
        // The silent-corruption hazard: same names, wrong order. The
        // bulk path would write `a`'s values into `b`'s column.
        let dest = vec!["b".to_string(), "a".to_string()];
        let target = vec![col("a"), col("b")];
        let err = verify_bulk_column_alignment(&dest, &target).expect_err("order mismatch");
        assert!(matches!(err, SqlError::BulkUnavailable(_)));
        let msg = err.to_string();
        assert!(
            msg.contains("Position 0") && msg.contains("\"b\"") && msg.contains("\"a\""),
            "useful diagnostic: {msg}"
        );
    }

    #[test]
    fn verify_alignment_rejects_extra_destination_columns() {
        // IDENTITY mismatch in the wild: dest has columns the source
        // doesn't write to. fetch_bulk_updatable_columns filters out
        // IDENTITY, but a regular extra column should still be caught.
        let dest = vec!["a".to_string(), "b".to_string(), "extra".to_string()];
        let target = vec![col("a"), col("b")];
        let err = verify_bulk_column_alignment(&dest, &target).expect_err("extra dest cols");
        assert!(matches!(err, SqlError::BulkUnavailable(_)));
    }

    // -------- #41: parse_mssql_qualified_identifier --------

    fn qual(schema: Option<&str>, name: &str) -> QualifiedIdentifier {
        QualifiedIdentifier {
            schema: schema.map(|s| s.to_string()),
            name: name.to_string(),
        }
    }

    #[test]
    fn parse_qualified_plain_unqualified() {
        assert_eq!(
            parse_mssql_qualified_identifier("test_users"),
            qual(None, "test_users")
        );
    }

    #[test]
    fn parse_qualified_dot_form() {
        assert_eq!(
            parse_mssql_qualified_identifier("dbo.test_users"),
            qual(Some("dbo"), "test_users")
        );
    }

    #[test]
    fn parse_qualified_bracketed_both_halves() {
        assert_eq!(
            parse_mssql_qualified_identifier("[dbo].[test_users]"),
            qual(Some("dbo"), "test_users")
        );
    }

    #[test]
    fn parse_qualified_bracketed_with_embedded_dot() {
        // The marquee #41 case: a schema name containing a dot is
        // valid SQL Server only when bracketed. Without bracket
        // parsing, `rsplit_once('.')` would have produced
        // `(Some("my.weird"), Some("[table]"))` — both halves bogus.
        assert_eq!(
            parse_mssql_qualified_identifier("[my.weird].[table]"),
            qual(Some("my.weird"), "table")
        );
    }

    #[test]
    fn parse_qualified_mixed_dot_and_brackets() {
        assert_eq!(
            parse_mssql_qualified_identifier("dbo.[test users]"),
            qual(Some("dbo"), "test users")
        );
        assert_eq!(
            parse_mssql_qualified_identifier("[dbo].test_users"),
            qual(Some("dbo"), "test_users")
        );
    }

    #[test]
    fn parse_qualified_unbracketed_with_space() {
        // Unbracketed identifiers terminate at the first `.`. This
        // is unconventional input ("[]"-less but with spaces); we
        // preserve it rather than rejecting — the downstream
        // INFORMATION_SCHEMA lookup will simply find no match.
        assert_eq!(
            parse_mssql_qualified_identifier("my table"),
            qual(None, "my table")
        );
    }

    #[test]
    fn parse_qualified_escaped_close_bracket() {
        // T-SQL escape: `]]` inside `[...]` decodes to a single `]`.
        // Matches `QUOTENAME` semantics.
        assert_eq!(
            parse_mssql_qualified_identifier("[wei]]rd].[table]"),
            qual(Some("wei]rd"), "table")
        );
    }

    #[test]
    fn parse_qualified_unmatched_bracket_is_defensive() {
        // Unmatched `[` is malformed; rather than panicking, return
        // the residual as the name and let the pre-flight lookup
        // produce its zero-columns diagnostic. The leading `[` is
        // consumed.
        assert_eq!(
            parse_mssql_qualified_identifier("[unfinished"),
            qual(None, "unfinished")
        );
    }

    #[test]
    fn parse_qualified_trims_surrounding_whitespace() {
        assert_eq!(
            parse_mssql_qualified_identifier("  dbo.test_users  "),
            qual(Some("dbo"), "test_users")
        );
    }

    // -------- C1 caveat fixes: column_flag_bool unit coverage --------
    //
    // The remaining caveat surface (Updateable filter for computed +
    // ROWVERSION columns, schema-qualified table-name split) is tied
    // to live SQL Server metadata — the integration test
    // `test_mssql_bulk_insert_rows_round_trip` exercises the happy
    // path with a real container; the unit coverage here pins down
    // the helper invariants the pre-flight relies on.

    #[test]
    fn column_flag_bool_handles_int_bool_null() {
        assert!(column_flag_bool(&Value::Bool(true)));
        assert!(!column_flag_bool(&Value::Bool(false)));
        assert!(column_flag_bool(&Value::Int64(1)));
        assert!(!column_flag_bool(&Value::Int64(0)));
        // COLUMNPROPERTY returns NULL for non-existent column/table;
        // treat as false so the column is *included*, deferring the
        // failure to the alignment check (which produces a clearer
        // "0 dest cols vs N source cols" diagnostic).
        assert!(!column_flag_bool(&Value::Null));
        // Unexpected variants default false (defensive).
        assert!(!column_flag_bool(&Value::String("yes".into())));
    }

    #[test]
    fn parse_decimal_rejects_scientific_notation() {
        assert!(parse_decimal_to_numeric("1.5e10").is_err());
        assert!(parse_decimal_to_numeric("1E5").is_err());
    }

    #[test]
    fn parse_decimal_rejects_malformed() {
        assert!(parse_decimal_to_numeric("").is_err());
        assert!(parse_decimal_to_numeric("abc").is_err());
        assert!(parse_decimal_to_numeric("1..5").is_err());
        assert!(parse_decimal_to_numeric(".").is_err());
    }

    #[test]
    fn value_to_column_data_handles_primitives() {
        assert!(matches!(
            value_to_column_data(&Value::Bool(true), TypeHint::Bool).unwrap(),
            ColumnData::Bit(Some(true))
        ));
        assert!(matches!(
            value_to_column_data(&Value::Int64(42), TypeHint::Int64).unwrap(),
            ColumnData::I64(Some(42))
        ));
        let f = value_to_column_data(&Value::Float64(1.5), TypeHint::Float64).unwrap();
        assert!(matches!(f, ColumnData::F64(Some(v)) if (v - 1.5).abs() < 1e-12));
    }

    #[test]
    fn value_to_column_data_decimal_routes_through_numeric() {
        let d = value_to_column_data(&Value::Decimal("12.34".into()), TypeHint::Decimal).unwrap();
        match d {
            ColumnData::Numeric(Some(n)) => {
                assert_eq!(n.value(), 1234);
                assert_eq!(n.scale(), 2);
            }
            other => panic!("expected Numeric, got {other:?}"),
        }
    }

    #[test]
    fn value_to_column_data_string_bytes_uuid() {
        match value_to_column_data(&Value::String("hi".into()), TypeHint::String).unwrap() {
            ColumnData::String(Some(s)) => assert_eq!(s.as_ref(), "hi"),
            other => panic!("expected String, got {other:?}"),
        }
        match value_to_column_data(&Value::Bytes(vec![1, 2, 3]), TypeHint::Bytes).unwrap() {
            ColumnData::Binary(Some(b)) => assert_eq!(b.as_ref(), &[1u8, 2, 3]),
            other => panic!("expected Binary, got {other:?}"),
        }
        match value_to_column_data(
            &Value::Uuid("550e8400-e29b-41d4-a716-446655440000".into()),
            TypeHint::Uuid,
        )
        .unwrap()
        {
            ColumnData::Guid(Some(u)) => {
                assert_eq!(u.to_string(), "550e8400-e29b-41d4-a716-446655440000");
            }
            other => panic!("expected Guid, got {other:?}"),
        }
    }

    #[test]
    fn value_to_column_data_json_and_array_serialize_as_nvarchar() {
        let j = serde_json::json!({"role": "admin"});
        match value_to_column_data(&Value::Json(j), TypeHint::Json).unwrap() {
            ColumnData::String(Some(s)) => {
                assert!(s.contains("\"role\":\"admin\""));
            }
            other => panic!("expected String for JSON, got {other:?}"),
        }
        let a = Value::Array(vec![Value::Int64(1), Value::Int64(2)]);
        match value_to_column_data(&a, TypeHint::Array).unwrap() {
            ColumnData::String(Some(s)) => assert_eq!(s.as_ref(), "[1,2]"),
            other => panic!("expected String for Array, got {other:?}"),
        }
    }

    #[test]
    fn value_to_column_data_null_picks_typed_none() {
        // Each TypeHint should produce its matching typed-None variant.
        assert!(matches!(
            value_to_column_data(&Value::Null, TypeHint::Bool).unwrap(),
            ColumnData::Bit(None)
        ));
        assert!(matches!(
            value_to_column_data(&Value::Null, TypeHint::Int64).unwrap(),
            ColumnData::I64(None)
        ));
        assert!(matches!(
            value_to_column_data(&Value::Null, TypeHint::Decimal).unwrap(),
            ColumnData::Numeric(None)
        ));
        assert!(matches!(
            value_to_column_data(&Value::Null, TypeHint::Bytes).unwrap(),
            ColumnData::Binary(None)
        ));
        assert!(matches!(
            value_to_column_data(&Value::Null, TypeHint::DateTimeTz).unwrap(),
            ColumnData::DateTimeOffset(None)
        ));
        assert!(matches!(
            value_to_column_data(&Value::Null, TypeHint::Uuid).unwrap(),
            ColumnData::Guid(None)
        ));
        // String/Json/Array/Other all map to NVARCHAR(MAX) → ColumnData::String(None).
        assert!(matches!(
            value_to_column_data(&Value::Null, TypeHint::Json).unwrap(),
            ColumnData::String(None)
        ));
        assert!(matches!(
            value_to_column_data(&Value::Null, TypeHint::Other).unwrap(),
            ColumnData::String(None)
        ));
    }

    // -------- bulk_insert_rows end-to-end (skip on absent container) --------

    /// Round-trip a scratch table via the bulk path. Verifies that
    /// tiberius's `bulk_insert` integration with `TokenRow` + our
    /// value→ColumnData translation produces readable rows on the
    /// destination. Uses a per-pid table so concurrent test runs
    /// don't collide.
    #[test]
    fn test_mssql_bulk_insert_rows_round_trip() {
        let Some(mut conn) = try_connect() else {
            eprintln!(
                "MSSQL test container not available, skipping test_mssql_bulk_insert_rows_round_trip"
            );
            return;
        };

        let pid = std::process::id();
        let table = format!("ferrule_bulk_test_{pid}");
        let _ = conn.execute(&format!(
            "IF OBJECT_ID('{table}', 'U') IS NOT NULL DROP TABLE {table}"
        ));
        conn.execute(&format!(
            "CREATE TABLE {table} (\
               id BIGINT NOT NULL, \
               name NVARCHAR(255) NULL, \
               active BIT NULL, \
               score DECIMAL(10,2) NULL, \
               meta NVARCHAR(MAX) NULL, \
               uid UNIQUEIDENTIFIER NULL\
             )"
        ))
        .expect("CREATE TABLE");

        let columns = vec![
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
            ColumnInfo {
                name: "active".into(),
                type_hint: TypeHint::Bool,
                nullable: true,
            },
            ColumnInfo {
                name: "score".into(),
                type_hint: TypeHint::Decimal,
                nullable: true,
            },
            ColumnInfo {
                name: "meta".into(),
                type_hint: TypeHint::Json,
                nullable: true,
            },
            ColumnInfo {
                name: "uid".into(),
                type_hint: TypeHint::Uuid,
                nullable: true,
            },
        ];

        let rows: Vec<Row> = vec![
            vec![
                Value::Int64(1),
                Value::String("Alice".into()),
                Value::Bool(true),
                Value::Decimal("99.50".into()),
                Value::Json(serde_json::json!({"role": "admin"})),
                Value::Uuid("550e8400-e29b-41d4-a716-446655440000".into()),
            ],
            vec![
                Value::Int64(2),
                Value::String("Bob".into()),
                Value::Bool(false),
                Value::Decimal("-7.25".into()),
                Value::Json(serde_json::json!({"role": "user"})),
                Value::Null,
            ],
            vec![
                Value::Int64(3),
                Value::Null,
                Value::Null,
                Value::Null,
                Value::Null,
                Value::Null,
            ],
        ];

        let n = conn
            .bulk_insert_rows(BulkInsert {
                table: &table,
                columns: &columns,
                rows: &rows,
                copy_format: crate::copy::CopyFormat::Text,
            })
            .expect("bulk_insert_rows");
        assert_eq!(n, 3);

        // Round-trip read.
        let result = conn
            .query(&format!(
                "SELECT id, name, active, score, meta, uid FROM {table} ORDER BY id"
            ))
            .expect("read-back query");
        assert_eq!(result.rows.len(), 3);

        // Row 1 — verify the decimal landed at the expected value.
        if let Value::Decimal(s) = &result.rows[0][3] {
            assert!(
                s.starts_with("99.5"),
                "row 1 score should be ~99.50, got {s:?}"
            );
        } else if let Value::Float64(f) = result.rows[0][3] {
            assert!((f - 99.5).abs() < 1e-6, "row 1 score got {f}");
        } else {
            panic!(
                "row 1 score should be Decimal or Float64, got {:?}",
                result.rows[0][3]
            );
        }

        // Row 2 — all of name/score/active populated; uid NULL.
        assert!(matches!(&result.rows[1][5], Value::Null));

        // Row 3 — everything NULL except id.
        assert!(matches!(&result.rows[2][1], Value::Null));
        assert!(matches!(&result.rows[2][2], Value::Null));
        assert!(matches!(&result.rows[2][3], Value::Null));

        // Cleanup.
        conn.execute(&format!("DROP TABLE {table}"))
            .expect("DROP TABLE");
    }

    #[test]
    fn test_mssql_primary_key() {
        let Some(mut conn) = try_connect() else {
            eprintln!("MSSQL test container not available, skipping test_mssql_primary_key");
            return;
        };
        let pk = conn.primary_key(None, "test_users").expect("primary_key");
        assert_eq!(pk, vec!["id".to_string()]);
    }

    #[test]
    fn test_mssql_list_foreign_keys() {
        let Some(mut conn) = try_connect() else {
            eprintln!("MSSQL test container not available, skipping test_mssql_list_foreign_keys");
            return;
        };
        let pid = std::process::id();
        let child = format!("ferrule_fk_test_orders_{pid}");
        let _ = conn.execute(&format!("DROP TABLE IF EXISTS {child}"));
        conn.execute(&format!(
            "CREATE TABLE {child} (\
               id INT IDENTITY(1,1) PRIMARY KEY, \
               user_id INT FOREIGN KEY REFERENCES test_users(id) ON DELETE CASCADE\
             )"
        ))
        .expect("CREATE TABLE");

        let fks = conn.list_foreign_keys(None).expect("list_foreign_keys");
        let matching: Vec<_> = fks.iter().filter(|fk| fk.child_table == child).collect();
        assert_eq!(matching.len(), 1, "expected 1 FK from {child}, got {fks:?}");
        let fk = matching[0];
        assert_eq!(fk.child_columns, vec!["user_id".to_string()]);
        assert_eq!(fk.parent_table, "test_users");
        assert_eq!(fk.parent_columns, vec!["id".to_string()]);
        assert_eq!(fk.on_delete.as_deref(), Some("CASCADE"));

        let _ = conn.execute(&format!("DROP TABLE {child}"));
    }

    /// End-to-end `--if-exists skip` then `upsert` round-trip against
    /// MSSQL. Exercises the `MERGE … WHEN NOT MATCHED` (Skip) and full
    /// `MERGE` (Upsert) code paths against a real TDS server.
    #[test]
    fn test_mssql_copy_skip_then_upsert() {
        use crate::backend::Backend;
        use crate::copy::{CopyOptions, CopySource, IfExists, copy_rows};

        let (Some(mut src), Some(mut dst)) = (try_connect(), try_connect()) else {
            eprintln!(
                "MSSQL test container not available, skipping test_mssql_copy_skip_then_upsert"
            );
            return;
        };

        let pid = std::process::id();
        let src_table = format!("ferrule_ms_skip_src_{pid}");
        let dst_table = format!("ferrule_ms_skip_dst_{pid}");
        let _ = src.execute(&format!("DROP TABLE IF EXISTS {src_table}"));
        let _ = dst.execute(&format!("DROP TABLE IF EXISTS {dst_table}"));
        src.execute(&format!(
            "CREATE TABLE {src_table} (id INT PRIMARY KEY, name NVARCHAR(64), val INT)"
        ))
        .expect("CREATE src");
        dst.execute(&format!(
            "CREATE TABLE {dst_table} (id INT PRIMARY KEY, name NVARCHAR(64), val INT)"
        ))
        .expect("CREATE dst");
        src.execute(&format!(
            "INSERT INTO {src_table} VALUES (1, 'new-1', 10), (2, 'new-2', 20)"
        ))
        .expect("seed src");
        dst.execute(&format!("INSERT INTO {dst_table} VALUES (1, 'old-1', 99)"))
            .expect("seed dst");

        // --- Skip ---------------------------------------------------------
        let opts = CopyOptions {
            source: CopySource::Query {
                sql: format!("SELECT * FROM {src_table} ORDER BY id"),
                into: dst_table.clone(),
            },
            if_exists: IfExists::Skip,
            ..Default::default()
        };
        copy_rows(&mut src, Backend::MsSql, &mut dst, Backend::MsSql, &opts)
            .expect("copy_rows skip");

        let out = dst
            .query(&format!(
                "SELECT id, name, val FROM {dst_table} ORDER BY id"
            ))
            .expect("verify skip");
        assert_eq!(out.rows.len(), 2);
        assert!(matches!(&out.rows[0][1], Value::String(s) if s == "old-1"));
        assert!(matches!(&out.rows[1][1], Value::String(s) if s == "new-2"));

        // --- Upsert -------------------------------------------------------
        let opts = CopyOptions {
            source: CopySource::Query {
                sql: format!("SELECT * FROM {src_table} ORDER BY id"),
                into: dst_table.clone(),
            },
            if_exists: IfExists::Upsert,
            ..Default::default()
        };
        copy_rows(&mut src, Backend::MsSql, &mut dst, Backend::MsSql, &opts)
            .expect("copy_rows upsert");

        let out = dst
            .query(&format!(
                "SELECT id, name, val FROM {dst_table} ORDER BY id"
            ))
            .expect("verify upsert");
        assert_eq!(out.rows.len(), 2);
        assert!(matches!(&out.rows[0][1], Value::String(s) if s == "new-1"));
        assert!(matches!(&out.rows[0][2], Value::Int64(10)));
        assert!(matches!(&out.rows[1][1], Value::String(s) if s == "new-2"));

        let _ = src.execute(&format!("DROP TABLE {src_table}"));
        let _ = dst.execute(&format!("DROP TABLE {dst_table}"));
    }
}
