#![allow(dead_code, unused_variables, unused_imports)]

use async_trait::async_trait;
use crate::connection::{Connection, ConnectOptions, ExecutionSummary, QueryResult};
use crate::error::CoreError;
use crate::url::DatabaseUrl;
use oracle::Connection as OracleConn;

pub struct OracleConnection {
    conn: OracleConn,
}

#[async_trait]
impl Connection for OracleConnection {
    async fn execute(
        &mut self,
        _sql: &str,
    ) -> Result<ExecutionSummary, CoreError> {
        tokio::task::spawn_blocking(|| {
            todo!("Wave 1: Oracle execute")
        })
        .await
        .map_err(|e| CoreError::QueryFailed(e.to_string()))?
    }

    async fn query(
        &mut self,
        _sql: &str,
    ) -> Result<QueryResult, CoreError> {
        tokio::task::spawn_blocking(|| {
            todo!("Wave 1: Oracle query")
        })
        .await
        .map_err(|e| CoreError::QueryFailed(e.to_string()))?
    }

    async fn ping(&mut self) -> Result<(), CoreError> {
        tokio::task::spawn_blocking(|| {
            todo!("Wave 1: Oracle ping")
        })
        .await
        .map_err(|e| CoreError::QueryFailed(e.to_string()))?
    }

    async fn list_tables(
        &mut self,
        _schema: Option<&str>,
    ) -> Result<Vec<String>, CoreError> {
        tokio::task::spawn_blocking(|| {
            todo!("Wave 1: Oracle list_tables")
        })
        .await
        .map_err(|e| CoreError::QueryFailed(e.to_string()))?
    }

    async fn describe_table(
        &mut self,
        _schema: Option<&str>,
        _table: &str,
    ) -> Result<QueryResult, CoreError> {
        tokio::task::spawn_blocking(|| {
            todo!("Wave 1: Oracle describe_table")
        })
        .await
        .map_err(|e| CoreError::QueryFailed(e.to_string()))?
    }
}

pub async fn connect(_url: &DatabaseUrl, _opts: &ConnectOptions) -> Result<OracleConnection, CoreError> {
    tokio::task::spawn_blocking(|| {
        todo!("Wave 1: Oracle connect")
    })
    .await
    .map_err(|e| CoreError::ConnectionFailed(e.to_string()))?
}
