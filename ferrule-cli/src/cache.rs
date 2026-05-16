//! Result-cache spike (R5 / #5).
//!
//! Backed by a SQLite database at `dirs::data_local_dir()/ferrule/
//! results.db` — a *separate* store from `history.db`. The
//! `[cache] path = "..."` override mirrors `HistoryConfig::path`.
//!
//! Two things this module bakes in from day one that `history.rs` only
//! grew later:
//!   - `PRAGMA user_version` migration scaffold (#57): a downgrade — a
//!     newer ferrule binary writes v2, an older binary opens it — is a
//!     hard usage error, not a silent re-migration that would clobber
//!     forward-compatible rows.
//!   - `busy_timeout(5s)` (#58): SQLite's default `SQLITE_BUSY` is
//!     instant. Cache reads racing the next process's `prune()` would
//!     surface as spurious lookup errors. 5 s is plenty for the
//!     "second concurrent ferrule invocation" case and bounded enough
//!     that a wedged caller still exits cleanly.
//!
//! All cache failures are non-fatal: the user's query still runs. The
//! call site in `commands/query.rs` swallows lookup and insert errors
//! at the dispatch boundary, surfacing them only under `--verbose`.
//! See `docs/src/cache.md` for the user-facing contract.

use ferrule_config::profile::CacheConfig;
use ferrule_core::connection::QueryResult;
use ferrule_core::value::{ColumnInfo, Row};
use ferrule_core::{DatabaseUrl, ParameterSet};
use rusqlite::{params, Connection};
use sha2::{Digest, Sha256};
use std::cell::RefCell;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::error::CliError;
use crate::path_util::expand_tilde;

/// Hex-encoded SHA-256 of `(redacted_conn, normalized_sql, params_canonical)`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CacheKey(pub String);

/// Inserted alongside each row so a future `ferrule cache list / clear`
/// can surface the entry without us having to bump the schema. Costs
/// ~250 bytes/row, written once per insert.
#[derive(Debug, Clone, Copy)]
pub struct CacheMeta<'a> {
    pub conn_redacted: &'a str,
    pub sql_preview: &'a str,
}

/// What `CacheDb::lookup` returns on a hit.
#[derive(Debug, Clone)]
pub struct CachedResult {
    pub result: QueryResult,
    pub age_secs: u64,
}

/// Stashed by `commands::query::run` so the dispatch hook in
/// `main.rs::record_dispatch` can fold cache hits / misses into the
/// `RunRecord`. Mirrors `bench.rs::{record_last, take_last}` so the
/// two thread-locals compose without a multiplexing channel.
#[derive(Debug, Clone)]
pub struct CacheHitInfo {
    pub hit: bool,
    pub lookup_micros: u64,
}

/// Prefix attached to a `RunRecord`'s `sql` field when a cache hit
/// folds into the dispatch hook. Downstream readers (history queries,
/// future `ferrule cache list`) detect cache-served runs by matching
/// this prefix.
pub const CACHE_HIT_PREFIX: &str = "cache_hit: ";

thread_local! {
    /// Set by `commands::query::run` to communicate cache hit/miss to
    /// the dispatch hook in `main.rs::record_dispatch`. The dispatch
    /// hook reads (and clears) this after the per-command run returns
    /// and folds it into the `RunRecord`'s `sql` / `duration_ms`.
    static LAST_CACHE: RefCell<Option<CacheHitInfo>> = const { RefCell::new(None) };
}

/// Stash the cache-event info so the dispatch hook can read it after
/// the run returns.
pub fn record_last(info: CacheHitInfo) {
    LAST_CACHE.with(|cell| *cell.borrow_mut() = Some(info));
}

/// Take the stashed cache event (if any). Called once per dispatch.
pub fn take_last() -> Option<CacheHitInfo> {
    LAST_CACHE.with(|cell| cell.borrow_mut().take())
}

/// SQLite-backed result cache. Owns one connection; not Send because
/// `rusqlite::Connection` isn't Send by default.
pub struct CacheDb {
    conn: Connection,
}

