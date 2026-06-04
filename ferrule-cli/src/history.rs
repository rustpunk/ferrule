//! Persistent query-history store (R4 / #4).
//!
//! Backed by a SQLite database under `dirs::data_local_dir()/ferrule/
//! history.db` (or wherever `[history] path = ...` points). Recording is
//! driven by a single `record_dispatch()` hook in `main.rs` that captures
//! every ferrule invocation's wall-clock duration, redacted connection
//! URL, command kind, SQL body, row count, and exit code.
//!
//! The store is the shared infrastructure that downstream phases of the
//! Query Telemetry Foundation sprint hang off:
//!   - Phase 2 (#16): slow-log tee + `ferrule history --slow` filter
//!   - Phase 3 (#15): `--bench N` records one summarised RunRecord
//!   - Phase 4 (#3):  `--fail-on-empty` records exit_code = 1 ("notable")
//!
//! See `docs/src/telemetry.md` (added in Phase 5) for the user-facing
//! contract.

use chrono::{DateTime, Duration, Utc};
use ferrule_config::{HistoryConfig, SlowLogConfig};
use rusqlite::{params, Connection};
use std::fs::OpenOptions;
use std::io::Write;
use std::path::PathBuf;
use std::sync::Mutex;

use crate::error::CliError;
use crate::path_util::expand_tilde;

/// A single ferrule invocation worth recording.
#[derive(Debug, Clone)]
pub struct RunRecord {
    pub ts: DateTime<Utc>,
    /// Redacted connection URL or registry name. May be `None` for
    /// commands that don't address a single connection (e.g.
    /// `ferrule history` itself, `ferrule conn list`).
    pub conn: Option<String>,
    /// Subcommand name (e.g. `"query"`, `"copy"`, `"export"`).
    pub command: String,
    /// SQL body, when applicable. `None` for non-query subcommands.
    pub sql: Option<String>,
    pub duration_ms: u64,
    /// Number of rows returned (SELECT) or affected (DML). `None` when
    /// the subcommand doesn't carry a row-count concept (e.g.
    /// `ferrule conn test`).
    pub rows: Option<i64>,
    pub exit_code: i32,
    /// `miette`-formatted error class on failure (e.g. `"connection"`,
    /// `"query"`, `"usage"`). `None` on success or notable-result exit.
    pub error: Option<String>,
}

/// Query predicate for `HistoryDb::query()`.
///
/// All fields are AND-combined; a field set to its default disables that
/// predicate. Reused unchanged by Phase 2 — `min_duration_ms` is the
/// slow-log filter.
#[derive(Debug, Default, Clone)]
pub struct HistoryFilter {
    pub last: Option<usize>,
    /// Connection-name glob (shell-style `*` / `?`). Matched against the
    /// redacted form stored in `RunRecord::conn`.
    pub conn: Option<String>,
    /// "Most recent N hours / days" — implemented by comparing against
    /// `Utc::now() - since`.
    pub since: Option<Duration>,
    /// Substring match (case-insensitive) against `sql`.
    pub grep: Option<String>,
    /// Sort by duration descending instead of timestamp descending.
    pub slowest: bool,
    /// Slow-query gate. Phase 2 wires this up to `[slow_log] threshold`.
    pub min_duration_ms: Option<u64>,
}

/// Slow-query log tee. Append-only file handle wrapped in a Mutex so
/// `HistoryDb::record()` stays `&mut self` without dragging async/Send
/// constraints onto the rusqlite connection.
struct SlowSink {
    file: Mutex<std::fs::File>,
    threshold_ms: u64,
    /// Path of the live log, needed to rename it on rotation. The
    /// archive lives at `<path>.1`.
    path: PathBuf,
    /// Rotate to `<path>.1` before a write that would push the file past
    /// this many bytes (#55). `None` disables rotation.
    max_size_bytes: Option<u64>,
}

/// SQLite-backed history store. Owns one connection; not Send because
/// `rusqlite::Connection` isn't Send by default.
pub struct HistoryDb {
    conn: Connection,
    slow: Option<SlowSink>,
}

impl std::fmt::Debug for HistoryDb {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HistoryDb")
            .field("slow", &self.slow.is_some())
            .finish_non_exhaustive()
    }
}

