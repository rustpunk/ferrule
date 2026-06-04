//! `ferrule history` — read the persistent query history (R4 / #4).
//!
//! The store is written by the dispatch hook in `main.rs::record_dispatch`.
//! This command is a thin reader over `HistoryDb::query()` that funnels the
//! results through the existing `format_result` machinery so the same
//! `--format table|json|csv|yaml|raw` selection works as on every other
//! read command.

use chrono::Duration;
use clap::{Args, Subcommand};
use ferrule_config::profile::GlobalConfig;
use ferrule_config::HistoryConfig;
use ferrule_core::formatter::format_result;
use ferrule_sql::connection::QueryResult;
use ferrule_sql::value::{ColumnInfo, TypeHint, Value};

use super::OutputFlags;
use crate::error::CliError;
use crate::history::{HistoryDb, HistoryFilter, RunRecord};

#[derive(Args, Clone, Debug)]
pub struct HistoryArgs {
    /// Return only the N most recent rows.
    #[arg(long, value_name = "N")]
    pub last: Option<usize>,

    /// Filter by connection-name glob (case-insensitive, shell-style
    /// `*` / `?`). Matched against the stored redacted URL.
    #[arg(long, value_name = "GLOB")]
    pub conn: Option<String>,

    /// Sort by duration descending instead of timestamp descending.
    #[arg(long)]
    pub slowest: bool,

    /// Substring match against the SQL body (case-insensitive).
    #[arg(long, value_name = "PATTERN")]
    pub grep: Option<String>,

    /// Time window like `1h`, `2d`, `30m`. Returns rows newer than now
    /// minus this duration.
    #[arg(long, value_name = "DURATION")]
    pub since: Option<String>,

    /// Slow-log filter: minimum query duration in milliseconds. Wins
    /// over `--slow` when both are set.
    #[arg(long, value_name = "MS")]
    pub min_duration_ms: Option<u64>,

    /// Restrict to runs that crossed the configured slow-log threshold
    /// (`[slow_log] threshold`). Disabled when neither this flag nor
    /// `--min-duration-ms` is set.
    #[arg(long)]
    pub slow: bool,

    #[command(flatten)]
    pub output: OutputFlags,

    /// Optional management subcommand. When omitted, `ferrule history`
    /// reads the store (the default behaviour). The only subcommand today
    /// is `prune`, which applies the retention policy on demand.
    #[command(subcommand)]
    pub command: Option<HistoryCommand>,
}

/// Management subcommands under `ferrule history`. The reader path (no
/// subcommand) is the common case; these mutate the store.
#[derive(Subcommand, Clone, Debug)]
pub enum HistoryCommand {
    /// Apply the retention policy now (delete rows past the age / row
    /// caps) instead of waiting for the opportunistic prune on the next
    /// recorded run.
    Prune {
        /// Report how many rows would be deleted without deleting them.
        #[arg(long)]
        dry_run: bool,
        /// Override `[history] max_age_days` for this prune only.
        #[arg(long, value_name = "DAYS")]
        max_age_days: Option<u32>,
        /// Override `[history] max_rows` for this prune only.
        #[arg(long, value_name = "N")]
        max_rows: Option<u64>,
    },
}

/// Arguments for `ferrule slow` — a thin alias for `ferrule history
/// --slow`. Drops the `--slow` flag (implied) and the
/// `--min-duration-ms` override; otherwise identical to
/// [`HistoryArgs`].
#[derive(Args, Clone, Debug)]
pub struct SlowArgs {
    #[arg(long, value_name = "N")]
    pub last: Option<usize>,

    #[arg(long, value_name = "GLOB")]
    pub conn: Option<String>,

    #[arg(long, value_name = "PATTERN")]
    pub grep: Option<String>,

    #[arg(long, value_name = "DURATION")]
    pub since: Option<String>,

    /// Override the configured slow-log threshold (milliseconds).
    #[arg(long, value_name = "MS")]
    pub min_duration_ms: Option<u64>,

    #[command(flatten)]
    pub output: OutputFlags,
}