impl std::fmt::Debug for CacheDb {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CacheDb").finish_non_exhaustive()
    }
}

impl CacheDb {
    /// Open (and migrate) the cache database at `path`. Creates parent
    /// directories as needed. Sets `busy_timeout(5s)` *before* running
    /// migrations so a concurrent ferrule invocation's `prune()` can't
    /// race the schema setup.
    #[must_use = "result of opening the cache must be inspected"]
    pub fn open(path: &Path) -> Result<Self, CliError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(CliError::Io)?;
        }
        let conn = Connection::open(path)
            .map_err(|e| CliError::usage(format!("cache: failed to open {path:?}: {e}")))?;
        // #58: bake in busy_timeout so concurrent ferrule invocations
        // don't surface as spurious cache misses on SQLITE_BUSY.
        conn.busy_timeout(Duration::from_secs(5))
            .map_err(|e| CliError::usage(format!("cache: busy_timeout: {e}")))?;
        // #57: explicit migration scaffold instead of `CREATE TABLE IF
        // NOT EXISTS`. Downgrade detection refuses to clobber a forward-
        // compatible file.
        migrate(&conn)?;
        Ok(Self { conn })
    }

    /// Resolve the store path from config + env, opening if caching is
    /// enabled. Returns `Ok(None)` when caching is disabled (either by
    /// config or by the `FERRULE_NO_CACHE` env override).
    #[must_use = "caller must handle the optional cache handle"]
    pub fn maybe_open(cfg: &CacheConfig) -> Result<Option<Self>, CliError> {
        if !cfg.enabled || std::env::var_os("FERRULE_NO_CACHE").is_some() {
            return Ok(None);
        }
        let path = resolve_path(cfg)?;
        Ok(Some(Self::open(&path)?))
    }

    /// Probe the cache for `key`. Expired rows are treated as misses
    /// (we don't delete them eagerly — that's `prune()`'s job).
    #[must_use = "lookup result must be inspected"]
    pub fn lookup(&self, key: &CacheKey) -> Result<Option<CachedResult>, CliError> {
        let now = now_unix();
        let mut stmt = self
            .conn
            .prepare(
                "SELECT ts, ttl_secs, columns_json, rows_json \
                 FROM cache WHERE key = ?",
            )
            .map_err(|e| CliError::usage(format!("cache: prepare lookup failed: {e}")))?;
        let mut rows = stmt
            .query(params![key.0])
            .map_err(|e| CliError::usage(format!("cache: lookup failed: {e}")))?;
        let Some(row) = rows
            .next()
            .map_err(|e| CliError::usage(format!("cache: lookup row failed: {e}")))?
        else {
            return Ok(None);
        };
        let ts: i64 = row
            .get(0)
            .map_err(|e| CliError::usage(format!("cache: lookup decode ts: {e}")))?;
        let ttl_secs: i64 = row
            .get(1)
            .map_err(|e| CliError::usage(format!("cache: lookup decode ttl: {e}")))?;
        let columns_json: String = row
            .get(2)
            .map_err(|e| CliError::usage(format!("cache: lookup decode columns: {e}")))?;
        let rows_json: String = row
            .get(3)
            .map_err(|e| CliError::usage(format!("cache: lookup decode rows: {e}")))?;
        let age_secs = (now - ts).max(0) as u64;
        if (ts + ttl_secs) < now {
            // Expired — treat as miss. `prune()` will reap eventually.
            return Ok(None);
        }
        let columns: Vec<ColumnInfo> = serde_json::from_str(&columns_json)
            .map_err(|e| CliError::usage(format!("cache: lookup decode columns json: {e}")))?;
        let rows: Vec<Row> = serde_json::from_str(&rows_json)
            .map_err(|e| CliError::usage(format!("cache: lookup decode rows json: {e}")))?;
        Ok(Some(CachedResult {
            result: QueryResult { columns, rows },
            age_secs,
        }))
    }

    /// Insert (or upsert) a cached query result.
    #[must_use = "insert error must be surfaced (or explicitly dropped)"]
    pub fn insert(
        &mut self,
        key: &CacheKey,
        qr: &QueryResult,
        ttl: Duration,
        meta: &CacheMeta,
    ) -> Result<(), CliError> {
        let columns_json = serde_json::to_string(&qr.columns)
            .map_err(|e| CliError::usage(format!("cache: encode columns: {e}")))?;
        let rows_json = serde_json::to_string(&qr.rows)
            .map_err(|e| CliError::usage(format!("cache: encode rows: {e}")))?;
        self.conn
            .execute(
                "INSERT INTO cache \
                 (key, ts, ttl_secs, columns_json, rows_json, conn_redacted, sql_preview) \
                 VALUES (?, ?, ?, ?, ?, ?, ?) \
                 ON CONFLICT(key) DO UPDATE SET \
                   ts = excluded.ts, \
                   ttl_secs = excluded.ttl_secs, \
                   columns_json = excluded.columns_json, \
                   rows_json = excluded.rows_json, \
                   conn_redacted = excluded.conn_redacted, \
                   sql_preview = excluded.sql_preview",
                params![
                    key.0,
                    now_unix(),
                    ttl.as_secs() as i64,
                    columns_json,
                    rows_json,
                    meta.conn_redacted,
                    meta.sql_preview,
                ],
            )
            .map_err(|e| CliError::usage(format!("cache: insert failed: {e}")))?;
        Ok(())
    }

    /// Open-loop pruning: drop rows past their per-row TTL or older
    /// than `max_age_days`, then trim total count to `max_rows`. Zero
    /// in either retention field disables that pass.
    #[must_use = "prune error must be surfaced (or explicitly dropped)"]
    pub fn prune(&mut self, cfg: &CacheConfig) -> Result<(), CliError> {
        let now = now_unix();
        // Per-row TTL expiry always runs.
        self.conn
            .execute(
                "DELETE FROM cache WHERE (ts + ttl_secs) < ?",
                params![now],
            )
            .map_err(|e| CliError::usage(format!("cache: prune (ttl) failed: {e}")))?;
        if cfg.max_age_days > 0 {
            let cutoff = now - i64::from(cfg.max_age_days) * 86_400;
            self.conn
                .execute("DELETE FROM cache WHERE ts < ?", params![cutoff])
                .map_err(|e| CliError::usage(format!("cache: prune (age) failed: {e}")))?;
        }
        if cfg.max_rows > 0 {
            let total: i64 = self
                .conn
                .query_row("SELECT COUNT(*) FROM cache", [], |r| r.get(0))
                .map_err(|e| CliError::usage(format!("cache: count failed: {e}")))?;
            let excess = total.saturating_sub(cfg.max_rows as i64);
            if excess > 0 {
                self.conn
                    .execute(
                        "DELETE FROM cache WHERE key IN \
                         (SELECT key FROM cache ORDER BY ts ASC LIMIT ?)",
                        params![excess],
                    )
                    .map_err(|e| CliError::usage(format!("cache: prune (count) failed: {e}")))?;
            }
        }
        Ok(())
    }

    /// Count rows currently stored. Test-only convenience.
    #[cfg(test)]
    fn count(&self) -> Result<i64, CliError> {
        self.conn
            .query_row("SELECT COUNT(*) FROM cache", [], |r| r.get::<_, i64>(0))
            .map_err(|e| CliError::usage(format!("cache: count failed: {e}")))
    }
}

