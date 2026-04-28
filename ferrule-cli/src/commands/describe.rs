use super::DescribeArgs;
use crate::error::CliError;
use ferrule_config::profile::GlobalConfig;
use ferrule_core::connection::ConnectOptions;
use ferrule_core::formatter::format_result;

pub async fn run(args: DescribeArgs, global_config: &GlobalConfig) -> Result<(), CliError> {
    let format = args.output.resolve_format(global_config);

    let total_start = std::time::Instant::now();

    let resolved = super::resolve_connection(
        &args.connection,
        None,
        args.conn_flags.ssh_tunnel.as_deref(),
        args.conn_flags.ssh_key.as_deref(),
        args.conn_flags.proxy_url.as_deref(),
        global_config,
    )
    .await?;
    super::check_daemon_ssh_compat(args.conn_flags.daemon, &resolved)?;

    if args.output.verbose {
        eprintln!("[ferrule] Resolved URL: {}", resolved.url.redacted());
    }

    // Route through daemon if requested
    if args.conn_flags.daemon {
        eprintln!("[ferrule] Routing via daemon...");
        let payload = crate::daemon::daemon_describe(
            &resolved.url,
            args.conn_flags.insecure,
            None,
            &args.table,
        )
        .await?;
        println!("{}", payload);
        return Ok(());
    }

    let opts = ConnectOptions {
        insecure: args.conn_flags.insecure,
    };
    if opts.insecure {
        eprintln!("Warning: --insecure disables TLS certificate verification.");
    }

    let conn_start = std::time::Instant::now();
    let mut conn = super::connect_resolved(resolved, &opts).await?;
    let conn_time = conn_start.elapsed();

    let query_start = std::time::Instant::now();
    let result = conn
        .describe_table(None, &args.table)
        .await
        .map_err(CliError::query)?;
    let query_time = query_start.elapsed();

    let format_start = std::time::Instant::now();
    let output = format_result(&result, format).map_err(CliError::query)?;
    let format_time = format_start.elapsed();

    println!("{}", output);

    if args.output.timing {
        eprintln!(
            "[ferrule] timing: connect={:.3}s query={:.3}s format={:.3}s total={:.3}s",
            conn_time.as_secs_f64(),
            query_time.as_secs_f64(),
            format_time.as_secs_f64(),
            total_start.elapsed().as_secs_f64(),
        );
    }

    Ok(())
}
