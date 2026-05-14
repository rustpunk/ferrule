use crate::connection::{
    BulkInsert, ConnectOptions, Connection, ExecutionSummary, QueryResult, StatementResult,
};
use crate::error::CoreError;
use crate::url::DatabaseUrl;
use crate::value::{ColumnInfo, TypeHint, Value};
use async_trait::async_trait;
use chrono::{DateTime, NaiveDate, NaiveDateTime, Utc};
use oracle::sql_type::ToSql;
use secrecy::ExposeSecret;
use std::sync::Arc;

#[derive(Debug)]
pub struct OracleConnection {
    conn: Arc<oracle::Connection>,
}

#[async_trait]
impl Connection for OracleConnection {
    async fn execute(&mut self, sql: &str) -> Result<ExecutionSummary, CoreError> {
        let sql = sql.to_string();
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || {
            let stmt = conn
                .execute(&sql, &[])
                .map_err(|e| CoreError::QueryFailed(e.to_string()))?;
            let row_count = stmt
                .row_count()
                .map_err(|e| CoreError::QueryFailed(e.to_string()))?;
            Ok(ExecutionSummary {
                rows_affected: Some(row_count),
                command_tag: None,
            })
        })
        .await
        .map_err(|e| CoreError::QueryFailed(e.to_string()))?
    }

    async fn query(&mut self, sql: &str) -> Result<QueryResult, CoreError> {
        let sql = sql.to_string();
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || {
            let result_set = conn
                .query(&sql, &[])
                .map_err(|e| CoreError::QueryFailed(e.to_string()))?;

            let col_info: Vec<ColumnInfo> = result_set
                .column_info()
                .iter()
                .map(|c| ColumnInfo {
                    name: c.name().to_string(),
                    type_hint: oracle_type_to_hint(c.oracle_type()),
                    nullable: c.nullable(),
                })
                .collect();

            let mut rows = Vec::new();
            for row_result in result_set {
                let row = row_result.map_err(|e| CoreError::QueryFailed(e.to_string()))?;
                let values: Vec<Value> = row
                    .sql_values()
                    .iter()
                    .enumerate()
                    .map(|(i, sql_val)| {
                        oracle_to_value(sql_val, row.column_info()[i].oracle_type())
                    })
                    .collect();
                rows.push(values);
            }

            Ok(QueryResult {
                columns: col_info,
                rows,
            })
        })
        .await
        .map_err(|e| CoreError::QueryFailed(e.to_string()))?
    }

    async fn execute_multi(&mut self, sql: &str) -> Result<Vec<StatementResult>, CoreError> {
        let statements =
            split_oracle_statements(sql).map_err(|e| CoreError::QueryFailed(e.to_string()))?;
        let mut results = Vec::with_capacity(statements.len());
        for stmt in statements {
            let stmt = stmt.trim();
            if stmt.is_empty() {
                continue;
            }
            match self.query(stmt).await {
                Ok(result) => results.push(StatementResult::Query(result)),
                Err(CoreError::QueryFailed(_)) => {
                    let summary = self.execute(stmt).await?;
                    results.push(StatementResult::Summary(summary));
                }
                Err(e) => return Err(e),
            }
        }
        Ok(results)
    }

    async fn ping(&mut self) -> Result<(), CoreError> {
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || {
            conn.ping()
                .map_err(|e| CoreError::ConnectionFailed(e.to_string()))
        })
        .await
        .map_err(|e| CoreError::ConnectionFailed(e.to_string()))?
    }

    async fn list_tables(&mut self, schema: Option<&str>) -> Result<Vec<String>, CoreError> {
        let sql = match schema {
            Some(s) => format!(
                "SELECT table_name FROM all_tables WHERE owner = '{}' ORDER BY table_name",
                escape_oracle_string(s)
            ),
            None => "SELECT table_name FROM user_tables ORDER BY table_name".to_string(),
        };
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

    async fn describe_table(
        &mut self,
        schema: Option<&str>,
        table: &str,
    ) -> Result<QueryResult, CoreError> {
        let sql = match schema {
            Some(s) => format!(
                "SELECT column_name, data_type, nullable, data_default, data_precision, data_scale \
                 FROM all_tab_columns \
                 WHERE owner = UPPER('{}') AND table_name = UPPER('{}') \
                 ORDER BY column_id",
                escape_oracle_string(s),
                escape_oracle_string(table),
            ),
            None => format!(
                "SELECT column_name, data_type, nullable, data_default, data_precision, data_scale \
                 FROM user_tab_columns \
                 WHERE table_name = UPPER('{}') \
                 ORDER BY column_id",
                escape_oracle_string(table),
            ),
        };
        self.query(&sql).await
    }

    async fn bulk_insert_rows(
        &mut self,
        target: BulkInsert<'_>,
    ) -> Result<usize, CoreError> {
        if target.rows.is_empty() {
            return Ok(0);
        }
        let row_count = target.rows.len();

        // Build the parameterized INSERT once: `:1, :2, ...`. Bind
        // by position, not by name — matches our positional `Row`
        // shape and avoids per-column name negotiation.
        let qtable =
            crate::copy::quote_identifier(target.table, crate::backend::Backend::Oracle);
        let cols = target
            .columns
            .iter()
            .map(|c| crate::copy::quote_identifier(&c.name, crate::backend::Backend::Oracle))
            .collect::<Vec<_>>()
            .join(", ");
        let placeholders = (1..=target.columns.len())
            .map(|i| format!(":{i}"))
            .collect::<Vec<_>>()
            .join(", ");
        let sql = format!("INSERT INTO {qtable} ({cols}) VALUES ({placeholders})");

        // Translate every row into owned bindings up front. Keeping
        // ownership outside the spawn_blocking closure keeps the
        // closure cheap and the error-mapping legible.
        let hints: Vec<TypeHint> = target.columns.iter().map(|c| c.type_hint).collect();
        let mut owned_rows: Vec<Vec<OwnedBind>> = Vec::with_capacity(row_count);
        for row in target.rows {
            let mut bound = Vec::with_capacity(row.len());
            for (idx, v) in row.iter().enumerate() {
                let hint = hints.get(idx).copied().unwrap_or(TypeHint::Other);
                bound.push(value_to_oracle_bind(v, hint)?);
            }
            owned_rows.push(bound);
        }

        // ODPI-C / `oracle::Batch` is sync — push the entire batch
        // build + append + execute loop into spawn_blocking. The
        // Arc clone goes in; the Batch borrows the Connection only
        // for the closure's lifetime.
        let conn_arc = self.conn.clone();
        tokio::task::spawn_blocking(move || {
            let mut batch = conn_arc
                .batch(&sql, row_count)
                .with_row_counts()
                .build()
                .map_err(map_oracle_bulk_error)?;
            for row in &owned_rows {
                let binds: Vec<&dyn ToSql> = row.iter().map(|b| b.as_to_sql()).collect();
                batch
                    .append_row(&binds)
                    .map_err(map_oracle_bulk_error)?;
            }
            // Always flush at the end. `append_row` auto-executes
            // when `batch_index == batch_size`, but a leading
            // execute() is a no-op on an empty batch, so this is
            // idempotent and safer than relying on auto-execute.
            batch.execute().map_err(map_oracle_bulk_error)?;
            Ok::<usize, CoreError>(row_count)
        })
        .await
        .map_err(|e| CoreError::QueryFailed(e.to_string()))?
    }
}

