use async_trait::async_trait;
use crate::connection::{Connection, ConnectOptions, ExecutionSummary, QueryResult, StatementResult};
use crate::error::CoreError;
use crate::url::DatabaseUrl;
use crate::value::{ColumnInfo, Row, TypeHint, Value};
use chrono::{DateTime as ChronoDateTime, FixedOffset, NaiveDate, NaiveDateTime, NaiveTime, Utc};
use secrecy::ExposeSecret;
use tiberius::{Client, ColumnType, EncryptionLevel};
use tokio::net::TcpStream;
use tokio_util::compat::TokioAsyncWriteCompatExt;

pub struct MssqlConnection {
    client: Client<tokio_util::compat::Compat<TcpStream>>,
}

#[async_trait]
impl Connection for MssqlConnection {
    async fn execute(&mut self, sql: &str) -> Result<ExecutionSummary, CoreError> {
        let result = self.client.execute(sql, &[])
            .await
            .map_err(|e| CoreError::QueryFailed(e.to_string()))?;
        let affected = result.rows_affected().first().copied();
        Ok(ExecutionSummary {
            rows_affected: affected,
            command_tag: None,
        })
    }

    async fn query(&mut self, sql: &str) -> Result<QueryResult, CoreError> {
        let rows = self.client.query(sql, &[])
            .await
            .map_err(|e| CoreError::QueryFailed(e.to_string()))?
            .into_first_result()
            .await
            .map_err(|e| CoreError::QueryFailed(e.to_string()))?;

        if rows.is_empty() {
            return Ok(QueryResult {
                columns: Vec::new(),
                rows: Vec::new(),
            });
        }

        let columns: Vec<ColumnInfo> = rows[0].columns().iter().map(|c| ColumnInfo {
            name: c.name().to_string(),
            type_hint: mssql_type_to_hint(c.column_type()),
            nullable: true,
        }).collect();

        let data_rows: Vec<Row> = rows.into_iter().map(|row| {
            row.columns().iter().enumerate()
                .map(|(i, col)| mssql_to_value(&row, i, col.column_type()))
                .collect()
        }).collect();

        Ok(QueryResult { columns, rows: data_rows })
    }

    async fn execute_multi(&mut self, sql: &str) -> Result<Vec<StatementResult>, CoreError> {
        let result_sets = self.client.query(sql, &[])
            .await
            .map_err(|e| CoreError::QueryFailed(e.to_string()))?
            .into_results()
            .await
            .map_err(|e| CoreError::QueryFailed(e.to_string()))?;

        let mut results = Vec::new();
        for rows in result_sets {
            if rows.is_empty() {
                results.push(StatementResult::Query(QueryResult {
                    columns: Vec::new(),
                    rows: Vec::new(),
                }));
                continue;
            }
            let columns: Vec<ColumnInfo> = rows[0].columns().iter().map(|c| ColumnInfo {
                name: c.name().to_string(),
                type_hint: mssql_type_to_hint(c.column_type()),
                nullable: true,
            }).collect();

            let data_rows: Vec<Row> = rows.into_iter().map(|row| {
                row.columns().iter().enumerate()
                    .map(|(i, col)| mssql_to_value(&row, i, col.column_type()))
                    .collect()
            }).collect();

            results.push(StatementResult::Query(QueryResult { columns, rows: data_rows }));
        }

        if results.is_empty() {
            let summary = self.execute(sql).await?;
            results.push(StatementResult::Summary(summary));
        }

        Ok(results)
    }

    async fn ping(&mut self) -> Result<(), CoreError> {
        self.client.query("SELECT 1", &[])
            .await
            .map_err(|e| CoreError::ConnectionFailed(e.to_string()))?
            .into_first_result()
            .await
            .map_err(|e| CoreError::ConnectionFailed(e.to_string()))?;
        Ok(())
    }