impl HistoryDb {
    /// Open (and migrate) the history database at `path`. Creates parent
    /// directories as needed.
    pub fn open(path: &std::path::Path) -> Result<Self, CliError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(CliError::Io)?;
        }
        let conn = Connection::open(path)
            .map_err(|e| CliError::usage(format!("history: failed to open {path:?}: {e}")))?;
        // #58: bake in busy_timeout so a second concurrent ferrule
        // invocation's record()/prune() doesn't surface as a spurious
        // SQLITE_BUSY failure. Set before migrate so the schema setup
        // itself is covered. Mirrors `cache.rs`.
        conn.busy_timeout(std::time::Duration::from_secs(5))
            .map_err(|e| CliError::usage(format!("history: busy_timeout: {e}")))?;
        // #57: explicit PRAGMA user_version migration scaffold instead of
        // a bare CREATE TABLE IF NOT EXISTS. Refuses to clobber a forward-
        // compatible file written by a newer binary. Mirrors `cache.rs`.
        migrate(&conn)?;
        Ok(Self { conn, slow: None })
    }

    /// Resolve the store path from config + env, opening if recording is
    /// enabled. Returns `Ok(None)` when recording is disabled (either by
    /// config or by the `FERRULE_NO_HISTORY` env override).
    ///
    /// When `slow_log.enabled`, also opens the side-channel tee file in
    /// append mode. Slow-log open failure is fatal because the user
    /// explicitly opted in — silently skipping would hide a real
    /// misconfiguration (bad path, no write permission).
    pub fn maybe_open(cfg: &HistoryConfig) -> Result<Option<Self>, CliError> {
        Self::maybe_open_with_slow(cfg, &SlowLogConfig::default())
    }

    pub fn maybe_open_with_slow(
        cfg: &HistoryConfig,
        slow: &SlowLogConfig,
    ) -> Result<Option<Self>, CliError> {
        if !cfg.enabled || std::env::var_os("FERRULE_NO_HISTORY").is_some() {
            return Ok(None);
        }
        let path = resolve_path(cfg)?;
        let mut db = Self::open(&path)?;
        if slow.enabled {
            let threshold_ms = slow
                .threshold_ms()
                .map_err(|e| CliError::usage(format!("slow_log: {e}")))?;
            let slow_path = resolve_slow_path(slow)?;
            if let Some(parent) = slow_path.parent() {
                std::fs::create_dir_all(parent).map_err(CliError::Io)?;
            }
            let max_size_bytes = slow
                .max_size_bytes()
                .map_err(|e| CliError::usage(format!("slow_log: {e}")))?;
            let file = OpenOptions::new()
                .append(true)
                .create(true)
                .open(&slow_path)
                .map_err(CliError::Io)?;
            db.slow = Some(SlowSink {
                file: Mutex::new(file),
                threshold_ms,
                path: slow_path,
                max_size_bytes,
            });
        }
        Ok(Some(db))
    }

    /// Insert one row, then opportunistically prune per retention config.
    /// Also tees to the slow-log when configured and the run exceeded
    /// `slow_log.threshold`.
    pub fn record(&mut self, record: &RunRecord, cfg: &HistoryConfig) -> Result<(), CliError> {
        self.conn
            .execute(
                "INSERT INTO history \
                 (ts, conn, command, sql, duration_ms, rows, exit_code, error) \
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
                params![
                    record.ts.to_rfc3339(),
                    record.conn,
                    record.command,
                    record.sql,
                    record.duration_ms as i64,
                    record.rows,
                    record.exit_code,
                    record.error,
                ],
            )
            .map_err(|e| CliError::usage(format!("history: record failed: {e}")))?;
        self.tee_slow(record)?;
        self.prune(cfg)?;
        Ok(())
    }

    fn tee_slow(&self, record: &RunRecord) -> Result<(), CliError> {
        let Some(sink) = self.slow.as_ref() else {
            return Ok(());
        };
        if record.duration_ms < sink.threshold_ms {
            return Ok(());
        }
        // Tab-separated: ts conn duration_ms sql_oneline rows
        let line = format!(
            "{}\t{}\t{}\t{}\t{}\n",
            record.ts.to_rfc3339(),
            record.conn.as_deref().unwrap_or(""),
            record.duration_ms,
            record
                .sql
                .as_deref()
                .map(oneline_for_log)
                .unwrap_or_default(),
            record
                .rows
                .map(|n| n.to_string())
                .unwrap_or_else(|| "-".to_string()),
        );
        let mut file = sink
            .file
            .lock()
            .map_err(|e| CliError::usage(format!("slow_log: lock poisoned: {e}")))?;
        // #55: single-archive rotation. Under the held lock so two
        // records in the same process can't both rotate. If appending
        // this line would push the file past the configured cap, move
        // the current log to `<path>.1` (overwriting any prior archive)
        // and start a fresh empty log. A file that is already over-cap
        // but empty-after-rotation still accepts an oversized line — the
        // cap bounds the *count* of retained lines, not a single line.
        if let Some(cap) = sink.max_size_bytes {
            let current = file.metadata().map_err(CliError::Io)?.len();
            if current > 0 && current.saturating_add(line.len() as u64) > cap {
                let archive = rotated_path(&sink.path);
                std::fs::rename(&sink.path, &archive).map_err(CliError::Io)?;
                let fresh = OpenOptions::new()
                    .append(true)
                    .create(true)
                    .open(&sink.path)
                    .map_err(CliError::Io)?;
                *file = fresh;
            }
        }
        file.write_all(line.as_bytes()).map_err(CliError::Io)?;
        Ok(())
    }

    /// Open-loop pruning: drop rows older than `max_age_days`, then
    /// trim total count to `max_rows`. Zero in either field disables
    /// that pass. Returns the number of rows deleted across both passes.
    /// The opportunistic caller in [`Self::record`] ignores the count;
    /// `ferrule history prune` reports it.
    pub fn prune(&mut self, cfg: &HistoryConfig) -> Result<u64, CliError> {
        let mut deleted: u64 = 0;
        if cfg.max_age_days > 0 {
            let cutoff = Utc::now() - Duration::days(i64::from(cfg.max_age_days));
            let n = self
                .conn
                .execute(
                    "DELETE FROM history WHERE ts < ?",
                    params![cutoff.to_rfc3339()],
                )
                .map_err(|e| CliError::usage(format!("history: prune (age) failed: {e}")))?;
            deleted += n as u64;
        }
        if cfg.max_rows > 0 {
            // Count cheap before deleting; SQLite uses a covering index on id.
            let total: i64 = self
                .conn
                .query_row("SELECT COUNT(*) FROM history", [], |r| r.get(0))
                .map_err(|e| CliError::usage(format!("history: count failed: {e}")))?;
            let excess = total.saturating_sub(cfg.max_rows as i64);
            if excess > 0 {
                let n = self
                    .conn
                    .execute(
                        "DELETE FROM history WHERE id IN \
                         (SELECT id FROM history ORDER BY id ASC LIMIT ?)",
                        params![excess],
                    )
                    .map_err(|e| CliError::usage(format!("history: prune (count) failed: {e}")))?;
                deleted += n as u64;
            }
        }
        Ok(deleted)
    }

    /// Count how many rows [`Self::prune`] would delete under `cfg`,
    /// without deleting anything. Mirrors prune's two-pass age/count
    /// logic with `COUNT` in place of `DELETE`, so `--dry-run` reports
    /// the same number a real prune would remove.
    pub fn prune_dry_run(&self, cfg: &HistoryConfig) -> Result<u64, CliError> {
        let mut would_delete: u64 = 0;
        // Pass 1: rows older than max_age_days.
        if cfg.max_age_days > 0 {
            let cutoff = Utc::now() - Duration::days(i64::from(cfg.max_age_days));
            let n: i64 = self
                .conn
                .query_row(
                    "SELECT COUNT(*) FROM history WHERE ts < ?",
                    params![cutoff.to_rfc3339()],
                    |r| r.get(0),
                )
                .map_err(|e| CliError::usage(format!("history: dry-run (age) failed: {e}")))?;
            would_delete += n as u64;
        }
        // Pass 2: count-based trim. Mirror prune: it computes excess from
        // the *current* total, then deletes the oldest `excess` rows.
        // The age pass runs first in prune, so the post-age survivor
        // count is `total - (age-deleted)`; compute it with the same
        // cutoff predicate to keep dry-run faithful.
        if cfg.max_rows > 0 {
            let surviving: i64 = if cfg.max_age_days > 0 {
                let cutoff = Utc::now() - Duration::days(i64::from(cfg.max_age_days));
                self.conn
                    .query_row(
                        "SELECT COUNT(*) FROM history WHERE ts >= ?",
                        params![cutoff.to_rfc3339()],
                        |r| r.get(0),
                    )
                    .map_err(|e| CliError::usage(format!("history: dry-run (count) failed: {e}")))?
            } else {
                self.conn
                    .query_row("SELECT COUNT(*) FROM history", [], |r| r.get(0))
                    .map_err(|e| CliError::usage(format!("history: dry-run (count) failed: {e}")))?
            };
            let excess = surviving.saturating_sub(cfg.max_rows as i64);
            if excess > 0 {
                would_delete += excess as u64;
            }
        }
        Ok(would_delete)
    }

    /// Read rows back from the store, ordered most-recent first (or
    /// slowest first when `filter.slowest` is set).
    pub fn query(&self, filter: &HistoryFilter) -> Result<Vec<RunRecord>, CliError> {
        let mut sql = String::from(
            "SELECT ts, conn, command, sql, duration_ms, rows, exit_code, error FROM history",
        );
        let mut clauses: Vec<String> = Vec::new();
        let mut binds: Vec<rusqlite::types::Value> = Vec::new();

        if let Some(since) = filter.since {
            let cutoff = Utc::now() - since;
            clauses.push("ts >= ?".into());
            binds.push(cutoff.to_rfc3339().into());
        }
        if let Some(min_ms) = filter.min_duration_ms {
            clauses.push("duration_ms >= ?".into());
            binds.push((min_ms as i64).into());
        }
        if let Some(grep) = filter.grep.as_deref().filter(|s| !s.is_empty()) {
            clauses.push("sql LIKE ? COLLATE NOCASE".into());
            binds.push(format!("%{grep}%").into());
        }

        if !clauses.is_empty() {
            sql.push_str(" WHERE ");
            sql.push_str(&clauses.join(" AND "));
        }

        sql.push_str(if filter.slowest {
            " ORDER BY duration_ms DESC, id DESC"
        } else {
            " ORDER BY id DESC"
        });

        // Apply LIMIT after conn-glob filtering (which we do post-SQL
        // because globbing in SQLite would need a custom function).
        // Over-fetch when conn-glob is set so the post-filter still
        // returns `last` rows.
        let fetch_limit = match (filter.last, filter.conn.as_deref()) {
            (Some(n), Some(_)) => Some(n * 8),
            (Some(n), None) => Some(n),
            _ => None,
        };
        if let Some(n) = fetch_limit {
            sql.push_str(" LIMIT ?");
            binds.push((n as i64).into());
        }

        let mut stmt = self
            .conn
            .prepare(&sql)
            .map_err(|e| CliError::usage(format!("history: prepare failed: {e}")))?;
        let bind_refs: Vec<&dyn rusqlite::ToSql> =
            binds.iter().map(|b| b as &dyn rusqlite::ToSql).collect();
        let rows = stmt
            .query_map(bind_refs.as_slice(), row_to_record)
            .map_err(|e| CliError::usage(format!("history: query failed: {e}")))?;

        let mut out: Vec<RunRecord> = Vec::new();
        for r in rows {
            let rec = r.map_err(|e| CliError::usage(format!("history: row decode failed: {e}")))?;
            if let Some(pat) = filter.conn.as_deref() {
                let stored = rec.conn.as_deref().unwrap_or("");
                if !glob_match(pat, stored) {
                    continue;
                }
            }
            out.push(rec);
            if let Some(n) = filter.last {
                if out.len() >= n {
                    break;
                }
            }
        }
        Ok(out)
    }

    /// Count rows currently stored. Test-only convenience.
    #[cfg(test)]
    fn count(&self) -> Result<i64, CliError> {
        self.conn
            .query_row("SELECT COUNT(*) FROM history", [], |r| r.get::<_, i64>(0))
            .map_err(|e| CliError::usage(format!("history: count failed: {e}")))
    }
}

