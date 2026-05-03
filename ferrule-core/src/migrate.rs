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

/// Migration engine bound to an open connection.
pub struct MigrationEngine {
    conn: Box<dyn Connection>,
    migrations_dir: PathBuf,
}

impl MigrationEngine {
    pub fn new(conn: Box<dyn Connection>, migrations_dir: PathBuf) -> Self {
        Self {
            conn,
            migrations_dir,
        }
    }

    /// Ensure the `__ferrule_migrations` tracking table exists.
    pub async fn ensure_migration_table(&mut self) -> Result<(), CoreError> {
        let sql = r#"
CREATE TABLE IF NOT EXISTS __ferrule_migrations (
    version TEXT PRIMARY KEY,
    applied_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
    checksum TEXT NOT NULL
)
"#;
        self.conn.execute(sql).await?;
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
    pub async fn last_applied(&mut self, n: usize) -> Result<Vec<AppliedMigration>, CoreError> {
        let sql = format!(
            "SELECT version, checksum FROM __ferrule_migrations ORDER BY applied_at DESC LIMIT {}",
            n
        );
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
