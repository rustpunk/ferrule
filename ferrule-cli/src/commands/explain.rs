use super::{check_daemon_ssh_compat, connect_resolved, resolve_connection, ConnectionFlags, OutputFlags};
use crate::error::CliError;
use clap::Args;
use ferrule_config::profile::GlobalConfig;
use ferrule_core::connection::ConnectOptions;
use ferrule_core::explain::{explain_sql, is_modifying, ExplainOutput};
use ferrule_core::formatter::format_result;

#[derive(Args, Clone, Debug)]
pub struct ExplainArgs {
    /// Connection name or raw URL
    pub connection: String,

    /// SQL statement to explain
    pub sql: String,

    /// Actually execute the statement to collect runtime statistics
    #[arg(long)]
    pub analyze: bool,

    #[command(flatten)]
    pub output: OutputFlags,

    #[command(flatten)]
    pub conn_flags: ConnectionFlags,
}

pub async fn run(args: ExplainArgs, global_config: &GlobalConfig) -> Result<(), CliError> {
    let format = args.output.resolve_format(global_config);

    if args.output.verbose {
        eprintln!("[ferrule] SQL: {}", args.sql);
    }

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

    if args.output.verbose {
        eprintln!("[ferrule] Resolved URL: {}", resolved.url.redacted());
    }

    let backend = ferrule_core::Backend::from_scheme(resolved.url.scheme()).ok_or_else(|| {
        CliError::usage(format!("Unsupported scheme: {}", resolved.url.scheme()))
    })?;

    let (wrapped_sql, explain_out) =
        explain_sql(&args.sql, backend, args.analyze).map_err(CliError::query)?;

    if is_modifying(&args.sql) {
        eprintln!(
            "Warning: EXPLAIN on modifying statement uses estimated plan (ANALYZE disabled)."
        );
    }

    if args.output.verbose {
        eprintln!("[ferrule] wrapped SQL: {}", wrapped_sql);
    }

    if args.conn_flags.daemon {
        eprintln!("[ferrule] Routing via daemon...");
        let payload = crate::daemon::daemon_query(
            &wrapped_sql,
            &resolved.url,
            args.conn_flags.insecure,
            format,
            None,
            None,
        )
        .await?;
        // Try to pretty-print JSON / XML
        print_payload(&payload, explain_out);
        return Ok(());
    }

    let opts = ConnectOptions {
        insecure: args.conn_flags.insecure,
    };
    if opts.insecure {
        eprintln!("Warning: --insecure disables TLS certificate verification.");
    }

    let mut conn = connect_resolved(resolved, &opts).await?;

    let result = conn.query(&wrapped_sql).await.map_err(CliError::query)?;

    let rendered = format_result(&result, format).map_err(CliError::query)?;
    print_payload(&rendered, explain_out);

    Ok(())
}

fn print_payload(payload: &str, out: ExplainOutput) {
    match out {
        ExplainOutput::Json => {
            if let Ok(val) = serde_json::from_str::<serde_json::Value>(payload) {
                if let Ok(pretty) = serde_json::to_string_pretty(&val) {
                    println!("{}", pretty);
                    return;
                }
            }
            println!("{}", payload);
        }
        ExplainOutput::Xml => {
            // Wave 2: raw XML output. Pretty-printing can be added later.
            println!("{}", payload);
        }
        ExplainOutput::Text => {
            println!("{}", payload);
        }
    }
}
