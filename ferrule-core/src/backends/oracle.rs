use crate::connection::{
    BulkInsert, ConnectOptions, Connection, ExecutionSummary, QueryResult, StatementResult,
};
use crate::error::CoreError;
use crate::url::DatabaseUrl;
use crate::value::{ColumnInfo, TypeHint, Value};
use async_trait::async_trait;
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
        _target: BulkInsert<'_>,
    ) -> Result<usize, CoreError> {
        // Phase 5 will implement `oracle::Batch` array DML. Until
        // then, the dispatcher in copy.rs degrades to the generic
        // INSERT path (which already uses the Oracle-specific
        // `INSERT ALL ... SELECT 1 FROM DUAL` form).
        Err(CoreError::BulkUnavailable(
            "Oracle bulk path not yet implemented (Phase 5)".into(),
        ))
    }
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
}
