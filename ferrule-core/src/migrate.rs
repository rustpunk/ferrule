//! Lightweight, multi-backend SQL migration runner.
//!
//! A `migrations/` directory of timestamp-ordered `.up.sql` / `.down.sql`
//! files is tracked in a `ferrule_migrations` table inside the target
//! database.  Pure SQL, no ORM, no DSL.
//!
//! File naming: `YYYYMMDDHHMMSS_<name>.{up,down}.sql` — lex sort = order.

use ferrule_sql::quote_string;
use ferrule_sql::{Connection, Dialect, SqlError};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
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

    /// Ensure the `ferrule_migrations` tracking table exists.
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
    pub async fn ensure_migration_table(&mut self) -> Result<(), SqlError> {
        let sql = match self.dialect {
            Dialect::Sqlite | Dialect::Postgres => {
                r#"CREATE TABLE IF NOT EXISTS ferrule_migrations (
    version TEXT PRIMARY KEY,
    applied_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
    checksum TEXT NOT NULL
)"#
                .to_string()
            }
            Dialect::MySql => r#"CREATE TABLE IF NOT EXISTS ferrule_migrations (
    version VARCHAR(255) PRIMARY KEY,
    applied_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
    checksum VARCHAR(64) NOT NULL
)"#
            .to_string(),
            Dialect::MsSql => r#"IF OBJECT_ID(N'ferrule_migrations', N'U') IS NULL
