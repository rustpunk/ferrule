use super::{check_daemon_ssh_compat, connect_resolved, resolve_connection, ConnectionFlags};
use crate::error::CliError;
use clap::Args;
use ferrule_config::profile::GlobalConfig;
use ferrule_core::connection::ConnectOptions;
use ferrule_core::{LoadFormat, LoadOptions};
use std::path::Path;

#[derive(Args, Clone, Debug)]
pub struct LoadArgs {
    /// Connection name or raw URL
    pub connection: String,

    /// Input file path
    pub file: String,

    /// Target table name (inferred from file stem if omitted)
    #[arg(short, long)]
    pub table: Option<String>,

    /// Input format (inferred from extension if omitted)
    #[arg(short, long, value_name = "FORMAT")]
    pub format: Option<String>,

    /// Create the table before loading (JSON only)
    #[arg(long)]
    pub create_table: bool,

    #[command(flatten)]
    pub conn_flags: ConnectionFlags,
}

pub async fn run(args: LoadArgs, global_config: &GlobalConfig) -> Result<(), CliError> {
    let path = Path::new(&args.file);

    let format = if let Some(f) = args.format {
        LoadFormat::parse(&f)
            .ok_or_else(|| CliError::usage(format!("Unknown load format: {}", f)))?
    } else {
        let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
        match ext.to_ascii_lowercase().as_str() {
            "csv" => LoadFormat::Csv,
            "json" => LoadFormat::Json,
            _ => {
                return Err(CliError::usage(
                    "Cannot infer format from extension. Use --format csv|json.",
                ));
            }
        }
    };

    let table = if let Some(t) = args.table {
        t
    } else {
        let stem = path.file_stem().and_then(|s| s.to_str()).ok_or_else(|| {
            CliError::usage("Cannot infer table name from file path. Use --table.")
        })?;
        stem.to_string()
    };

    if args.create_table && format != LoadFormat::Json {
        return Err(CliError::usage(
            "--create-table is only supported with JSON input.",
        ));
    }

    let data = tokio::fs::read_to_string(path)
        .await
        .map_err(|e| CliError::usage(format!("Cannot read file '{}': {}", args.file, e)))?;

    let resolved = resolve_connection(
        &args.connection,
        None,
        args.conn_flags.ssh_tunnel.as_deref(),
        args.conn_flags.ssh_key.as_deref(),
        args.conn_flags.proxy_url.as_deref(),
        global_config,
    )
    .await?;
    check_daemon_ssh_compat(args.conn_flags.daemon, &resolved)?;

    let backend = ferrule_core::Backend::from_scheme(resolved.url.scheme())
        .ok_or_else(|| CliError::usage(format!("Unsupported scheme: {}", resolved.url.scheme())))?;

    let opts = LoadOptions {
        format,
        table,
        create_table: args.create_table,
        ..LoadOptions::default()
    };

    if args.conn_flags.daemon {
        eprintln!("[ferrule] Routing via daemon...");
        // Daemon path not fully implemented for load in Wave 2.
        return Err(CliError::usage(
            "Load via daemon is not yet supported. Omit --daemon.",
        ));
    }

    let opts_conn = ConnectOptions {
        insecure: args.conn_flags.insecure,
    };
    if opts_conn.insecure {
        eprintln!("Warning: --insecure disables TLS certificate verification.");
    }

    let mut conn = connect_resolved(resolved, &opts_conn).await?;

    let loaded = ferrule_core::load_data(conn.as_mut(), &data, backend, &opts)
        .await
        .map_err(CliError::query)?;

    println!("Loaded {} rows into '{}'.", loaded, opts.table);
    Ok(())
}
