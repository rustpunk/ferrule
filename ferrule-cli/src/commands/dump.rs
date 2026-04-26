use super::{resolve_connection, ConnectionFlags, OutputFlags};
use crate::error::CliError;
use clap::Args;
use ferrule_config::profile::GlobalConfig;
use ferrule_core::backend::connect;
use ferrule_core::connection::ConnectOptions;
use ferrule_core::{DumpFormat, DumpOptions};

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

    #[command(flatten)]
    pub output: OutputFlags,

    #[command(flatten)]
    pub conn_flags: ConnectionFlags,
}

pub async fn run(args: DumpArgs, global_config: &GlobalConfig) -> Result<(), CliError> {
    let format = args
        .dump_format
        .as_deref()
        .and_then(DumpFormat::parse)
        .unwrap_or(DumpFormat::Csv);

    let mut opts = DumpOptions {
        format,
        ..DumpOptions::default()
    };
    opts.schema = args.schema.clone();

    let url = resolve_connection(&args.connection, None, global_config).await?;

    let backend = ferrule_core::Backend::from_scheme(url.scheme())
        .ok_or_else(|| CliError::usage(format!("Unsupported scheme: {}", url.scheme())))?;

    if args.conn_flags.daemon {
        eprintln!("[ferrule] Routing via daemon...");
        // Daemon path: build a simple SELECT and route through daemon.
        // Full dump via daemon is left for Wave 3 streaming.
        let sql = format!("SELECT * FROM {}", quote_identifier(&args.table));
        let payload = crate::daemon::daemon_query(
            &sql,
            &url,
            args.conn_flags.insecure,
            ferrule_core::OutputFormat::Json,
            None,
            None,
        )
        .await?;
        write_output(&args, &payload)?;
        return Ok(());
    }

    let opts_conn = ConnectOptions {
        insecure: args.conn_flags.insecure,
    };
    if opts_conn.insecure {
        eprintln!("Warning: --insecure disables TLS certificate verification.");
    }

    let mut conn = connect(&url, &opts_conn)
        .await
        .map_err(CliError::connection)?;

    let dumped = ferrule_core::dump_table(conn.as_mut(), &args.table, backend, &opts)
        .await
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