CREATE TABLE ferrule_migrations (
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
    EXECUTE IMMEDIATE 'CREATE TABLE ferrule_migrations (
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
    pub async fn pending_migrations(&mut self) -> Result<Vec<MigrationFile>, SqlError> {
        let applied = self.applied_versions().await?;
        let mut pending = self.scan_dir(Direction::Up)?;
        pending.retain(|m| !applied.contains(&m.version));
        Ok(pending)
    }

    /// Apply a single migration (`.up.sql`).
    ///
    /// The migration script and the `ferrule_migrations` tracking-row
    /// `INSERT` are committed as a single unit on backends with
    /// transactional DDL (SQLite, Postgres, MSSQL): both succeed or both
    /// roll back, so a mid-script failure can never leave the migration
    /// recorded-but-partial or applied-but-untracked. On MySQL and Oracle,
    /// DDL implicitly commits, so the two steps run best-effort and a
    /// failure in the middle can leave the schema partially applied — see
    /// [`MigrationEngine::apply_atomic`] for the per-dialect details.
    pub async fn apply_up(&mut self, file: &MigrationFile) -> Result<(), SqlError> {
        let sql = tokio::fs::read_to_string(&file.path).await.map_err(|e| {
            SqlError::QueryFailed(format!(
                "cannot read migration {}: {}",
                file.path.display(),
                e
            ))
        })?;
        let checksum = hex_digest(&sql);

        validate_version(&file.version)?;
        let track = format!(
            "INSERT INTO ferrule_migrations (version, checksum) VALUES ({}, {})",
            quote_string(&file.version),
            quote_string(&checksum)
        );
        self.apply_atomic(&sql, &track).await
    }

    /// Rollback a single migration (`.down.sql`).
    ///
    /// The rollback script and the `ferrule_migrations` tracking-row
    /// `DELETE` are committed together on backends with transactional DDL
    /// (SQLite, Postgres, MSSQL): the row is removed only if the entire
    /// down script succeeds, so a mid-script failure can never leave the
    /// schema half-rolled-back while the row still marks the migration
    /// applied. On MySQL and Oracle, DDL implicitly commits, so the two
    /// steps run best-effort — see [`MigrationEngine::apply_atomic`].
    pub async fn apply_down(&mut self, file: &MigrationFile) -> Result<(), SqlError> {
        let sql = tokio::fs::read_to_string(&file.path).await.map_err(|e| {
            SqlError::QueryFailed(format!(
                "cannot read migration {}: {}",
                file.path.display(),
                e
            ))
        })?;

        validate_version(&file.version)?;
        let track = format!(
            "DELETE FROM ferrule_migrations WHERE version = {}",
            quote_string(&file.version)
        );
        self.apply_atomic(&sql, &track).await
    }

    /// Run a migration `script` and its tracking-table statement `track`
    /// (the `INSERT` for an up, the `DELETE` for a down) as a single unit.
    ///
    /// Atomicity depends on whether the backend supports transactional DDL:
    ///
    /// - **SQLite / Postgres / MSSQL** — DDL participates in transactions,
    ///   so the script and the tracking statement are wrapped in one
    ///   `BEGIN`/`COMMIT` batch. If any statement fails, an explicit
    ///   `ROLLBACK` discards every change (schema and tracking row) so the
    ///   migration is left exactly as it was before the attempt. MSSQL
    ///   additionally sets `XACT_ABORT ON`, which makes a runtime error
    ///   abort the whole batch (T-SQL does not roll back on error by
    ///   default).
    /// - **MySQL / Oracle** — DDL implicitly commits, so wrapping it in a
    ///   transaction would not protect it. The script and the tracking
    ///   statement run as two separate operations (best-effort). A failure
    ///   partway through the script can therefore leave the schema partially
    ///   changed and the tracking row out of sync; this is an inherent
    ///   limitation of these engines, not a bug in the runner. Oracle does
    ///   not autocommit DML, so an explicit `COMMIT` persists the
    ///   tracking-row write (and any DML in the script); MySQL autocommits,
    ///   so it needs none.
    async fn apply_atomic(&mut self, script: &str, track: &str) -> Result<(), SqlError> {
        match self.dialect {
            Dialect::Sqlite | Dialect::Postgres | Dialect::MsSql => {
                let (begin, prelude) = match self.dialect {
                    Dialect::MsSql => ("BEGIN TRANSACTION;", "SET XACT_ABORT ON;\n"),
                    _ => ("BEGIN;", ""),
                };
                // Separate the migration script from the tracking statement
                // with exactly one `;`. A bare/empty statement (`;` on its own)
                // makes SQLite's per-statement execution path fail with the
                // misleading "not an error", so only emit a terminator when the
                // script actually has content.
                let script = script.trim();
                let body = if script.is_empty() {
                    String::new()
                } else if script.ends_with(';') {
                    format!("{script}\n")
                } else {
                    format!("{script};\n")
                };
                let batch = format!("{prelude}{begin}\n{body}{track};\nCOMMIT;");
                // SQLite and Postgres run the whole batch through
                // `execute_multi` (SQLite splits it statement-by-statement;
                // Postgres sends it as one `simple_query`). MSSQL must use
                // `execute` instead: its `execute_multi` runs the batch via
                // tiberius' result-returning `query` path, whose metadata pass
                // executes a DDL batch twice (the second `CREATE TABLE` then
                // fails "already exists"). `execute` uses the plain
                // non-resultset path and runs the batch exactly once.
                let run = match self.dialect {
                    Dialect::MsSql => self.conn.execute(&batch).await.map(|_| ()),
                    _ => self.conn.execute_multi(&batch).await.map(|_| ()),
                };
                match run {
                    Ok(()) => Ok(()),
                    Err(e) => {
                        // Discard any partial work and return the original
                        // error. The rollback is best-effort: if it also
                        // fails (e.g. the connection is gone, or XACT_ABORT
                        // already rolled back) the caller still sees the
                        // underlying migration failure.
                        let _ = self.conn.execute("ROLLBACK;").await;
                        Err(e)
                    }
                }
            }
            Dialect::MySql | Dialect::Oracle => {
                self.conn.execute_multi(script).await?;
                self.conn.execute(track).await?;
                // Oracle does not autocommit DML: without an explicit COMMIT
                // the tracking-row INSERT/DELETE (and any DML in the script)
                // is rolled back when the connection closes. MySQL autocommits
                // each statement, so this is Oracle-only.
                if self.dialect == Dialect::Oracle {
                    self.conn.execute("COMMIT").await?;
                }
                Ok(())
            }
        }
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
    pub async fn last_applied(&mut self, n: usize) -> Result<Vec<AppliedMigration>, SqlError> {
        let order = "ORDER BY applied_at DESC, version DESC";
        let sql = match self.dialect {
            Dialect::Sqlite | Dialect::Postgres | Dialect::MySql => {
                format!("SELECT version, checksum FROM ferrule_migrations {order} LIMIT {n}")
            }
            Dialect::MsSql => {
                format!("SELECT TOP {n} version, checksum FROM ferrule_migrations {order}")
            }
            Dialect::Oracle => {
                format!(
                    "SELECT version, checksum FROM ferrule_migrations {order} FETCH FIRST {n} ROWS ONLY"
                )
            }
        };
        self.query_applied(&sql).await
    }

    /// Read **every** applied migration from the tracking table, ordered
    /// most-recent first (`applied_at DESC, version DESC`).
    ///
    /// Unlike [`MigrationEngine::last_applied`] this applies no row cap, so
    /// `migrate history` and `migrate verify` see the full set rather than a
    /// silently truncated window. The ordering is identical to
    /// `last_applied` and needs no dialect-specific limit clause, so the
    /// same query runs on all backends.
    pub async fn all_applied(&mut self) -> Result<Vec<AppliedMigration>, SqlError> {
        let sql =
            "SELECT version, checksum FROM ferrule_migrations ORDER BY applied_at DESC, version DESC";
        self.query_applied(sql).await
    }

    /// Run a `SELECT version, checksum FROM ferrule_migrations ...` query
    /// and collect the rows into [`AppliedMigration`]s.
    async fn query_applied(&mut self, sql: &str) -> Result<Vec<AppliedMigration>, SqlError> {
        let result = self.conn.query(sql).await?;
        let mut out = Vec::with_capacity(result.rows.len());
        for row in result.rows {
            let version = row[0].to_string();
            let checksum = row[1].to_string();
            out.push(AppliedMigration { version, checksum });
        }
        Ok(out)
    }

    /// Verify that a single migration's `.up.sql` file on disk still matches
    /// the checksum recorded in the database. Returns `Ok(())` if clean,
    /// `Err` on drift.
    ///
    /// The on-disk file is resolved by reusing [`MigrationEngine::scan_dir`]
    /// (`Direction::Up`) and matching the derived version **exactly**, the
    /// same way `apply_up` and `pending_migrations` identify files. An
    /// earlier `name.starts_with(version)` prefix match could bind the
    /// wrong file (e.g. version `2026` matching `20260602_x.up.sql`),
    /// reporting spurious drift or masking real drift.
    pub async fn verify_checksum(&mut self, version: &str) -> Result<(), SqlError> {
        validate_version(version)?;
        let sql = format!(
            "SELECT checksum FROM ferrule_migrations WHERE version = {}",
            quote_string(version)
        );
        let result = self.conn.query(&sql).await?;
        let db_checksum = result
            .rows
            .first()
            .map(|r| r[0].to_string())
            .ok_or_else(|| {
                SqlError::QueryFailed(format!(
                    "migration '{}' not found in tracking table",
                    version
                ))
            })?;

        let up_files = self.scan_dir(Direction::Up)?;
        let file = up_files
            .iter()
            .find(|f| f.version == version)
            .ok_or_else(|| {
                SqlError::QueryFailed(format!(
                    "migration file for version '{}' not found in {}",
                    version,
                    self.migrations_dir.display()
                ))
            })?;

        let content = tokio::fs::read_to_string(&file.path).await.map_err(|e| {
            SqlError::QueryFailed(format!(
                "cannot read migration file {}: {}",
                file.path.display(),
                e
            ))
        })?;
        let file_checksum = hex_digest(&content);

        if db_checksum != file_checksum {
            return Err(SqlError::QueryFailed(format!(
                "checksum mismatch for migration '{}':\n  db:    {}\n  file:  {}\n  The migration file was edited after it was applied.",
                version, db_checksum, file_checksum
            )));
        }
        Ok(())
    }

    // ------------------------------------------------------------------
    // Internal helpers
    // ------------------------------------------------------------------

    pub async fn applied_versions(&mut self) -> Result<HashSet<String>, SqlError> {
        let sql = "SELECT version FROM ferrule_migrations";
        let result = self.conn.query(sql).await?;
        let mut set = HashSet::with_capacity(result.rows.len());
        for row in result.rows {
            set.insert(row[0].to_string());
        }
        Ok(set)
    }

    /// Scan the migrations directory for files in the given `direction`,
    /// returning them sorted lexicographically by version.
    ///
    /// The version is the substring before the first `_` in the file stem
    /// (`20260602120000_add_users.up.sql` -> `20260602120000`). Because
    /// two distinct files can collapse onto the same version under that
    /// rule, this detects the collision up front and returns a
    /// [`SqlError::QueryFailed`] naming the conflicting files. Surfacing
    /// it here — before any `apply_up` runs — prevents a duplicate version
    /// from being discovered only when the second tracking-row `INSERT`
    /// hits the primary-key constraint mid-run, which would leave one
    /// migration's DDL applied but untracked.
    pub fn scan_dir(&self, direction: Direction) -> Result<Vec<MigrationFile>, SqlError> {
        let ext = match direction {
            Direction::Up => ".up.sql",
            Direction::Down => ".down.sql",
        };
        let mut files = Vec::new();
        let entries = std::fs::read_dir(&self.migrations_dir).map_err(|e| {
            SqlError::QueryFailed(format!(
                "cannot read migrations directory '{}': {}",
                self.migrations_dir.display(),
                e
            ))
        })?;
        for entry in entries {
            let entry = entry.map_err(|e| {
                SqlError::QueryFailed(format!("cannot read directory entry: {}", e))
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

        // Reject two files that derive the same version: applying both
        // would commit the first migration's DDL then violate the
        // tracking table's primary key on the second.
        let mut seen: HashMap<&str, &PathBuf> = HashMap::with_capacity(files.len());
        for file in &files {
            if let Some(prev) = seen.insert(file.version.as_str(), &file.path) {
                return Err(SqlError::QueryFailed(format!(
                    "duplicate migration version '{}' derived from two files:\n  {}\n  {}\nRename one so each version (the text before the first '_') is unique.",
                    file.version,
                    prev.display(),
                    file.path.display()
                )));
            }
        }

        Ok(files)
    }

    /// Build a `version -> file-checksum` map by scanning the up migrations
    /// directory exactly once.
    ///
    /// The checksum is the SHA-256 hex digest of the `.up.sql` file's
    /// contents, identical to what [`MigrationEngine::apply_up`] records in
    /// the tracking table. This lets `verify` compare every applied
    /// migration against on-disk content using the checksums it already has
    /// from the applied-list query — one directory scan and zero extra
    /// `SELECT`s, instead of re-reading the directory and re-querying the
    /// database once per applied migration.
    pub async fn on_disk_checksums(&self) -> Result<HashMap<String, String>, SqlError> {
        let files = self.scan_dir(Direction::Up)?;
        let mut map = HashMap::with_capacity(files.len());
        for file in files {
            let content = tokio::fs::read_to_string(&file.path).await.map_err(|e| {
                SqlError::QueryFailed(format!(
                    "cannot read migration file {}: {}",
                    file.path.display(),
                    e
                ))
            })?;
            map.insert(file.version, hex_digest(&content));
        }
        Ok(map)
    }

    /// Verify that every supplied applied migration still matches its
    /// `.up.sql` file on disk, using checksums already held in hand.
    ///
    /// `applied` is the list returned by [`MigrationEngine::last_applied`]
    /// (or any caller-built list of applied versions + recorded checksums).
    /// The directory is scanned once via
    /// [`MigrationEngine::on_disk_checksums`]; no per-migration queries are
    /// issued. The returned vector lists every drifted migration with a
    /// human-readable reason — empty means all clean.
    pub async fn verify_applied(
        &self,
        applied: &[AppliedMigration],
    ) -> Result<Vec<ChecksumDrift>, SqlError> {
        let on_disk = self.on_disk_checksums().await?;
        let mut drift = Vec::new();
        for migration in applied {
            match on_disk.get(&migration.version) {
                None => drift.push(ChecksumDrift {
                    version: migration.version.clone(),
                    reason: format!(
                        "migration file for version '{}' not found in {}",
                        migration.version,
                        self.migrations_dir.display()
                    ),
                }),
                Some(file_checksum) if *file_checksum != migration.checksum => {
                    drift.push(ChecksumDrift {
                        version: migration.version.clone(),
                        reason: format!(
                            "checksum mismatch:\n      db:   {}\n      file: {}\n      The migration file was edited after it was applied.",
                            migration.checksum, file_checksum
                        ),
                    });
                }
                Some(_) => {}
            }
        }
        Ok(drift)
    }
}

/// A migration that has been applied, as read from the tracking table.
#[derive(Debug, Clone)]
pub struct AppliedMigration {
    pub version: String,
    pub checksum: String,
}

/// A drifted migration found by [`MigrationEngine::verify_applied`]:
/// either its `.up.sql` file is missing on disk, or its on-disk checksum
/// no longer matches the value recorded when it was applied.
#[derive(Debug, Clone)]
pub struct ChecksumDrift {
    pub version: String,
    pub reason: String,
}

/// Generate a hex SHA-256 digest of the input string.
fn hex_digest(input: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(input.as_bytes());
    let result = hasher.finalize();
    hex::encode(result)
}

/// Reject a migration version that cannot be safely interpolated into a
/// single-quoted SQL literal.
///
/// Versions are interpolated into the tracking-table `INSERT`/`DELETE`/`SELECT`
/// statements and quoted with [`ferrule_sql::quote_string`], which doubles
/// embedded `'`. That alone is enough for SQLite, Postgres, MSSQL, and Oracle,
/// but MySQL interprets `\` as an escape inside string literals by default, so
/// a backslash in a version could still break out of the literal. The version
/// originates from a filename stem (the text before the first `_`), which the
/// naming convention defines as timestamp digits, so any `'` or `\` indicates a
/// malformed name rather than a legitimate version; reject it before building
/// the statement.
fn validate_version(version: &str) -> Result<(), SqlError> {
    if version.contains('\'') || version.contains('\\') {
        return Err(SqlError::QueryFailed(format!(
            "migration version '{version}' contains a quote or backslash; \
             rename the file so the version (the text before the first '_') \
             has no ' or \\ characters"
        )));
    }
    Ok(())
}

#[cfg(all(test, feature = "sqlite"))]
mod tests {
    //! SQLite-backed integration tests for the migration engine.
    //!
    //! SQLite needs no external container, so these exercise the real
    //! apply/rollback/verify code paths end-to-end (including the
    //! transactional-atomicity batch in [`MigrationEngine::apply_atomic`]).

    use super::*;
    use ferrule_sql::ConnectOptions;
    use ferrule_sql::DatabaseUrl;
    use std::sync::atomic::{AtomicU64, Ordering};

    static CTR: AtomicU64 = AtomicU64::new(0);

    /// A throwaway temp directory (migrations + db file) cleaned up on drop.
    struct TestDir {
        base: PathBuf,
        mig: PathBuf,
        db: PathBuf,
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.base);
        }
    }

    fn test_dir() -> TestDir {
        let pid = std::process::id();
        let n = CTR.fetch_add(1, Ordering::SeqCst);
        let base = std::env::temp_dir().join(format!("ferrule-migrate-test-{pid}-{n}"));
        let mig = base.join("mig");
        std::fs::create_dir_all(&mig).expect("create temp migrations dir");
        let db = base.join("test.db");
        TestDir { base, mig, db }
    }

    async fn engine(t: &TestDir) -> MigrationEngine {
        let url =
            DatabaseUrl::parse(&format!("sqlite://{}", t.db.display())).expect("parse sqlite url");
        let conn = ferrule_sql::backends::sqlite::connect(&url, &ConnectOptions::default())
            .await
            .expect("connect sqlite");
        MigrationEngine::new(Box::new(conn), t.mig.clone(), Dialect::Sqlite)
    }

    fn write_pair(t: &TestDir, stem: &str, up: &str, down: &str) {
        std::fs::write(t.mig.join(format!("{stem}.up.sql")), up).expect("write up");
        std::fs::write(t.mig.join(format!("{stem}.down.sql")), down).expect("write down");
    }

    #[tokio::test]
    async fn lifecycle_up_verify_down() {
        let t = test_dir();
        write_pair(
            &t,
            "20240101_users",
            "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT);\n\
             INSERT INTO users (name) VALUES ('a');\n",
            "DROP TABLE users;\n",
        );
        let mut eng = engine(&t).await;
        eng.ensure_migration_table().await.unwrap();

        let pending = eng.pending_migrations().await.unwrap();
        assert_eq!(pending.len(), 1);
        eng.apply_up(&pending[0]).await.unwrap();

        // Recorded, schema + data present, no drift, nothing left pending.
        assert!(eng.applied_versions().await.unwrap().contains("20240101"));
        let rows = eng.conn.query("SELECT name FROM users").await.unwrap();
        assert_eq!(rows.rows.len(), 1);
        let all = eng.all_applied().await.unwrap();
        assert!(eng.verify_applied(&all).await.unwrap().is_empty());
        assert!(eng.pending_migrations().await.unwrap().is_empty());

        // Roll back: tracking row gone and the table dropped.
        let downs = eng.scan_dir(Direction::Down).unwrap();
        eng.apply_down(&downs[0]).await.unwrap();
        assert!(eng.applied_versions().await.unwrap().is_empty());
        let tbls = eng
            .conn
            .query("SELECT name FROM sqlite_master WHERE type='table' AND name='users'")
            .await
            .unwrap();
        assert_eq!(tbls.rows.len(), 0);
    }

    #[tokio::test]
    async fn apply_up_rolls_back_on_failure() {
        // Regression guard: the second statement fails, so the whole
        // migration (the first statement + the tracking row) must roll back.
        let t = test_dir();
        write_pair(
            &t,
            "20240101_bad",
            "CREATE TABLE keep_me (id INTEGER);\nCREATE TABLE keep_me (id INTEGER);\n",
            "DROP TABLE keep_me;\n",
        );
        let mut eng = engine(&t).await;
        eng.ensure_migration_table().await.unwrap();
        let pending = eng.pending_migrations().await.unwrap();

        assert!(
            eng.apply_up(&pending[0]).await.is_err(),
            "apply_up must fail on the duplicate CREATE"
        );
        let tbls = eng
            .conn
            .query("SELECT name FROM sqlite_master WHERE type='table' AND name='keep_me'")
            .await
            .unwrap();
        assert_eq!(tbls.rows.len(), 0, "partial schema must be rolled back");
        assert!(
            eng.applied_versions().await.unwrap().is_empty(),
            "a failed migration must not be recorded"
        );
    }

    #[tokio::test]
    async fn duplicate_version_is_rejected() {
        let t = test_dir();
        write_pair(&t, "20240101_a", "CREATE TABLE a(x);\n", "DROP TABLE a;\n");
        write_pair(&t, "20240101_b", "CREATE TABLE b(x);\n", "DROP TABLE b;\n");
        let eng = engine(&t).await;
        assert!(
            eng.scan_dir(Direction::Up).is_err(),
            "two files deriving the same version must be rejected up front"
        );
    }

    #[tokio::test]
    async fn verify_applied_detects_checksum_drift() {
        let t = test_dir();
        write_pair(&t, "20240101_e", "CREATE TABLE e(x);\n", "DROP TABLE e;\n");
        let mut eng = engine(&t).await;
        eng.ensure_migration_table().await.unwrap();
        let pending = eng.pending_migrations().await.unwrap();
        eng.apply_up(&pending[0]).await.unwrap();

        // Edit the applied file: verify must flag the checksum drift.
        std::fs::write(t.mig.join("20240101_e.up.sql"), "CREATE TABLE e(x, y);\n").unwrap();
        let all = eng.all_applied().await.unwrap();
        let drift = eng.verify_applied(&all).await.unwrap();
        assert_eq!(drift.len(), 1);
        assert_eq!(drift[0].version, "20240101");
    }

    #[tokio::test]
    async fn verify_applied_detects_missing_file() {
        let t = test_dir();
        write_pair(&t, "20240101_g", "CREATE TABLE g(x);\n", "DROP TABLE g;\n");
        let mut eng = engine(&t).await;
        eng.ensure_migration_table().await.unwrap();
        let pending = eng.pending_migrations().await.unwrap();
        eng.apply_up(&pending[0]).await.unwrap();

        // Delete the on-disk file: verify must report applied-but-missing drift.
        std::fs::remove_file(t.mig.join("20240101_g.up.sql")).unwrap();
        std::fs::remove_file(t.mig.join("20240101_g.down.sql")).unwrap();
        let all = eng.all_applied().await.unwrap();
        let drift = eng.verify_applied(&all).await.unwrap();
        assert_eq!(drift.len(), 1);
        assert_eq!(drift[0].version, "20240101");
    }
}
