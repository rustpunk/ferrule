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

    /// Watch a file for changes instead of polling on interval
    #[arg(long, value_name = "PATH")]
    pub file_path: Option<std::path::PathBuf>,

    /// Exit immediately on connection/query failure
    #[arg(long)]
    pub exit_on_error: bool,

    /// Ring terminal bell when output changes
    #[arg(long)]
    pub bell: bool,

    #[command(flatten)]
    pub output: OutputFlags,

    #[command(flatten)]
    pub conn_flags: ConnectionFlags,

    /// Connection password
    #[arg(short = 'p', long)]
    pub password: Option<String>,
}

pub fn run(args: WatchArgs, global_config: &GlobalConfig) -> Result<(), CliError> {
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
        exit_on_error: args.exit_on_error,
        bell: args.bell,
        format,
        limit,
        offset,
        timing: args.output.timing,
        verbose: args.output.verbose,
        conn_flags: args.conn_flags.clone(),
        global_config: global_config.clone(),
        print_lock: print_lock.clone(),
        file_path: args.file_path.clone(),
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
        args.conn_flags.proxy_url.as_deref(),
        global_config,
    )?;

    // Ctrl-C handling lives on a dedicated background thread that owns a
    // tiny current-thread runtime solely to await the signal. When it
    // fires it flips `running`, which the synchronous `watch_loop`
    // observes at its next sleep boundary. Keeping the signal wait off
    // the main thread lets the loop create ferrule-sql connections
    // (each owning a private runtime) without nesting runtimes.
    let r = running.clone();
    std::thread::spawn(move || {
        if let Ok(rt) = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            rt.block_on(async {
                tokio::signal::ctrl_c().await.ok();
            });
        }
        r.store(false, Ordering::Relaxed);
    });

    watch_loop(&opts, &running)?;

    {
        let _guard = print_lock.lock();
        eprintln!("\n[watch] stopped.");
    }

    Ok(())
}