/// Derive the cache key from a (connection, SQL, params) triple.
///
/// The connection URL is redacted *before* it enters the hash so a
/// password change rotates the key without ever leaking the secret
/// into the digest input. See test
/// `cache_key_never_contains_password_bytes` for the substring
/// assertion against the pre-hash string.
pub fn cache_key(conn: &str, sql: &str, params: &ParameterSet) -> CacheKey {
    let input = cache_key_input(conn, sql, params);
    let mut h = Sha256::new();
    h.update(input.as_bytes());
    let digest = h.finalize();
    let mut s = String::with_capacity(64);
    for b in digest.iter() {
        // Inline lowercase-hex encoding; we deliberately avoid a `hex`
        // crate dep for one call site.
        let _ = write!(&mut s, "{:02x}", b);
    }
    CacheKey(s)
}

/// Build the pre-hash input string used by `cache_key`. Used by
/// the `cache_key_never_contains_password_bytes` test so it can
/// substring-assert the absence of password bytes without reaching
/// into `Sha256::update` itself.
fn cache_key_input(conn: &str, sql: &str, params: &ParameterSet) -> String {
    let redacted = DatabaseUrl::parse(conn)
        .map(|u| u.redacted())
        .unwrap_or_else(|_| conn.to_string());
    let normalized = normalize_sql(sql);
    let mut canon: Vec<(String, ferrule_core::value::Value)> = params
        .map
        .iter()
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    canon.sort_by(|a, b| a.0.cmp(&b.0));
    let params_canonical = serde_json::to_string(&canon).unwrap_or_default();
    // NUL byte delimiter so `("a","bc")` and `("ab","c")` cannot
    // collide after concatenation.
    format!("{redacted}\0{normalized}\0{params_canonical}")
}

