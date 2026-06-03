use super::{
    check_daemon_ssh_compat, connect_resolved, resolve_connection, ConnectionFlags, OutputFlags,
};
use crate::error::CliError;
use clap::Args;
use ferrule_config::profile::GlobalConfig;
use ferrule_core::{DumpFormat, DumpOptions};
use ferrule_sql::connection::ConnectOptions;

#[derive(Args, Clone, Debug)]
pub struct DumpArgs {
    /// Connection name or raw URL
    pub connection: String,

    /// Table name to dump
    pub table: String,

    /// Output file (stdout if omitted)
    #[arg(long)]
    pub file: Option<String>,

    /// Dump format
    #[arg(long, value_name = "FORMAT")]
    pub dump_format: Option<String>,

    /// Schema name (if applicable)
    #[arg(long)]
    pub schema: Option<String>,

    /// Produce a byte-stable, diff-friendly SQL dump. Rows are sorted
    /// server-side by primary key (or by every column when the table
    /// has no PK), and each row becomes its own `INSERT INTO ...
    /// VALUES (...);` statement. Only affects `--dump-format sql`.
    #[arg(long)]
    pub deterministic: bool,

    #[command(flatten)]
    pub output: OutputFlags,

    #[command(flatten)]
    pub conn_flags: ConnectionFlags,
}

pub fn run(args: DumpArgs, global_config: &GlobalConfig) -> Result<(), CliError> {
    let format = args
        .dump_format
        .as_deref()
        .and_then(DumpFormat::parse)
        .unwrap_or(DumpFormat::Csv);

    let mut opts = DumpOptions {
        format,
        deterministic: args.deterministic,
        ..DumpOptions::default()
    };
    opts.schema = args.schema.clone();

    let resolved = resolve_connection(
        &args.connection,
        None,
        args.conn_flags.ssh_tunnel.as_deref(),
        args.conn_flags.ssh_key.as_deref(),
        args.conn_flags.proxy_url.as_deref(),
        global_config,
    )?;
    check_daemon_ssh_compat(args.conn_flags.daemon, &resolved)?;

    let backend = ferrule_sql::Backend::from_scheme(resolved.url.scheme())
        .ok_or_else(|| CliError::usage(format!("Unsupported scheme: {}", resolved.url.scheme())))?;

    if args.deterministic && args.conn_flags.daemon {
        return Err(CliError::usage(
            "--deterministic is not supported via --daemon yet \
             (daemon streaming lands in Wave 3); run direct without --daemon.",
        ));
    }

    if args.conn_flags.daemon {
        eprintln!("[ferrule] Routing via daemon...");
        // Daemon path: build a simple SELECT and route through daemon.
        // Full dump via daemon is left for Wave 3 streaming.
        let sql = format!("SELECT * FROM {}", quote_identifier(&args.table));
        let payload = crate::daemon::daemon_query(
            &sql,
            &resolved.url,
            args.conn_flags.insecure,
            ferrule_core::OutputFormat::Json,
            None,
            None,
        )?;
        write_output(&args, &payload)?;
        return Ok(());
    }

    let opts_conn = ConnectOptions {
        insecure: args.conn_flags.insecure,
        password: None,
    };
    if opts_conn.insecure {
        eprintln!("Warning: --insecure disables TLS certificate verification.");
    }

    let mut conn = connect_resolved(resolved, &opts_conn)?;

    let dumped = ferrule_core::dump_table(conn.as_mut(), &args.table, backend, &opts)
        .map_err(CliError::query)?;

    write_output(&args, &dumped)?;
    Ok(())
}

fn write_output(args: &DumpArgs, content: &str) -> Result<(), CliError> {
    if let Some(path) = &args.file {
        std::fs::write(path, content).map_err(CliError::Io)?;
        eprintln!("Wrote to {}", path);
    } else {
        println!("{}", content);
    }
    Ok(())
}

fn quote_identifier(id: &str) -> String {
    format!("\"{}\"", id.replace('\"', "\"\""))
}