fn row_to_record(row: &rusqlite::Row<'_>) -> rusqlite::Result<RunRecord> {
    let ts_str: String = row.get(0)?;
    let ts = DateTime::parse_from_rfc3339(&ts_str)
        .map(|dt| dt.with_timezone(&Utc))
        .unwrap_or_else(|_| Utc::now());
    Ok(RunRecord {
        ts,
        conn: row.get::<_, Option<String>>(1)?,
        command: row.get(2)?,
        sql: row.get::<_, Option<String>>(3)?,
        duration_ms: row.get::<_, i64>(4)? as u64,
        rows: row.get::<_, Option<i64>>(5)?,
        exit_code: row.get(6)?,
        error: row.get::<_, Option<String>>(7)?,
    })
}

fn resolve_path(cfg: &HistoryConfig) -> Result<PathBuf, CliError> {
    if let Some(p) = cfg.path.as_deref() {
        return Ok(expand_tilde(p));
    }
    let base = dirs::data_local_dir().ok_or_else(|| {
        CliError::usage("history: could not determine data-local directory for default path")
    })?;
    Ok(base.join("ferrule").join("history.db"))
}

fn resolve_slow_path(cfg: &SlowLogConfig) -> Result<PathBuf, CliError> {
    if let Some(p) = cfg.path.as_deref() {
        return Ok(expand_tilde(p));
    }
    let base = dirs::data_local_dir().ok_or_else(|| {
        CliError::usage("slow_log: could not determine data-local directory for default path")
    })?;
    Ok(base.join("ferrule").join("slow.log"))
}

