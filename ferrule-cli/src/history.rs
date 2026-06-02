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
}

/// SQLite-backed history store. Owns one connection; not Send because
/// `rusqlite::Connection` isn't Send by default.
pub struct HistoryDb {
    conn: Connection,
    slow: Option<SlowSink>,
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
        conn.execute_batch(SCHEMA)
            .map_err(|e| CliError::usage(format!("history: migration failed: {e}")))?;
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
            let file = OpenOptions::new()
                .append(true)
                .create(true)
                .open(&slow_path)
                .map_err(CliError::Io)?;
            db.slow = Some(SlowSink {
                file: Mutex::new(file),
                threshold_ms,
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
        file.write_all(line.as_bytes()).map_err(CliError::Io)?;
        Ok(())
    }

    /// Open-loop pruning: drop rows older than `max_age_days`, then
    /// trim total count to `max_rows`. Zero in either field disables
    /// that pass.
    pub fn prune(&mut self, cfg: &HistoryConfig) -> Result<(), CliError> {
        if cfg.max_age_days > 0 {
            let cutoff = Utc::now() - Duration::days(i64::from(cfg.max_age_days));
            self.conn
                .execute(
                    "DELETE FROM history WHERE ts < ?",
                    params![cutoff.to_rfc3339()],
                )
                .map_err(|e| CliError::usage(format!("history: prune (age) failed: {e}")))?;
        }
        if cfg.max_rows > 0 {
            // Count cheap before deleting; SQLite uses a covering index on id.
            let total: i64 = self
                .conn
                .query_row("SELECT COUNT(*) FROM history", [], |r| r.get(0))
                .map_err(|e| CliError::usage(format!("history: count failed: {e}")))?;
            let excess = total.saturating_sub(cfg.max_rows as i64);
            if excess > 0 {
                self.conn
                    .execute(
                        "DELETE FROM history WHERE id IN \
                         (SELECT id FROM history ORDER BY id ASC LIMIT ?)",
                        params![excess],
                    )
                    .map_err(|e| CliError::usage(format!("history: prune (count) failed: {e}")))?;
            }
        }
        Ok(())
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

const SCHEMA: &str = r#"
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
        conn.execute_batch(SCHEMA).unwrap();
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
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(SCHEMA).unwrap();
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
        assert_eq!(lines.len(), 2, "only slow runs should be teed; got {body:?}");
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
}
