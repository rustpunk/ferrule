use crate::connection::{
    BulkInsert, ConnectOptions, Connection, ExecutionSummary, QueryResult, StatementResult,
};
use crate::error::CoreError;
use crate::url::DatabaseUrl;
use crate::value::{ColumnInfo, Row, TypeHint, Value};
use async_trait::async_trait;
use bytes::Bytes;
use futures_util::sink::SinkExt;
use secrecy::ExposeSecret;
use std::sync::Arc;
use tokio_postgres::types::Type;

pub struct PostgresConnection {
    client: tokio_postgres::Client,
}

#[async_trait]
impl Connection for PostgresConnection {
    async fn execute(&mut self, sql: &str) -> Result<ExecutionSummary, CoreError> {
        let rows_affected = self
            .client
            .execute(sql, &[])
            .await
            .map_err(|e| CoreError::QueryFailed(e.to_string()))?;
        Ok(ExecutionSummary {
            rows_affected: Some(rows_affected as u64),
            command_tag: None,
        })
    }

    async fn query(&mut self, sql: &str) -> Result<QueryResult, CoreError> {
        let rows = self
            .client
            .query(sql, &[])
            .await
            .map_err(|e| CoreError::QueryFailed(e.to_string()))?;
        if rows.is_empty() {
            return Ok(QueryResult {
                columns: Vec::new(),
                rows: Vec::new(),
            });
        }
        let first = &rows[0];
        let columns: Vec<ColumnInfo> = first
            .columns()
            .iter()
            .map(|c| ColumnInfo {
                name: c.name().to_string(),
                type_hint: pg_type_to_hint(c.type_()),
                nullable: true,
            })
            .collect();

        let data_rows: Vec<Row> = rows
            .iter()
            .map(|row| {
                (0..columns.len())
                    .map(|i| pg_to_value(row, i, row.columns()[i].type_()))
                    .collect()
            })
            .collect();

        Ok(QueryResult {
            columns,
            rows: data_rows,
        })
    }

    async fn execute_multi(&mut self, sql: &str) -> Result<Vec<StatementResult>, CoreError> {
        let msgs = self
            .client
            .simple_query(sql)
            .await
            .map_err(|e| CoreError::QueryFailed(e.to_string()))?;

        let mut results = Vec::new();
        let mut current_columns: Vec<ColumnInfo> = Vec::new();
        let mut current_rows: Vec<Row> = Vec::new();

        for msg in msgs {
            use tokio_postgres::SimpleQueryMessage;
            match msg {
                SimpleQueryMessage::Row(row) => {
                    if current_columns.is_empty() {
                        current_columns = row
                            .columns()
                            .iter()
                            .map(|c| ColumnInfo {
                                name: c.name().to_string(),
                                type_hint: TypeHint::Other,
                                nullable: true,
                            })
                            .collect();
                    }
                    let values: Vec<Value> = (0..row.len())
                        .map(|i| match row.get(i) {
                            Some(s) => Value::String(s.to_string()),
                            None => Value::Null,
                        })
                        .collect();
                    current_rows.push(values);
                }
                SimpleQueryMessage::CommandComplete(n) => {
                    if !current_columns.is_empty() {
                        results.push(StatementResult::Query(QueryResult {
                            columns: std::mem::take(&mut current_columns),
                            rows: std::mem::take(&mut current_rows),
                        }));
                    } else {
                        results.push(StatementResult::Summary(ExecutionSummary {
                            rows_affected: Some(n),
                            command_tag: None,
                        }));
                    }
                }
                _ => {}
            }
        }

        if !current_columns.is_empty() {
            results.push(StatementResult::Query(QueryResult {
                columns: std::mem::take(&mut current_columns),
                rows: std::mem::take(&mut current_rows),
            }));
        }

        Ok(results)
    }

    async fn ping(&mut self) -> Result<(), CoreError> {
        self.client
            .execute("SELECT 1", &[])
            .await
            .map_err(|e| CoreError::ConnectionFailed(e.to_string()))?;
        Ok(())
    }

    async fn list_tables(&mut self, schema: Option<&str>) -> Result<Vec<String>, CoreError> {
        let schema = schema.unwrap_or("public");
        let rows = self
            .client
            .query(
                "SELECT table_name FROM information_schema.tables WHERE table_schema = $1 AND table_type = 'BASE TABLE' ORDER BY table_name",
                &[&schema,
                ],
            )
            .await
            .map_err(|e| CoreError::QueryFailed(e.to_string()))?;
        let names = rows
            .into_iter()
            .map(|row| row.get::<_, String>(0))
            .collect();
        Ok(names)
    }

