#![allow(dead_code, unused_variables, unused_imports)]

use async_trait::async_trait;
use crate::connection::{Connection, ConnectOptions, ExecutionSummary, QueryResult};
use crate::error::CoreError;
use crate::url::DatabaseUrl;
use mysql_async::Conn;

pub struct MySqlConnection {
    conn: Conn,
}

#[async_trait]
impl Connection for MySqlConnection {
    async fn execute(
        &mut self,
        _sql: &str,
    ) -> Result<ExecutionSummary, CoreError> {
        todo!("Wave 1: MySQL execute")
    }

    async fn query(
        &mut self,
        _sql: &str,
    ) -> Result<QueryResult, CoreError> {
        todo!("Wave 1: MySQL query")
    }

    async fn ping(&mut self) -> Result<(), CoreError> {
        todo!("Wave 1: MySQL ping")
    }

    async fn list_tables(
        &mut self,
        _schema: Option<&str>,
    ) -> Result<Vec<String>, CoreError> {
        todo!("Wave 1: MySQL list_tables")
    }

    async fn describe_table(
        &mut self,
        _schema: Option<&str>,
        _table: &str,
    ) -> Result<QueryResult, CoreError> {
        todo!("Wave 1: MySQL describe_table")
    }
}

pub async fn connect(_url: &DatabaseUrl, _opts: &ConnectOptions) -> Result<MySqlConnection, CoreError> {
    todo!("Wave 1: MySQL connect")
}
