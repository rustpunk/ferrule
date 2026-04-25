#![allow(dead_code, unused_variables, unused_imports)]

use async_trait::async_trait;
use crate::connection::{Connection, ConnectOptions, ExecutionSummary, QueryResult};
use crate::error::CoreError;
use crate::url::DatabaseUrl;
use tiberius::Client;
use tokio::net::TcpStream;
use tokio_util::compat::TokioAsyncWriteCompatExt;

pub struct MsSqlConnection {
    client: Client<tokio_util::compat::Compat<TcpStream>>,
}

#[async_trait]
impl Connection for MsSqlConnection {
    async fn execute(
        &mut self,
        _sql: &str,
    ) -> Result<ExecutionSummary, CoreError> {
        todo!("Wave 1: MSSQL execute")
    }

    async fn query(
        &mut self,
        _sql: &str,
    ) -> Result<QueryResult, CoreError> {
        todo!("Wave 1: MSSQL query")
    }

    async fn ping(&mut self) -> Result<(), CoreError> {
        todo!("Wave 1: MSSQL ping")
    }

    async fn list_tables(
        &mut self,
        _schema: Option<&str>,
    ) -> Result<Vec<String>, CoreError> {
        todo!("Wave 1: MSSQL list_tables")
    }

    async fn describe_table(
        &mut self,
        _schema: Option<&str>,
        _table: &str,
    ) -> Result<QueryResult, CoreError> {
        todo!("Wave 1: MSSQL describe_table")
    }
}

pub async fn connect(_url: &DatabaseUrl, _opts: &ConnectOptions) -> Result<MsSqlConnection, CoreError> {
    todo!("Wave 1: MSSQL connect")
}
