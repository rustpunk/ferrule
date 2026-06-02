//! Lightweight, multi-backend SQL migration runner.
//!
//! A `migrations/` directory of timestamp-ordered `.up.sql` / `.down.sql`
//! files is tracked in a `__ferrule_migrations` table inside the target
//! database.  Pure SQL, no ORM, no DSL.
//!
//! File naming: `YYYYMMDDHHMMSS_<name>.{up,down}.sql` — lex sort = order.

use crate::connection::Connection;
use crate::error::CoreError;
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::path::PathBuf;

/// A single discovered migration file.
#[derive(Debug, Clone)]
pub struct MigrationFile {
    pub version: String,
    pub path: PathBuf,
    pub direction: Direction,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Up,
    Down,
}

/// SQL dialect of the target database, used to generate portable DDL
/// and queries for the migration engine.
///
/// Derived from the connection URL scheme (see [`Dialect::from_scheme`]).
/// Deliberately not feature-gated: it is a pure data classification of
/// the SQL we emit and carries no driver dependency, so it stays
/// available regardless of which backend features are compiled in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dialect {
    Sqlite,
    Postgres,
    MySql,
    MsSql,
    Oracle,
}

impl Dialect {
    /// Map a connection URL scheme to a [`Dialect`].
    ///
    /// Returns `None` for unrecognised schemes; callers fall back to
    /// [`Dialect::Sqlite`] semantics (ANSI `LIMIT`, `TEXT` columns).
    #[must_use]
    pub fn from_scheme(scheme: &str) -> Option<Self> {
        match scheme {
            "sqlite" => Some(Self::Sqlite),
            "postgres" | "postgresql" => Some(Self::Postgres),
            "mysql" | "mariadb" => Some(Self::MySql),
            "mssql" | "sqlserver" | "tds" => Some(Self::MsSql),
            "oracle" => Some(Self::Oracle),
            _ => None,
        }
    }
}

/// Migration engine bound to an open connection.
pub struct MigrationEngine {
    conn: Box<dyn Connection>,
    migrations_dir: PathBuf,
    dialect: Dialect,
}

impl MigrationEngine {
    pub fn new(conn: Box<dyn Connection>, migrations_dir: PathBuf, dialect: Dialect) -> Self {
        Self {
            conn,
            migrations_dir,
            dialect,
        }
    }