    async fn describe_table(
        &mut self,
        schema: Option<&str>,
        table: &str,
    ) -> Result<QueryResult, CoreError> {
        let schema = schema.unwrap_or("public");
        let rows = self
            .client
            .query(
                "SELECT column_name, data_type, is_nullable, column_default, numeric_precision, numeric_scale FROM information_schema.columns WHERE table_schema = $1 AND table_name = $2 ORDER BY ordinal_position",
                &[&schema,
                    &table,
                ],
            )
            .await
            .map_err(|e| CoreError::QueryFailed(e.to_string()))?;

        let columns = vec![
            ColumnInfo {
                name: "column_name".to_string(),
                type_hint: TypeHint::String,
                nullable: true,
            },
            ColumnInfo {
                name: "data_type".to_string(),
                type_hint: TypeHint::String,
                nullable: true,
            },
            ColumnInfo {
                name: "is_nullable".to_string(),
                type_hint: TypeHint::String,
                nullable: true,
            },
            ColumnInfo {
                name: "column_default".to_string(),
                type_hint: TypeHint::String,
                nullable: true,
            },
            ColumnInfo {
                name: "numeric_precision".to_string(),
                type_hint: TypeHint::Int64,
                nullable: true,
            },
            ColumnInfo {
                name: "numeric_scale".to_string(),
                type_hint: TypeHint::Int64,
                nullable: true,
            },
        ];

        let data_rows: Vec<Row> = rows
            .iter()
            .map(|row| {
                vec![
                    row.try_get::<_, Option<String>>("column_name")
                        .unwrap_or(None)
                        .map(Value::String)
                        .unwrap_or(Value::Null),
                    row.try_get::<_, Option<String>>("data_type")
                        .unwrap_or(None)
                        .map(Value::String)
                        .unwrap_or(Value::Null),
                    row.try_get::<_, Option<String>>("is_nullable")
                        .unwrap_or(None)
                        .map(Value::String)
                        .unwrap_or(Value::Null),
                    row.try_get::<_, Option<String>>("column_default")
                        .unwrap_or(None)
                        .map(Value::String)
                        .unwrap_or(Value::Null),
                    row.try_get::<_, Option<i32>>("numeric_precision")
                        .unwrap_or(None)
                        .map(|v| Value::Int64(i64::from(v)))
                        .unwrap_or(Value::Null),
                    row.try_get::<_, Option<i32>>("numeric_scale")
                        .unwrap_or(None)
                        .map(|v| Value::Int64(i64::from(v)))
                        .unwrap_or(Value::Null),
                ]
            })
            .collect();

        Ok(QueryResult {
            columns,
            rows: data_rows,
        })
    }

    async fn bulk_insert_rows(
        &mut self,
        target: BulkInsert<'_>,
    ) -> Result<usize, CoreError> {
        if target.rows.is_empty() {
            return Ok(0);
        }
        // COPY ... FROM STDIN bypasses parse/plan per-row, so even
        // single-byte payloads see the speedup. The Phase 1
        // dispatcher already short-circuits empty batches before
        // calling here, but defend in depth.
        let table = crate::copy::quote_identifier(target.table, crate::backend::Backend::Postgres);
        let cols = target
            .columns
            .iter()
            .map(|c| crate::copy::quote_identifier(&c.name, crate::backend::Backend::Postgres))
            .collect::<Vec<_>>()
            .join(", ");
        let stmt = format!("COPY {table} ({cols}) FROM STDIN WITH (FORMAT TEXT)");

        let sink = self
            .client
            .copy_in::<_, Bytes>(stmt.as_str())
            .await
            .map_err(|e| pg_text_copy::classify_copy_error(&e))?;
        tokio::pin!(sink);

        // Render each row into one tab-separated line and stream
        // into the sink one row at a time. Buffering inside Bytes
        // is small (one row per allocation); CopyInSink will batch
        // these into network frames internally.
        let hints: Vec<TypeHint> = target.columns.iter().map(|c| c.type_hint).collect();
        for row in target.rows {
            let buf = pg_text_copy::encode_row(row, &hints)?;
            sink.send(buf)
                .await
                .map_err(|e| CoreError::QueryFailed(format!("COPY send: {e}")))?;
        }

        let rows = sink
            .as_mut()
            .finish()
            .await
            .map_err(|e| CoreError::QueryFailed(format!("COPY finish: {e}")))?;
        Ok(rows as usize)
    }
}

/// Postgres TEXT-COPY encoder.
///
/// Each row becomes one tab-separated, newline-terminated line in
/// the wire format documented at
/// <https://www.postgresql.org/docs/current/sql-copy.html#id-1.9.3.55.9.2>.
/// Notable rules:
///   - `NULL` is the two-char sequence `\N` (backslash + capital N).
///   - Field text escapes: `\` → `\\`, `\t` → `\\t`, `\n` → `\\n`,
///     `\r` → `\\r`, `\0` is invalid.
///   - Backslash MUST be escaped first, otherwise a literal `\.` at
///     the start of a logical line would be parsed as the end-of-data
///     marker and truncate the stream.
///   - BYTEA goes in as `\x` + lowercase hex.
///   - BOOLEAN is `t` / `f`.
///   - JSON/JSONB receives the compact `serde_json::to_string` form,
///     then the same text escapes.
mod pg_text_copy {
    use crate::error::CoreError;
    use crate::value::{TypeHint, Value};
    use bytes::Bytes;