    async fn list_tables(&mut self, schema: Option<&str>) -> Result<Vec<String>, CoreError> {
        let schema = schema.unwrap_or("dbo");
        let sql = format!(
            "SELECT TABLE_NAME AS table_name FROM information_schema.tables WHERE table_schema = '{}' AND table_type = 'BASE TABLE' ORDER BY table_name",
            escape_mssql_string(schema)
        );
        let result = self.query(&sql).await?;
        let names: Vec<String> = result.rows.into_iter()
            .filter_map(|row| row.into_iter().next().and_then(|v| match v {
                Value::String(s) => Some(s),
                _ => None,
            }))
            .collect();
        Ok(names)
    }

    async fn describe_table(
        &mut self,
        schema: Option<&str>,
        table: &str,
    ) -> Result<QueryResult, CoreError> {
        let schema = schema.unwrap_or("dbo");
        let sql = format!(
            "SELECT COLUMN_NAME AS column_name, DATA_TYPE AS data_type, IS_NULLABLE AS is_nullable, COLUMN_DEFAULT AS column_default, NUMERIC_PRECISION AS numeric_precision, NUMERIC_SCALE AS numeric_scale FROM information_schema.columns WHERE table_schema = '{}' AND table_name = '{}' ORDER BY ORDINAL_POSITION",
            escape_mssql_string(schema),
            escape_mssql_string(table)
        );
        self.query(&sql).await
    }
}

