//! `ferrule history` — read the persistent query history (R4 / #4).
//!
//! The store is written by the dispatch hook in `main.rs::record_dispatch`.
//! This command is a thin reader over `HistoryDb::query()` that funnels the
//! results through the existing `format_result` machinery so the same
//! `--format table|json|csv|yaml|raw` selection works as on every other
//! read command.

use chrono::Duration;
use clap::Args;
use ferrule_config::profile::GlobalConfig;
use ferrule_core::connection::QueryResult;
use ferrule_core::formatter::format_result;
use ferrule_core::value::{ColumnInfo, TypeHint, Value};

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

    /// Slow-log filter: minimum query duration in milliseconds. Phase 2
    /// wires `--slow` into this from the `[slow_log] threshold` config.
    #[arg(long, value_name = "MS")]
    pub min_duration_ms: Option<u64>,

    #[command(flatten)]
    pub output: OutputFlags,
}

pub async fn run(args: HistoryArgs, global_config: &GlobalConfig) -> Result<(), CliError> {
    let format = args.output.resolve_format(global_config);
    let limit = args.output.resolve_limit(global_config);
    let offset = args.output.offset;

    let mut db = HistoryDb::maybe_open(&global_config.history)?.ok_or_else(|| {
        CliError::usage(
            "history is disabled. Enable [history] enabled = true in your config, \
             or unset FERRULE_NO_HISTORY for this invocation.",
        )
    })?;
    let _ = &mut db; // db is read-only here; future prune subcommand will mutate.

    let since = match args.since.as_deref() {
        Some(s) => Some(parse_since(s)?),
        None => None,
    };

    let filter = HistoryFilter {
        last: args.last,
        conn: args.conn.clone(),
        since,
        grep: args.grep.clone(),
        slowest: args.slowest,
        min_duration_ms: args.min_duration_ms,
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

/// Lightweight `1h` / `30m` / `2d` parser. We don't pull in `humantime`
/// for one call site — Phase 2 will add the dep when slow-log threshold
/// parsing needs it.
fn parse_since(s: &str) -> Result<Duration, CliError> {
    let s = s.trim();
    if s.is_empty() {
        return Err(CliError::usage("--since requires a value like 1h, 30m, 2d"));
    }
    let (num, suffix) = s.split_at(
        s.find(|c: char| !c.is_ascii_digit())
            .ok_or_else(|| CliError::usage(format!("--since: missing unit suffix in '{s}'")))?,
    );
    let n: i64 = num
        .parse()
        .map_err(|_| CliError::usage(format!("--since: invalid number '{num}'")))?;
    let dur = match suffix {
        "s" | "sec" | "secs" => Duration::seconds(n),
        "m" | "min" | "mins" => Duration::minutes(n),
        "h" | "hr" | "hrs" => Duration::hours(n),
        "d" | "day" | "days" => Duration::days(n),
        _ => return Err(CliError::usage(format!("--since: unknown unit '{suffix}'"))),
    };
    Ok(dur)
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
}