    /// Encode one row into a single `Bytes` payload ready to send.
    /// `hints` is the destination column type for each cell;
    /// currently only used to route `Value::Json` through compact
    /// JSON serialization, but kept in the signature so binary COPY
    /// (a future opt-in) can swap encoders without changing callers.
    pub fn encode_row(row: &[Value], hints: &[TypeHint]) -> Result<Bytes, CoreError> {
        // Pre-size: average ~8 bytes/cell + tabs/newline. Will grow.
        let mut buf = String::with_capacity(row.len() * 12 + 1);
        for (i, value) in row.iter().enumerate() {
            if i > 0 {
                buf.push('\t');
            }
            let hint = hints.get(i).copied().unwrap_or(TypeHint::Other);
            encode_value(&mut buf, value, hint)?;
        }
        buf.push('\n');
        Ok(Bytes::from(buf.into_bytes()))
    }

    fn encode_value(out: &mut String, v: &Value, hint: TypeHint) -> Result<(), CoreError> {
        match v {
            Value::Null => out.push_str("\\N"),
            Value::Bool(b) => out.push(if *b { 't' } else { 'f' }),
            Value::Int64(n) => {
                use std::fmt::Write;
                let _ = write!(out, "{n}");
            }
            Value::Float64(f) => {
                if f.is_nan() {
                    out.push_str("NaN");
                } else if f.is_infinite() {
                    out.push_str(if *f > 0.0 { "Infinity" } else { "-Infinity" });
                } else {
                    use std::fmt::Write;
                    let _ = write!(out, "{f}");
                }
            }
            Value::Decimal(s) => push_escaped(out, s),
            Value::String(s) => push_escaped(out, s),
            Value::Bytes(b) => {
                out.push_str("\\\\x");
                use std::fmt::Write;
                for byte in b {
                    let _ = write!(out, "{byte:02x}");
                }
            }
            Value::Date(d) => {
                use std::fmt::Write;
                let _ = write!(out, "{d}");
            }
            Value::Time(t) => {
                use std::fmt::Write;
                let _ = write!(out, "{t}");
            }
            Value::DateTime(dt) => {
                // Postgres `TIMESTAMP` (without TZ) accepts ISO-8601
                // YYYY-MM-DDTHH:MM:SS[.fff]. Chrono's NaiveDateTime
                // Display already emits exactly that.
                use std::fmt::Write;
                let _ = write!(out, "{dt}");
            }
            Value::DateTimeTz(dt) => {
                // Postgres `TIMESTAMPTZ` accepts RFC 3339.
                out.push_str(&dt.to_rfc3339());
            }
            Value::Json(j) => {
                let rendered = serde_json::to_string(j).map_err(|e| {
                    CoreError::QueryFailed(format!("PG bulk: JSON serialize: {e}"))
                })?;
                push_escaped(out, &rendered);
            }
            Value::Uuid(s) => push_escaped(out, s),
            Value::Array(a) => {
                // ferrule's DDL translator maps Array → JSONB on PG, so
                // serialize as JSON. Native PG arrays (`int[]`, `text[]`)
                // are out of scope until DDL translation grows a real
                // array type — file separately if needed.
                let _ = hint; // reserved for future binary-COPY routing
                let rendered = serde_json::to_string(a).map_err(|e| {
                    CoreError::QueryFailed(format!("PG bulk: array serialize: {e}"))
                })?;
                push_escaped(out, &rendered);
            }
        }
        Ok(())
    }

    /// Apply PG text-COPY string escapes. Backslash MUST be escaped
    /// first — see module docs.
    fn push_escaped(out: &mut String, s: &str) {
        for ch in s.chars() {
            match ch {
                '\\' => out.push_str("\\\\"),
                '\t' => out.push_str("\\t"),
                '\n' => out.push_str("\\n"),
                '\r' => out.push_str("\\r"),
                '\0' => {
                    // Postgres rejects null bytes inside text columns.
                    // Encode as the explicit replacement; downstream
                    // INSERT path would have rejected this too.
                    out.push_str("\\x00");
                }
                other => out.push(other),
            }
        }
    }