pub async fn connect(url: &DatabaseUrl, opts: &ConnectOptions) -> Result<MssqlConnection, CoreError> {
    let mut config = tiberius::Config::new();
    config.host(url.host().unwrap_or("localhost"));
    config.port(url.port().unwrap_or(1433));

    if !url.username().is_empty() {
        let password = url.password()
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
    if let Some(trust) = params.get("trust_server_certificate").or_else(|| params.get("trustServerCertificate")) {
        if trust == "true" || trust == "yes" || trust == "1" {
            config.trust_cert();
        }
    }

    let tcp = tokio::net::TcpStream::connect(config.get_addr())
        .await
        .map_err(|e| CoreError::ConnectionFailed(e.to_string()))?;
    tcp.set_nodelay(true)
        .map_err(|e| CoreError::ConnectionFailed(e.to_string()))?;

    let client = tiberius::Client::connect(config, tcp.compat_write())
        .await
        .map_err(|e| CoreError::ConnectionFailed(e.to_string()))?;

    Ok(MssqlConnection { client })
}

fn mssql_type_to_hint(col_type: ColumnType) -> TypeHint {
    match col_type {
        ColumnType::Bit | ColumnType::Bitn => TypeHint::Bool,
        ColumnType::Int1 | ColumnType::Int2 | ColumnType::Int4 | ColumnType::Int8 | ColumnType::Intn => TypeHint::Int64,
        ColumnType::Float4 | ColumnType::Float8 | ColumnType::Floatn => TypeHint::Float64,
        ColumnType::Decimaln | ColumnType::Numericn | ColumnType::Money | ColumnType::Money4 => TypeHint::Decimal,
        ColumnType::BigVarChar | ColumnType::BigChar | ColumnType::NVarchar | ColumnType::NChar | ColumnType::Text | ColumnType::NText | ColumnType::Xml => TypeHint::String,
        ColumnType::BigVarBin | ColumnType::BigBinary | ColumnType::Image => TypeHint::Bytes,
        ColumnType::Datetime4 | ColumnType::Datetime | ColumnType::Datetimen | ColumnType::Datetime2 => TypeHint::DateTime,
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
        ColumnType::Bit | ColumnType::Bitn => {
            opt(row.try_get::<bool, _>(idx)).map(Value::Bool).unwrap_or(Value::Null)
        }
        ColumnType::Int1 => {
            opt(row.try_get::<u8, _>(idx)).map(|v| Value::Int64(v as i64)).unwrap_or(Value::Null)
        }
        ColumnType::Int2 => {
            opt(row.try_get::<i16, _>(idx)).map(|v| Value::Int64(v as i64)).unwrap_or(Value::Null)
        }
        ColumnType::Int4 => {
            opt(row.try_get::<i32, _>(idx)).map(|v| Value::Int64(v as i64)).unwrap_or(Value::Null)
        }
        ColumnType::Int8 => {
            opt(row.try_get::<i64, _>(idx)).map(Value::Int64).unwrap_or(Value::Null)
        }
        ColumnType::Intn => {
            opt(row.try_get::<i64, _>(idx)).map(Value::Int64)
                .or_else(|| opt(row.try_get::<i32, _>(idx)).map(|v| Value::Int64(v as i64)))
                .or_else(|| opt(row.try_get::<i16, _>(idx)).map(|v| Value::Int64(v as i64)))
                .or_else(|| opt(row.try_get::<u8, _>(idx)).map(|v| Value::Int64(v as i64)))
                .unwrap_or(Value::Null)
        }
        ColumnType::Float4 => {
            opt(row.try_get::<f32, _>(idx)).map(|v| Value::Float64(v as f64)).unwrap_or(Value::Null)
        }
        ColumnType::Float8 => {
            opt(row.try_get::<f64, _>(idx)).map(Value::Float64).unwrap_or(Value::Null)
        }
        ColumnType::Floatn => {
            opt(row.try_get::<f64, _>(idx)).map(Value::Float64)
                .or_else(|| opt(row.try_get::<f32, _>(idx)).map(|v| Value::Float64(v as f64)))
                .unwrap_or(Value::Null)
        }
        ColumnType::Money | ColumnType::Money4 => {
            opt(row.try_get::<f64, _>(idx))
                .map(|v| Value::Decimal(format!("{:.4}", v)))
                .unwrap_or(Value::Null)
        }
        ColumnType::Decimaln | ColumnType::Numericn => {
            opt(row.try_get::<tiberius::numeric::Numeric, _>(idx))
                .map(|v| Value::Decimal(v.to_string()))
                .unwrap_or(Value::Null)
        }
        ColumnType::BigVarChar | ColumnType::BigChar | ColumnType::NVarchar | ColumnType::NChar | ColumnType::Text | ColumnType::NText => {
            opt(row.try_get::<&str, _>(idx)).map(|v| Value::String(v.to_string())).unwrap_or(Value::Null)
        }
        ColumnType::Xml => {
            opt(row.try_get::<&tiberius::xml::XmlData, _>(idx))
                .map(|v| Value::String(v.to_string()))
                .unwrap_or(Value::Null)
        }
        ColumnType::BigVarBin | ColumnType::BigBinary | ColumnType::Image => {
            opt(row.try_get::<&[u8], _>(idx)).map(|v| Value::Bytes(v.to_vec())).unwrap_or(Value::Null)
        }
        ColumnType::Guid => {
            opt(row.try_get::<tiberius::Uuid, _>(idx))
                .map(|v| Value::Uuid(v.to_string()))
                .unwrap_or(Value::Null)
        }
        ColumnType::Datetime4 | ColumnType::Datetime | ColumnType::Datetimen | ColumnType::Datetime2 => {
            opt(row.try_get::<NaiveDateTime, _>(idx)).map(Value::DateTime).unwrap_or(Value::Null)
        }
        ColumnType::Daten => {
            opt(row.try_get::<NaiveDate, _>(idx)).map(Value::Date).unwrap_or(Value::Null)
        }
        ColumnType::Timen => {
            opt(row.try_get::<NaiveTime, _>(idx)).map(Value::Time).unwrap_or(Value::Null)
        }
        ColumnType::DatetimeOffsetn => {
            opt(row.try_get::<ChronoDateTime<FixedOffset>, _>(idx))
                .map(|v| Value::DateTimeTz(v.with_timezone(&Utc)))
                .or_else(|| opt(row.try_get::<ChronoDateTime<Utc>, _>(idx)).map(Value::DateTimeTz))
                .unwrap_or(Value::Null)
        }
        ColumnType::Udt | ColumnType::SSVariant => {
            opt(row.try_get::<&str, _>(idx)).map(|v| Value::String(v.to_string())).unwrap_or(Value::Null)
        }
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

    const TEST_MSSQL_URL: &str = "mssql://sa:Ferrule123!@127.0.0.1:11433/ferrule?trustServerCertificate=true";

    async fn try_connect() -> Option<MssqlConnection> {
        let url = DatabaseUrl::parse(TEST_MSSQL_URL).ok()?;
        let conn = connect(&url, &ConnectOptions::default()).await.ok()?;
        Some(conn)
    }

    #[tokio::test]
    async fn test_mssql_ping() {
        let Some(mut conn) = try_connect().await else {
            eprintln!("MSSQL test container not available, skipping test_mssql_ping");
            return;
        };
        conn.ping().await.expect("ping should succeed");
    }

    #[tokio::test]
    async fn test_mssql_query() {
        let Some(mut conn) = try_connect().await else {
            eprintln!("MSSQL test container not available, skipping test_mssql_query");
            return;
        };
        let result = conn.query("SELECT * FROM test_users").await.expect("query should succeed");
        assert!(!result.columns.is_empty(), "should have columns");
        assert!(!result.rows.is_empty(), "should have rows");
    }

    #[tokio::test]
    async fn test_mssql_execute() {
        let Some(mut conn) = try_connect().await else {
            eprintln!("MSSQL test container not available, skipping test_mssql_execute");
            return;
        };
        let summary = conn.execute("INSERT INTO test_users (name, age) VALUES ('TestUser', 99)").await
            .expect("execute should succeed");
        assert!(summary.rows_affected.is_some_and(|n| n > 0), "should have affected rows");
    }

    #[tokio::test]
    async fn test_mssql_list_tables() {
        let Some(mut conn) = try_connect().await else {
            eprintln!("MSSQL test container not available, skipping test_mssql_list_tables");
            return;
        };
        let tables = conn.list_tables(None).await.expect("list_tables should succeed");
        assert!(tables.contains(&"test_users".to_string()), "should contain test_users");
    }

    #[tokio::test]
    async fn test_mssql_describe_table() {
        let Some(mut conn) = try_connect().await else {
            eprintln!("MSSQL test container not available, skipping test_mssql_describe_table");
            return;
        };
        let result = conn.describe_table(None, "test_users").await.expect("describe_table should succeed");
        assert_eq!(result.columns.len(), 6, "should return 6 metadata columns");
        let col_names: Vec<String> = result.columns.iter().map(|c| c.name.clone()).collect();
        assert_eq!(col_names, vec![
            "column_name", "data_type", "is_nullable", "column_default", "numeric_precision", "numeric_scale"
        ]);
    }

    #[tokio::test]
    async fn test_mssql_type_mapping() {
        let Some(mut conn) = try_connect().await else {
            eprintln!("MSSQL test container not available, skipping test_mssql_type_mapping");
            return;
        };
        let result = conn.query("SELECT name, age, score, active, meta FROM test_users WHERE name = 'Alice'").await
            .expect("query should succeed");
        assert_eq!(result.rows.len(), 1);
        let row = &result.rows[0];
        assert!(matches!(row[0], Value::String(_)), "name should be String");
        assert!(matches!(row[1], Value::Int64(_)), "age should be Int64");
        assert!(matches!(row[2], Value::Float64(_) | Value::Decimal(_)), "score should be Float64 or Decimal");
        assert!(matches!(row[3], Value::Int64(_) | Value::Bool(_)), "active should be Int64 or Bool");
        assert!(matches!(row[4], Value::Json(_) | Value::String(_)), "meta should be Json or String");
    }
}