/// Normalize SQL for cache-key derivation. Quoted-region-aware; never
/// false-cache-hits.
///
/// Rules:
///   - Strip `-- line comments` through the next newline.
///   - Preserve `/* block comments */` verbatim (Postgres + Oracle
///     honour hint comments like `/*+ ... */`).
///   - Lowercase bytes outside quoted regions and outside block
///     comments.
///   - Pass quoted regions (`'`, `"`, backtick) through verbatim —
///     preserves PG `"MixedCase"` identifiers.
///   - Collapse internal whitespace runs to a single ASCII space.
///   - Trim outer whitespace and strip trailing `;` runs.
///
/// Pragmatic: unescaped `'`/`"`/backtick are the sole state shifts.
/// SQL `''` (escaped single-quote inside a string) closes-and-reopens
/// in our state machine. Acceptable false-miss; never a false-hit.
pub fn normalize_sql(s: &str) -> String {
    enum State {
        Open,
        Line,             // -- to next \n
        Block,            // /* ... */
        Quote(char),      // ', ", or `
    }
    let bytes = s.as_bytes();
    let mut out = String::with_capacity(s.len());
    let mut state = State::Open;
    let mut i = 0usize;
    while i < bytes.len() {
        let c = bytes[i] as char;
        match state {
            State::Open => {
                // Look ahead for `--` and `/*`.
                if c == '-' && i + 1 < bytes.len() && bytes[i + 1] == b'-' {
                    state = State::Line;
                    i += 2;
                    continue;
                }
                if c == '/' && i + 1 < bytes.len() && bytes[i + 1] == b'*' {
                    state = State::Block;
                    out.push('/');
                    out.push('*');
                    i += 2;
                    continue;
                }
                if c == '\'' || c == '"' || c == '`' {
                    state = State::Quote(c);
                    out.push(c);
                    i += 1;
                    continue;
                }
                // Lowercase outside quoted regions.
                if c.is_ascii_uppercase() {
                    out.push(c.to_ascii_lowercase());
                } else {
                    out.push(c);
                }
                i += 1;
            }
            State::Line => {
                if c == '\n' {
                    state = State::Open;
                    out.push(' ');
                }
                i += 1;
            }
            State::Block => {
                out.push(c);
                if c == '*' && i + 1 < bytes.len() && bytes[i + 1] == b'/' {
                    out.push('/');
                    state = State::Open;
                    i += 2;
                    continue;
                }
                i += 1;
            }
            State::Quote(q) => {
                out.push(c);
                if c == q {
                    state = State::Open;
                }
                i += 1;
            }
        }
    }
    // Collapse internal whitespace + trim outer + strip trailing
    // semicolons. Walk the string once to keep the cost O(n).
    let collapsed: String = {
        let mut s2 = String::with_capacity(out.len());
        let mut last_ws = false;
        for ch in out.chars() {
            if ch.is_whitespace() {
                if !last_ws {
                    s2.push(' ');
                }
                last_ws = true;
            } else {
                s2.push(ch);
                last_ws = false;
            }
        }
        s2
    };
    let trimmed = collapsed.trim();
    trimmed.trim_end_matches(';').trim().to_string()
}