/// Classify an `oracle::Error` raised by the `Batch` path.
///
/// `BulkUnavailable` is only used when the generic
/// `INSERT ALL ... SELECT 1 FROM DUAL` path could *actually* succeed
/// where bulk failed — i.e., when the failure is specific to the
/// array-DML protocol shape and not to schema/privilege concerns the
/// generic path would also hit. Today no such Oracle-specific
/// recoverable error class is known, so every failure surfaces as
/// `QueryFailed` immediately. (Notably, ORA-01031 "insufficient
/// privilege" and ORA-00942 "table or view does not exist" were
/// previously flagged as `BulkUnavailable`, but the generic path
/// fails identically on both — the suggested fallback was misleading.)
///
/// ORA-26026 / ORA-12838 from direct-path inserts are reserved for
/// the future `--oracle-direct-path` opt-in (#37), where falling
/// back to safe array-DML is the right behaviour.
fn map_oracle_bulk_error(e: oracle::Error) -> CoreError {
    let msg = e.to_string();
    // dpi_code is typed `Option<i32>`. ODPI-C 1047 is the
    // Instant-Client-not-loaded structural error (see the connect
    // path at `map_oracle_error`). In practice this can't fire here
    // because we'd have failed at connect time, but defend in depth.
    if let Some(code) = e.dpi_code() {
        if code == 1047 || msg.contains("libclntsh") {
            return CoreError::ConnectionFailed(format!(
                "Oracle Instant Client not loaded: {msg}"
            ));
        }
    }
    CoreError::QueryFailed(format!("Oracle bulk: {msg}"))
}

/// Owned, typed binding for a single column slot in one row of an
/// `oracle::Batch`. Each variant holds an `Option<T>` because
/// `Option<T>: ToSql` is how the oracle crate represents typed NULL
/// — the destination column's [`TypeHint`] selects the variant for
/// `Value::Null` so the bind metadata stays stable across rows.
enum OwnedBind {
    /// `i64` — used for `Value::Int64` and `Value::Bool` (ferrule's
    /// Oracle DDL maps Bool → `NUMBER(1)`, so 0/1 round-trips
    /// cleanly through an `i64` bind).
    I64(Option<i64>),
    /// `f64` — used for `Value::Float64`. NaN/Inf pass through
    /// because ferrule's Oracle DDL maps `Float64 → BINARY_DOUBLE`,
    /// which natively accepts both values via the IEEE-754 bit
    /// pattern. (Note: this diverges from the MySQL bulk path, which
    /// substitutes NULL because MySQL `DOUBLE` literals have no
    /// NaN/Inf representation in LOAD DATA.)
    F64(Option<f64>),
    /// Text — used for `Value::String`, `Value::Decimal` (Oracle
    /// auto-coerces string → NUMBER), `Value::Json`, `Value::Array`,
    /// and `Value::Time` (rendered as `HH:MM:SS` for CLOB or
    /// auto-coerce to TIMESTAMP).
    Text(Option<String>),
    /// Raw bytes — used for `Value::Bytes` (BLOB) and `Value::Uuid`
    /// (16-byte RAW after hex parsing).
    Bytes(Option<Vec<u8>>),
    /// `NaiveDate` — used for `Value::Date` (DATE column).
    Date(Option<NaiveDate>),
    /// `NaiveDateTime` — used for `Value::DateTime` and `Value::Time`
    /// (the latter paired with an epoch date for TIMESTAMP).
    DateTime(Option<NaiveDateTime>),
    /// `DateTime<Utc>` — used for `Value::DateTimeTz` (TIMESTAMP WITH TIME ZONE).
    DateTimeTz(Option<DateTime<Utc>>),
}

impl OwnedBind {
    fn as_to_sql(&self) -> &dyn ToSql {
        match self {
            Self::I64(v) => v,
            Self::F64(v) => v,
            Self::Text(v) => v,
            Self::Bytes(v) => v,
            Self::Date(v) => v,
            Self::DateTime(v) => v,
            Self::DateTimeTz(v) => v,
        }
    }
}

/// Translate one `Value` (paired with the destination column's
/// [`TypeHint`]) into an [`OwnedBind`] ready to feed an
/// `oracle::Batch`. The hint matters for [`Value::Null`] (it picks
/// the typed-NULL variant so per-column bind metadata stays stable
/// across rows) and for [`Value::Time`] (Oracle has no TIME type;
/// we pair the time with an epoch date for TIMESTAMP coercion).
fn value_to_oracle_bind(v: &Value, hint: TypeHint) -> Result<OwnedBind, CoreError> {
    Ok(match v {
        Value::Null => null_bind_for_hint(hint),
        Value::Bool(b) => OwnedBind::I64(Some(if *b { 1 } else { 0 })),
        Value::Int64(n) => OwnedBind::I64(Some(*n)),
        Value::Float64(f) => {
            // Oracle BINARY_DOUBLE accepts NaN/Inf but NUMBER does
            // not, and our DDL maps Float64 → BINARY_DOUBLE — so
            // the bind succeeds either way. Pass through.
            OwnedBind::F64(Some(*f))
        }
        // Decimal: rely on Oracle's implicit string → NUMBER
        // conversion. Matches the generic INSERT path's behaviour
        // (`Value::Decimal(s)` is inlined as the bare numeric
        // string).
        Value::Decimal(s) => OwnedBind::Text(Some(s.clone())),
        Value::String(s) => OwnedBind::Text(Some(s.clone())),
        Value::Bytes(b) => OwnedBind::Bytes(Some(b.clone())),
        Value::Date(d) => OwnedBind::Date(Some(*d)),
        Value::Time(t) => {
            // Oracle has no TIME-only type; our DDL maps TypeHint::Time
            // → TIMESTAMP. Pair the time with the Unix epoch date so
            // the TIMESTAMP receives the time-of-day portion intact.
            // Same pattern as MySQL: ferrule already returns Time only
            // for backends with a true TIME type, so the date is a
            // sentinel the user wouldn't otherwise see.
            let epoch = NaiveDate::from_ymd_opt(1970, 1, 1).unwrap();
            OwnedBind::DateTime(Some(NaiveDateTime::new(epoch, *t)))
        }
        Value::DateTime(dt) => OwnedBind::DateTime(Some(*dt)),
        Value::DateTimeTz(dt) => OwnedBind::DateTimeTz(Some(*dt)),
        Value::Json(j) => {
            let rendered = serde_json::to_string(j).map_err(|e| {
                CoreError::QueryFailed(format!("Oracle bulk: JSON serialize: {e}"))
            })?;
            OwnedBind::Text(Some(rendered))
        }
        Value::Uuid(s) => {
            // Oracle DDL maps Uuid → RAW(16). Parse the canonical
            // hex form into 16 bytes; reject malformed input as a
            // hard error (QueryFailed) — same outcome as the
            // generic INSERT path would have hit.
            let parsed = parse_uuid_hex(s).map_err(|e| {
                CoreError::QueryFailed(format!("Oracle bulk: UUID {s:?}: {e}"))
            })?;
            OwnedBind::Bytes(Some(parsed))
        }
        Value::Array(a) => {
            // Oracle DDL maps Array → CLOB; serialize compact JSON.
            let rendered = serde_json::to_string(a).map_err(|e| {
                CoreError::QueryFailed(format!("Oracle bulk: array serialize: {e}"))
            })?;
            OwnedBind::Text(Some(rendered))
        }
    })
}

