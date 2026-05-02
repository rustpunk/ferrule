use crate::connection::{
    ConnectOptions, Connection, ExecutionSummary, QueryResult, StatementResult,
};
use crate::error::CoreError;
use crate::url::DatabaseUrl;
use crate::value::{ColumnInfo, Row, TypeHint, Value};
use async_trait::async_trait;
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
}