fn oneline_for_log(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The single rotation archive path for a slow-log file: `<path>.1`.
/// Appends `.1` to the file name (preserving any existing extension) so
/// `slow.log` rotates to `slow.log.1`.
fn rotated_path(path: &std::path::Path) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(".1");
    PathBuf::from(name)
}

/// Shell-style glob: `*` matches any run, `?` matches a single char.
/// Empty pattern matches anything. Case-insensitive.
fn glob_match(pat: &str, s: &str) -> bool {
    if pat.is_empty() {
        return true;
    }
    let pat: Vec<char> = pat.to_lowercase().chars().collect();
    let s: Vec<char> = s.to_lowercase().chars().collect();
    glob_inner(&pat, &s)
}

fn glob_inner(pat: &[char], s: &[char]) -> bool {
    let mut pi = 0usize;
    let mut si = 0usize;
    let mut star: Option<(usize, usize)> = None;
    while si < s.len() {
        if pi < pat.len() && (pat[pi] == '?' || pat[pi] == s[si]) {
            pi += 1;
            si += 1;
        } else if pi < pat.len() && pat[pi] == '*' {
            star = Some((pi, si));
            pi += 1;
        } else if let Some((sp, ss)) = star {
            pi = sp + 1;
            si = ss + 1;
            star = Some((sp, si));
        } else {
            return false;
        }
    }
    while pi < pat.len() && pat[pi] == '*' {
        pi += 1;
    }
    pi == pat.len()
}

