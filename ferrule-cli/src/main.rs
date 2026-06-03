use clap::{Parser, Subcommand};

mod bench;
mod cache;
mod commands;
mod daemon;
mod error;
mod history;
mod output;
mod path_util;
mod repl;
mod ssh_flags;
mod ssh_keys;
mod watch;

use commands::{
    BookmarkArgs, ConnArgs, CopyArgs, DescribeArgs, DiffArgs, DumpArgs, ExplainArgs, ExportArgs,
    HistoryArgs, LoadArgs, MigrateArgs, QueryArgs, ReplArgs, SlowArgs, TablesArgs, WatchArgs,
};
use error::CliError;
use history::{HistoryDb, RunRecord};

/// Ferrule — the collar that joins you to your data.
#[derive(Parser)]
#[command(name = "ferrule")]
#[command(version)]
#[command(about = "A Rust-native database query CLI")]
struct Cli {
    /// Path to config file
    #[arg(short, long, global = true)]
    config: Option<String>,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Manage saved connections
    #[command(alias = "conn")]
    Connection(ConnArgs),

    /// Manage query bookmarks
    Bookmark(BookmarkArgs),

    /// Explain a query execution plan
    Explain(ExplainArgs),

    /// Dump a table to CSV/JSON/SQL
    Dump(DumpArgs),

    /// Load data from CSV/JSON into a table
    Load(LoadArgs),

    /// Interactive REPL
    #[command(alias = "r")]
    Repl(ReplArgs),

    /// Execute a SQL query
    #[command(alias = "q")]
    Query(QueryArgs),

    /// List tables
    Tables(TablesArgs),

    /// Describe a table
    Describe(DescribeArgs),

    /// Diff schemas between two connections
    Diff(DiffArgs),

    /// Copy rows between two connections (cross-DB)
    // CopyArgs is the largest variant (per-side --src-*/--dst-* flags
    // push it past Query's footprint); box to keep the enum compact.
    Copy(Box<CopyArgs>),

    /// Export query results to CSV/JSON/SQL
    Export(ExportArgs),

    /// Schema migrations
    Migrate(MigrateArgs),

    /// Watch a query and re-execute periodically
    Watch(WatchArgs),

    /// Show recent ferrule invocations from the persistent history log
    History(HistoryArgs),

    /// Show only slow runs (alias for `ferrule history --slow`)
    Slow(SlowArgs),
}

fn run_daemon_mode() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    rt.block_on(daemon::run_daemon_server())?;
    Ok(())
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() > 1 && args[1] == "__daemon" {
        if let Err(e) = run_daemon_mode() {
            eprintln!("Daemon error: {e}");
            // Internal __daemon-mode failure is consumed by the parent
            // ferrule process, but pick a category-correct code so
            // a hand-invoked `ferrule __daemon` still classifies right.
            std::process::exit(error::exit::QUERY);
        }
        return;
    }

    miette::set_hook(Box::new(|_| {
        Box::new(
            miette::MietteHandlerOpts::new()
                .terminal_links(true)
                .build(),
        )
    }))
    .ok();

    // #64: ferrule-sql is now synchronous and owns a private
    // current-thread runtime inside each connection handle. The CLI
    // therefore drives dispatch on the calling thread directly — no
    // outer runtime — so those per-connection runtimes never nest
    // inside an ambient one ("cannot start a runtime from within a
    // runtime"). The two subsystems that still need async (the
    // connection-pool daemon and `--watch`) own a local runtime at
    // their own call sites and hop connection work onto a blocking
    // thread; an async embedder of ferrule-sql does the same via
    // `tokio::task::spawn_blocking`.
    let result: Result<(), CliError> = {
        let cli = Cli::parse();
        let global_config =
            ferrule_config::GlobalConfig::load(cli.config.as_deref()).unwrap_or_default();

        let snapshot = Snapshot::capture(&cli.command);
        let start = std::time::Instant::now();

        let outcome = match cli.command {
            Commands::Connection(args) => commands::conn::run(args, &global_config),
            Commands::Query(args) => commands::query::run(args, &global_config),
            Commands::Bookmark(args) => commands::bookmark::run(args, &global_config),
            Commands::Explain(args) => commands::explain::run(args, &global_config),
            Commands::Repl(args) => commands::repl::run(args, &global_config),
            Commands::Watch(args) => commands::watch::run(args, &global_config),
            Commands::Dump(args) => commands::dump::run(args, &global_config),
            Commands::Export(args) => commands::export::run(args, &global_config),
            Commands::Load(args) => commands::load::run(args, &global_config),
            Commands::Tables(args) => commands::tables::run(args, &global_config),
            Commands::Describe(args) => commands::describe::run(args, &global_config),
            Commands::Diff(args) => commands::diff::run(args, &global_config),
            Commands::Copy(args) => commands::copy::run(*args, &global_config),
            Commands::Migrate(args) => commands::migrate::run(args, &global_config),
            Commands::History(args) => commands::history::run(args, &global_config),
            Commands::Slow(args) => commands::history::run_slow(args, &global_config),
        };

        record_dispatch(&global_config, snapshot, start.elapsed(), &outcome);
        outcome
    };

    if let Err(err) = result {
        let code = err.exit_code();
        // ResultNotable is "command succeeded with a gate-worthy
        // result" — not an error. Print a plain stderr line instead of
        // routing through the miette error renderer.
        if let CliError::ResultNotable(msg) = &err {
            eprintln!("ferrule: {msg}");
        } else {
            let report = miette::Report::new(err);
            eprintln!("{:?}", report);
        }
        std::process::exit(code);
    }
}