    /// Classify a `tokio_postgres::Error` raised by `copy_in`.
    /// Returns [`CoreError::BulkUnavailable`] only when the error
    /// names a *recoverable* condition (target is a non-table
    /// relation that COPY refuses but a generic INSERT with rules
    /// or INSTEAD OF triggers can target), so the Auto dispatcher
    /// can fall back. Everything else surfaces as `QueryFailed`
    /// because a fallback after a partial bulk send would
    /// double-insert.
    ///
    /// SQLSTATE-based rather than substring-based: PG raises
    /// `wrong_object_type` (42809) when COPY is issued against a
    /// view / mat view / foreign table / sequence. Substring
    /// matching on the English error message (`"cannot copy
    /// to/from"`) was previously used but is fragile across server
    /// locales and minor version wording changes.
    pub fn classify_copy_error(e: &tokio_postgres::Error) -> CoreError {
        use tokio_postgres::error::SqlState;
        if let Some(code) = e.code() {
            if *code == SqlState::WRONG_OBJECT_TYPE {
                return CoreError::BulkUnavailable(format!("PG rejected COPY: {e}"));
            }
        }
        CoreError::QueryFailed(format!("COPY setup: {e}"))
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use chrono::{NaiveDate, NaiveDateTime, NaiveTime, TimeZone, Utc};

        fn enc1(v: Value, hint: TypeHint) -> String {
            let bytes = encode_row(&[v], &[hint]).expect("encode_row");
            // Trim the trailing newline so tests assert on the field content.
            let s = std::str::from_utf8(&bytes).unwrap().to_string();
            assert!(s.ends_with('\n'));
            s.trim_end_matches('\n').to_string()
        }

        #[test]
        fn encode_null_is_backslash_n() {
            assert_eq!(enc1(Value::Null, TypeHint::Null), "\\N");
        }

        #[test]
        fn encode_bool_is_t_or_f() {
            assert_eq!(enc1(Value::Bool(true), TypeHint::Bool), "t");
            assert_eq!(enc1(Value::Bool(false), TypeHint::Bool), "f");
        }

        #[test]
        fn encode_int_and_float() {
            assert_eq!(enc1(Value::Int64(42), TypeHint::Int64), "42");
            assert_eq!(enc1(Value::Int64(-7), TypeHint::Int64), "-7");
            assert_eq!(enc1(Value::Float64(1.5), TypeHint::Float64), "1.5");
        }

        #[test]
        fn encode_float_nan_and_inf() {
            assert_eq!(enc1(Value::Float64(f64::NAN), TypeHint::Float64), "NaN");
            assert_eq!(
                enc1(Value::Float64(f64::INFINITY), TypeHint::Float64),
                "Infinity"
            );
            assert_eq!(
                enc1(Value::Float64(f64::NEG_INFINITY), TypeHint::Float64),
                "-Infinity"
            );
        }

        #[test]
        fn encode_string_escapes_backslash_first() {
            // Critical: a literal `\.` at the start of a logical line
            // would otherwise be parsed as the end-of-data sentinel.
            // Backslash escaped first means input `\.` → `\\.`, which
            // PG decodes back to `\.` as a normal value.
            assert_eq!(enc1(Value::String("\\.\n".into()), TypeHint::String), "\\\\.\\n");
        }

        #[test]
        fn encode_string_escapes_tab_cr_lf() {
            assert_eq!(
                enc1(Value::String("a\tb\nc\rd".into()), TypeHint::String),
                "a\\tb\\nc\\rd"
            );
        }

        #[test]
        fn encode_string_passes_through_normal_chars() {
            assert_eq!(
                enc1(Value::String("héllo, world 🐈".into()), TypeHint::String),
                "héllo, world 🐈"
            );
        }

        #[test]
        fn encode_string_replaces_nul() {
            // Postgres rejects \0 in text; emit `\x00` so the column
            // gets a printable marker. Downstream INSERT path would
            // have errored similarly — bulk and generic agree.
            assert_eq!(
                enc1(Value::String("a\0b".into()), TypeHint::String),
                "a\\x00b"
            );
        }

        #[test]
        fn encode_bytes_is_hex_with_double_backslash_x() {
            // Field-level `\x` prefix would itself be interpreted by
            // PG; the encoder emits the literal characters `\`, `x`,
            // and the hex pairs. PG's text-COPY parser then sees a
            // BYTEA-shaped value once unescaped.
            assert_eq!(
                enc1(Value::Bytes(vec![0xDE, 0xAD, 0xBE, 0xEF]), TypeHint::Bytes),
                "\\\\xdeadbeef"
            );
        }

        #[test]
        fn encode_date_time_datetime() {
            let d = NaiveDate::from_ymd_opt(2026, 5, 14).unwrap();
            let t = NaiveTime::from_hms_opt(12, 34, 56).unwrap();
            let dt = NaiveDateTime::new(d, t);
            assert_eq!(enc1(Value::Date(d), TypeHint::Date), "2026-05-14");
            assert_eq!(enc1(Value::Time(t), TypeHint::Time), "12:34:56");
            assert_eq!(enc1(Value::DateTime(dt), TypeHint::DateTime), "2026-05-14 12:34:56");
        }

        #[test]
        fn encode_datetimetz_is_rfc3339() {
            let dt = Utc.with_ymd_and_hms(2026, 5, 14, 12, 34, 56).unwrap();
            assert_eq!(
                enc1(Value::DateTimeTz(dt), TypeHint::DateTimeTz),
                "2026-05-14T12:34:56+00:00"
            );
        }

        #[test]
        fn encode_json_is_compact_with_escapes() {
            let j = serde_json::json!({"role": "admin", "active": true});
            // Object key order from serde_json::json! matches source.
            let encoded = enc1(Value::Json(j), TypeHint::Json);
            // We can't predict key order, so check that the JSON
            // is compact (no spaces between key:value) and that
            // the literal quotes aren't escaped by text-COPY rules.
            assert!(encoded.contains("\"role\":\"admin\""));
            assert!(encoded.contains("\"active\":true"));
        }

        #[test]
        fn encode_uuid_passes_through() {
            assert_eq!(
                enc1(
                    Value::Uuid("550e8400-e29b-41d4-a716-446655440000".into()),
                    TypeHint::Uuid
                ),
                "550e8400-e29b-41d4-a716-446655440000"
            );
        }

        #[test]
        fn encode_array_is_compact_json() {
            let a = Value::Array(vec![Value::Int64(1), Value::Int64(2), Value::Int64(3)]);
            assert_eq!(enc1(a, TypeHint::Array), "[1,2,3]");
        }

        #[test]
        fn encode_decimal_passes_through_with_escapes() {
            assert_eq!(
                enc1(Value::Decimal("99.5".into()), TypeHint::Decimal),
                "99.5"
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
            let hints = vec![TypeHint::Int64, TypeHint::String, TypeHint::Null, TypeHint::Bool];
            let bytes = encode_row(&row, &hints).unwrap();
            assert_eq!(
                std::str::from_utf8(&bytes).unwrap(),
                "1\tAlice\t\\N\tt\n"
            );
        }

        #[test]
        fn encode_row_empty_row_is_just_newline() {
            // A genuinely zero-column row is degenerate but the
            // encoder must not panic. PG won't accept it but the
            // dispatcher short-circuits empty *batches* before
            // calling here, not empty *rows*.
            let bytes = encode_row(&[], &[]).unwrap();
            assert_eq!(std::str::from_utf8(&bytes).unwrap(), "\n");
        }
    }
}

pub async fn connect(
    url: &DatabaseUrl,
    opts: &ConnectOptions,
) -> Result<PostgresConnection, CoreError> {
    let config = match url.raw().parse::<tokio_postgres::Config>() {
        Ok(cfg) => cfg,
        Err(_) => build_config_from_url(url)?,
    };

    let tls_connector = build_tls_connector(opts)
        .await
        .map_err(CoreError::TlsError)?;

    let (client, connection) = config
        .connect(tls_connector)
        .await
        .map_err(|e| CoreError::ConnectionFailed(e.to_string()))?;

    tokio::spawn(async move {
        if let Err(e) = connection.await {
            eprintln!("[ferrule] Postgres background connection error: {}", e);
        }
    });

    Ok(PostgresConnection { client })
}

/// Connect over a pre-built `AsyncRead + AsyncWrite` stream
/// instead of opening a TCP socket. Used by the SSH tunnel `Stream`
/// transport and by HTTP CONNECT proxy direct DB connections:
/// tokio-postgres negotiates Postgres protocol (and TLS, if
/// `sslmode` requires it) end-to-end through the supplied stream.
///
/// Reuses the same TLS connector logic as [`connect`], so a URL like
/// `postgres://app:pwd@db/myapp?sslmode=require` gets SSH transport
/// (or proxy) AND TLS to the database — the two layers compose.
pub async fn connect_with_stream<S>(
    url: &DatabaseUrl,
    opts: &ConnectOptions,
    stream: S,
) -> Result<PostgresConnection, CoreError>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    use tokio_postgres::tls::MakeTlsConnect;