const SCHEMA_V1: &str = r#"
CREATE TABLE IF NOT EXISTS history (
    id           INTEGER PRIMARY KEY AUTOINCREMENT,
    ts           TEXT    NOT NULL,
    conn         TEXT,
    command      TEXT    NOT NULL,
    sql          TEXT,
    duration_ms  INTEGER NOT NULL,
    rows         INTEGER,
    exit_code    INTEGER NOT NULL,
    error        TEXT
);
CREATE INDEX IF NOT EXISTS history_ts_idx       ON history(ts);
CREATE INDEX IF NOT EXISTS history_duration_idx ON history(duration_ms);
"#;

const LATEST_VERSION: u32 = 1;

/// Versioned schema migrations. Append new tuples; never edit historical
/// ones. The migrator runs every entry whose version is strictly greater
/// than the current `PRAGMA user_version`. Mirrors `cache.rs::MIGRATIONS`.
///
/// Note on existing files: a pre-scaffold `history.db` (written by a
/// ferrule that ran the bare schema and never set `user_version`) reports
/// `user_version = 0`, so v1 re-runs — harmlessly, because the v1 SQL is
/// all `CREATE … IF NOT EXISTS` — and stamps the file to 1. No data loss.
const MIGRATIONS: &[(u32, &str)] = &[(1, SCHEMA_V1)];

