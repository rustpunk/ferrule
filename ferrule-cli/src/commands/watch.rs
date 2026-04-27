use crate::commands::{resolve_connection, ConnectionFlags, OutputFlags};
use crate::error::CliError;
use crate::watch::{watch_loop, WatchOptions};
use clap::Args;
use ferrule_config::profile::GlobalConfig;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

/// Watch command arguments.
#[derive(Args, Clone, Debug)]
pub struct WatchArgs {
    /// Connection name or raw URL
    pub connection: String,

    /// SQL statement to watch
    pub sql: String,

    /// Poll interval in seconds (default: 5)
    #[arg(short, long, value_name = "SECS", default_value = "5")]
    pub interval: u64,

    /// Maximum number of iterations
    #[arg(long, value_name = "N")]
    pub max_iterations: Option<u64>,

    /// Only print when output changes
    #[arg(long)]
    pub diff: bool,

    #[command(flatten)]
    pub output: OutputFlags,

    #[command(flatten)]
    pub conn_flags: ConnectionFlags,

    /// Connection password
    #[arg(short = 'p', long)]
    pub password: Option<String>,
}

pub async fn run(args: WatchArgs, global_config: &GlobalConfig) -> Result<(), CliError> {
    if args.interval == 0 {
        return Err(CliError::usage("--interval must be at least 1 second"));
    }

    let format = args.output.resolve_format(global_config);
    let limit = args.output.resolve_limit(global_config);
    let offset = args.output.offset;

    let print_lock = Arc::new(std::sync::Mutex::new(()));
    let running = Arc::new(AtomicBool::new(true));
    let interval_secs = Arc::new(AtomicU64::new(args.interval));

    let opts = WatchOptions {
        connection: args.connection.clone(),
        sql: args.sql.clone(),
        interval_secs: interval_secs.clone(),
        max_iterations: args.max_iterations,
        diff: args.diff,
        format,
        limit,
        offset,
        timing: args.output.timing,
        verbose: args.output.verbose,
        conn_flags: args.conn_flags.clone(),
        global_config: global_config.clone(),
        print_lock: print_lock.clone(),
    };

    // Resolve connection once to validate before entering the loop.
    // Discard the resolved tunnel handle (if any): the watch loop
    // re-resolves on every iteration via `crate::watch::watch_loop`,
    // which spins up a fresh tunnel per poll.
    let _resolved = resolve_connection(
        &args.connection,
        args.password,
        args.conn_flags.ssh_tunnel.as_deref(),
        args.conn_flags.ssh_key.as_deref(),
        global_config,
    )
    .await?;

    let r = running.clone();
    tokio::spawn(async move {
        tokio::signal::ctrl_c().await.ok();
        r.store(false, Ordering::Relaxed);
    });

    watch_loop(&opts, &running).await?;

    {
        let _guard = print_lock.lock();
        eprintln!("\n[watch] stopped.");
    }

    Ok(())
}