    /// Ensure the `__ferrule_migrations` tracking table exists.
    ///
    /// The DDL is dialect-specific because the canonical column types,
    /// the timestamp default, and the "create if absent" idiom differ
    /// across backends:
    ///
    /// - **SQLite / Postgres** — `TEXT` keys are valid and
    ///   `CREATE TABLE IF NOT EXISTS` is supported.
    /// - **MySQL** — `TEXT` cannot be a `PRIMARY KEY` without a prefix
    ///   length, so the keyed columns use `VARCHAR`.
    /// - **MSSQL** — `CREATE TABLE IF NOT EXISTS` is not valid T-SQL,
    ///   `TEXT` cannot key a table, and the `TIMESTAMP` type is a
    ///   rowversion (not a datetime); we guard creation with
    ///   `IF OBJECT_ID(...) IS NULL` and use `DATETIME2`.
    /// - **Oracle** — has no `TEXT` type and `IF NOT EXISTS` is
    ///   unsupported pre-23c, so creation runs inside a PL/SQL block
    ///   that swallows ORA-00955 (name already used).
    pub async fn ensure_migration_table(&mut self) -> Result<(), CoreError> {
        let sql = match self.dialect {
            Dialect::Sqlite | Dialect::Postgres => {
                r#"CREATE TABLE IF NOT EXISTS __ferrule_migrations (
    version TEXT PRIMARY KEY,
    applied_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
    checksum TEXT NOT NULL
)"#
                .to_string()
            }
            Dialect::MySql => r#"CREATE TABLE IF NOT EXISTS __ferrule_migrations (
    version VARCHAR(255) PRIMARY KEY,
    applied_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
    checksum VARCHAR(64) NOT NULL
)"#
            .to_string(),
            Dialect::MsSql => r#"IF OBJECT_ID(N'__ferrule_migrations', N'U') IS NULL
CREATE TABLE __ferrule_migrations (
    version NVARCHAR(255) PRIMARY KEY,
    applied_at DATETIME2 DEFAULT SYSUTCDATETIME(),
    checksum NVARCHAR(64) NOT NULL
)"#
            .to_string(),
            Dialect::Oracle => {
                // ORA-00955 ("name is already used by an existing object")
                // is the table-already-exists signal; swallow it so the
                // call is idempotent like the other dialects.
                r#"BEGIN
    EXECUTE IMMEDIATE 'CREATE TABLE __ferrule_migrations (
        version VARCHAR2(255) PRIMARY KEY,
        applied_at TIMESTAMP DEFAULT SYSTIMESTAMP,
        checksum VARCHAR2(64) NOT NULL
    )';
EXCEPTION
    WHEN OTHERS THEN
        IF SQLCODE != -955 THEN
            RAISE;
        END IF;
END;"#
                    .to_string()
            }
        };
        self.conn.execute(&sql).await?;
        Ok(())
    }

    /// Return the list of migrations that have **not** yet been applied,
    /// sorted lexicographically by version.
    pub async fn pending_migrations(&mut self) -> Result<Vec<MigrationFile>, CoreError> {
        let applied = self.applied_versions().await?;
        let mut pending = self.scan_dir(Direction::Up)?;
        pending.retain(|m| !applied.contains(&m.version));
        Ok(pending)
    }

    /// Apply a single migration (`.up.sql`).
    pub async fn apply_up(&mut self, file: &MigrationFile) -> Result<(), CoreError> {
        let sql = tokio::fs::read_to_string(&file.path).await.map_err(|e| {
            CoreError::QueryFailed(format!(
                "cannot read migration {}: {}",
                file.path.display(),
                e
            ))
        })?;
        let checksum = hex_digest(&sql);

        // Execute inside a transaction when possible (SQLite/Postgres/MySQL).
        // For MSSQL/Oracle we still execute the script; rollback semantics
        // vary by backend DDL behaviour.
        self.conn.execute_multi(&sql).await?;

        let insert = format!(
            "INSERT INTO __ferrule_migrations (version, checksum) VALUES ('{}', '{}')",
            escape_sql_literal(&file.version),
            escape_sql_literal(&checksum)
        );
        self.conn.execute(&insert).await?;
        Ok(())
    }

    /// Rollback a single migration (`.down.sql`).
    pub async fn apply_down(&mut self, file: &MigrationFile) -> Result<(), CoreError> {
        let sql = tokio::fs::read_to_string(&file.path).await.map_err(|e| {
            CoreError::QueryFailed(format!(
                "cannot read migration {}: {}",
                file.path.display(),
                e
            ))
        })?;

        self.conn.execute_multi(&sql).await?;

        let delete = format!(
            "DELETE FROM __ferrule_migrations WHERE version = '{}'",
            escape_sql_literal(&file.version)
        );
        self.conn.execute(&delete).await?;
        Ok(())
    }

    /// Read the last N applied versions from the tracking table,
    /// ordered by most-recent first.
    ///
    /// The ordering uses `version DESC` as a deterministic tiebreak after
    /// `applied_at DESC`: `applied_at` has second granularity, and
    /// `migrate up` can apply a whole batch inside a single second, so
    /// without the tiebreak `down` could roll back an arbitrary member of
    /// that batch rather than the genuinely newest one.
    ///
    /// The row-limit clause is dialect-specific: SQLite, Postgres, and
    /// MySQL accept `LIMIT n`; MSSQL uses `SELECT TOP n`; Oracle (12c+)
    /// uses `FETCH FIRST n ROWS ONLY`.
    pub async fn last_applied(&mut self, n: usize) -> Result<Vec<AppliedMigration>, CoreError> {
        let order = "ORDER BY applied_at DESC, version DESC";
        let sql = match self.dialect {
            Dialect::Sqlite | Dialect::Postgres | Dialect::MySql => {
                format!("SELECT version, checksum FROM __ferrule_migrations {order} LIMIT {n}")
            }
            Dialect::MsSql => {
                format!("SELECT TOP {n} version, checksum FROM __ferrule_migrations {order}")
            }
            Dialect::Oracle => {
                format!(
                    "SELECT version, checksum FROM __ferrule_migrations {order} FETCH FIRST {n} ROWS ONLY"
                )
            }
        };
        let result = self.conn.query(&sql).await?;
        let mut out = Vec::with_capacity(result.rows.len());
        for row in result.rows {
            let version = row[0].to_string();
            let checksum = row[1].to_string();
            out.push(AppliedMigration { version, checksum });
        }
        Ok(out)
    }

    /// Verify that a migration file on disk still matches the checksum
    /// recorded in the database.  Returns `Ok(())` if clean, `Err` on drift.
    pub async fn verify_checksum(&mut self, version: &str) -> Result<(), CoreError> {
        let sql = format!(
            "SELECT checksum FROM __ferrule_migrations WHERE version = '{}'",
            escape_sql_literal(version)
        );
        let result = self.conn.query(&sql).await?;
        let db_checksum = result
            .rows
            .first()
            .map(|r| r[0].to_string())
            .ok_or_else(|| {
                CoreError::QueryFailed(format!(
                    "migration '{}' not found in tracking table",
                    version
                ))
            })?;

        let _up_path = self.migrations_dir.join(format!("{}_*.up.sql", version));
        // Find the actual file — there may be multiple matches if the
        // user renamed the descriptive part.
        let entries = tokio::fs::read_dir(&self.migrations_dir)
            .await
            .map_err(|e| CoreError::QueryFailed(format!("cannot read migrations dir: {}", e)))?;

        let mut found = None;
        let mut entries = entries;
        while let Some(entry) = entries
            .next_entry()
            .await
            .map_err(|e| CoreError::QueryFailed(format!("cannot read migrations dir: {}", e)))?
        {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name.starts_with(version) && name.ends_with(".up.sql") {
                let content = tokio::fs::read_to_string(entry.path()).await.map_err(|e| {
                    CoreError::QueryFailed(format!(
                        "cannot read migration file {}: {}",
                        entry.path().display(),
                        e
                    ))
                })?;
                found = Some(hex_digest(&content));
                break;
            }
        }

        let file_checksum = found.ok_or_else(|| {
            CoreError::QueryFailed(format!(
                "migration file for version '{}' not found in {}",
                version,
                self.migrations_dir.display()
            ))
        })?;

        if db_checksum != file_checksum {
            return Err(CoreError::QueryFailed(format!(
                "checksum mismatch for migration '{}':\n  db:    {}\n  file:  {}\n  The migration file was edited after it was applied.",
                version, db_checksum, file_checksum
            )));
        }
        Ok(())
    }

    // ------------------------------------------------------------------
    // Internal helpers
    // ------------------------------------------------------------------

    pub async fn applied_versions(&mut self) -> Result<HashSet<String>, CoreError> {
        let sql = "SELECT version FROM __ferrule_migrations";
        let result = self.conn.query(sql).await?;
        let mut set = HashSet::with_capacity(result.rows.len());
        for row in result.rows {
            set.insert(row[0].to_string());
        }
        Ok(set)
    }

    pub fn scan_dir(&self, direction: Direction) -> Result<Vec<MigrationFile>, CoreError> {
        let ext = match direction {
            Direction::Up => ".up.sql",
            Direction::Down => ".down.sql",
        };
        let mut files = Vec::new();
        let entries = std::fs::read_dir(&self.migrations_dir).map_err(|e| {
            CoreError::QueryFailed(format!(
                "cannot read migrations directory '{}': {}",
                self.migrations_dir.display(),
                e
            ))
        })?;
        for entry in entries {
            let entry = entry.map_err(|e| {
                CoreError::QueryFailed(format!("cannot read directory entry: {}", e))
            })?;
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name.ends_with(ext) {
                let stem = name.strip_suffix(ext).unwrap_or(&name);
                let version = stem.split('_').next().unwrap_or(stem).to_string();
                files.push(MigrationFile {
                    version,
                    path: entry.path(),
                    direction,
                });
            }
        }
        files.sort_by(|a, b| a.version.cmp(&b.version));
        Ok(files)
    }
}

/// A migration that has been applied, as read from the tracking table.
#[derive(Debug, Clone)]
pub struct AppliedMigration {
    pub version: String,
    pub checksum: String,
}

/// Generate a hex SHA-256 digest of the input string.
fn hex_digest(input: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(input.as_bytes());
    let result = hasher.finalize();
    hex::encode(result)
}

/// Escape a string for safe use inside a SQL single-quoted literal.
/// Replaces `'` with `''`.
fn escape_sql_literal(s: &str) -> String {
    s.replace('\'', "''")
}