/// #57: `PRAGMA user_version` migration scaffold. A downgrade — a newer
/// ferrule binary wrote a higher `user_version`, an older binary opens it —
/// is a hard usage error, not a silent re-migration that would clobber a
/// forward-compatible file. Mirrors `cache.rs::migrate`.
fn migrate(conn: &Connection) -> Result<(), CliError> {
    let current: u32 = conn
        .query_row("PRAGMA user_version", [], |r| r.get::<_, i64>(0))
        .map_err(|e| CliError::usage(format!("history: read user_version: {e}")))?
        as u32;
    if current > LATEST_VERSION {
        return Err(CliError::usage(format!(
            "history: history.db user_version={current} is newer than this \
             binary supports (max {LATEST_VERSION}). Downgrade detected — \
             refusing to clobber. Delete history.db or upgrade ferrule."
        )));
    }
    for (v, sql) in MIGRATIONS.iter().filter(|(v, _)| *v > current) {
        conn.execute_batch(sql)
            .map_err(|e| CliError::usage(format!("history: migration v{v} failed: {e}")))?;
        conn.execute_batch(&format!("PRAGMA user_version = {v}"))
            .map_err(|e| CliError::usage(format!("history: bump user_version to {v}: {e}")))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(sql: &str, dur_ms: u64) -> RunRecord {
        RunRecord {
            ts: Utc::now(),
            conn: Some("postgres://user:***@host:5432/db".into()),
            command: "query".into(),
            sql: Some(sql.into()),
            duration_ms: dur_ms,
            rows: Some(1),
            exit_code: 0,
            error: None,
        }
    }

    fn open_memory() -> HistoryDb {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        HistoryDb { conn, slow: None }
    }

    #[test]
    fn record_then_query_round_trip() {
        let mut db = open_memory();
        let cfg = HistoryConfig::default();
        db.record(&rec("SELECT 1", 5), &cfg).unwrap();
        db.record(&rec("SELECT 2", 50), &cfg).unwrap();
        let rows = db.query(&HistoryFilter::default()).unwrap();
        assert_eq!(rows.len(), 2);
        // newest first
        assert_eq!(rows[0].sql.as_deref(), Some("SELECT 2"));
    }

    #[test]
    fn filter_last_caps_result_count() {
        let mut db = open_memory();
        let cfg = HistoryConfig::default();
        for i in 0..10 {
            db.record(&rec(&format!("Q{i}"), 10), &cfg).unwrap();
        }
        let rows = db
            .query(&HistoryFilter {
                last: Some(3),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].sql.as_deref(), Some("Q9"));
    }

    #[test]
    fn filter_min_duration_ms_acts_as_slow_gate() {
        let mut db = open_memory();
        let cfg = HistoryConfig::default();
        db.record(&rec("fast", 5), &cfg).unwrap();
        db.record(&rec("slow", 5_000), &cfg).unwrap();
        let rows = db
            .query(&HistoryFilter {
                min_duration_ms: Some(1_000),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].sql.as_deref(), Some("slow"));
    }

    #[test]
    fn filter_slowest_orders_by_duration() {
        let mut db = open_memory();
        let cfg = HistoryConfig::default();
        db.record(&rec("a", 10), &cfg).unwrap();
        db.record(&rec("b", 1_000), &cfg).unwrap();
        db.record(&rec("c", 100), &cfg).unwrap();
        let rows = db
            .query(&HistoryFilter {
                slowest: true,
                ..Default::default()
            })
            .unwrap();
        let sqls: Vec<_> = rows.iter().filter_map(|r| r.sql.as_deref()).collect();
        assert_eq!(sqls, ["b", "c", "a"]);
    }

    #[test]
    fn filter_grep_is_case_insensitive_substring() {
        let mut db = open_memory();
        let cfg = HistoryConfig::default();
        db.record(&rec("SELECT * FROM USERS", 10), &cfg).unwrap();
        db.record(&rec("INSERT INTO orders ...", 10), &cfg).unwrap();
        let rows = db
            .query(&HistoryFilter {
                grep: Some("users".into()),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert!(rows[0].sql.as_deref().unwrap().contains("USERS"));
    }

    #[test]
    fn filter_conn_glob_matches() {
        let mut db = open_memory();
        let cfg = HistoryConfig::default();
        let mut prod = rec("SELECT 1", 5);
        prod.conn = Some("postgres://x:***@prod-host:5432/db".into());
        let mut dev = rec("SELECT 1", 5);
        dev.conn = Some("postgres://x:***@dev-host:5432/db".into());
        db.record(&prod, &cfg).unwrap();
        db.record(&dev, &cfg).unwrap();
        let rows = db
            .query(&HistoryFilter {
                conn: Some("*prod*".into()),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert!(rows[0].conn.as_deref().unwrap().contains("prod"));
    }

    #[test]
    fn prune_drops_rows_older_than_max_age_days() {
        let mut db = open_memory();
        let cfg = HistoryConfig {
            max_age_days: 7,
            max_rows: 0,
            ..Default::default()
        };
        // Insert a stale row directly.
        let stale = Utc::now() - Duration::days(30);
        db.conn
            .execute(
                "INSERT INTO history (ts, conn, command, sql, duration_ms, rows, exit_code, error) \
                 VALUES (?, NULL, 'query', 'stale', 1, NULL, 0, NULL)",
                params![stale.to_rfc3339()],
            )
            .unwrap();
        db.record(&rec("fresh", 1), &cfg).unwrap();
        let rows = db.query(&HistoryFilter::default()).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].sql.as_deref(), Some("fresh"));
    }

    #[test]
    fn prune_trims_oldest_when_over_max_rows() {
        let mut db = open_memory();
        let cfg = HistoryConfig {
            max_age_days: 0,
            max_rows: 5,
            ..Default::default()
        };
        for i in 0..10 {
            db.record(&rec(&format!("Q{i}"), 1), &cfg).unwrap();
        }
        assert_eq!(db.count().unwrap(), 5);
        let rows = db.query(&HistoryFilter::default()).unwrap();
        // newest five survive; the oldest (Q0..Q4) are gone.
        let sqls: Vec<_> = rows.iter().filter_map(|r| r.sql.as_deref()).collect();
        assert_eq!(sqls, ["Q9", "Q8", "Q7", "Q6", "Q5"]);
    }

    // #48: prune returns the number of rows it deleted, and a dry run
    // reports the same number without touching the store.
    #[test]
    fn prune_returns_deleted_count_and_dry_run_matches() {
        let mut db = open_memory();
        // Insert 10 rows with pruning disabled so the count stays at 10.
        let no_prune = HistoryConfig {
            max_age_days: 0,
            max_rows: 0,
            ..Default::default()
        };
        for i in 0..10 {
            db.record(&rec(&format!("Q{i}"), 1), &no_prune).unwrap();
        }
        assert_eq!(db.count().unwrap(), 10);

        // A prune to max_rows = 4 should drop 6 rows.
        let trim = HistoryConfig {
            max_age_days: 0,
            max_rows: 4,
            ..Default::default()
        };
        // Dry run first: reports 6, deletes nothing.
        let would = db.prune_dry_run(&trim).unwrap();
        assert_eq!(would, 6, "dry run should report 6 would-be deletions");
        assert_eq!(db.count().unwrap(), 10, "dry run must not delete");

        // Real prune: deletes exactly the 6 the dry run predicted.
        let deleted = db.prune(&trim).unwrap();
        assert_eq!(deleted, would, "real delete count must match dry run");
        assert_eq!(deleted, 6);
        assert_eq!(db.count().unwrap(), 4);
    }

    // #48: a second prune on an already-trimmed store deletes nothing.
    #[test]
    fn prune_is_idempotent_on_trimmed_store() {
        let mut db = open_memory();
        let trim = HistoryConfig {
            max_age_days: 0,
            max_rows: 3,
            ..Default::default()
        };
        for i in 0..3 {
            db.record(&rec(&format!("Q{i}"), 1), &trim).unwrap();
        }
        assert_eq!(db.prune_dry_run(&trim).unwrap(), 0);
        assert_eq!(db.prune(&trim).unwrap(), 0);
        assert_eq!(db.count().unwrap(), 3);
    }

    #[test]
    fn maybe_open_returns_none_when_disabled() {
        let cfg = HistoryConfig {
            enabled: false,
            ..Default::default()
        };
        assert!(HistoryDb::maybe_open(&cfg).unwrap().is_none());
    }

    #[test]
    fn glob_matches_star_question_and_literal() {
        assert!(glob_match("*prod*", "postgres://prod.host/db"));
        assert!(glob_match("postgres://*", "postgres://x/y"));
        assert!(glob_match("???", "abc"));
        assert!(!glob_match("???", "abcd"));
        assert!(glob_match("", "anything"));
    }

    fn open_memory_with_slow(path: &std::path::Path, threshold_ms: u64) -> HistoryDb {
        open_memory_with_slow_cap(path, threshold_ms, None)
    }

    fn open_memory_with_slow_cap(
        path: &std::path::Path,
        threshold_ms: u64,
        max_size_bytes: Option<u64>,
    ) -> HistoryDb {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        let file = OpenOptions::new()
            .append(true)
            .create(true)
            .open(path)
            .unwrap();
        HistoryDb {
            conn,
            slow: Some(SlowSink {
                file: Mutex::new(file),
                threshold_ms,
                path: path.to_path_buf(),
                max_size_bytes,
            }),
        }
    }

    #[test]
    fn slow_log_tees_when_threshold_crossed() {
        let tmp = tempfile::tempdir().unwrap();
        let log = tmp.path().join("slow.log");
        let mut db = open_memory_with_slow(&log, 100);
        let cfg = HistoryConfig::default();

        db.record(&rec("fast", 5), &cfg).unwrap();
        db.record(&rec("slow-1", 150), &cfg).unwrap();
        db.record(&rec("slow-2", 250), &cfg).unwrap();

        let body = std::fs::read_to_string(&log).unwrap();
        let lines: Vec<&str> = body.lines().collect();
        assert_eq!(
            lines.len(),
            2,
            "only slow runs should be teed; got {body:?}"
        );
        assert!(lines[0].contains("slow-1"));
        assert!(lines[1].contains("slow-2"));
        // Tab-separated.
        assert_eq!(lines[0].matches('\t').count(), 4);
    }

    #[test]
    fn slow_log_skips_below_threshold() {
        let tmp = tempfile::tempdir().unwrap();
        let log = tmp.path().join("slow.log");
        let mut db = open_memory_with_slow(&log, 10_000);
        let cfg = HistoryConfig::default();
        db.record(&rec("never-slow", 5), &cfg).unwrap();
        let body = std::fs::read_to_string(&log).unwrap_or_default();
        assert!(body.is_empty(), "slow.log should be empty, got {body:?}");
    }

    // #55: when the slow log crosses the configured byte cap, the live
    // file is rotated to `<path>.1` (single archive) and a fresh log is
    // started. The oldest teed lines end up in `.1`; the newest stay in
    // the live log. A second rotation overwrites `.1` rather than
    // accumulating `.2`, `.3`, …
    #[test]
    fn slow_log_rotates_single_archive_on_cap() {
        let tmp = tempfile::tempdir().unwrap();
        let log = tmp.path().join("slow.log");
        let archive = tmp.path().join("slow.log.1");
        // A single teed line for "slow-N" is well under 200 bytes; cap
        // at 200 forces a rotation after a few lines.
        let mut db = open_memory_with_slow_cap(&log, 100, Some(200));
        let cfg = HistoryConfig::default();

        // Record enough slow rows to rotate at least twice. Each line is
        // tagged with a monotonic marker so we can assert which file it
        // landed in.
        for i in 0..40 {
            db.record(&rec(&format!("marker-{i:02}"), 150), &cfg)
                .unwrap();
        }

        // Both files must exist after crossing the cap.
        assert!(log.exists(), "live log must exist");
        assert!(archive.exists(), "archive (.1) must exist after rotation");

        // The live log must respect the cap (allowing the over-cap final
        // line that triggered the *next* rotation not to have happened
        // yet — i.e. length <= cap + one line).
        let live = std::fs::read_to_string(&log).unwrap();
        let archived = std::fs::read_to_string(&archive).unwrap();
        let max_line = live
            .lines()
            .chain(archived.lines())
            .map(|l| l.len() + 1)
            .max()
            .unwrap_or(0) as u64;
        assert!(
            live.len() as u64 <= 200 + max_line,
            "live log {} bytes exceeds cap+one-line; got:\n{live}",
            live.len()
        );

        // Single-archive semantics: no `.2` is ever produced.
        let two = tmp.path().join("slow.log.2");
        assert!(
            !two.exists(),
            "only one archive (.1) should exist, never .2"
        );

        // The newest marker lives in the live log; an older marker lives
        // in the archive (proving rotation moved old lines aside).
        assert!(
            live.contains("marker-39"),
            "newest line should be in the live log; live:\n{live}"
        );
        let newest_in_archive = archived.contains("marker-39");
        assert!(
            !newest_in_archive,
            "newest line must not be in the archive; archive:\n{archived}"
        );
        assert!(
            archived.contains("marker-"),
            "archive should hold older teed lines; archive:\n{archived}"
        );
    }

    // #55: with no cap configured the log grows unbounded and no archive
    // is created — rotation is strictly opt-in.
    #[test]
    fn slow_log_no_cap_never_rotates() {
        let tmp = tempfile::tempdir().unwrap();
        let log = tmp.path().join("slow.log");
        let archive = tmp.path().join("slow.log.1");
        let mut db = open_memory_with_slow_cap(&log, 100, None);
        let cfg = HistoryConfig::default();
        for i in 0..50 {
            db.record(&rec(&format!("line-{i}"), 150), &cfg).unwrap();
        }
        assert!(log.exists());
        assert!(!archive.exists(), "no archive without a configured cap");
        let body = std::fs::read_to_string(&log).unwrap();
        assert_eq!(body.lines().count(), 50, "all lines stay in one file");
    }

    #[test]
    fn open_on_disk_creates_parent_and_persists() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("nested/sub/history.db");
        let mut db = HistoryDb::open(&path).unwrap();
        let cfg = HistoryConfig::default();
        db.record(&rec("durable", 7), &cfg).unwrap();
        drop(db);
        let db2 = HistoryDb::open(&path).unwrap();
        let rows = db2.query(&HistoryFilter::default()).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].sql.as_deref(), Some("durable"));
    }

    // #57: the migration scaffold stamps a fresh file to LATEST_VERSION and
    // is idempotent across reopens.
    #[test]
    fn migrate_stamps_and_is_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("history.db");
        let db1 = HistoryDb::open(&path).unwrap();
        let v1: i64 = db1
            .conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(v1, i64::from(LATEST_VERSION));
        drop(db1);
        // Reopen: user_version already current, no migration re-runs.
        let db2 = HistoryDb::open(&path).unwrap();
        let v2: i64 = db2
            .conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(v2, i64::from(LATEST_VERSION));
    }

    // #57: a pre-scaffold history.db (user_version never set, i.e. 0) with
    // existing rows upgrades cleanly — the idempotent CREATE … IF NOT EXISTS
    // preserves the data and the file is stamped to LATEST_VERSION.
    #[test]
    fn migrate_upgrades_pre_scaffold_file_without_data_loss() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("history.db");
        // Simulate an old file: schema applied, user_version left at 0.
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(SCHEMA_V1).unwrap();
            conn.execute(
                "INSERT INTO history (ts, command, duration_ms, exit_code) \
                 VALUES ('2020-01-01T00:00:00Z', 'query', 1, 0)",
                [],
            )
            .unwrap();
            let v: i64 = conn
                .query_row("PRAGMA user_version", [], |r| r.get(0))
                .unwrap();
            assert_eq!(v, 0, "precondition: legacy file is at user_version 0");
        }
        let db = HistoryDb::open(&path).unwrap();
        let v: i64 = db
            .conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(v, i64::from(LATEST_VERSION), "legacy file stamped forward");
        assert_eq!(
            db.query(&HistoryFilter::default()).unwrap().len(),
            1,
            "pre-existing row survived the upgrade"
        );
    }

    // #57: a file written by a newer binary (user_version > LATEST_VERSION)
    // is a hard error, not a silent re-migration.
    #[test]
    fn migrate_rejects_future_version() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("history.db");
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch("PRAGMA user_version = 99").unwrap();
        }
        let err = HistoryDb::open(&path).expect_err("downgrade must be rejected");
        let msg = match err {
            CliError::Usage(m) => m,
            other => panic!("expected CliError::Usage, got {other:?}"),
        };
        assert!(
            msg.contains("user_version=99"),
            "missing version in msg: {msg}"
        );
        assert!(msg.contains("Downgrade"), "missing downgrade label: {msg}");
    }

    // #58: busy_timeout is set so concurrent ferrule processes don't fail
    // instantly on SQLITE_BUSY.
    #[test]
    fn busy_timeout_set() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("history.db");
        let db = HistoryDb::open(&path).unwrap();
        let bt: i64 = db
            .conn
            .query_row("PRAGMA busy_timeout", [], |r| r.get(0))
            .unwrap();
        assert_eq!(bt, 5_000, "busy_timeout must be 5s in ms");
    }
}
