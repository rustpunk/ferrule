use crate::error::SqlError;
use crate::value::{ColumnInfo, Row};
use async_trait::async_trait;
use secrecy::SecretString;

/// Backend-agnostic connection options.
///
/// `password` carries a credential the *caller* already resolved
/// (env var, OS keyring, interactive prompt, or any host-supplied
/// source). `ferrule-sql` performs no credential resolution itself —
/// it has no dependency on `rpassword`, an OS keyring, or `dirs`. An
/// embedded library consumer supplies the secret here rather than
/// baking it into the connection URL.
///
/// When `password` is `Some`, it takes precedence over any password
/// component embedded in the connection URL. When it is `None`, each
/// backend falls back to the URL's password component, so callers
/// that prefer to embed the password in the URL keep working.
///
/// The secret is wrapped in [`SecretString`] so it is redacted in
/// `Debug` output and zeroized on drop.
#[derive(Debug, Clone, Default)]
pub struct ConnectOptions {
    /// Disable TLS certificate verification. Emits a warning on stderr.
    pub insecure: bool,
    /// Caller-resolved credential. Takes precedence over the URL's
    /// password component; `None` falls back to the URL.
    pub password: Option<SecretString>,
}

impl ConnectOptions {
    /// Resolve the effective password for this connection: the
    /// caller-supplied [`ConnectOptions::password`] if present,
    /// otherwise the password component of `url`.
    ///
    /// This is the single precedence rule every backend honors so the
    /// "resolved secret wins, URL is the fallback" contract lives in
    /// one place. The returned [`SecretString`] keeps the credential
    /// redacted and zeroize-on-drop right up to the point each driver
    /// consumes it via `expose_secret()`.
    #[must_use]
    pub fn effective_password(&self, url: &crate::url::DatabaseUrl) -> Option<SecretString> {
        self.password.clone().or_else(|| url.password())
    }
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
///
/// `copy_format` is consulted only by the Postgres backend and selects
/// between `COPY … WITH (FORMAT TEXT)` and `COPY … WITH (FORMAT BINARY)`.
/// All other backends ignore the field — their bulk paths use a
/// protocol-native wire format (TDS, MySQL `LOAD DATA`, ODPI-C array
/// DML).
#[derive(Debug)]
pub struct BulkInsert<'a> {
    pub table: &'a str,
    pub columns: &'a [ColumnInfo],
    pub rows: &'a [Row],
    pub copy_format: crate::copy::CopyFormat,
}

/// A foreign-key edge in a schema, returned by
/// [`Connection::list_foreign_keys`]. The columns lists are aligned:
/// `child_columns[i]` references `parent_columns[i]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForeignKey {
    pub child_table: String,
    pub child_columns: Vec<String>,
    pub parent_table: String,
    pub parent_columns: Vec<String>,
    /// Informational only: e.g. `"CASCADE"`, `"SET NULL"`, `"NO ACTION"`.
    /// `None` when the backend doesn't expose this (or it isn't set).
    pub on_delete: Option<String>,
}

/// Trait implemented by every backend connection.
#[async_trait]
pub trait Connection: Send {
    /// Execute a statement that may not return rows (INSERT, UPDATE, CREATE, etc.).
    async fn execute(&mut self, sql: &str) -> Result<ExecutionSummary, SqlError>;

    /// Execute a SELECT-like query and return rows.
    async fn query(&mut self, sql: &str) -> Result<QueryResult, SqlError>;

    /// Execute one or more statements.
    ///
    /// The default implementation tries `query()` first, then falls back to
    /// `execute()` — i.e. single-statement only. Backends that natively
    /// support multi-resultsets (Postgres, MSSQL) should override this.
    async fn execute_multi(&mut self, sql: &str) -> Result<Vec<StatementResult>, SqlError> {
        match self.query(sql).await {
            Ok(result) => Ok(vec![StatementResult::Query(result)]),
            Err(SqlError::QueryFailed(_)) => {
                let summary = self.execute(sql).await?;
                Ok(vec![StatementResult::Summary(summary)])
            }
            Err(e) => Err(e),
        }
    }

