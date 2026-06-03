use super::{ConnectionFlags, OutputFlags};
use crate::error::CliError;
use crate::repl::{resolve_initial_connection, run_repl_loop, Repl};
use clap::Args;
use ferrule_config::profile::GlobalConfig;
use is_terminal::IsTerminal;

#[derive(Args, Clone, Debug)]
pub struct ReplArgs {
    /// Connection name or raw URL (optional; uses default if omitted)
    pub connection: Option<String>,

    #[command(flatten)]
    pub output: OutputFlags,

    #[command(flatten)]
    pub conn_flags: ConnectionFlags,
}

pub fn run(args: ReplArgs, global_config: &GlobalConfig) -> Result<(), CliError> {
    if !std::io::stdin().is_terminal() {
        return Err(CliError::usage("REPL requires an interactive terminal."));
    }

    let connection_str = resolve_initial_connection(args.connection, global_config)?;

    let format = args.output.resolve_format(global_config);
    let limit = args.output.resolve_limit(global_config);
    let offset = args.output.offset;
    let timing = args.output.timing;
    let verbose = args.output.verbose;

    let mut repl = Repl::new(
        &connection_str,
        args.output,
        args.conn_flags.clone(),
        global_config,
    )?;

    // Override defaults from CLI flags
    repl.state.format = format;
    repl.state.limit = limit;
    repl.state.offset = offset;
    repl.state.timing = timing;
    repl.state.verbose = verbose;

    if args.conn_flags.insecure {
        eprintln!("Warning: --insecure disables TLS certificate verification.");
    }

    // Print welcome
    println!(
        "ferrule {} — connected to {} ({:?})",
        env!("CARGO_PKG_VERSION"),
        repl.url.redacted(),
        repl.backend
    );
    println!("Type \\help for help, \\q to quit.");

    let result = run_repl_loop(&mut repl);
    // Graceful cleanup: ping once more but ignore errors.
    let _ = repl.conn.ping();
    result?;

    Ok(())
}