/// Typed `None` for a given column hint. Selecting the right
/// variant ensures every row in the batch binds the same Oracle
/// type for a given column, even if some rows have `Value::Null`
/// at that position. Mismatched bind types would surface as
/// ORA-01036 ("illegal variable name/number") or similar.
fn null_bind_for_hint(hint: TypeHint) -> OwnedBind {
    match hint {
        TypeHint::Bool | TypeHint::Int64 => OwnedBind::I64(None),
        TypeHint::Float64 => OwnedBind::F64(None),
        TypeHint::Bytes | TypeHint::Uuid => OwnedBind::Bytes(None),
        TypeHint::Date => OwnedBind::Date(None),
        TypeHint::Time | TypeHint::DateTime => OwnedBind::DateTime(None),
        TypeHint::DateTimeTz => OwnedBind::DateTimeTz(None),
        // Decimal / String / Json / Array / Other / Null all bind
        // as text — the DDL translator maps these to NUMBER (for
        // Decimal) or CLOB / NVARCHAR (for the rest), and Oracle
        // implicitly converts a NVARCHAR2 bind into either.
        _ => OwnedBind::Text(None),
    }
}

/// Parse a UUID hex string into 16 raw bytes for binding into an
/// Oracle `RAW(16)` column. Accepts the canonical 8-4-4-4-12 dashed
/// form, the 32-character undashed form, an optional `urn:uuid:`
/// prefix (RFC 4122 §3, case-insensitive per the RFC), and Microsoft /
/// .NET curly-brace form `{...}`. Hex digits are case-insensitive
/// (`u8::from_str_radix` accepts a-f and A-F). Surrounding whitespace
/// is tolerated.
fn parse_uuid_hex(s: &str) -> Result<Vec<u8>, String> {
    let trimmed = s.trim();
    // RFC 4122 §3: "The prefix 'urn:uuid:' is case insensitive".
    // Match any casing — `urn:uuid:`, `URN:UUID:`, `Urn:Uuid:`,
    // `uRn:UuId:`, etc.
    const URN_PREFIX: &str = "urn:uuid:";
    let stripped_urn = if trimmed.len() >= URN_PREFIX.len()
        && trimmed.is_char_boundary(URN_PREFIX.len())
        && trimmed[..URN_PREFIX.len()].eq_ignore_ascii_case(URN_PREFIX)
    {
        // Per #42 — tolerate whitespace between the URN prefix and
        // the hex digits. Real-world sources (JDBC `toString` on
        // some drivers, hand-pasted examples from RFC docs) often
        // include `urn:uuid:  550e8400-...`. trim_start matches
        // every other production UUID parser's leniency.
        trimmed[URN_PREFIX.len()..].trim_start()
    } else {
        trimmed
    };
    // .NET / Microsoft curly-brace form: `{550e8400-...}`. Both braces
    // must be present; one without the other is a malformed input.
    let stripped_braces = if let Some(inner) = stripped_urn.strip_prefix('{') {
        inner
            .strip_suffix('}')
            .ok_or_else(|| "leading `{` without matching `}`".to_string())?
    } else if stripped_urn.starts_with('}') || stripped_urn.ends_with('}') {
        return Err("unmatched `}` in UUID".to_string());
    } else {
        stripped_urn
    };
    let stripped: String = stripped_braces.chars().filter(|c| *c != '-').collect();
    if stripped.len() != 32 {
        return Err(format!(
            "expected 32 hex characters (after stripping dashes / prefix / braces), got {}",
            stripped.len()
        ));
    }
    let mut out = Vec::with_capacity(16);
    for chunk in stripped.as_bytes().chunks(2) {
        let pair = std::str::from_utf8(chunk).map_err(|e| e.to_string())?;
        out.push(u8::from_str_radix(pair, 16).map_err(|e| e.to_string())?);
    }
    Ok(out)
}

pub async fn connect(
    url: &DatabaseUrl,
    _opts: &ConnectOptions,
) -> Result<OracleConnection, CoreError> {
    let host = url.host().unwrap_or("localhost").to_string();
    let port = url.port().unwrap_or(1521);
    let username = url.username().to_string();
    let password = url
        .password()
        .map(|p| p.expose_secret().to_string())
        .unwrap_or_default();
    let service = url.database().to_string();

    let connect_string = if service.is_empty() {
        format!("{}:{}", host, port)
    } else {
        format!("//{}:{}/{}", host, port, service)
    };

    tokio::task::spawn_blocking(move || {
        let conn = oracle::Connection::connect(&username, &password, &connect_string)
            .map_err(map_oracle_error)?;
        Ok(OracleConnection {
            conn: Arc::new(conn),
        })
    })
    .await
    .map_err(|e| CoreError::ConnectionFailed(e.to_string()))?
}