    let config = match url.raw().parse::<tokio_postgres::Config>() {
        Ok(cfg) => cfg,
        Err(_) => build_config_from_url(url)?,
    };

    let mut make_tls = build_tls_connector(opts)
        .await
        .map_err(CoreError::TlsError)?;
    let hostname = url.host().unwrap_or("localhost");
    let tls = <tokio_postgres_rustls::MakeRustlsConnect as MakeTlsConnect<S>>::make_tls_connect(
        &mut make_tls,
        hostname,
    )
    .map_err(|e| CoreError::TlsError(format!("make_tls_connect failed: {e:?}")))?;

    let (client, connection) = config
        .connect_raw(stream, tls)
        .await
        .map_err(|e| CoreError::ConnectionFailed(e.to_string()))?;

    tokio::spawn(async move {
        if let Err(e) = connection.await {
            eprintln!("[ferrule] Postgres background connection error: {}", e);
        }
    });

    Ok(PostgresConnection { client })
}

fn build_config_from_url(url: &DatabaseUrl) -> Result<tokio_postgres::Config, CoreError> {
    let mut config = tokio_postgres::Config::new();
    if let Some(host) = url.host() {
        config.host(host);
    } else {
        config.host("localhost");
    }
    config.port(url.port().unwrap_or(5432));
    if !url.username().is_empty() {
        config.user(url.username());
    }
    if let Some(pwd) = url.password() {
        config.password(pwd.expose_secret());
    }
    if !url.database().is_empty() {
        config.dbname(url.database());
    }
    Ok(config)
}