/// Pre-dispatch snapshot. Captured from the parsed `Commands` value so
/// the dispatch hook can build a `RunRecord` without re-matching after
/// the per-command `args` is moved into the run function.
struct Snapshot {
    command: &'static str,
    conn: Option<String>,
    sql: Option<String>,
    /// Skip recording entirely — used for `ferrule history` itself to
    /// avoid logging the act of reading the history log.
    skip: bool,
}

impl Snapshot {
    fn capture(cmd: &Commands) -> Self {
        let (name, conn, sql, skip) = match cmd {
            Commands::Query(a) => ("query", Some(redact(&a.connection)), a.sql.clone(), false),
            Commands::Watch(a) => (
                "watch",
                Some(redact(&a.connection)),
                Some(a.sql.clone()),
                false,
            ),
            Commands::Tables(a) => ("tables", Some(redact(&a.connection)), None, false),
            Commands::Describe(a) => (
                "describe",
                Some(redact(&a.connection)),
                Some(a.table.clone()),
                false,
            ),
            Commands::Explain(a) => (
                "explain",
                Some(redact(&a.connection)),
                Some(a.sql.clone()),
                false,
            ),
            Commands::Dump(a) => ("dump", Some(redact(&a.connection)), None, false),
            Commands::Load(a) => ("load", Some(redact(&a.connection)), None, false),
            Commands::Export(a) => (
                "export",
                Some(redact(&a.connection)),
                Some(a.sql.clone()),
                false,
            ),
            Commands::Diff(a) => (
                "diff",
                Some(format!(
                    "{} | {}",
                    redact(&a.connection_a),
                    redact(&a.connection_b)
                )),
                None,
                false,
            ),
            Commands::Copy(a) => (
                "copy",
                Some(format!("{} | {}", redact(&a.source), redact(&a.dest))),
                a.query.clone(),
                false,
            ),
            Commands::Migrate(_) => ("migrate", None, None, false),
            Commands::Bookmark(_) => ("bookmark", None, None, false),
            Commands::Connection(_) => ("conn", None, None, false),
            Commands::Repl(a) => ("repl", a.connection.as_deref().map(redact), None, false),
            // Don't recursively log every `ferrule history` read.
            Commands::History(_) => ("history", None, None, true),
            Commands::Slow(_) => ("slow", None, None, true),
        };
        Self {
            command: name,
            conn,
            sql,
            skip,
        }
    }
}

/// Redact a connection argument before recording. Raw URLs are parsed and
/// passed through `DatabaseUrl::redacted()` (which scrubs the password);
/// registry names and SQLite paths fall through unchanged.
fn redact(s: &str) -> String {
    ferrule_sql::DatabaseUrl::parse(s)
        .map(|u| u.redacted())
        .unwrap_or_else(|_| s.to_string())
}

fn record_dispatch(
    global_config: &ferrule_config::GlobalConfig,
    snapshot: Snapshot,
    elapsed: std::time::Duration,
    outcome: &Result<(), CliError>,
) {
    if snapshot.skip {
        return;
    }
    let mut db =
        match HistoryDb::maybe_open_with_slow(&global_config.history, &global_config.slow_log) {
            Ok(Some(db)) => db,
            Ok(None) => return,
            Err(_) => return, // history failures must never block the user's command
        };
    let (exit_code, error_class) = match outcome {
        Ok(()) => (0, None),
        Err(e) => (e.exit_code(), Some(error_class(e).to_string())),
    };
    // Bench mode (Phase 3) stashes a one-row rollup in a thread-local
    // before returning; fold it into the RunRecord so the history table
    // shows one record per bench run, not N. The dispatch hook is the
    // only consumer. Cache (Phase 5) stashes a hit/miss event in a
    // sibling thread-local; bench wins if both fire because `--bench`
    // implicitly disables the cache.
    let bench_taken;
    let (mut sql, rows, mut duration_ms) = match bench::take_last() {
        Some((rollup_sql, samples)) => {
            bench_taken = true;
            (Some(rollup_sql), Some(samples), elapsed.as_millis() as u64)
        }
        None => {
            bench_taken = false;
            (snapshot.sql, None, elapsed.as_millis() as u64)
        }
    };
    if !bench_taken {
        if let Some(info) = cache::take_last() {
            if info.hit {
                sql = Some(format!(
                    "{}{}",
                    cache::CACHE_HIT_PREFIX,
                    sql.unwrap_or_default()
                ));
                // Ceiling division so a sub-millisecond cache lookup
                // doesn't round down to 0 ms and look like a phantom
                // zero-duration row in the history table.
                duration_ms = info.lookup_micros.div_ceil(1_000);
            }
            // miss: snapshot untouched. The recorded duration is the
            // real elapsed query time, which is what we want.
        }
    }
    let record = RunRecord {
        ts: chrono::Utc::now(),
        conn: snapshot.conn,
        command: snapshot.command.to_string(),
        sql,
        duration_ms,
        rows,
        exit_code,
        error: error_class,
    };
    let _ = db.record(&record, &global_config.history);
}

fn error_class(err: &CliError) -> &'static str {
    match err {
        CliError::Connection(_) => "connection",
        CliError::Query(_) => "query",
        CliError::Registry(_) => "registry",
        CliError::Io(_) => "io",
        CliError::Usage(_) => "usage",
        CliError::ResultNotable(_) => "result_notable",
    }
}