/// Split a SQL string into individual statements by `;`.
/// Ignores semicolons inside:
///   - single-quoted strings (`''` escape)
///   - `--` line comments and `/* */` block comments
///   - PL/SQL blocks (`BEGIN … END`, `DECLARE … BEGIN … END`) and
///     nested control structures (`IF … END IF`, `LOOP … END LOOP`,
///     `CASE … END CASE` or `CASE … END`).
fn split_oracle_statements(sql: &str) -> Result<Vec<&str>, String> {
    let mut statements = Vec::new();
    let mut start = 0usize;
    let mut i = 0usize;
    let bytes = sql.as_bytes();
    let mut block_depth = 0usize;
    let mut case_depth = 0usize;
    let mut loop_depth = 0usize;

    while i < bytes.len() {
        match bytes[i] {
            b'\'' => {
                i += 1;
                while i < bytes.len() {
                    if bytes[i] == b'\'' {
                        if i + 1 < bytes.len() && bytes[i + 1] == b'\'' {
                            i += 2;
                        } else {
                            i += 1;
                            break;
                        }
                    } else {
                        i += 1;
                    }
                }
            }
            b'-' if i + 1 < bytes.len() && bytes[i + 1] == b'-' => {
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
            }
            b'/' if i + 1 < bytes.len() && bytes[i + 1] == b'*' => {
                i += 2;
                while i + 1 < bytes.len() {
                    if bytes[i] == b'*' && bytes[i + 1] == b'/' {
                        i += 2;
                        break;
                    }
                    i += 1;
                }
            }
            b';' if block_depth == 0 && case_depth == 0 && loop_depth == 0 => {
                let candidate = &sql[start..=i];
                let trimmed = candidate.trim();
                if !trimmed.is_empty() && trimmed != ";" {
                    statements.push(trimmed);
                }
                i += 1;
                start = i;
            }
            _ => {
                if matches_keyword(bytes, i, "begin") || matches_keyword(bytes, i, "declare") {
                    block_depth += 1;
                    i += if matches_keyword(bytes, i, "begin") {
                        5
                    } else {
                        7
                    };
                } else if matches_keyword(bytes, i, "case") {
                    case_depth += 1;
                    i += 4;
                } else if matches_keyword(bytes, i, "loop") {
                    loop_depth += 1;
                    i += 4;
                } else if matches_keyword(bytes, i, "end") {
                    match end_suffix(bytes, i) {
                        Some("case") => {
                            case_depth = case_depth.saturating_sub(1);
                            i += 3; // skip "END"
                            while i < bytes.len() && bytes[i].is_ascii_whitespace() {
                                i += 1;
                            }
                            i += keyword_len(bytes, i); // skip "CASE"
                        }
                        Some("if") | Some("loop") => {
                            // END IF / END LOOP do not affect tracked depths
                            i += 3; // skip "END"
                            while i < bytes.len() && bytes[i].is_ascii_whitespace() {
                                i += 1;
                            }
                            i += keyword_len(bytes, i); // skip suffix
                        }
                        _ => {
                            if case_depth > 0 {
                                case_depth -= 1;
                            } else if loop_depth > 0 {
                                loop_depth -= 1;
                            } else {
                                block_depth = block_depth.saturating_sub(1);
                            }
                            i += 3;
                        }
                    }
                } else {
                    i += 1;
                }
            }
        }
    }

    if start < sql.len() {
        let tail = &sql[start..];
        if !tail.trim().is_empty() {
            statements.push(tail.trim_end());
        }
    }

    Ok(statements)
}

/// Case-insensitive keyword match with word-boundary guards.
fn matches_keyword(bytes: &[u8], at: usize, keyword: &str) -> bool {
    let klen = keyword.len();
    if at + klen > bytes.len() {
        return false;
    }
    for (i, b) in keyword.bytes().enumerate() {
        if bytes[at + i].to_ascii_lowercase() != b {
            return false;
        }
    }
    // Preceding boundary
    if at > 0 {
        let prev = bytes[at - 1];
        if prev.is_ascii_alphanumeric() || prev == b'_' {
            return false;
        }
    }
    // Following boundary
    if at + klen < bytes.len() {
        let next = bytes[at + klen];
        if next.is_ascii_alphanumeric() || next == b'_' {
            return false;
        }
    }
    true
}

/// Length of the word starting at `at`.
fn keyword_len(bytes: &[u8], at: usize) -> usize {
    let mut j = at;
    while j < bytes.len() && (bytes[j].is_ascii_alphanumeric() || bytes[j] == b'_') {
        j += 1;
    }
    j - at
}

/// After a bare `END`, peek the next non-whitespace token.
fn end_suffix(bytes: &[u8], end_pos: usize) -> Option<&'static str> {
    let mut j = end_pos + 3;
    while j < bytes.len() && bytes[j].is_ascii_whitespace() {
        j += 1;
    }
    for kw in ["if", "loop", "case"] {
        if matches_keyword(bytes, j, kw) {
            return Some(kw);
        }
    }
    None
}

fn map_oracle_error(e: oracle::Error) -> CoreError {
    let msg = e.to_string();
    if e.dpi_code() == Some(1047) || msg.contains("libclntsh") {
        CoreError::ConnectionFailed(format!(
            "Oracle Instant Client not found. Install it from \
             https://www.oracle.com/database/technologies/instant-client/downloads.html \
             and ensure it is on your LD_LIBRARY_PATH (Linux), DYLD_LIBRARY_PATH (macOS), \
             or PATH (Windows). Original error: {}",
            msg
        ))
    } else {
        CoreError::ConnectionFailed(msg)
    }
}

fn oracle_type_to_hint(ora_type: &oracle::sql_type::OracleType) -> TypeHint {
    use oracle::sql_type::OracleType;
    match ora_type {
        OracleType::Number(_, 0) => TypeHint::Int64,
        OracleType::Number(_, _) | OracleType::Float(_) => TypeHint::Decimal,
        OracleType::BinaryFloat | OracleType::BinaryDouble => TypeHint::Float64,
        OracleType::Int64 => TypeHint::Int64,
        OracleType::Varchar2(_)
        | OracleType::NVarchar2(_)
        | OracleType::Char(_)
        | OracleType::NChar(_)
        | OracleType::CLOB
        | OracleType::NCLOB
        | OracleType::Long
        | OracleType::Rowid
        | OracleType::Xml => TypeHint::String,
        OracleType::BLOB | OracleType::BFILE | OracleType::Raw(_) | OracleType::LongRaw => {
            TypeHint::Bytes
        }
        OracleType::Date | OracleType::Timestamp(_) => TypeHint::DateTime,
        OracleType::TimestampTZ(_) | OracleType::TimestampLTZ(_) => TypeHint::DateTimeTz,
        OracleType::Boolean => TypeHint::Bool,
        OracleType::Json => TypeHint::Json,
        _ => TypeHint::Other,
    }
}

