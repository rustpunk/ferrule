use crate::connection::{
    AsyncConnection, BulkInsert, ConnectOptions, ExecutionSummary, ForeignKey, QueryResult,
    StatementResult,
};
use crate::error::SqlError;
use crate::url::DatabaseUrl;
use crate::value::{ColumnInfo, Row, TypeHint, Value};
use async_trait::async_trait;
use bytes::Bytes;
use chrono::{NaiveDate, NaiveDateTime, NaiveTime, Utc};
use futures_util::stream::StreamExt;
use mysql_async::prelude::Queryable;
use secrecy::ExposeSecret;

pub struct MySqlConnection {
    conn: mysql_async::Conn,
}

#[async_trait]
impl AsyncConnection for MySqlConnection {
    async fn execute(&mut self, sql: &str) -> Result<ExecutionSummary, SqlError> {
        self.conn
            .query_drop(sql)
            .await
            .map_err(|e| SqlError::QueryFailed(e.to_string()))?;
        let affected = self.conn.affected_rows();
        Ok(ExecutionSummary {
            rows_affected: Some(affected),
            command_tag: None,
        })
    }

    async fn query(&mut self, sql: &str) -> Result<QueryResult, SqlError> {
        let mut result = self
            .conn
            .query_iter(sql)
            .await
            .map_err(|e| SqlError::QueryFailed(e.to_string()))?;

        let columns_ref = result.columns_ref();
        let columns: Vec<ColumnInfo> = columns_ref
            .iter()
            .map(|c| ColumnInfo {
                name: c.name_str().to_string(),
                type_hint: TypeHint::Other,
                nullable: true,
            })
            .collect();

        let mysql_rows = result
            .collect::<mysql_async::Row>()
            .await
            .map_err(|e| SqlError::QueryFailed(e.to_string()))?;

        // Discard any remaining result sets so the connection stays clean.
        result
            .drop_result()
            .await
            .map_err(|e| SqlError::QueryFailed(e.to_string()))?;

        let rows: Vec<Row> = mysql_rows
            .into_iter()
            .map(|row| {
                let col_types: Vec<_> = row
                    .columns_ref()
                    .iter()
                    .map(|c| (c.column_type(), c.column_length()))
                    .collect();
                row.unwrap()
                    .into_iter()
                    .enumerate()
                    .map(|(i, v)| mysql_to_value(v, col_types[i].0, col_types[i].1))
                    .collect()
            })
            .collect();

        Ok(QueryResult { columns, rows })
    }

    async fn execute_multi(&mut self, sql: &str) -> Result<Vec<StatementResult>, SqlError> {
        let mut result = self
            .conn
            .query_iter(sql)
            .await
            .map_err(|e| SqlError::QueryFailed(e.to_string()))?;

        let mut results = Vec::new();

        loop {
            let columns_ref = result.columns_ref();
            if columns_ref.is_empty() {
                let affected = result.affected_rows();
                result
                    .collect::<mysql_async::Row>()
                    .await
                    .map_err(|e| SqlError::QueryFailed(e.to_string()))?;
                results.push(StatementResult::Summary(ExecutionSummary {
                    rows_affected: Some(affected),
                    command_tag: None,
                }));
            } else {
                let columns: Vec<ColumnInfo> = columns_ref
                    .iter()
                    .map(|c| ColumnInfo {
                        name: c.name_str().to_string(),
                        type_hint: TypeHint::Other,
                        nullable: true,
                    })
                    .collect();

                let mysql_rows = result
                    .collect::<mysql_async::Row>()
                    .await
                    .map_err(|e| SqlError::QueryFailed(e.to_string()))?;

                let rows: Vec<Row> = mysql_rows
                    .into_iter()
                    .map(|row| {
                        let col_types: Vec<_> = row
                            .columns_ref()
                            .iter()
                            .map(|c| (c.column_type(), c.column_length()))
                            .collect();
                        row.unwrap()
                            .into_iter()
                            .enumerate()
                            .map(|(i, v)| mysql_to_value(v, col_types[i].0, col_types[i].1))
                            .collect()
                    })
                    .collect();

                results.push(StatementResult::Query(QueryResult { columns, rows }));
            }

            if result.is_empty() {
                break;
            }
        }

        Ok(results)
    }

    async fn ping(&mut self) -> Result<(), SqlError> {
        self.conn
            .ping()
            .await
            .map_err(|e| SqlError::ConnectionFailed(e.to_string()))?;
        Ok(())
    }

