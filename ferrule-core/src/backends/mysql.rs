use async_trait::async_trait;
use crate::connection::{Connection, ConnectOptions, ExecutionSummary, QueryResult, StatementResult};
use crate::error::CoreError;
use crate::url::DatabaseUrl;
use crate::value::{ColumnInfo, Row, TypeHint, Value};
use secrecy::ExposeSecret;
use chrono::{NaiveDate, NaiveTime, NaiveDateTime, Utc};
use mysql_async::prelude::Queryable;

pub struct MySqlConnection {
    conn: mysql_async::Conn,
}

#[async_trait]
impl Connection for MySqlConnection {
    async fn execute(&mut self, sql: &str) -> Result<ExecutionSummary, CoreError> {
        self.conn
            .query_drop(sql)
            .await
            .map_err(|e| CoreError::QueryFailed(e.to_string()))?;
        let affected = self.conn.affected_rows();
        Ok(ExecutionSummary {
            rows_affected: Some(affected),
            command_tag: None,
        })
    }

    async fn query(&mut self, sql: &str) -> Result<QueryResult, CoreError> {
        let mut result = self.conn
            .query_iter(sql)
            .await
            .map_err(|e| CoreError::QueryFailed(e.to_string()))?;

        let columns_ref = result.columns_ref();
        let columns: Vec<ColumnInfo> = columns_ref.iter().map(|c| ColumnInfo {
            name: c.name_str().to_string(),
            type_hint: TypeHint::Other,
            nullable: true,
        }).collect();

        let mysql_rows = result
            .collect::<mysql_async::Row>()
            .await
            .map_err(|e| CoreError::QueryFailed(e.to_string()))?;

        // Discard any remaining result sets so the connection stays clean.
        result
            .drop_result()
            .await
            .map_err(|e| CoreError::QueryFailed(e.to_string()))?;

        let rows: Vec<Row> = mysql_rows.into_iter().map(|row| {
            let col_types: Vec<_> = row.columns_ref().iter()
                .map(|c| (c.column_type(), c.column_length()))
                .collect();
            row.unwrap().into_iter().enumerate()
                .map(|(i, v)| mysql_to_value(v, col_types[i].0, col_types[i].1))
                .collect()
        }).collect();

        Ok(QueryResult { columns, rows })
    }

    async fn execute_multi(&mut self, sql: &str) -> Result<Vec<StatementResult>, CoreError> {
        let mut result = self.conn
            .query_iter(sql)
            .await
            .map_err(|e| CoreError::QueryFailed(e.to_string()))?;

        let mut results = Vec::new();

        loop {
            let columns_ref = result.columns_ref();
            if columns_ref.is_empty() {
                let affected = result.affected_rows();
                result
                    .collect::<mysql_async::Row>()
                    .await
                    .map_err(|e| CoreError::QueryFailed(e.to_string()))?;
                results.push(StatementResult::Summary(ExecutionSummary {
                    rows_affected: Some(affected),
                    command_tag: None,
                }));
            } else {
                let columns: Vec<ColumnInfo> = columns_ref.iter().map(|c| ColumnInfo {
                    name: c.name_str().to_string(),
                    type_hint: TypeHint::Other,
                    nullable: true,
                }).collect();

                let mysql_rows = result
                    .collect::<mysql_async::Row>()
                    .await
                    .map_err(|e| CoreError::QueryFailed(e.to_string()))?;

                let rows: Vec<Row> = mysql_rows.into_iter().map(|row| {
                    let col_types: Vec<_> = row.columns_ref().iter()
                        .map(|c| (c.column_type(), c.column_length()))
                        .collect();
                    row.unwrap().into_iter().enumerate()
                        .map(|(i, v)| mysql_to_value(v, col_types[i].0, col_types[i].1))
                        .collect()
                }).collect();

                results.push(StatementResult::Query(QueryResult { columns, rows }));
            }

            if result.is_empty() {
                break;
            }
        }

        Ok(results)
    }

    async fn ping(&mut self) -> Result<(), CoreError> {
        self.conn
            .ping()
            .await
            .map_err(|e| CoreError::ConnectionFailed(e.to_string()))?;
        Ok(())
    }