fn oracle_to_value(sql_val: &oracle::SqlValue, ora_type: &oracle::sql_type::OracleType) -> Value {
    use oracle::sql_type::OracleType;
    match ora_type {
        OracleType::Number(_, 0) => {
            if let Ok(Some(v)) = sql_val.get::<Option<i64>>() {
                Value::Int64(v)
            } else if let Ok(Some(v)) = sql_val.get::<Option<String>>() {
                Value::Decimal(v)
            } else {
                Value::Null
            }
        }
        OracleType::Number(_, _) | OracleType::Float(_) => {
            if let Ok(Some(v)) = sql_val.get::<Option<String>>() {
                Value::Decimal(v)
            } else {
                Value::Null
            }
        }
        OracleType::BinaryFloat | OracleType::BinaryDouble => sql_val
            .get::<Option<f64>>()
            .unwrap_or(None)
            .map(Value::Float64)
            .unwrap_or(Value::Null),
        OracleType::Int64 => sql_val
            .get::<Option<i64>>()
            .unwrap_or(None)
            .map(Value::Int64)
            .unwrap_or(Value::Null),
        OracleType::Varchar2(_)
        | OracleType::NVarchar2(_)
        | OracleType::Char(_)
        | OracleType::NChar(_)
        | OracleType::CLOB
        | OracleType::NCLOB
        | OracleType::Long
        | OracleType::Rowid
        | OracleType::Xml => sql_val
            .get::<Option<String>>()
            .unwrap_or(None)
            .map(Value::String)
            .unwrap_or(Value::Null),
        OracleType::BLOB | OracleType::BFILE | OracleType::Raw(_) | OracleType::LongRaw => sql_val
            .get::<Option<Vec<u8>>>()
            .unwrap_or(None)
            .map(Value::Bytes)
            .unwrap_or(Value::Null),
        OracleType::Date | OracleType::Timestamp(_) => sql_val
            .get::<Option<chrono::NaiveDateTime>>()
            .unwrap_or(None)
            .map(Value::DateTime)
            .unwrap_or(Value::Null),
        OracleType::TimestampTZ(_) | OracleType::TimestampLTZ(_) => sql_val
            .get::<Option<chrono::DateTime<chrono::Utc>>>()
            .unwrap_or(None)
            .map(Value::DateTimeTz)
            .unwrap_or(Value::Null),
        OracleType::Boolean => sql_val
            .get::<Option<bool>>()
            .unwrap_or(None)
            .map(Value::Bool)
            .unwrap_or(Value::Null),
        OracleType::Json => {
            if let Ok(Some(s)) = sql_val.get::<Option<String>>() {
                serde_json::from_str(&s)
                    .map(Value::Json)
                    .unwrap_or(Value::String(s))
            } else {
                Value::Null
            }
        }
        _ => sql_val
            .get::<Option<String>>()
            .unwrap_or(None)
            .map(Value::String)
            .unwrap_or(Value::Null),
    }
}