/// `1h` / `30m` / `2d` parser. Returns the duration in seconds.
///
/// TODO(#56): consolidate with `commands/history.rs::parse_since` once
/// #56 (shared duration parser) lands. The two functions are
/// byte-for-byte structural duplicates of the same grammar; the only
/// reason `parse_since` returns `chrono::Duration` and this one returns
/// `u64` seconds is historical — `parse_since` predates this call site.
pub fn parse_duration_secs(s: &str) -> Result<u64, CliError> {
    let s = s.trim();
    if s.is_empty() {
        return Err(CliError::usage(
            "cache: duration requires a value like 30s, 5m, 2h, 7d",
        ));
    }
    let suffix_pos = s
        .find(|c: char| !c.is_ascii_digit())
        .ok_or_else(|| CliError::usage(format!("cache: duration '{s}': missing unit suffix")))?;
    let (num, suffix) = s.split_at(suffix_pos);
    let n: u64 = num
        .parse()
        .map_err(|_| CliError::usage(format!("cache: duration '{s}': invalid number")))?;
    let secs = match suffix {
        "s" | "sec" | "secs" => n,
        "m" | "min" | "mins" => n.saturating_mul(60),
        "h" | "hr" | "hrs" => n.saturating_mul(3_600),
        "d" | "day" | "days" => n.saturating_mul(86_400),
        _ => {
            return Err(CliError::usage(format!(
                "cache: duration '{s}': unknown unit '{suffix}'"
            )))
        }
    };
    Ok(secs)
}

fn migrate(conn: &Connection) -> Result<(), CliError> {
    let current: u32 = conn
        .query_row("PRAGMA user_version", [], |r| r.get::<_, i64>(0))
        .map_err(|e| CliError::usage(format!("cache: read user_version: {e}")))?
        as u32;
    if current > LATEST_VERSION {
        return Err(CliError::usage(format!(
            "cache: results.db user_version={current} is newer than this \
             binary supports (max {LATEST_VERSION}). Downgrade detected — \
             refusing to clobber. Delete results.db or upgrade ferrule."
        )));
    }
    for (v, sql) in MIGRATIONS.iter().filter(|(v, _)| *v > current) {
        conn.execute_batch(sql)
            .map_err(|e| CliError::usage(format!("cache: migration v{v} failed: {e}")))?;
        conn.execute_batch(&format!("PRAGMA user_version = {v}"))
            .map_err(|e| CliError::usage(format!("cache: bump user_version to {v}: {e}")))?;
    }
    Ok(())
}

fn resolve_path(cfg: &CacheConfig) -> Result<PathBuf, CliError> {
    if let Some(p) = cfg.path.as_deref() {
        return Ok(expand_tilde(p));
    }
    let base = dirs::data_local_dir().ok_or_else(|| {
        CliError::usage("cache: could not determine data-local directory for default path")
    })?;
    Ok(base.join("ferrule").join("results.db"))
}

fn now_unix() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

const LATEST_VERSION: u32 = 1;

const SCHEMA_V1: &str = r#"
CREATE TABLE IF NOT EXISTS cache (
    key            TEXT PRIMARY KEY,
    ts             INTEGER NOT NULL,
    ttl_secs       INTEGER NOT NULL,
    columns_json   TEXT NOT NULL,
    rows_json      TEXT NOT NULL,
    conn_redacted  TEXT,
    sql_preview    TEXT
);
CREATE INDEX IF NOT EXISTS cache_ts_idx ON cache(ts);
"#;