impl SlowArgs {
    fn into_history(self) -> HistoryArgs {
        HistoryArgs {
            last: self.last,
            conn: self.conn,
            slowest: true,
            grep: self.grep,
            since: self.since,
            min_duration_ms: self.min_duration_ms,
            slow: self.min_duration_ms.is_none(),
            output: self.output,
            // `ferrule slow` is read-only; it never carries a prune
            // subcommand.
            command: None,
        }
    }
}

pub fn run_slow(args: SlowArgs, global_config: &GlobalConfig) -> Result<(), CliError> {
    run(args.into_history(), global_config)
}

pub fn run(args: HistoryArgs, global_config: &GlobalConfig) -> Result<(), CliError> {
    // Management subcommands branch off before the reader path.
    if let Some(command) = args.command {
        return run_command(command, global_config);
    }

    let format = args.output.resolve_format(global_config);
    let limit = args.output.resolve_limit(global_config);
    let offset = args.output.offset;

    let db = HistoryDb::maybe_open(&global_config.history)?.ok_or_else(|| {
        CliError::usage(
            "history is disabled. Enable [history] enabled = true in your config, \
             or unset FERRULE_NO_HISTORY for this invocation.",
        )
    })?;

    let since = match args.since.as_deref() {
        Some(s) => Some(parse_since(s)?),
        None => None,
    };

    let min_duration_ms = match (args.min_duration_ms, args.slow) {
        (Some(n), _) => Some(n),
        (None, true) => Some(
            global_config
                .slow_log
                .threshold_ms()
                .map_err(|e| CliError::usage(format!("slow_log threshold: {e}")))?,
        ),
        (None, false) => None,
    };

    let filter = HistoryFilter {
        last: args.last,
        conn: args.conn.clone(),
        since,
        grep: args.grep.clone(),
        slowest: args.slowest,
        min_duration_ms,
    };

    let rows = db.query(&filter)?;
    let mut result = render(rows);

    if let Some(off) = offset {
        if off >= result.rows.len() {
            result.rows.clear();
        } else {
            result.rows = result.rows.split_off(off);
        }
    }
    if let Some(n) = limit {
        if result.rows.len() > n {
            result.rows.truncate(n);
        }
    }

    let rendered = format_result(&result, format).map_err(CliError::query)?;
    println!("{}", rendered);
    Ok(())
}

/// Dispatch a `ferrule history <subcommand>`. Today only `prune`.
fn run_command(command: HistoryCommand, global_config: &GlobalConfig) -> Result<(), CliError> {
    match command {
        HistoryCommand::Prune {
            dry_run,
            max_age_days,
            max_rows,
        } => run_prune(dry_run, max_age_days, max_rows, global_config),
    }
}

/// `ferrule history prune [--dry-run] [--max-age-days N] [--max-rows N]`.
///
/// Opens the store mutably and applies the retention policy on demand,
/// using the global `[history]` caps unless overridden per-flag. With
/// `--dry-run`, reports the would-be deletion count without deleting.
fn run_prune(
    dry_run: bool,
    max_age_days: Option<u32>,
    max_rows: Option<u64>,
    global_config: &GlobalConfig,
) -> Result<(), CliError> {
    let mut db = HistoryDb::maybe_open(&global_config.history)?.ok_or_else(|| {
        CliError::usage(
            "history is disabled. Enable [history] enabled = true in your config, \
             or unset FERRULE_NO_HISTORY for this invocation.",
        )
    })?;

    // Start from the configured caps, then apply per-flag overrides.
    let cfg = effective_prune_config(&global_config.history, max_age_days, max_rows);

    if dry_run {
        let n = db.prune_dry_run(&cfg)?;
        println!("Would delete {n} row(s) (dry run; nothing was removed).");
    } else {
        let n = db.prune(&cfg)?;
        println!("Deleted {n} row(s).");
    }
    Ok(())
}