fn escape_oracle_string(s: &str) -> String {
    s.replace("'", "''")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::url::DatabaseUrl;
    use chrono::NaiveTime;

    async fn try_connect() -> Option<OracleConnection> {
        let raw = std::env::var("ORACLE_TEST_URL").ok()?;
        let url = DatabaseUrl::parse(&raw).ok()?;
        let conn = connect(&url, &ConnectOptions::default()).await.ok()?;
        Some(conn)
    }

    #[tokio::test]
    async fn test_oracle_connect() {
        let Some(_conn) = try_connect().await else {
            eprintln!("ORACLE_TEST_URL not set or unreachable; skipping test_oracle_connect");
            return;
        };
        println!("Oracle connection established successfully");
    }

    #[tokio::test]
    async fn test_oracle_ping() {
        let Some(mut conn) = try_connect().await else {
            eprintln!("ORACLE_TEST_URL not set or unreachable; skipping test_oracle_ping");
            return;
        };
        conn.ping().await.expect("ping should succeed");
    }

    #[tokio::test]
    async fn test_oracle_query() {
        let Some(mut conn) = try_connect().await else {
            eprintln!("ORACLE_TEST_URL not set or unreachable; skipping test_oracle_query");
            return;
        };
        let result = conn
            .query("SELECT * FROM test_users")
            .await
            .expect("query should succeed");
        assert!(!result.columns.is_empty(), "should have columns");
        assert!(!result.rows.is_empty(), "should have rows");
    }

    #[tokio::test]
    async fn test_oracle_execute() {
        let Some(mut conn) = try_connect().await else {
            eprintln!("ORACLE_TEST_URL not set or unreachable; skipping test_oracle_execute");
            return;
        };
        let summary = conn
            .execute("INSERT INTO test_users (name, age) VALUES ('TestUser', 99)")
            .await
            .expect("execute should succeed");
        assert!(
            summary.rows_affected.is_some_and(|n| n > 0),
            "should have affected rows"
        );
    }

    #[tokio::test]
    async fn test_oracle_list_tables() {
        let Some(mut conn) = try_connect().await else {
            eprintln!("ORACLE_TEST_URL not set or unreachable; skipping test_oracle_list_tables");
            return;
        };
        let tables = conn
            .list_tables(None)
            .await
            .expect("list_tables should succeed");
        assert!(
            tables.iter().any(|t| t.eq_ignore_ascii_case("test_users")),
            "should contain test_users (got: {:?})",
            tables
        );
    }

    #[tokio::test]
    async fn test_oracle_describe_table() {
        let Some(mut conn) = try_connect().await else {
            eprintln!(
                "ORACLE_TEST_URL not set or unreachable; skipping test_oracle_describe_table"
            );
            return;
        };
        let result = conn
            .describe_table(None, "test_users")
            .await
            .expect("describe_table should succeed");
        assert_eq!(result.columns.len(), 6, "should return 6 metadata columns");
        // Oracle column names from data dictionary are uppercase by default.
        // Just verify the count — exact names depend on Oracle metadata casing.
        assert!(!result.columns.is_empty(), "should have describe columns");
    }

    #[tokio::test]
    async fn test_oracle_type_mapping() {
        let Some(mut conn) = try_connect().await else {
            eprintln!("ORACLE_TEST_URL not set or unreachable; skipping test_oracle_type_mapping");
            return;
        };
        let result = conn
            .query("SELECT name, age, score, active, meta FROM test_users WHERE name = 'Alice'")
            .await
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

    #[tokio::test]
    async fn test_oracle_missing_client_error() {
        if std::env::var("ORACLE_TEST_URL").is_ok() {
            eprintln!(
                "ORACLE_TEST_URL is set; skipping test_oracle_missing_client_error to avoid \
                 conflict with live environment"
            );
            return;
        }
        // If libclntsh.so is present on the system (even broken/extracted DB-home libs),
        // ODPI-C init may segfault instead of returning a clean error. Only attempt this
        // test when no Oracle client library is visible to the dynamic linker.
        let lib_present = std::process::Command::new("ldconfig")
            .args(["-p"])
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).contains("libclntsh.so"))
            .unwrap_or(false);
        if lib_present {
            eprintln!(
                "Oracle client library (libclntsh.so) is present on this system; \
                 skipping test_oracle_missing_client_error because ODPI-C init may segfault \
                 with broken/extracted DB-home libraries."
            );
            return;
        }
        let url = DatabaseUrl::parse("oracle://user:pass@127.0.0.1:1521/XEPDB1").unwrap();
        let result = connect(&url, &ConnectOptions::default()).await;
        assert!(
            result.is_err(),
            "should fail when Instant Client is missing"
        );
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("Oracle Instant Client not found")
                || err.contains("DPI-1047")
                || err.contains("connection failed"),
            "error should mention missing client or connection failure: {err}"
        );
    }

    // ── split_oracle_statements unit tests (no DB required) ─────────────────────

    #[test]
    fn test_split_begin_end() {
        let stmts = split_oracle_statements("BEGIN NULL; END;").unwrap();
        assert_eq!(stmts.len(), 1);
        assert_eq!(stmts[0], "BEGIN NULL; END;");
    }

    #[test]
    fn test_split_declare_begin_end() {
        let stmts = split_oracle_statements("DECLARE x INT; BEGIN NULL; END;").unwrap();
        assert_eq!(stmts.len(), 1);
        assert_eq!(stmts[0], "DECLARE x INT; BEGIN NULL; END;");
    }

    #[test]
    fn test_split_nested_begin() {
        let stmts = split_oracle_statements("BEGIN BEGIN NULL; END; END;").unwrap();
        assert_eq!(stmts.len(), 1);
        assert_eq!(stmts[0], "BEGIN BEGIN NULL; END; END;");
    }

    #[test]
    fn test_split_end_if_not_block_end() {
        let stmts = split_oracle_statements("BEGIN IF TRUE THEN NULL; END IF; END;").unwrap();
        assert_eq!(stmts.len(), 1);
        assert_eq!(stmts[0], "BEGIN IF TRUE THEN NULL; END IF; END;");
    }

    #[test]
    fn test_split_end_loop_not_block_end() {
        let stmts = split_oracle_statements("BEGIN LOOP NULL; END LOOP; END;").unwrap();
        assert_eq!(stmts.len(), 1);
        assert_eq!(stmts[0], "BEGIN LOOP NULL; END LOOP; END;");
    }

    #[test]
    fn test_split_end_case_not_block_end() {
        let stmts =
            split_oracle_statements("BEGIN CASE WHEN 1=1 THEN NULL; END CASE; END;").unwrap();
        assert_eq!(stmts.len(), 1);
        assert_eq!(stmts[0], "BEGIN CASE WHEN 1=1 THEN NULL; END CASE; END;");
    }

    #[test]
    fn test_split_case_expr_bare_end() {
        let stmts = split_oracle_statements("BEGIN x := CASE WHEN 1=1 THEN 1 END; END;").unwrap();
        assert_eq!(stmts.len(), 1);
        assert_eq!(stmts[0], "BEGIN x := CASE WHEN 1=1 THEN 1 END; END;");
    }

    #[test]
    fn test_split_case_insensitive() {
        let stmts = split_oracle_statements("begin null; end;").unwrap();
        assert_eq!(stmts.len(), 1);
        assert_eq!(stmts[0], "begin null; end;");
    }

    #[test]
    fn test_split_string_ignores_keywords() {
        let stmts = split_oracle_statements("SELECT 'BEGIN END CASE LOOP' FROM DUAL;").unwrap();
        assert_eq!(stmts.len(), 1);
        assert_eq!(stmts[0], "SELECT 'BEGIN END CASE LOOP' FROM DUAL;");
    }

    #[test]
    fn test_split_comment_ignores_keywords() {
        let stmts = split_oracle_statements("/* BEGIN END CASE */ SELECT 1;").unwrap();
        assert_eq!(stmts.len(), 1);
        assert_eq!(stmts[0], "/* BEGIN END CASE */ SELECT 1;");
    }

    #[test]
    fn test_split_multiple_statements() {
        let stmts = split_oracle_statements("BEGIN NULL; END; SELECT 1;").unwrap();
        assert_eq!(stmts.len(), 2);
        assert_eq!(stmts[0], "BEGIN NULL; END;");
        assert_eq!(stmts[1], "SELECT 1;");
    }

    #[test]
    fn test_split_trailing_no_semicolon() {
        // A semicolon is required between statements; without one the tail is
        // treated as a continuation of the current statement.
        let stmts = split_oracle_statements("BEGIN NULL; END\n SELECT 1").unwrap();
        assert_eq!(stmts.len(), 1);
        assert_eq!(stmts[0], "BEGIN NULL; END\n SELECT 1");
    }

    #[test]
    fn test_split_empty_and_whitespace() {
        let stmts = split_oracle_statements("  ;  ;  BEGIN NULL; END;  ;  ").unwrap();
        assert_eq!(stmts.len(), 1);
        assert_eq!(stmts[0], "BEGIN NULL; END;");
    }

    #[test]
    fn test_split_deeply_nested_case() {
        let sql = "BEGIN CASE WHEN 1=1 THEN CASE WHEN 2=2 THEN 2 END; END CASE; END;";
        let stmts = split_oracle_statements(sql).unwrap();
        assert_eq!(stmts.len(), 1);
        assert_eq!(stmts[0], sql);
    }

    #[test]
    fn test_split_mixed_block_and_dml() {
        let stmts =
            split_oracle_statements("BEGIN INSERT INTO t VALUES (1); END; COMMIT;").unwrap();
        assert_eq!(stmts.len(), 2);
        assert_eq!(stmts[0], "BEGIN INSERT INTO t VALUES (1); END;");
        assert_eq!(stmts[1], "COMMIT;");
    }

    // -------- value_to_oracle_bind + parse_uuid_hex unit tests --------

    #[test]
    fn parse_uuid_with_dashes_round_trips() {
        let bytes =
            parse_uuid_hex("550e8400-e29b-41d4-a716-446655440000").expect("parse");
        assert_eq!(
            bytes,
            vec![
                0x55, 0x0e, 0x84, 0x00, 0xe2, 0x9b, 0x41, 0xd4, 0xa7, 0x16, 0x44, 0x66,
                0x55, 0x44, 0x00, 0x00
            ]
        );
    }

    #[test]
    fn parse_uuid_without_dashes_works() {
        let bytes =
            parse_uuid_hex("550e8400e29b41d4a716446655440000").expect("parse");
        assert_eq!(bytes.len(), 16);
    }

    #[test]
    fn parse_uuid_rejects_short_or_invalid() {
        assert!(parse_uuid_hex("550e8400").is_err());
        assert!(parse_uuid_hex("ZZZe8400-e29b-41d4-a716-446655440000").is_err());
    }

    // -------- L2: parse_uuid_hex accepts more real-world forms --------

    #[test]
    fn parse_uuid_accepts_urn_prefix() {
        // RFC 4122 §3 — UUIDs from URN-aware sources arrive prefixed.
        let canonical =
            parse_uuid_hex("urn:uuid:550e8400-e29b-41d4-a716-446655440000").expect("parse");
        assert_eq!(canonical.len(), 16);
        // RFC 4122 §3: "The prefix 'urn:uuid:' is case insensitive".
        // Every casing of the prefix MUST decode to the same bytes.
        for prefix in &[
            "urn:uuid:",
            "URN:UUID:",
            "Urn:Uuid:",
            "uRn:UuId:",
            "URN:uuid:",
            "urn:UUID:",
        ] {
            let s = format!("{prefix}550e8400-e29b-41d4-a716-446655440000");
            let parsed =
                parse_uuid_hex(&s).unwrap_or_else(|e| panic!("prefix {prefix:?} should parse: {e}"));
            assert_eq!(parsed, canonical, "prefix {prefix:?} mismatch");
        }
    }

    #[test]
    fn parse_uuid_accepts_curly_brace_form() {
        // Microsoft / .NET registry form: `{550e8400-...}`.
        let bytes =
            parse_uuid_hex("{550e8400-e29b-41d4-a716-446655440000}").expect("parse");
        assert_eq!(bytes.len(), 16);
        // Also accepts braces around the no-dash form.
        let bytes2 = parse_uuid_hex("{550e8400e29b41d4a716446655440000}").expect("parse");
        assert_eq!(bytes, bytes2);
    }

    #[test]
    fn parse_uuid_accepts_uppercase_hex() {
        let bytes =
            parse_uuid_hex("550E8400-E29B-41D4-A716-446655440000").expect("parse");
        let lower =
            parse_uuid_hex("550e8400-e29b-41d4-a716-446655440000").expect("parse");
        assert_eq!(bytes, lower);
    }

    #[test]
    fn parse_uuid_trims_surrounding_whitespace() {
        let bytes =
            parse_uuid_hex("  550e8400-e29b-41d4-a716-446655440000\t\n").expect("parse");
        assert_eq!(bytes.len(), 16);
    }

    #[test]
    fn parse_uuid_tolerates_whitespace_after_urn_prefix() {
        // Per #42 — real-world sources (some JDBC drivers, hand-pasted
        // examples) emit `urn:uuid:  <hex>`. Trim post-prefix
        // whitespace so these decode to the same bytes as the
        // canonical form.
        let canonical =
            parse_uuid_hex("urn:uuid:550e8400-e29b-41d4-a716-446655440000").expect("canonical");
        for shape in &[
            "urn:uuid: 550e8400-e29b-41d4-a716-446655440000",
            "urn:uuid:  550e8400-e29b-41d4-a716-446655440000",
            "urn:uuid:\t550e8400-e29b-41d4-a716-446655440000",
            "urn:uuid: \t 550e8400-e29b-41d4-a716-446655440000",
            // Mixed-case prefix + post-prefix whitespace.
            "URN:UUID:  550e8400-e29b-41d4-a716-446655440000",
            "Urn:Uuid:\t550e8400-e29b-41d4-a716-446655440000",
        ] {
            let parsed =
                parse_uuid_hex(shape).unwrap_or_else(|e| panic!("shape {shape:?}: {e}"));
            assert_eq!(parsed, canonical, "shape {shape:?} should round-trip");
        }
    }

    #[test]
    fn parse_uuid_rejects_unmatched_braces() {
        assert!(parse_uuid_hex("{550e8400-e29b-41d4-a716-446655440000").is_err());
        assert!(parse_uuid_hex("550e8400-e29b-41d4-a716-446655440000}").is_err());
    }

    #[test]
    fn bind_int_and_bool_share_i64_variant() {
        assert!(matches!(
            value_to_oracle_bind(&Value::Int64(42), TypeHint::Int64).unwrap(),
            OwnedBind::I64(Some(42))
        ));
        assert!(matches!(
            value_to_oracle_bind(&Value::Bool(true), TypeHint::Bool).unwrap(),
            OwnedBind::I64(Some(1))
        ));
        assert!(matches!(
            value_to_oracle_bind(&Value::Bool(false), TypeHint::Bool).unwrap(),
            OwnedBind::I64(Some(0))
        ));
    }

    #[test]
    fn bind_float_passes_through() {
        let b = value_to_oracle_bind(&Value::Float64(1.5), TypeHint::Float64).unwrap();
        assert!(matches!(b, OwnedBind::F64(Some(v)) if (v - 1.5).abs() < 1e-12));
    }

    #[test]
    fn bind_string_decimal_json_array_all_route_through_text() {
        assert!(matches!(
            value_to_oracle_bind(&Value::String("hi".into()), TypeHint::String).unwrap(),
            OwnedBind::Text(Some(_))
        ));
        match value_to_oracle_bind(&Value::Decimal("99.5".into()), TypeHint::Decimal).unwrap() {
            OwnedBind::Text(Some(s)) => assert_eq!(s, "99.5"),
            other => panic!("expected Text, got {other:?}", other = match other {
                OwnedBind::I64(_) => "I64",
                OwnedBind::F64(_) => "F64",
                OwnedBind::Text(_) => "Text",
                OwnedBind::Bytes(_) => "Bytes",
                OwnedBind::Date(_) => "Date",
                OwnedBind::DateTime(_) => "DateTime",
                OwnedBind::DateTimeTz(_) => "DateTimeTz",
            }),
        }
        match value_to_oracle_bind(
            &Value::Json(serde_json::json!({"role": "admin"})),
            TypeHint::Json,
        )
        .unwrap()
        {
            OwnedBind::Text(Some(s)) => assert!(s.contains("\"role\":\"admin\"")),
            _ => panic!("expected Text for JSON"),
        }
        match value_to_oracle_bind(
            &Value::Array(vec![Value::Int64(1), Value::Int64(2)]),
            TypeHint::Array,
        )
        .unwrap()
        {
            OwnedBind::Text(Some(s)) => assert_eq!(s, "[1,2]"),
            _ => panic!("expected Text for Array"),
        }
    }

    #[test]
    fn bind_bytes_passes_through() {
        match value_to_oracle_bind(&Value::Bytes(vec![1, 2, 3]), TypeHint::Bytes).unwrap() {
            OwnedBind::Bytes(Some(b)) => assert_eq!(b, vec![1, 2, 3]),
            _ => panic!("expected Bytes"),
        }
    }

    #[test]
    fn bind_uuid_converts_to_raw_16() {
        match value_to_oracle_bind(
            &Value::Uuid("550e8400-e29b-41d4-a716-446655440000".into()),
            TypeHint::Uuid,
        )
        .unwrap()
        {
            OwnedBind::Bytes(Some(b)) => assert_eq!(b.len(), 16),
            _ => panic!("expected Bytes for Uuid"),
        }
    }

    #[test]
    fn bind_time_pairs_with_epoch_date() {
        let t = NaiveTime::from_hms_opt(12, 34, 56).unwrap();
        match value_to_oracle_bind(&Value::Time(t), TypeHint::Time).unwrap() {
            OwnedBind::DateTime(Some(dt)) => {
                assert_eq!(dt.date(), NaiveDate::from_ymd_opt(1970, 1, 1).unwrap());
                assert_eq!(dt.time(), t);
            }
            _ => panic!("expected DateTime for Time"),
        }
    }

    #[test]
    fn bind_null_uses_typed_none_per_hint() {
        // The hint drives the typed-NULL variant — sending the wrong
        // typed NULL across rows would surface as a bind-type mismatch.
        assert!(matches!(
            value_to_oracle_bind(&Value::Null, TypeHint::Int64).unwrap(),
            OwnedBind::I64(None)
        ));
        assert!(matches!(
            value_to_oracle_bind(&Value::Null, TypeHint::Bool).unwrap(),
            OwnedBind::I64(None)
        ));
        assert!(matches!(
            value_to_oracle_bind(&Value::Null, TypeHint::Float64).unwrap(),
            OwnedBind::F64(None)
        ));
        assert!(matches!(
            value_to_oracle_bind(&Value::Null, TypeHint::Bytes).unwrap(),
            OwnedBind::Bytes(None)
        ));
        assert!(matches!(
            value_to_oracle_bind(&Value::Null, TypeHint::Uuid).unwrap(),
            OwnedBind::Bytes(None)
        ));
        assert!(matches!(
            value_to_oracle_bind(&Value::Null, TypeHint::Date).unwrap(),
            OwnedBind::Date(None)
        ));
        assert!(matches!(
            value_to_oracle_bind(&Value::Null, TypeHint::DateTime).unwrap(),
            OwnedBind::DateTime(None)
        ));
        assert!(matches!(
            value_to_oracle_bind(&Value::Null, TypeHint::DateTimeTz).unwrap(),
            OwnedBind::DateTimeTz(None)
        ));
        // Decimal / String / Json / Array / Other / Null all go to
        // Text — NUMBER and CLOB columns accept string binds via
        // implicit conversion.
        assert!(matches!(
            value_to_oracle_bind(&Value::Null, TypeHint::Decimal).unwrap(),
            OwnedBind::Text(None)
        ));
        assert!(matches!(
            value_to_oracle_bind(&Value::Null, TypeHint::Json).unwrap(),
            OwnedBind::Text(None)
        ));
        assert!(matches!(
            value_to_oracle_bind(&Value::Null, TypeHint::Other).unwrap(),
            OwnedBind::Text(None)
        ));
    }

    // -------- bulk_insert_rows end-to-end (skip on absent container) --------

    /// Round-trip a scratch table via the Oracle bulk path. Skips
    /// when ORACLE_TEST_URL is not set or unreachable. Uses a
    /// per-pid table so concurrent runs do not collide.
    #[tokio::test]
    async fn test_oracle_bulk_insert_rows_round_trip() {
        let Some(mut conn) = try_connect().await else {
            eprintln!(
                "ORACLE_TEST_URL not set or unreachable; skipping test_oracle_bulk_insert_rows_round_trip"
            );
            return;
        };

        let pid = std::process::id();
        let table = format!("ferrule_bulk_test_{pid}");
        let _ = conn
            .execute(&format!("DROP TABLE {table}"))
            .await;
        conn.execute(&format!(
            "CREATE TABLE {table} (\
               id NUMBER(19) NOT NULL, \
               name VARCHAR2(255) NULL, \
               active NUMBER(1) NULL, \
               score NUMBER(10,2) NULL, \
               meta CLOB NULL, \
               guid RAW(16) NULL\
             )"
        ))
        .await
        .expect("CREATE TABLE");

        let columns = vec![
            ColumnInfo { name: "id".into(), type_hint: TypeHint::Int64, nullable: false },
            ColumnInfo { name: "name".into(), type_hint: TypeHint::String, nullable: true },
            ColumnInfo { name: "active".into(), type_hint: TypeHint::Bool, nullable: true },
            ColumnInfo { name: "score".into(), type_hint: TypeHint::Decimal, nullable: true },
            ColumnInfo { name: "meta".into(), type_hint: TypeHint::Json, nullable: true },
            ColumnInfo { name: "guid".into(), type_hint: TypeHint::Uuid, nullable: true },
        ];

        let rows: Vec<crate::value::Row> = vec![
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
            })
            .await
            .expect("bulk_insert_rows");
        assert_eq!(n, 3);

        // Oracle: commit before the round-trip read since the
        // generic INSERT path doesn't auto-commit either. Inline test
        // simulates what copy.rs's outer transaction does for
        // --atomic; without it, the rows would not be visible.
        conn.execute("COMMIT").await.expect("COMMIT");

        let result = conn
            .query(&format!(
                "SELECT id, name, active, score, guid FROM {table} ORDER BY id"
            ))
            .await
            .expect("read-back query");
        assert_eq!(result.rows.len(), 3);

        // Row 1: id=1, name=Alice. score may come back as Decimal
        // or Float64 depending on whether the column metadata triggers
        // the Decimal path.
        if let Value::String(s) = &result.rows[0][1] {
            assert_eq!(s, "Alice");
        } else {
            panic!("row 1 name should be String, got {:?}", result.rows[0][1]);
        }

        // Row 2 — guid NULL.
        assert!(matches!(&result.rows[1][4], Value::Null));

        // Row 3 — everything NULL except id.
        assert!(matches!(&result.rows[2][1], Value::Null));
        assert!(matches!(&result.rows[2][3], Value::Null));
        assert!(matches!(&result.rows[2][4], Value::Null));

        let _ = conn
            .execute(&format!("DROP TABLE {table}"))
            .await;
    }
}