async fn build_tls_connector(
    opts: &ConnectOptions,
) -> Result<tokio_postgres_rustls::MakeRustlsConnect, String> {
    use rustls::{ClientConfig, RootCertStore};

    let mut root_store = RootCertStore::empty();
    root_store.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());

    let config = if opts.insecure {
        let verifier = Arc::new(InsecureVerifier);
        ClientConfig::builder()
            .dangerous()
            .with_custom_certificate_verifier(verifier)
            .with_no_client_auth()
    } else {
        ClientConfig::builder()
            .with_root_certificates(root_store)
            .with_no_client_auth()
    };

    Ok(tokio_postgres_rustls::MakeRustlsConnect::new(config))
}

/// A rustls certificate verifier that accepts any certificate.
/// Used when the user passes `--insecure`.
#[derive(Debug)]
struct InsecureVerifier;

impl rustls::client::danger::ServerCertVerifier for InsecureVerifier {
    fn verify_server_cert(
        &self,
        _end_entity: &rustls::pki_types::CertificateDer<'_>,
        _intermediates: &[rustls::pki_types::CertificateDer<'_>],
        _server_name: &rustls::pki_types::ServerName<'_>,
        _ocsp_response: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &rustls::pki_types::CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &rustls::pki_types::CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        vec![
            rustls::SignatureScheme::ECDSA_NISTP256_SHA256,
            rustls::SignatureScheme::ECDSA_NISTP384_SHA384,
            rustls::SignatureScheme::RSA_PSS_SHA256,
            rustls::SignatureScheme::RSA_PSS_SHA384,
            rustls::SignatureScheme::RSA_PSS_SHA512,
            rustls::SignatureScheme::RSA_PKCS1_SHA256,
            rustls::SignatureScheme::RSA_PKCS1_SHA384,
            rustls::SignatureScheme::RSA_PKCS1_SHA512,
            rustls::SignatureScheme::ED25519,
        ]
    }
}

fn pg_type_to_hint(ty: &Type) -> TypeHint {
    match ty {
        &Type::BOOL => TypeHint::Bool,
        &Type::INT2 | &Type::INT4 | &Type::INT8 => TypeHint::Int64,
        &Type::FLOAT4 | &Type::FLOAT8 => TypeHint::Float64,
        &Type::NUMERIC => TypeHint::Decimal,
        &Type::TEXT | &Type::VARCHAR | &Type::BPCHAR | &Type::NAME => TypeHint::String,
        &Type::BYTEA => TypeHint::Bytes,
        &Type::DATE => TypeHint::Date,
        &Type::TIME => TypeHint::Time,
        &Type::TIMESTAMP => TypeHint::DateTime,
        &Type::TIMESTAMPTZ => TypeHint::DateTimeTz,
        &Type::JSON | &Type::JSONB => TypeHint::Json,
        &Type::UUID => TypeHint::Uuid,
        _ if ty.name().starts_with('_') => TypeHint::Array,
        _ => TypeHint::Other,
    }
}

fn pg_to_value(row: &tokio_postgres::Row, col: usize, pg_type: &Type) -> Value {
    use tokio_postgres::types::Type;

    // For nullable types, try Option first
    match pg_type {
        &Type::BOOL => row
            .try_get::<_, Option<bool>>(col)
            .unwrap_or(None)
            .map(Value::Bool)
            .unwrap_or(Value::Null),
        &Type::INT2 => row
            .try_get::<_, Option<i16>>(col)
            .unwrap_or(None)
            .map(|v| Value::Int64(i64::from(v)))
            .unwrap_or(Value::Null),
        &Type::INT4 => row
            .try_get::<_, Option<i32>>(col)
            .unwrap_or(None)
            .map(|v| Value::Int64(i64::from(v)))
            .unwrap_or(Value::Null),
        &Type::INT8 => row
            .try_get::<_, Option<i64>>(col)
            .unwrap_or(None)
            .map(Value::Int64)
            .unwrap_or(Value::Null),
        &Type::FLOAT4 => row
            .try_get::<_, Option<f32>>(col)
            .unwrap_or(None)
            .map(|v| Value::Float64(f64::from(v)))
            .unwrap_or(Value::Null),
        &Type::FLOAT8 => row
            .try_get::<_, Option<f64>>(col)
            .unwrap_or(None)
            .map(Value::Float64)
            .unwrap_or(Value::Null),
        &Type::NUMERIC => row
            .try_get::<_, Option<rust_decimal::Decimal>>(col)
            .unwrap_or(None)
            .map(|d| Value::Decimal(d.to_string()))
            .unwrap_or(Value::Null),
        &Type::TEXT | &Type::VARCHAR | &Type::BPCHAR | &Type::NAME => row
            .try_get::<_, Option<String>>(col)
            .unwrap_or(None)
            .map(Value::String)
            .unwrap_or(Value::Null),
        &Type::BYTEA => row
            .try_get::<_, Option<Vec<u8>>>(col)
            .unwrap_or(None)
            .map(Value::Bytes)
            .unwrap_or(Value::Null),
        &Type::DATE => row
            .try_get::<_, Option<chrono::NaiveDate>>(col)
            .unwrap_or(None)
            .map(Value::Date)
            .unwrap_or(Value::Null),
        &Type::TIME => row
            .try_get::<_, Option<chrono::NaiveTime>>(col)
            .unwrap_or(None)
            .map(Value::Time)
            .unwrap_or(Value::Null),
        &Type::TIMESTAMP => row
            .try_get::<_, Option<chrono::NaiveDateTime>>(col)
            .unwrap_or(None)
            .map(Value::DateTime)
            .unwrap_or(Value::Null),
        &Type::TIMESTAMPTZ => row
            .try_get::<_, Option<chrono::DateTime<chrono::Utc>>>(col)
            .unwrap_or(None)
            .map(Value::DateTimeTz)
            .unwrap_or(Value::Null),
        &Type::JSON | &Type::JSONB => row
            .try_get::<_, Option<serde_json::Value>>(col)
            .unwrap_or(None)
            .map(Value::Json)
            .unwrap_or(Value::Null),
        &Type::UUID => row
            .try_get::<_, Option<uuid::Uuid>>(col)
            .unwrap_or(None)
            .map(|u| Value::Uuid(u.to_string()))
            .unwrap_or(Value::Null),
        // Arrays and anything else: fall back to string representation
        _ => row
            .try_get::<_, Option<String>>(col)
            .unwrap_or(None)
            .map(Value::String)
            .unwrap_or(Value::Null),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Container URL pinned in CLAUDE.md "How to Test → Postgres". Tests skip
    /// gracefully when the container is not running, so they're safe in CI
    /// environments without docker.
    const TEST_POSTGRES_URL: &str =
        "postgres://ferrule:ferrule@127.0.0.1:15432/ferrule?sslmode=disable";

    async fn try_connect() -> Option<PostgresConnection> {
        let url = DatabaseUrl::parse(TEST_POSTGRES_URL).ok()?;
        let conn = connect(&url, &ConnectOptions::default()).await.ok()?;
        Some(conn)
    }

    #[tokio::test]
    async fn test_postgres_ping() {
        let Some(mut conn) = try_connect().await else {
            eprintln!("Postgres test container not available, skipping test_postgres_ping");
            return;
        };
        conn.ping().await.expect("ping should succeed");
    }

    #[tokio::test]
    async fn test_postgres_query() {
        let Some(mut conn) = try_connect().await else {
            eprintln!("Postgres test container not available, skipping test_postgres_query");
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
    async fn test_postgres_execute() {
        let Some(mut conn) = try_connect().await else {
            eprintln!("Postgres test container not available, skipping test_postgres_execute");
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
    async fn test_postgres_list_tables() {
        let Some(mut conn) = try_connect().await else {
            eprintln!("Postgres test container not available, skipping test_postgres_list_tables");
            return;
        };
        let tables = conn
            .list_tables(None)
            .await
            .expect("list_tables should succeed");
        assert!(
            tables.contains(&"test_users".to_string()),
            "should contain test_users, got: {tables:?}"
        );
    }

    #[tokio::test]
    async fn test_postgres_describe_table() {
        let Some(mut conn) = try_connect().await else {
            eprintln!(
                "Postgres test container not available, skipping test_postgres_describe_table"
            );
            return;
        };
        let result = conn
            .describe_table(None, "test_users")
            .await
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
                "numeric_scale",
            ]
        );
        // The seeded table has 8 columns (id/name/age/score/created_at/active/meta/uid).
        assert!(
            result.rows.len() >= 6,
            "expected at least 6 rows, got {}",
            result.rows.len()
        );
    }

    #[tokio::test]
    async fn test_postgres_type_mapping() {
        let Some(mut conn) = try_connect().await else {
            eprintln!("Postgres test container not available, skipping test_postgres_type_mapping");
            return;
        };
        let result = conn
            .query(
                "SELECT name, age, score, active, meta, uid FROM test_users \
                 WHERE name = 'Alice'",
            )
            .await
            .expect("query should succeed");
        assert_eq!(result.rows.len(), 1, "expected exactly Alice");
        let row = &result.rows[0];
        assert!(matches!(row[0], Value::String(_)), "name should be String");
        assert!(matches!(row[1], Value::Int64(_)), "age should be Int64");
        assert!(
            matches!(row[2], Value::Decimal(_) | Value::Float64(_)),
            "score (NUMERIC) should be Decimal or Float64"
        );
        assert!(matches!(row[3], Value::Bool(_)), "active should be Bool");
        assert!(
            matches!(row[4], Value::Json(_)),
            "meta (JSONB) should be Json"
        );
        assert!(matches!(row[5], Value::Uuid(_)), "uid should be Uuid");
    }

    #[tokio::test]
    async fn test_postgres_timestamptz_mapping() {
        let Some(mut conn) = try_connect().await else {
            eprintln!(
                "Postgres test container not available, skipping test_postgres_timestamptz_mapping"
            );
            return;
        };
        let result = conn
            .query("SELECT created_at FROM test_users WHERE name = 'Alice'")
            .await
            .expect("query should succeed");
        assert_eq!(result.rows.len(), 1);
        assert!(
            matches!(result.rows[0][0], Value::DateTimeTz(_)),
            "created_at (TIMESTAMPTZ) should be DateTimeTz, got {:?}",
            result.rows[0][0]
        );
    }

    /// End-to-end check that `bulk_insert_rows` actually streams
    /// through `COPY ... FROM STDIN`. Creates a scratch table per
    /// test invocation so seeded `test_users` rows are untouched.
    #[tokio::test]
    async fn test_postgres_bulk_insert_rows_round_trip() {
        let Some(mut conn) = try_connect().await else {
            eprintln!(
                "Postgres test container not available, skipping test_postgres_bulk_insert_rows_round_trip"
            );
            return;
        };

        let pid = std::process::id();
        let table = format!("ferrule_bulk_test_{pid}");
        let _ = conn
            .execute(&format!("DROP TABLE IF EXISTS {table}"))
            .await;
        conn.execute(&format!(
            "CREATE TABLE {table} (\
               id BIGINT, \
               name TEXT, \
               active BOOLEAN, \
               score DOUBLE PRECISION, \
               meta JSONB, \
               tricky TEXT\
             )"
        ))
        .await
        .expect("CREATE TABLE");

        let columns = vec![
            ColumnInfo { name: "id".into(), type_hint: TypeHint::Int64, nullable: false },
            ColumnInfo { name: "name".into(), type_hint: TypeHint::String, nullable: true },
            ColumnInfo { name: "active".into(), type_hint: TypeHint::Bool, nullable: true },
            ColumnInfo { name: "score".into(), type_hint: TypeHint::Float64, nullable: true },
            ColumnInfo { name: "meta".into(), type_hint: TypeHint::Json, nullable: true },
            ColumnInfo { name: "tricky".into(), type_hint: TypeHint::String, nullable: true },
        ];

        // Five rows. Row 3 hits the backslash/tab/newline escape
        // path that PG would otherwise misinterpret. Row 4 exercises
        // NULL in the middle of a row.
        let rows: Vec<Row> = vec![
            vec![
                Value::Int64(1),
                Value::String("Alice".into()),
                Value::Bool(true),
                Value::Float64(99.5),
                Value::Json(serde_json::json!({"role": "admin"})),
                Value::String("plain".into()),
            ],
            vec![
                Value::Int64(2),
                Value::String("Bob".into()),
                Value::Bool(false),
                Value::Float64(88.25),
                Value::Json(serde_json::json!({"role": "user"})),
                Value::String("comma,sep".into()),
            ],
            vec![
                Value::Int64(3),
                Value::String("Esc\\\t\nape".into()),
                Value::Bool(true),
                Value::Float64(0.0),
                Value::Json(serde_json::Value::Null),
                Value::String("\\.".into()),
            ],
            vec![
                Value::Int64(4),
                Value::Null,
                Value::Null,
                Value::Null,
                Value::Null,
                Value::Null,
            ],
            vec![
                Value::Int64(5),
                Value::String("nan-and-inf".into()),
                Value::Bool(true),
                Value::Float64(f64::INFINITY),
                Value::Json(serde_json::json!([1, 2, 3])),
                Value::String("héllo 🐈".into()),
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
        assert_eq!(n, 5, "bulk should return rows-accepted = 5");

        // Verify count + a couple of the tricky values made the round trip.
        let count = conn
            .query(&format!("SELECT count(*)::bigint FROM {table}"))
            .await
            .unwrap();
        assert!(matches!(count.rows[0][0], Value::Int64(5)));

        let r3 = conn
            .query(&format!(
                "SELECT name, tricky FROM {table} WHERE id = 3"
            ))
            .await
            .unwrap();
        assert_eq!(r3.rows.len(), 1);
        if let Value::String(name) = &r3.rows[0][0] {
            assert_eq!(name, "Esc\\\t\nape", "row 3 name should round-trip with raw bytes");
        } else {
            panic!("row 3 name should be String, got {:?}", r3.rows[0][0]);
        }
        if let Value::String(tricky) = &r3.rows[0][1] {
            assert_eq!(tricky, "\\.", "row 3 tricky should be literal backslash-dot");
        } else {
            panic!("row 3 tricky should be String, got {:?}", r3.rows[0][1]);
        }

        // Row 4 — all NULL columns except id.
        let r4 = conn
            .query(&format!("SELECT name, active FROM {table} WHERE id = 4"))
            .await
            .unwrap();
        assert!(matches!(r4.rows[0][0], Value::Null));
        assert!(matches!(r4.rows[0][1], Value::Null));

        // Cleanup.
        conn.execute(&format!("DROP TABLE {table}"))
            .await
            .expect("DROP TABLE");
    }
}