    /// Test connectivity (ping / `SELECT 1`).
    async fn ping(&mut self) -> Result<(), SqlError>;

    /// List tables in the given schema (or default schema if `None`).
    async fn list_tables(&mut self, schema: Option<&str>) -> Result<Vec<String>, SqlError>;

    /// Describe the columns of a single table.
    async fn describe_table(
        &mut self,
        schema: Option<&str>,
        table: &str,
    ) -> Result<QueryResult, SqlError>;

    /// Return the column names of `table`'s primary key, in key
    /// position order. Returns an empty `Vec` when the table has no
    /// declared PK. The `schema` argument follows the same default-
    /// schema convention as [`describe_table`](Self::describe_table).
    ///
    /// Implementations must not infer a PK from unique indexes — only
    /// declared primary-key constraints. Callers that want to override
    /// the conflict key supply it explicitly.
    async fn primary_key(
        &mut self,
        schema: Option<&str>,
        table: &str,
    ) -> Result<Vec<String>, SqlError>;

    /// Return every foreign-key edge in `schema` (or the default
    /// schema if `None`). Used by schema-level copy to topologically
    /// order tables — load parents before children.
    ///
    /// Backends without referential integrity (SQLite when
    /// `foreign_keys=off`, MySQL on MyISAM, etc.) still return any
    /// declared FKs; runtime enforcement is a separate concern.
    async fn list_foreign_keys(
        &mut self,
        schema: Option<&str>,
    ) -> Result<Vec<ForeignKey>, SqlError>;

    /// Insert `target.rows` into `target.table` using the backend's
    /// native bulk loader (Postgres `COPY FROM STDIN`, MSSQL
    /// `BulkLoadRequest`, MySQL `LOAD DATA LOCAL INFILE`, Oracle
    /// `oracle::Batch`). Returns the number of rows accepted.
    ///
    /// Backends that have no native bulk loader (SQLite, and the
    /// proxy / tunnel wrappers in their current shape) must return
    /// [`SqlError::BulkUnavailable`] so the caller can route the
    /// batch through the generic INSERT path. Treat this method as
    /// required — forgetting to implement it on a new backend or
    /// wrapper is a bug we want to catch at compile time, not at
    /// runtime in the "just slow" form.
    async fn bulk_insert_rows(&mut self, target: BulkInsert<'_>) -> Result<usize, SqlError>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::url::DatabaseUrl;
    use secrecy::ExposeSecret;

    /// A caller-resolved secret in `ConnectOptions::password` wins over
    /// the password embedded in the URL — this is the precedence rule
    /// that lets an embedder hand ferrule-sql a credential it resolved
    /// (keyring, prompt) instead of baking it into the URL.
    #[test]
    fn effective_password_prefers_opts_over_url() {
        let url = DatabaseUrl::parse("postgres://user:url_pw@localhost/db").unwrap();
        let opts = ConnectOptions {
            password: Some(SecretString::new("resolved_pw".into())),
            ..Default::default()
        };
        let got = opts.effective_password(&url).expect("a password");
        assert_eq!(got.expose_secret(), "resolved_pw");
    }

    /// With no caller-supplied secret, ferrule-sql falls back to the
    /// URL's password component, so the URL-embedded path keeps working.
    #[test]
    fn effective_password_falls_back_to_url() {
        let url = DatabaseUrl::parse("postgres://user:url_pw@localhost/db").unwrap();
        let opts = ConnectOptions::default();
        let got = opts.effective_password(&url).expect("a password");
        assert_eq!(got.expose_secret(), "url_pw");
    }

    /// No secret anywhere yields `None` — backends then connect without
    /// a password (e.g. trust/peer auth or a passwordless local socket).
    #[test]
    fn effective_password_none_when_absent_everywhere() {
        let url = DatabaseUrl::parse("postgres://user@localhost/db").unwrap();
        let opts = ConnectOptions::default();
        assert!(opts.effective_password(&url).is_none());
    }
}