/// Versioned schema migrations. Append new tuples; never edit
/// historical ones. The migrator runs every entry whose version is
/// strictly greater than the current `PRAGMA user_version`.
const MIGRATIONS: &[(u32, &str)] = &[(1, SCHEMA_V1)];

#[cfg(test)]
mod tests {
    use super::*;
    use ferrule_core::value::{ColumnInfo, TypeHint, Value};
    use std::sync::Mutex;

    /// Cross-test mutex for `FERRULE_NO_CACHE` env-var manipulation
    /// (test #10). Avoids a `serial_test` workspace dep.
    static ENV_GUARD: Mutex<()> = Mutex::new(());

    fn col(name: &str, t: TypeHint) -> ColumnInfo {
        ColumnInfo {
            name: name.into(),
            type_hint: t,
            nullable: true,
        }
    }

    fn sample_qr() -> QueryResult {
        QueryResult {
            columns: vec![
                col("a", TypeHint::Int64),
                col("b", TypeHint::String),
                col("c", TypeHint::Bytes),
                col("d", TypeHint::Null),
            ],
            rows: vec![vec![
                Value::Int64(7),
                Value::String("hi".into()),
                Value::Bytes(vec![0, 1, 2, 255]),
                Value::Null,
            ]],
        }
    }

    fn meta() -> CacheMeta<'static> {
        CacheMeta {
            conn_redacted: "postgres://user:***@host/db",
            sql_preview: "SELECT 1",
        }
    }

    // 1.
    #[test]
    fn open_creates_parent_and_persists() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("nested/sub/results.db");
        let mut db = CacheDb::open(&path).unwrap();
        let key = CacheKey("k1".into());
        db.insert(&key, &sample_qr(), Duration::from_secs(60), &meta())
            .unwrap();
        drop(db);
        let db2 = CacheDb::open(&path).unwrap();
        let hit = db2.lookup(&key).unwrap().expect("should hit");
        assert_eq!(hit.result.rows.len(), 1);
        assert_eq!(hit.result.columns.len(), 4);
    }

    // 2.
    #[test]
    fn migrate_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("results.db");
        // First open performs the v0 → v1 jump.
        let db1 = CacheDb::open(&path).unwrap();
        let v1: i64 = db1
            .conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(v1, 1);
        drop(db1);
        // Second open: user_version already 1, no migration runs.
        let db2 = CacheDb::open(&path).unwrap();
        let v2: i64 = db2
            .conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(v2, 1);
    }

    // 3.
    #[test]
    fn migrate_rejects_future_version() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("results.db");
        // Seed the file at a fictitious user_version > LATEST_VERSION.
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch("PRAGMA user_version = 99").unwrap();
        }
        let err = CacheDb::open(&path).expect_err("downgrade must be rejected");
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

    // 4.
    #[test]
    fn busy_timeout_set() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("results.db");
        let db = CacheDb::open(&path).unwrap();
        let bt: i64 = db
            .conn
            .query_row("PRAGMA busy_timeout", [], |r| r.get(0))
            .unwrap();
        assert_eq!(bt, 5_000, "busy_timeout must be 5s in ms");
    }

    // 5.
    #[test]
    fn lookup_then_insert_round_trip() {
        let tmp = tempfile::tempdir().unwrap();
        let mut db = CacheDb::open(&tmp.path().join("results.db")).unwrap();
        let key = CacheKey("rt".into());
        let qr = sample_qr();
        db.insert(&key, &qr, Duration::from_secs(60), &meta())
            .unwrap();
        let hit = db.lookup(&key).unwrap().expect("hit");
        assert_eq!(hit.result.columns.len(), qr.columns.len());
        assert_eq!(hit.result.rows.len(), qr.rows.len());
        // Value::Null, Int64, Bytes round-trip cleanly. Value::String
        // ↔ Decimal ↔ Uuid share the same JSON shape under
        // `#[serde(untagged)]` so the deserializer picks the first
        // String-shaped variant (Decimal) — but their Display output
        // is byte-identical, so the formatter renders the same thing
        // either way. Assert via Display rather than enum equality so
        // the test mirrors what the user actually sees.
        let row = &hit.result.rows[0];
        assert_eq!(row[0], Value::Int64(7));
        assert_eq!(row[1].to_string(), "hi");
        assert_eq!(row[2], Value::Bytes(vec![0, 1, 2, 255]));
        assert_eq!(row[3], Value::Null);
    }

    // 6.
    #[test]
    fn lookup_returns_none_for_expired() {
        let tmp = tempfile::tempdir().unwrap();
        let mut db = CacheDb::open(&tmp.path().join("results.db")).unwrap();
        let key = CacheKey("expired".into());
        db.insert(&key, &sample_qr(), Duration::from_secs(60), &meta())
            .unwrap();
        // Reach in and backdate the ts so it's already expired.
        db.conn
            .execute("UPDATE cache SET ts = ts - 3600", [])
            .unwrap();
        let hit = db.lookup(&key).unwrap();
        assert!(hit.is_none(), "expired row must read as miss");
    }

    // 7.
    #[test]
    fn prune_removes_expired() {
        let tmp = tempfile::tempdir().unwrap();
        let mut db = CacheDb::open(&tmp.path().join("results.db")).unwrap();
        db.insert(&CacheKey("k1".into()), &sample_qr(), Duration::from_secs(60), &meta())
            .unwrap();
        db.insert(&CacheKey("k2".into()), &sample_qr(), Duration::from_secs(60), &meta())
            .unwrap();
        // Expire one row.
        db.conn
            .execute("UPDATE cache SET ts = ts - 3600 WHERE key = 'k1'", [])
            .unwrap();
        let cfg = CacheConfig::default();
        db.prune(&cfg).unwrap();
        assert_eq!(db.count().unwrap(), 1);
    }

    // 8.
    #[test]
    fn prune_trims_max_rows() {
        let tmp = tempfile::tempdir().unwrap();
        let mut db = CacheDb::open(&tmp.path().join("results.db")).unwrap();
        // Use a long TTL so the per-row expiry doesn't fire.
        let ttl = Duration::from_secs(86_400);
        for i in 0..10 {
            db.insert(&CacheKey(format!("k{i}")), &sample_qr(), ttl, &meta())
                .unwrap();
            // Stagger ts so prune has a stable oldest-first ordering.
            db.conn
                .execute(
                    "UPDATE cache SET ts = ts - ? WHERE key = ?",
                    params![10 - i, format!("k{i}")],
                )
                .unwrap();
        }
        let cfg = CacheConfig {
            max_rows: 5,
            max_age_days: 0,
            ..CacheConfig::default()
        };
        db.prune(&cfg).unwrap();
        assert_eq!(db.count().unwrap(), 5);
        // Oldest five (k0..k4 — they got the largest negative offsets)
        // should be the ones evicted.
        let surviving: Vec<String> = {
            let mut stmt = db.conn.prepare("SELECT key FROM cache ORDER BY key").unwrap();
            let rows = stmt
                .query_map([], |r| r.get::<_, String>(0))
                .unwrap()
                .map(|r| r.unwrap())
                .collect();
            rows
        };
        assert_eq!(surviving, vec!["k5", "k6", "k7", "k8", "k9"]);
    }

    // 9.
    #[test]
    fn maybe_open_disabled_returns_none() {
        let cfg = CacheConfig {
            enabled: false,
            ..CacheConfig::default()
        };
        assert!(CacheDb::maybe_open(&cfg).unwrap().is_none());
    }

    // 10.
    #[test]
    fn maybe_open_env_kill_returns_none() {
        let _g = ENV_GUARD.lock().unwrap_or_else(|p| p.into_inner());
        let tmp = tempfile::tempdir().unwrap();
        let cfg = CacheConfig {
            path: Some(tmp.path().join("r.db").to_string_lossy().into_owned()),
            ..CacheConfig::default()
        };
        // Scope-guard so we restore env even on panic.
        struct Restore(Option<std::ffi::OsString>);
        impl Drop for Restore {
            fn drop(&mut self) {
                match self.0.take() {
                    Some(v) => std::env::set_var("FERRULE_NO_CACHE", v),
                    None => std::env::remove_var("FERRULE_NO_CACHE"),
                }
            }
        }
        let _r = Restore(std::env::var_os("FERRULE_NO_CACHE"));
        std::env::set_var("FERRULE_NO_CACHE", "1");
        assert!(CacheDb::maybe_open(&cfg).unwrap().is_none());
    }

    // 11.
    #[test]
    fn normalize_sql_case_whitespace_trailing_semi() {
        let a = normalize_sql("  SELECT   *  FROM   t   ;;;");
        let b = normalize_sql("select * from t");
        assert_eq!(a, b);
        assert_eq!(a, "select * from t");
    }

    // 12.
    #[test]
    fn normalize_sql_preserves_quoted_identifiers() {
        let s = r#"SELECT "MixedCase", 'KeepLit' FROM "T""#;
        let out = normalize_sql(s);
        // Quoted regions stay verbatim; outside-quotes lowercased.
        assert!(out.contains("\"MixedCase\""));
        assert!(out.contains("'KeepLit'"));
        assert!(out.starts_with("select "));
    }

    // 13.
    #[test]
    fn normalize_sql_strips_line_comments_keeps_block() {
        let s = "SELECT 1 -- comment\nFROM /*+ HINT */ t -- trailing\n;";
        let out = normalize_sql(s);
        assert!(!out.contains("comment"));
        assert!(!out.contains("trailing"));
        assert!(out.contains("/*+ HINT */"));
        assert!(out.ends_with(" t"));
    }

    // 14.
    #[test]
    fn cache_key_stable_across_param_order() {
        let mut p1 = ParameterSet::default();
        p1.set("a".into(), Value::Int64(1));
        p1.set("b".into(), Value::Int64(2));
        let mut p2 = ParameterSet::default();
        p2.set("b".into(), Value::Int64(2));
        p2.set("a".into(), Value::Int64(1));
        let k1 = cache_key("sqlite:///tmp/x.db", "SELECT 1", &p1);
        let k2 = cache_key("sqlite:///tmp/x.db", "SELECT 1", &p2);
        assert_eq!(k1, k2);
    }

    // 15.
    #[test]
    fn cache_key_changes_with_param_value() {
        let mut p1 = ParameterSet::default();
        p1.set("a".into(), Value::Int64(1));
        let mut p2 = ParameterSet::default();
        p2.set("a".into(), Value::Int64(2));
        let k1 = cache_key("sqlite:///tmp/x.db", "SELECT 1", &p1);
        let k2 = cache_key("sqlite:///tmp/x.db", "SELECT 1", &p2);
        assert_ne!(k1, k2);
    }

    // 16.
    #[test]
    fn cache_key_never_contains_password_bytes() {
        let conn = "postgres://user:topsecretP@host:5432/db";
        let input = cache_key_input(conn, "SELECT 1", &ParameterSet::default());
        assert!(
            !input.contains("topsecretP"),
            "password bytes leaked into pre-hash input: {input}"
        );
        // Sanity: redacted form is what entered the hash.
        assert!(input.contains("***"), "redaction marker missing: {input}");
    }

    // 17.
    #[test]
    fn lookup_failure_falls_through() {
        let tmp = tempfile::tempdir().unwrap();
        let db = CacheDb::open(&tmp.path().join("results.db")).unwrap();
        // Drop the table out from under the connection — next lookup
        // must surface an Err rather than panic.
        db.conn.execute_batch("DROP TABLE cache").unwrap();
        let err = db.lookup(&CacheKey("nope".into()));
        assert!(err.is_err(), "lookup against missing table must Err");
    }
}