/// Clone the global history config and overlay any per-flag retention
/// overrides. A `Some` flag wins over the configured value; `None`
/// leaves the config value in place.
fn effective_prune_config(
    base: &HistoryConfig,
    max_age_days: Option<u32>,
    max_rows: Option<u64>,
) -> HistoryConfig {
    let mut cfg = base.clone();
    if let Some(days) = max_age_days {
        cfg.max_age_days = days;
    }
    if let Some(rows) = max_rows {
        cfg.max_rows = rows;
    }
    cfg
}

fn render(rows: Vec<RunRecord>) -> QueryResult {
    let columns = vec![
        col("ts", TypeHint::DateTimeTz),
        col("conn", TypeHint::String),
        col("command", TypeHint::String),
        col("duration_ms", TypeHint::Int64),
        col("rows", TypeHint::Int64),
        col("exit_code", TypeHint::Int64),
        col("sql", TypeHint::String),
        col("error", TypeHint::String),
    ];
    let body = rows
        .into_iter()
        .map(|r| {
            vec![
                Value::DateTimeTz(r.ts),
                r.conn.map(Value::String).unwrap_or(Value::Null),
                Value::String(r.command),
                Value::Int64(r.duration_ms as i64),
                r.rows.map(Value::Int64).unwrap_or(Value::Null),
                Value::Int64(i64::from(r.exit_code)),
                r.sql
                    .map(|s| Value::String(oneline(&s)))
                    .unwrap_or(Value::Null),
                r.error.map(Value::String).unwrap_or(Value::Null),
            ]
        })
        .collect();
    QueryResult {
        columns,
        rows: body,
    }
}

fn col(name: &str, ty: TypeHint) -> ColumnInfo {
    ColumnInfo {
        name: name.into(),
        type_hint: ty,
        nullable: true,
    }
}

fn oneline(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Parse `ferrule history --since DURATION` (`30s`, `5m`, `2h`, `3d`, …).
///
/// Delegates to the shared [`ferrule_config::parse::parse_duration`] so the
/// recognised units stay in lock-step with the `[slow_log] threshold`
/// parser. Like that parser, a bare integer (no unit) is rejected.
fn parse_since(s: &str) -> Result<Duration, CliError> {
    ferrule_config::parse::parse_duration(s).map_err(|e| CliError::usage(format!("--since: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_since_accepts_basic_units() {
        assert_eq!(parse_since("30s").unwrap(), Duration::seconds(30));
        assert_eq!(parse_since("5m").unwrap(), Duration::minutes(5));
        assert_eq!(parse_since("2h").unwrap(), Duration::hours(2));
        assert_eq!(parse_since("3d").unwrap(), Duration::days(3));
        assert_eq!(parse_since("90mins").unwrap(), Duration::minutes(90));
    }

    #[test]
    fn parse_since_rejects_bad_inputs() {
        assert!(parse_since("").is_err());
        assert!(parse_since("hour").is_err());
        assert!(parse_since("10").is_err());
        assert!(parse_since("10x").is_err());
    }

    #[test]
    fn oneline_collapses_whitespace() {
        assert_eq!(oneline("SELECT\n  *\nFROM   x"), "SELECT * FROM x");
    }

    // #48: per-flag overrides win over the configured caps; absent flags
    // leave the config value in place.
    #[test]
    fn effective_prune_config_applies_overrides() {
        let base = HistoryConfig {
            max_age_days: 30,
            max_rows: 0,
            ..Default::default()
        };
        // No overrides -> config unchanged.
        let same = effective_prune_config(&base, None, None);
        assert_eq!(same.max_age_days, 30);
        assert_eq!(same.max_rows, 0);
        // max_rows override fires even when config has it at 0.
        let overridden = effective_prune_config(&base, None, Some(5));
        assert_eq!(overridden.max_age_days, 30, "untouched flag keeps config");
        assert_eq!(overridden.max_rows, 5, "override wins");
        // Both overrides.
        let both = effective_prune_config(&base, Some(7), Some(100));
        assert_eq!(both.max_age_days, 7);
        assert_eq!(both.max_rows, 100);
    }
}