    async fn list_tables(&mut self, schema: Option<&str>) -> Result<Vec<String>, CoreError> {
        let sql = match schema {
            Some(s) => format!("SHOW TABLES FROM `{}`", escape_mysql_identifier(s)),
            None => "SHOW TABLES".to_string(),
        };
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
        let schema = match schema {
            Some(s) => s.to_string(),
            None => {
                let db_query = self.query("SELECT DATABASE()").await?;
                db_query.rows.into_iter().next()
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
}

pub async fn connect(url: &DatabaseUrl, opts: &ConnectOptions) -> Result<MySqlConnection, CoreError> {
    let mut builder = mysql_async::OptsBuilder::default()
        .ip_or_hostname(url.host().unwrap_or("localhost"))
        .tcp_port(url.port().unwrap_or(3306));

    if !url.username().is_empty() {
        builder = builder.user(Some(url.username()));
    }
    if let Some(pass) = url.password() {
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
                let ssl_opts = mysql_async::SslOpts::default()
                    .with_danger_accept_invalid_certs(true);
                builder = builder.ssl_opts(Some(ssl_opts));
            }
            "preferred" => {
                // Default behavior – no-op
            }
            "required" => {
                let ssl_opts = mysql_async::SslOpts::default()
                    .with_danger_accept_invalid_certs(false);
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
        .map_err(|e| CoreError::ConnectionFailed(e.to_string()))?;

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
            CT::MYSQL_TYPE_JSON => {
                serde_json::from_slice(&b)
                    .map(Value::Json)
                    .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&b).into_owned()))
            }
            CT::MYSQL_TYPE_DECIMAL | CT::MYSQL_TYPE_NEWDECIMAL => {
                Value::Decimal(String::from_utf8_lossy(&b).into_owned())
            }
            CT::MYSQL_TYPE_TINY_BLOB | CT::MYSQL_TYPE_MEDIUM_BLOB
            | CT::MYSQL_TYPE_LONG_BLOB | CT::MYSQL_TYPE_BLOB => Value::Bytes(b),
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
            CT::MYSQL_TYPE_SHORT | CT::MYSQL_TYPE_LONG | CT::MYSQL_TYPE_INT24
            | CT::MYSQL_TYPE_LONGLONG | CT::MYSQL_TYPE_YEAR => {
                String::from_utf8_lossy(&b)
                    .parse::<i64>()
                    .map(Value::Int64)
                    .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&b).into_owned()))
            }
            CT::MYSQL_TYPE_FLOAT | CT::MYSQL_TYPE_DOUBLE => {
                String::from_utf8_lossy(&b)
                    .parse::<f64>()
                    .map(Value::Float64)
                    .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&b).into_owned()))
            }
            CT::MYSQL_TYPE_DATE => {
                NaiveDate::parse_from_str(&String::from_utf8_lossy(&b), "%Y-%m-%d")
                    .map(Value::Date)
                    .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&b).into_owned()))
            }
            CT::MYSQL_TYPE_TIME => {
                NaiveTime::parse_from_str(&String::from_utf8_lossy(&b), "%H:%M:%S")
                    .or_else(|_| NaiveTime::parse_from_str(&String::from_utf8_lossy(&b), "%H:%M:%S%.f"))
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
            CT::MYSQL_TYPE_DATE => {
                NaiveDate::from_ymd_opt(year as i32, month as u32, day as u32)
                    .map(Value::Date)
                    .unwrap_or_else(|| {
                        Value::String(format!("{:04}-{:02}-{:02}", year, month, day))
                    })
            }
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
            _ => {
                NaiveDate::from_ymd_opt(year as i32, month as u32, day as u32)
                    .and_then(|d| d.and_hms_micro_opt(hour as u32, min as u32, sec as u32, usec))
                    .map(Value::DateTime)
                    .unwrap_or_else(|| {
                        Value::String(format!(
                            "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
                            year, month, day, hour, min, sec
                        ))
                    })
            }
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

fn escape_mysql_identifier(name: &str) -> String {
    name.replace('`', "``")
}

fn escape_mysql_string(s: &str) -> String {
    s.replace("'", "''")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::url::DatabaseUrl;

    const TEST_MYSQL_URL: &str = "mysql://root:ferrule@127.0.0.1:13306/ferrule";

    async fn try_connect() -> Option<MySqlConnection> {
        let url = DatabaseUrl::parse(TEST_MYSQL_URL).ok()?;
        let conn = connect(&url, &ConnectOptions::default()).await.ok()?;
        Some(conn)
    }

    #[tokio::test]
    async fn test_mysql_ping() {
        let Some(mut conn) = try_connect().await else {
            eprintln!("MySQL test container not available, skipping test_mysql_ping");
            return;
        };
        conn.ping().await.expect("ping should succeed");
    }

    #[tokio::test]
    async fn test_mysql_query() {
        let Some(mut conn) = try_connect().await else {
            eprintln!("MySQL test container not available, skipping test_mysql_query");
            return;
        };
        let result = conn.query("SELECT * FROM test_users").await.expect("query should succeed");
        assert!(!result.columns.is_empty(), "should have columns");
        assert!(!result.rows.is_empty(), "should have rows");
    }

    #[tokio::test]
    async fn test_mysql_execute() {
        let Some(mut conn) = try_connect().await else {
            eprintln!("MySQL test container not available, skipping test_mysql_execute");
            return;
        };
        let summary = conn.execute("INSERT INTO test_users (name, age) VALUES ('TestUser', 99)").await
            .expect("execute should succeed");
        assert!(summary.rows_affected.is_some_and(|n| n > 0), "should have affected rows");
    }

    #[tokio::test]
    async fn test_mysql_list_tables() {
        let Some(mut conn) = try_connect().await else {
            eprintln!("MySQL test container not available, skipping test_mysql_list_tables");
            return;
        };
        let tables = conn.list_tables(None).await.expect("list_tables should succeed");
        assert!(tables.contains(&"test_users".to_string()), "should contain test_users");
    }

    #[tokio::test]
    async fn test_mysql_describe_table() {
        let Some(mut conn) = try_connect().await else {
            eprintln!("MySQL test container not available, skipping test_mysql_describe_table");
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
    async fn test_mysql_type_mapping() {
        let Some(mut conn) = try_connect().await else {
            eprintln!("MySQL test container not available, skipping test_mysql_type_mapping");
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