    async fn list_tables(&mut self, schema: Option<&str>) -> Result<Vec<String>, SqlError> {
        let sql = match schema {
            Some(s) => format!("SHOW TABLES FROM `{}`", escape_mysql_identifier(s)),
            None => "SHOW TABLES".to_string(),
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
    ) -> Result<QueryResult, SqlError> {
        let schema = match schema {
            Some(s) => s.to_string(),
            None => {
                let db_query = self.query("SELECT DATABASE()").await?;
                db_query
                    .rows
                    .into_iter()
                    .next()
                    .and_then(|row| row.into_iter().next())
                    .and_then(|v| match v {
                        Value::String(s) => Some(s),
                        _ => None,
                    })
                    .unwrap_or_default()
            }
        };

        let sql = format!(
            "SELECT column_name AS `column_name`, \
             data_type AS `data_type`, \
             is_nullable AS `is_nullable`, \
             column_default AS `column_default`, \
             numeric_precision AS `numeric_precision`, \
             numeric_scale AS `numeric_scale` \
             FROM information_schema.columns \
             WHERE table_schema = '{}' AND table_name = '{}' \
             ORDER BY ordinal_position",
            escape_mysql_string(&schema),
            escape_mysql_string(table)
        );
        self.query(&sql).await
    }

    async fn primary_key(
        &mut self,
        schema: Option<&str>,
        table: &str,
    ) -> Result<Vec<String>, SqlError> {
        let schema = match schema {
            Some(s) => s.to_string(),
            None => current_database(self).await?,
        };
        let sql = format!(
            "SELECT column_name FROM information_schema.key_column_usage \
             WHERE table_schema = '{}' AND table_name = '{}' \
               AND constraint_name = 'PRIMARY' \
             ORDER BY ordinal_position",
            escape_mysql_string(&schema),
            escape_mysql_string(table)
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
        let schema = match schema {
            Some(s) => s.to_string(),
            None => current_database(self).await?,
        };
        // KEY_COLUMN_USAGE has one row per (constraint, column position).
        // referential_constraints gives ON DELETE; join to surface it.
        let sql = format!(
            "SELECT k.constraint_name, k.table_name, k.column_name, \
                    k.referenced_table_name, k.referenced_column_name, \
                    rc.delete_rule \
             FROM information_schema.key_column_usage k \
             JOIN information_schema.referential_constraints rc \
               ON rc.constraint_schema = k.constraint_schema \
              AND rc.constraint_name = k.constraint_name \
             WHERE k.table_schema = '{}' AND k.referenced_table_name IS NOT NULL \
             ORDER BY k.constraint_name, k.ordinal_position",
            escape_mysql_string(&schema)
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
                Some(Value::String(s)) => Some(s),
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

        // Encode all rows upfront. Each row becomes one tab-separated
        // newline-terminated line, with the MySQL LOAD DATA escape
        // set applied byte-by-byte. The buffer is a `Vec<u8>` rather
        // than a `String` so `Value::Bytes` can carry arbitrary
        // (non-UTF-8) bytes into a BLOB column safely.
        let hints: Vec<TypeHint> = target.columns.iter().map(|c| c.type_hint).collect();
        let mut chunks: Vec<Bytes> = Vec::with_capacity(target.rows.len());
        for row in target.rows {
            let bytes = my_load_data::encode_row(row, &hints)?;
            chunks.push(bytes);
        }

        // mysql_async exposes a *local* per-call infile handler that
        // is consumed by a single `LOAD DATA LOCAL INFILE` and
        // auto-cleared afterwards (also on Drop / Conn::reset). This
        // gives us exactly the lifecycle we need: the handler is
        // only present while this function is running, so a hostile
        // `LOAD DATA LOCAL INFILE '/etc/passwd'` typed into
        // `ferrule query` on this connection fails with
        // `LocalInfileError::NoHandler` (no global handler installed
        // at connect time). Security falls out of the API shape.
        //
        // The handler future must be `Sync`, so construct the stream
        // *inside* the async block — capturing only `Vec<Bytes>`
        // (which is `Sync`) rather than a pre-boxed stream (which is
        // not). Matches the mysql_async docs example.
        self.conn.set_infile_handler(async move {
            Ok(futures_util::stream::iter(chunks).map(Ok).boxed())
        });

        // Quote the table and column list. The literal filename
        // ('ferrule_bulk') does not name a real file — the local
        // handler returns our encoded bytes instead, ignoring the
        // server-supplied path entirely.
        let qtable = my_load_data::backtick_quote(target.table);
        let cols = target
            .columns
            .iter()
            .map(|c| my_load_data::backtick_quote(&c.name))
            .collect::<Vec<_>>()
            .join(", ");
        let load_sql = format!(
            "LOAD DATA LOCAL INFILE 'ferrule_bulk' INTO TABLE {qtable} \
             CHARACTER SET utf8mb4 \
             FIELDS TERMINATED BY '\\t' ESCAPED BY '\\\\' \
             LINES TERMINATED BY '\\n' \
             ({cols})"
        );

        // query_drop discards the result set but stores
        // affected_rows on the connection.
        let load_result = self.conn.query_drop(load_sql).await;

        if let Err(e) = load_result {
            // C2: ensure no infile handler lingers on the connection
            // after a failed LOAD DATA. mysql_async only consumes the
            // local handler when the *server* asks for the file (see
            // `helpers.rs::handle_local_infile`); on a parse error,
            // missing-table error, or any failure *before* that
            // server prompt, the handler stays in
            // `Conn::inner.infile_handler` ready to be honored by the
            // *next* query on this connection — including a hostile
            // user-issued `LOAD DATA LOCAL INFILE '/etc/passwd'`.
            //
            // Two-step defense:
            //
            // 1. `Conn::reset()` issues `COM_RESET_CONNECTION` and
            //    explicitly clears `infile_handler = None`
            //    (mysql_async src/conn/mod.rs:1146). On modern
            //    servers this fully neutralizes the threat.
            //
            // 2. Reset returns `Ok(false)` without clearing on
            //    pre-MySQL-5.7.3 / pre-MariaDB-10.2.4 servers, and
            //    `Err(_)` on transport failure. In both cases we
            //    install a *poison* local handler that refuses any
            //    subsequent LOAD DATA on this connection. The poison
            //    handler is one-shot per mysql_async semantics;
            //    after it fires once `infile_handler` is `None` and
            //    further LOAD DATA queries fail with
            //    `LocalInfileError::NoHandler`. Installing it on top
            //    of a stale handler overwrites it, so the leaked
            //    bytes become unreachable even when reset is a no-op.
            let _ = self.conn.reset().await;
            self.conn.set_infile_handler(async {
                Err(mysql_async::Error::from(
                    mysql_async::LocalInfileError::other(std::io::Error::new(
                        std::io::ErrorKind::PermissionDenied,
                        "ferrule: LOAD DATA LOCAL INFILE refused — connection \
                         state may be tainted after a failed bulk operation. \
                         Reconnect to re-enable bulk_insert_rows.",
                    )),
                ))
            });
            return Err(my_load_data::classify_load_error(e));
        }

        Ok(self.conn.affected_rows() as usize)
    }
}

pub(crate) async fn connect(
    url: &DatabaseUrl,
    opts: &ConnectOptions,
) -> Result<MySqlConnection, SqlError> {
    let mut builder = mysql_async::OptsBuilder::default()
        .ip_or_hostname(url.host().unwrap_or("localhost"))
        .tcp_port(url.port().unwrap_or(3306));

    if !url.username().is_empty() {
        builder = builder.user(Some(url.username()));
    }
    // A caller-resolved secret takes precedence over the URL password.
    if let Some(pass) = opts.effective_password(url) {
        builder = builder.pass(Some(pass.expose_secret()));
    }
    let db = url.database();
    if !db.is_empty() {
        builder = builder.db_name(Some(db));
    }

    if opts.insecure {
        let ssl_opts = mysql_async::SslOpts::default()
            .with_danger_accept_invalid_certs(true)
            .with_danger_skip_domain_validation(true);
        builder = builder.ssl_opts(Some(ssl_opts));
    }

    if let Some(ssl_mode) = url.params().get("ssl-mode") {
        match ssl_mode.as_str() {
            "disabled" | "disable" => {
                let ssl_opts =
                    mysql_async::SslOpts::default().with_danger_accept_invalid_certs(true);
                builder = builder.ssl_opts(Some(ssl_opts));
            }
            "preferred" => {
                // Default behavior – no-op
            }
            "required" => {
                let ssl_opts =
                    mysql_async::SslOpts::default().with_danger_accept_invalid_certs(false);
                builder = builder.ssl_opts(Some(ssl_opts));
            }
            "verify-ca" | "verify-identity" => {
                let ssl_opts = mysql_async::SslOpts::default()
                    .with_danger_accept_invalid_certs(false)
                    .with_danger_skip_domain_validation(false);
                builder = builder.ssl_opts(Some(ssl_opts));
            }
            _ => {}
        }
    }

    let conn_opts: mysql_async::Opts = builder.into();
    let conn = mysql_async::Conn::new(conn_opts)
        .await
        .map_err(|e| SqlError::ConnectionFailed(e.to_string()))?;

    Ok(MySqlConnection { conn })
}

fn mysql_to_value(
    value: mysql_async::Value,
    column_type: mysql_async::consts::ColumnType,
    column_length: u32,
) -> Value {
    use mysql_async::consts::ColumnType as CT;

    match value {
        mysql_async::Value::NULL => Value::Null,
        mysql_async::Value::Bytes(b) => match column_type {
            CT::MYSQL_TYPE_JSON => serde_json::from_slice(&b)
                .map(Value::Json)
                .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&b).into_owned())),
            CT::MYSQL_TYPE_DECIMAL | CT::MYSQL_TYPE_NEWDECIMAL => {
                Value::Decimal(String::from_utf8_lossy(&b).into_owned())
            }
            CT::MYSQL_TYPE_TINY_BLOB
            | CT::MYSQL_TYPE_MEDIUM_BLOB
            | CT::MYSQL_TYPE_LONG_BLOB
            | CT::MYSQL_TYPE_BLOB => Value::Bytes(b),
            CT::MYSQL_TYPE_TINY => {
                let s = String::from_utf8_lossy(&b);
                if column_length == 1 {
                    Value::Bool(s != "0")
                } else {
                    s.parse::<i64>()
                        .map(Value::Int64)
                        .unwrap_or_else(|_| Value::String(s.into_owned()))
                }
            }
            CT::MYSQL_TYPE_SHORT
            | CT::MYSQL_TYPE_LONG
            | CT::MYSQL_TYPE_INT24
            | CT::MYSQL_TYPE_LONGLONG
            | CT::MYSQL_TYPE_YEAR => String::from_utf8_lossy(&b)
                .parse::<i64>()
                .map(Value::Int64)
                .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&b).into_owned())),
            CT::MYSQL_TYPE_FLOAT | CT::MYSQL_TYPE_DOUBLE => String::from_utf8_lossy(&b)
                .parse::<f64>()
                .map(Value::Float64)
                .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&b).into_owned())),
            CT::MYSQL_TYPE_DATE => {
                NaiveDate::parse_from_str(&String::from_utf8_lossy(&b), "%Y-%m-%d")
                    .map(Value::Date)
                    .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&b).into_owned()))
            }
            CT::MYSQL_TYPE_TIME => {
                NaiveTime::parse_from_str(&String::from_utf8_lossy(&b), "%H:%M:%S")
                    .or_else(|_| {
                        NaiveTime::parse_from_str(&String::from_utf8_lossy(&b), "%H:%M:%S%.f")
                    })
                    .map(Value::Time)
                    .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&b).into_owned()))
            }
            CT::MYSQL_TYPE_DATETIME | CT::MYSQL_TYPE_DATETIME2 => {
                parse_naive_datetime(&String::from_utf8_lossy(&b))
                    .map(Value::DateTime)
                    .unwrap_or_else(|| Value::String(String::from_utf8_lossy(&b).into_owned()))
            }
            CT::MYSQL_TYPE_TIMESTAMP | CT::MYSQL_TYPE_TIMESTAMP2 => {
                parse_naive_datetime(&String::from_utf8_lossy(&b))
                    .and_then(|dt| dt.and_local_timezone(Utc).single())
                    .map(Value::DateTimeTz)
                    .unwrap_or_else(|| Value::String(String::from_utf8_lossy(&b).into_owned()))
            }
            _ => String::from_utf8(b)
                .map(Value::String)
                .unwrap_or_else(|e| Value::Bytes(e.into_bytes())),
        },
        mysql_async::Value::Int(i) => {
            if column_type == CT::MYSQL_TYPE_TINY && column_length == 1 {
                Value::Bool(i != 0)
            } else {
                Value::Int64(i)
            }
        }
        mysql_async::Value::UInt(u) => {
            if column_type == CT::MYSQL_TYPE_TINY && column_length == 1 {
                Value::Bool(u != 0)
            } else {
                Value::Int64(u as i64)
            }
        }
        mysql_async::Value::Float(f) => Value::Float64(f64::from(f)),
        mysql_async::Value::Double(d) => Value::Float64(d),
        mysql_async::Value::Date(year, month, day, hour, min, sec, usec) => match column_type {
            CT::MYSQL_TYPE_DATE => NaiveDate::from_ymd_opt(year as i32, month as u32, day as u32)
                .map(Value::Date)
                .unwrap_or_else(|| Value::String(format!("{:04}-{:02}-{:02}", year, month, day))),
            CT::MYSQL_TYPE_TIMESTAMP | CT::MYSQL_TYPE_TIMESTAMP2 => {
                NaiveDate::from_ymd_opt(year as i32, month as u32, day as u32)
                    .and_then(|d| d.and_hms_micro_opt(hour as u32, min as u32, sec as u32, usec))
                    .and_then(|dt| dt.and_local_timezone(Utc).single())
                    .map(Value::DateTimeTz)
                    .unwrap_or_else(|| {
                        Value::String(format!(
                            "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
                            year, month, day, hour, min, sec
                        ))
                    })
            }
            _ => NaiveDate::from_ymd_opt(year as i32, month as u32, day as u32)
                .and_then(|d| d.and_hms_micro_opt(hour as u32, min as u32, sec as u32, usec))
                .map(Value::DateTime)
                .unwrap_or_else(|| {
                    Value::String(format!(
                        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
                        year, month, day, hour, min, sec
                    ))
                }),
        },
        mysql_async::Value::Time(neg, days, hours, minutes, seconds, _usec) => {
            let total_hours = days * 24 + u32::from(hours);
            Value::String(format!(
                "{}{:02}:{:02}:{:02}",
                if neg { "-" } else { "" },
                total_hours,
                minutes,
                seconds
            ))
        }
    }
}

