use crate::error::CoreError;
use crate::value::{ColumnInfo, Row};
use async_trait::async_trait;

/// Backend-agnostic connection options.
#[derive(Debug, Clone, Default)]
pub struct ConnectOptions {
    /// Disable TLS certificate verification. Emits a warning on stderr.
    pub insecure: bool,
}

/// Result of a query — columns plus rows.
#[derive(Debug, Clone)]
pub struct QueryResult {
    pub columns: Vec<ColumnInfo>,
    pub rows: Vec<Row>,
}

/// Summary of a non-query execution (DML / DDL).
#[derive(Debug, Clone)]
pub struct ExecutionSummary {
    pub rows_affected: Option<u64>,
    pub command_tag: Option<String>,
}

/// A single statement within a multi-statement batch.
#[derive(Debug, Clone)]
pub enum StatementResult {
    Query(QueryResult),
    Summary(ExecutionSummary),
}

/// Payload for [`Connection::bulk_insert_rows`].
///
/// `table` is unquoted — each backend is responsible for quoting it
/// for its own dialect. `columns` is the destination column order;
/// each row in `rows` must have the same length and match positionally.
#[derive(Debug)]
pub struct BulkInsert<'a> {
    pub table: &'a str,
    pub columns: &'a [ColumnInfo],
    pub rows: &'a [Row],
}

/// Trait implemented by every backend connection.
#[async_trait]
pub trait Connection: Send {
    /// Execute a statement that may not return rows (INSERT, UPDATE, CREATE, etc.).
    async fn execute(&mut self, sql: &str) -> Result<ExecutionSummary, CoreError>;

    /// Execute a SELECT-like query and return rows.
    async fn query(&mut self, sql: &str) -> Result<QueryResult, CoreError>;

    /// Execute one or more statements.
    ///
    /// The default implementation tries `query()` first, then falls back to
    /// `execute()` — i.e. single-statement only. Backends that natively
    /// support multi-resultsets (Postgres, MSSQL) should override this.
    async fn execute_multi(&mut self, sql: &str) -> Result<Vec<StatementResult>, CoreError> {
        match self.query(sql).await {
            Ok(result) => Ok(vec![StatementResult::Query(result)]),
            Err(CoreError::QueryFailed(_)) => {
                let summary = self.execute(sql).await?;
                Ok(vec![StatementResult::Summary(summary)])
            }
            Err(e) => Err(e),
        }
    }

    /// Test connectivity (ping / `SELECT 1`).
    async fn ping(&mut self) -> Result<(), CoreError>;

    /// List tables in the given schema (or default schema if `None`).
    async fn list_tables(&mut self, schema: Option<&str>) -> Result<Vec<String>, CoreError>;

    /// Describe the columns of a single table.
    async fn describe_table(
        &mut self,
        schema: Option<&str>,
        table: &str,
    ) -> Result<QueryResult, CoreError>;

    /// Insert `target.rows` into `target.table` using the backend's
    /// native bulk loader (Postgres `COPY FROM STDIN`, MSSQL
    /// `BulkLoadRequest`, MySQL `LOAD DATA LOCAL INFILE`, Oracle
    /// `oracle::Batch`). Returns the number of rows accepted.
    ///
    /// Backends that have no native bulk loader (SQLite, and the
    /// proxy / tunnel wrappers in their current shape) must return
    /// [`CoreError::BulkUnavailable`] so the caller can route the
    /// batch through the generic INSERT path. Treat this method as
    /// required — forgetting to implement it on a new backend or
    /// wrapper is a bug we want to catch at compile time, not at
    /// runtime in the "just slow" form.
    async fn bulk_insert_rows(
        &mut self,
        target: BulkInsert<'_>,
    ) -> Result<usize, CoreError>;
}