fn parse_naive_datetime(s: &str) -> Option<NaiveDateTime> {
    NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S%.f")
        .ok()
        .or_else(|| NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S").ok())
}

async fn current_database(conn: &mut MySqlConnection) -> Result<String, SqlError> {
    let result = conn.query("SELECT DATABASE()").await?;
    Ok(result
        .rows
        .into_iter()
        .next()
        .and_then(|row| row.into_iter().next())
        .and_then(|v| match v {
            Value::String(s) => Some(s),
            _ => None,
        })
        .unwrap_or_default())
}

fn escape_mysql_identifier(name: &str) -> String {
    name.replace('`', "``")
}

fn escape_mysql_string(s: &str) -> String {
    s.replace("'", "''")
}

/// MySQL `LOAD DATA LOCAL INFILE` encoder + helpers.
///
/// Each row is encoded as one tab-separated, newline-terminated line.
/// MySQL's `LOAD DATA` default escape set (with `ESCAPED BY '\\'`):
///
/// - `\\` → literal backslash
/// - `\t` → literal tab
/// - `\n` → literal newline
/// - `\r` → literal carriage return
/// - `\0` → literal NUL byte
/// - `\N` → SQL NULL
///
/// The encoder mirrors these byte-by-byte. Backslash escapes FIRST so
/// a literal backslash isn't double-decoded.
///
/// The buffer is a `Vec<u8>` (not `String`) so `Value::Bytes` payloads
/// — which may contain arbitrary non-UTF-8 bytes — can flow into a
/// BLOB column without lossy conversion.
mod my_load_data {
    use crate::error::SqlError;
    use crate::value::{TypeHint, Value};
    use bytes::Bytes;

    /// Encode one row.
    pub fn encode_row(row: &[Value], hints: &[TypeHint]) -> Result<Bytes, SqlError> {
        let mut buf: Vec<u8> = Vec::with_capacity(row.len() * 16 + 1);
        for (i, value) in row.iter().enumerate() {
            if i > 0 {
                buf.push(b'\t');
            }
            let hint = hints.get(i).copied().unwrap_or(TypeHint::Other);
            encode_value(&mut buf, value, hint)?;
        }
        buf.push(b'\n');
        Ok(Bytes::from(buf))
    }

    fn encode_value(out: &mut Vec<u8>, v: &Value, hint: TypeHint) -> Result<(), SqlError> {
        match v {
            Value::Null => out.extend_from_slice(b"\\N"),
            Value::Bool(b) => out.push(if *b { b'1' } else { b'0' }),
            Value::Int64(n) => out.extend_from_slice(n.to_string().as_bytes()),
            Value::Float64(f) => {
                if f.is_nan() {
                    // MySQL has no NaN literal for DOUBLE; the
                    // generic-INSERT path also can't represent NaN.
                    // Substitute NULL so bulk and generic agree.
                    out.extend_from_slice(b"\\N");
                } else if f.is_infinite() {
                    // No literal — substitute NULL (same rationale).
                    out.extend_from_slice(b"\\N");
                } else {
                    out.extend_from_slice(f.to_string().as_bytes());
                }
            }
            Value::Decimal(s) => push_escaped(out, s.as_bytes()),
            Value::String(s) => push_escaped(out, s.as_bytes()),
            Value::Bytes(b) => push_escaped(out, b),
            Value::Date(d) => out.extend_from_slice(d.to_string().as_bytes()),
            Value::Time(t) => out.extend_from_slice(t.to_string().as_bytes()),
            Value::DateTime(dt) => {
                // MySQL DATETIME accepts 'YYYY-MM-DD HH:MM:SS[.fff]'.
                // chrono's NaiveDateTime Display matches.
                out.extend_from_slice(dt.to_string().as_bytes());
            }
            Value::DateTimeTz(dt) => {
                // MySQL DATETIME has no timezone; the DDL translator
                // maps DateTimeTz → DATETIME. Convert to UTC and
                // render naive.
                let naive = dt.naive_utc();
                out.extend_from_slice(naive.to_string().as_bytes());
            }
            Value::Json(j) => {
                let rendered = serde_json::to_string(j).map_err(|e| {
                    SqlError::QueryFailed(format!("MySQL bulk: JSON serialize: {e}"))
                })?;
                push_escaped(out, rendered.as_bytes());
            }
            Value::Uuid(s) => push_escaped(out, s.as_bytes()),
            Value::Array(a) => {
                // DDL translator maps Array → JSON on MySQL.
                let _ = hint;
                let rendered = serde_json::to_string(a).map_err(|e| {
                    SqlError::QueryFailed(format!("MySQL bulk: array serialize: {e}"))
                })?;
                push_escaped(out, rendered.as_bytes());
            }
        }
        Ok(())
    }

    /// Apply MySQL LOAD DATA escape rules byte-by-byte. Backslash
    /// is escaped FIRST, otherwise a literal backslash in field text
    /// would be misinterpreted on decode.
    fn push_escaped(out: &mut Vec<u8>, bytes: &[u8]) {
        for &b in bytes {
            match b {
                b'\\' => out.extend_from_slice(b"\\\\"),
                b'\t' => out.extend_from_slice(b"\\t"),
                b'\n' => out.extend_from_slice(b"\\n"),
                // `\r` matters because (a) some MySQL clients
                // interpret a bare `\r` immediately before `\n` as
                // part of a CRLF row terminator under certain
                // `LINES TERMINATED BY` settings, and (b) even when
                // the server doesn't, a bare `\r` survives as a data
                // byte and corrupts `VARCHAR` round-trips.
                b'\r' => out.extend_from_slice(b"\\r"),
                b'\0' => out.extend_from_slice(b"\\0"),
                other => out.push(other),
            }
        }
    }

    /// Quote an identifier for MySQL using backticks. Embedded
    /// backticks are doubled.
    pub fn backtick_quote(s: &str) -> String {
        format!("`{}`", s.replace('`', "``"))
    }

    /// Classify a `mysql_async::Error` raised by `LOAD DATA LOCAL
    /// INFILE`. Server error codes for "loading local data is
    /// disabled" / "command not allowed" return
    /// [`SqlError::BulkUnavailable`] so the Auto dispatcher can
    /// fall back. Anything else stays `QueryFailed`.
    pub fn classify_load_error(e: mysql_async::Error) -> SqlError {
        match &e {
            mysql_async::Error::Server(srv) => {
                // 1148 ER_NOT_ALLOWED_COMMAND, 3948
                // ER_CLIENT_LOCAL_FILES_DISABLED — the two codes the
                // mysql_async docs page calls out for LOAD DATA
                // LOCAL being disabled. 3950 ER_LOAD_DATA_INFILE_
                // _DISABLED appears in some MySQL builds; include it.
                if matches!(srv.code, 1148 | 3948 | 3950) {
                    return SqlError::BulkUnavailable(format!(
                        "MySQL server rejected LOAD DATA LOCAL INFILE \
                         (error {}: {}). Enable `local_infile=ON` server-side, \
                         or pass `--bulk-native=off` to use the generic path.",
                        srv.code, srv.message
                    ));
                }
                SqlError::QueryFailed(format!("MySQL bulk LOAD DATA: {srv}"))
            }
            _ => SqlError::QueryFailed(format!("MySQL bulk LOAD DATA: {e}")),
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use chrono::{NaiveDate, NaiveDateTime, NaiveTime, TimeZone, Utc};

        fn enc1(v: Value, hint: TypeHint) -> Vec<u8> {
            let bytes = encode_row(&[v], &[hint]).expect("encode_row");
            // Strip trailing newline so tests assert on field content.
            assert_eq!(bytes.last().copied(), Some(b'\n'));
            bytes[..bytes.len() - 1].to_vec()
        }

        fn enc1_str(v: Value, hint: TypeHint) -> String {
            String::from_utf8(enc1(v, hint)).expect("UTF-8")
        }

        #[test]
        fn encode_null_is_backslash_n() {
            assert_eq!(enc1_str(Value::Null, TypeHint::Null), "\\N");
        }

        #[test]
        fn encode_bool_is_one_or_zero() {
            assert_eq!(enc1_str(Value::Bool(true), TypeHint::Bool), "1");
            assert_eq!(enc1_str(Value::Bool(false), TypeHint::Bool), "0");
        }

        #[test]
        fn encode_int_and_float() {
            assert_eq!(enc1_str(Value::Int64(42), TypeHint::Int64), "42");
            assert_eq!(enc1_str(Value::Int64(-7), TypeHint::Int64), "-7");
            assert_eq!(enc1_str(Value::Float64(1.5), TypeHint::Float64), "1.5");
        }

        #[test]
        fn encode_float_nan_and_inf_become_null() {
            // MySQL has no NaN/Inf literal for DOUBLE; the bulk path
            // substitutes NULL to stay consistent with the generic
            // INSERT path's reject-or-NULL outcome.
            assert_eq!(enc1_str(Value::Float64(f64::NAN), TypeHint::Float64), "\\N");
            assert_eq!(
                enc1_str(Value::Float64(f64::INFINITY), TypeHint::Float64),
                "\\N"
            );
        }

        #[test]
        fn encode_string_escapes_backslash_first() {
            // Backslash MUST be the first replacement applied.
            // Input `\t` (literal two chars `\` and `t`) → `\\t`,
            // input `\n` → `\\n`, input `\` → `\\`. If any of those
            // mappings ran before backslash-doubling, the result
            // would be wrong (`\\` then `\t` → `\\t` instead of
            // `\\\\t`, etc.).
            assert_eq!(
                enc1_str(Value::String("a\\b".into()), TypeHint::String),
                "a\\\\b"
            );
            assert_eq!(
                enc1_str(Value::String("a\tb".into()), TypeHint::String),
                "a\\tb"
            );
            assert_eq!(
                enc1_str(Value::String("a\nb".into()), TypeHint::String),
                "a\\nb"
            );
            // H1 regression guard: \r MUST be escaped as `\r` too,
            // otherwise CRLF source data corrupts VARCHAR round-trips
            // and may interact badly with LINES TERMINATED BY on some
            // server configurations.
            assert_eq!(
                enc1_str(Value::String("a\rb".into()), TypeHint::String),
                "a\\rb"
            );
            assert_eq!(
                enc1_str(Value::String("a\r\nb".into()), TypeHint::String),
                "a\\r\\nb"
            );
        }

        #[test]
        fn encode_string_escapes_nul_byte() {
            // `\0` is a hard error inside MySQL text columns unless
            // escaped; the encoder produces `\0` which the receiver
            // decodes back to a NUL byte. The byte goes into BLOB
            // columns intact via this path.
            let out = enc1(Value::String("a\0b".into()), TypeHint::String);
            assert_eq!(out, b"a\\0b");
        }

        #[test]
        fn encode_bytes_preserves_arbitrary_payload() {
            // BLOB columns: an arbitrary byte sequence (including
            // non-UTF-8 bytes like 0xFF) must flow through with
            // escapes applied to the metacharacters only.
            let raw = vec![0x01u8, b'\t', 0xFF, b'\\', b'\n', 0x00, b'Z'];
            let out = enc1(Value::Bytes(raw), TypeHint::Bytes);
            assert_eq!(
                out,
                vec![0x01u8, b'\\', b't', 0xFF, b'\\', b'\\', b'\\', b'n', b'\\', b'0', b'Z']
            );
        }

        #[test]
        fn encode_date_time_datetime() {
            let d = NaiveDate::from_ymd_opt(2026, 5, 14).unwrap();
            let t = NaiveTime::from_hms_opt(12, 34, 56).unwrap();
            let dt = NaiveDateTime::new(d, t);
            assert_eq!(enc1_str(Value::Date(d), TypeHint::Date), "2026-05-14");
            assert_eq!(enc1_str(Value::Time(t), TypeHint::Time), "12:34:56");
            assert_eq!(
                enc1_str(Value::DateTime(dt), TypeHint::DateTime),
                "2026-05-14 12:34:56"
            );
        }

        #[test]
        fn encode_datetimetz_converts_to_utc_naive() {
            let dt = Utc.with_ymd_and_hms(2026, 5, 14, 12, 34, 56).unwrap();
            assert_eq!(
                enc1_str(Value::DateTimeTz(dt), TypeHint::DateTimeTz),
                "2026-05-14 12:34:56"
            );
        }

        #[test]
        fn encode_json_is_compact_then_escaped() {
            let j = serde_json::json!({"role": "admin"});
            let s = enc1_str(Value::Json(j), TypeHint::Json);
            // serde_json compact form keeps no spaces between :/, ;
            // the encoder then escapes nothing in this payload
            // because there are no metacharacters.
            assert!(s.contains("\"role\":\"admin\""));
            assert!(!s.contains(' '));
        }

        #[test]
        fn encode_array_is_compact_json() {
            let a = Value::Array(vec![Value::Int64(1), Value::Int64(2), Value::Int64(3)]);
            assert_eq!(enc1_str(a, TypeHint::Array), "[1,2,3]");
        }

        #[test]
        fn encode_uuid_passes_through() {
            assert_eq!(
                enc1_str(
                    Value::Uuid("550e8400-e29b-41d4-a716-446655440000".into()),
                    TypeHint::Uuid
                ),
                "550e8400-e29b-41d4-a716-446655440000"
            );
        }

        #[test]
        fn encode_row_with_multiple_cells_uses_tab_separator() {
            let row = vec![
                Value::Int64(1),
                Value::String("Alice".into()),
                Value::Null,
                Value::Bool(true),
            ];
            let hints = vec![
                TypeHint::Int64,
                TypeHint::String,
                TypeHint::Null,
                TypeHint::Bool,
            ];
            let bytes = encode_row(&row, &hints).unwrap();
            assert_eq!(&bytes[..], b"1\tAlice\t\\N\t1\n");
        }

        #[test]
        fn backtick_quote_doubles_embedded_backticks() {
            assert_eq!(backtick_quote("plain"), "`plain`");
            assert_eq!(backtick_quote("with`tick"), "`with``tick`");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::url::DatabaseUrl;

    const TEST_MYSQL_URL: &str = "mysql://root:ferrule@127.0.0.1:13306/ferrule";

    fn try_connect() -> Option<Box<dyn crate::Connection>> {
        let url = DatabaseUrl::parse(TEST_MYSQL_URL).ok()?;
        let conn = crate::connect(&url, &ConnectOptions::default(), None).ok()?;
        Some(conn)
    }

    #[test]
    fn test_mysql_ping() {
        let Some(mut conn) = try_connect() else {
            eprintln!("MySQL test container not available, skipping test_mysql_ping");
            return;
        };
        conn.ping().expect("ping should succeed");
    }

    #[test]
    fn test_mysql_query() {
        let Some(mut conn) = try_connect() else {
            eprintln!("MySQL test container not available, skipping test_mysql_query");
            return;
        };
        let result = conn
            .query("SELECT * FROM test_users")
            .expect("query should succeed");
        assert!(!result.columns.is_empty(), "should have columns");
        assert!(!result.rows.is_empty(), "should have rows");
    }

    #[test]
    fn test_mysql_execute() {
        let Some(mut conn) = try_connect() else {
            eprintln!("MySQL test container not available, skipping test_mysql_execute");
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
    fn test_mysql_list_tables() {
        let Some(mut conn) = try_connect() else {
            eprintln!("MySQL test container not available, skipping test_mysql_list_tables");
            return;
        };
        let tables = conn.list_tables(None).expect("list_tables should succeed");
        assert!(
            tables.contains(&"test_users".to_string()),
            "should contain test_users"
        );
    }

    #[test]
    fn test_mysql_describe_table() {
        let Some(mut conn) = try_connect() else {
            eprintln!("MySQL test container not available, skipping test_mysql_describe_table");
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
    fn test_mysql_execute_multi() {
        let Some(mut conn) = try_connect() else {
            eprintln!("MySQL test container not available, skipping test_mysql_execute_multi");
            return;
        };
        // Clean up any previous test row
        let _ = conn.execute("DELETE FROM test_users WHERE name = 'MultiUser'");
        let results = conn
            .execute_multi("INSERT INTO test_users (name, age) VALUES ('MultiUser', 42); SELECT COUNT(*) FROM test_users;")
            .expect("execute_multi should succeed");
        assert_eq!(results.len(), 2, "should have two result sets");
        // First result: DML summary
        assert!(
            matches!(&results[0], StatementResult::Summary(s) if s.rows_affected.is_some_and(|n| n > 0)),
            "first result should be a DML summary with affected rows"
        );
        // Second result: SELECT query
        assert!(
            matches!(&results[1], StatementResult::Query(_)),
            "second result should be a Query"
        );
    }

    #[test]
    fn test_mysql_type_mapping() {
        let Some(mut conn) = try_connect() else {
            eprintln!("MySQL test container not available, skipping test_mysql_type_mapping");
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

    /// End-to-end: bulk-load a scratch table via LOAD DATA LOCAL
    /// INFILE and verify the rows round-trip. Includes a row with
    /// backslash/tab/newline payloads, an all-NULLs row, and a BLOB
    /// with non-UTF-8 bytes to exercise the binary path.
    #[test]
    fn test_mysql_bulk_insert_rows_round_trip() {
        let Some(mut conn) = try_connect() else {
            eprintln!(
                "MySQL test container not available, skipping test_mysql_bulk_insert_rows_round_trip"
            );
            return;
        };

        // The mysql_async client sets `LOCAL_INFILE` on the
        // connection handshake automatically, but the server-side
        // toggle `local_infile=ON` is required too. The CLAUDE.md
        // test container ships with the MySQL 8 default, which is
        // OFF for `LOAD DATA LOCAL`. Flip it on for the duration of
        // this test. If the user doesn't have SUPER, the SET fails
        // and we degrade by skipping the test (rather than the bulk
        // path silently failing).
        if conn.execute("SET GLOBAL local_infile = ON").is_err() {
            eprintln!(
                "MySQL test container does not allow toggling local_infile; \
                 skipping test_mysql_bulk_insert_rows_round_trip"
            );
            return;
        }

        let pid = std::process::id();
        let table = format!("ferrule_bulk_test_{pid}");
        let _ = conn.execute(&format!("DROP TABLE IF EXISTS {table}"));
        conn.execute(&format!(
            "CREATE TABLE {table} (\
               id BIGINT NOT NULL, \
               name VARCHAR(255) NULL, \
               active TINYINT(1) NULL, \
               blob_data BLOB NULL, \
               meta JSON NULL, \
               tricky TEXT NULL\
             ) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4"
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
                name: "blob_data".into(),
                type_hint: TypeHint::Bytes,
                nullable: true,
            },
            ColumnInfo {
                name: "meta".into(),
                type_hint: TypeHint::Json,
                nullable: true,
            },
            ColumnInfo {
                name: "tricky".into(),
                type_hint: TypeHint::String,
                nullable: true,
            },
        ];

        let rows: Vec<Row> = vec![
            vec![
                Value::Int64(1),
                Value::String("Alice".into()),
                Value::Bool(true),
                Value::Bytes(vec![0xDE, 0xAD, 0xBE, 0xEF]),
                Value::Json(serde_json::json!({"role": "admin"})),
                Value::String("plain".into()),
            ],
            vec![
                Value::Int64(2),
                Value::String("Esc\\\t\nape".into()),
                Value::Bool(false),
                Value::Bytes(vec![0x00, 0x09, 0x0A, 0xFF]),
                Value::Json(serde_json::Value::Null),
                Value::String("\\.".into()),
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

        let result = conn
            .query(&format!(
                "SELECT id, name, active, blob_data, tricky FROM {table} ORDER BY id"
            ))
            .unwrap();
        assert_eq!(result.rows.len(), 3);

        // Row 2 — verify the tricky escaped payload round-tripped.
        if let Value::String(s) = &result.rows[1][1] {
            assert_eq!(
                s, "Esc\\\t\nape",
                "row 2 name should preserve backslash/tab/nl"
            );
        } else {
            panic!("row 2 name should be String, got {:?}", result.rows[1][1]);
        }
        if let Value::Bytes(b) = &result.rows[1][3] {
            assert_eq!(b.as_slice(), &[0x00u8, 0x09, 0x0A, 0xFF]);
        } else {
            panic!(
                "row 2 blob_data should be Bytes, got {:?}",
                result.rows[1][3]
            );
        }
        if let Value::String(s) = &result.rows[1][4] {
            assert_eq!(s, "\\.", "row 2 tricky should be literal backslash-dot");
        } else {
            panic!("row 2 tricky should be String, got {:?}", result.rows[1][4]);
        }

        // Row 3 — everything NULL except id.
        assert!(matches!(&result.rows[2][1], Value::Null));
        assert!(matches!(&result.rows[2][2], Value::Null));
        assert!(matches!(&result.rows[2][3], Value::Null));

        // Cleanup.
        conn.execute(&format!("DROP TABLE {table}"))
            .expect("DROP TABLE");
    }

    /// Security regression: a user-issued `LOAD DATA LOCAL INFILE`
    /// outside of `bulk_insert_rows` MUST be rejected — there is no
    /// global handler installed, and the per-call local handler is
    /// only present during a bulk_insert_rows call. mysql_async
    /// raises `LocalInfileError::NoHandler` in that case, which
    /// `MySqlConnection::execute` surfaces as a `QueryFailed`.
    #[test]
    fn test_mysql_load_data_without_bulk_in_progress_rejected() {
        let Some(mut conn) = try_connect() else {
            eprintln!(
                "MySQL test container not available, skipping test_mysql_load_data_without_bulk_in_progress_rejected"
            );
            return;
        };

        // Enable server-side local_infile so the *server* is willing
        // to ask the client for the file. We need that step to fail
        // here, not the server config, to prove the client refused.
        if conn.execute("SET GLOBAL local_infile = ON").is_err() {
            eprintln!(
                "MySQL test container does not allow toggling local_infile; \
                 skipping test_mysql_load_data_without_bulk_in_progress_rejected"
            );
            return;
        }

        let pid = std::process::id();
        let table = format!("ferrule_bulk_security_test_{pid}");
        let _ = conn.execute(&format!("DROP TABLE IF EXISTS {table}"));
        conn.execute(&format!(
            "CREATE TABLE {table} (id INT, line TEXT) ENGINE=InnoDB"
        ))
        .expect("CREATE TABLE");

        // Try to coerce the client into shipping /etc/passwd.
        let result = conn.execute(&format!(
            "LOAD DATA LOCAL INFILE '/etc/passwd' INTO TABLE {table} \
                 FIELDS TERMINATED BY ':' (id, line)"
        ));

        // The client refuses because no infile handler is
        // installed at this point. mysql_async raises a query
        // error; ferrule surfaces it as QueryFailed.
        let err = result
            .expect_err("LOAD DATA LOCAL INFILE without bulk_insert_rows in progress must fail");
        let msg = err.to_string();
        // Look for any hint that the failure is handler-related —
        // exact mysql_async wording may evolve.
        assert!(
            msg.to_lowercase().contains("handler")
                || msg.to_lowercase().contains("local_infile")
                || msg.to_lowercase().contains("infile"),
            "expected handler/infile rejection, got: {msg}"
        );

        // Sanity: confirm nothing landed in the table.
        let count = conn
            .query(&format!("SELECT COUNT(*) FROM {table}"))
            .unwrap();
        match &count.rows[0][0] {
            Value::Int64(n) => assert_eq!(*n, 0, "no rows should have been inserted"),
            other => panic!("unexpected count shape: {other:?}"),
        }

        let _ = conn.execute(&format!("DROP TABLE {table}"));
    }

    #[test]
    fn test_mysql_primary_key() {
        let Some(mut conn) = try_connect() else {
            eprintln!("MySQL test container not available, skipping test_mysql_primary_key");
            return;
        };
        let pk = conn.primary_key(None, "test_users").expect("primary_key");
        assert_eq!(pk, vec!["id".to_string()]);
    }

    #[test]
    fn test_mysql_list_foreign_keys() {
        let Some(mut conn) = try_connect() else {
            eprintln!("MySQL test container not available, skipping test_mysql_list_foreign_keys");
            return;
        };
        let pid = std::process::id();
        let child = format!("ferrule_fk_test_orders_{pid}");
        let _ = conn.execute(&format!("DROP TABLE IF EXISTS {child}"));
        conn.execute(&format!(
            "CREATE TABLE {child} (\
               id INT AUTO_INCREMENT PRIMARY KEY, \
               user_id INT, \
               FOREIGN KEY (user_id) REFERENCES test_users(id) ON DELETE CASCADE\
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
    /// MySQL. Uses `INSERT IGNORE` and `ON DUPLICATE KEY UPDATE`.
    #[test]
    fn test_mysql_copy_skip_then_upsert() {
        use crate::backend::Backend;
        use crate::copy::{copy_rows, CopyOptions, CopySource, IfExists};

        let (Some(mut src), Some(mut dst)) = (try_connect(), try_connect()) else {
            eprintln!(
                "MySQL test container not available, skipping test_mysql_copy_skip_then_upsert"
            );
            return;
        };

        let pid = std::process::id();
        let src_table = format!("ferrule_my_skip_src_{pid}");
        let dst_table = format!("ferrule_my_skip_dst_{pid}");
        let _ = src.execute(&format!("DROP TABLE IF EXISTS {src_table}"));
        let _ = dst.execute(&format!("DROP TABLE IF EXISTS {dst_table}"));
        src.execute(&format!(
            "CREATE TABLE {src_table} (id INT PRIMARY KEY, name VARCHAR(64), val INT)"
        ))
        .expect("CREATE src");
        dst.execute(&format!(
            "CREATE TABLE {dst_table} (id INT PRIMARY KEY, name VARCHAR(64), val INT)"
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
        copy_rows(&mut src, Backend::MySql, &mut dst, Backend::MySql, &opts)
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
        copy_rows(&mut src, Backend::MySql, &mut dst, Backend::MySql, &opts)
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
