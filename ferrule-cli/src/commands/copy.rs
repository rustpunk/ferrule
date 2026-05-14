use super::CopyArgs;
use crate::error::CliError;
use ferrule_config::profile::GlobalConfig;
use ferrule_core::backend::Backend;
use ferrule_core::connection::ConnectOptions;
use ferrule_core::copy::{copy_rows, BulkMode, CopyOptions, CopySource, IfExists};
use is_terminal::IsTerminal;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

pub async fn run(args: CopyArgs, global_config: &GlobalConfig) -> Result<(), CliError> {
    // --- arg validation ------------------------------------------------
    let source = match (args.table.clone(), args.query.clone(), args.into.clone()) {
        (Some(t), None, _) => CopySource::Table(t),
        (None, Some(q), Some(i)) => CopySource::Query { sql: q, into: i },
        (None, Some(_), None) => {
            return Err(CliError::usage(
                "--query requires --into NAME for the target table.",
            ));
        }
        (None, None, _) => {
            return Err(CliError::usage(
                "Pass --table NAME or --query SQL --into NAME to choose a source.",
            ));
        }
        (Some(_), Some(_), _) => {
            // clap's conflicts_with should catch this, but belt-and-braces.
            return Err(CliError::usage(
                "--table and --query are mutually exclusive.",
            ));
        }
    };

    let if_exists = IfExists::parse(&args.if_exists).ok_or_else(|| {
        CliError::usage(format!(
            "Unknown --if-exists strategy '{}'. Use: error, append, truncate.",
            args.if_exists
        ))
    })?;

    let bulk_mode = BulkMode::parse(&args.bulk_native).ok_or_else(|| {
        CliError::usage(format!(
            "Unknown --bulk-native mode '{}'. Use: off, auto, on.",
            args.bulk_native
        ))
    })?;

    if if_exists == IfExists::Truncate && !args.yes && std::io::stdin().is_terminal() {
        return Err(CliError::usage(
            "--if-exists truncate is destructive: it will DELETE every row \
             in the target table. Re-run with --yes to confirm, or pick \
             --if-exists append.",
        ));
    }

    if args.batch == 0 {
        return Err(CliError::usage("--batch must be greater than zero."));
    }

    // --- resolve and connect both sides -------------------------------
    let resolved_src = super::resolve_connection(
        &args.source,
        args.password_src,
        args.conn_flags.ssh_tunnel.as_deref(),
        args.conn_flags.ssh_key.as_deref(),
        args.conn_flags.proxy_url.as_deref(),
        global_config,
    )
    .await?;
    let resolved_dst = super::resolve_connection(
        &args.dest,
        args.password_dst,
        args.conn_flags.ssh_tunnel.as_deref(),
        args.conn_flags.ssh_key.as_deref(),
        args.conn_flags.proxy_url.as_deref(),
        global_config,
    )
    .await?;
    super::check_daemon_ssh_compat(args.conn_flags.daemon, &resolved_src)?;
    super::check_daemon_ssh_compat(args.conn_flags.daemon, &resolved_dst)?;

    let backend_src = Backend::from_scheme(resolved_src.url.scheme()).ok_or_else(|| {
        CliError::usage(format!(
            "Unsupported source scheme: {}",
            resolved_src.url.scheme()
        ))
    })?;
    let backend_dst = Backend::from_scheme(resolved_dst.url.scheme()).ok_or_else(|| {
        CliError::usage(format!(
            "Unsupported destination scheme: {}",
            resolved_dst.url.scheme()
        ))
    })?;

    if args.output.verbose {
        eprintln!(
            "[ferrule] Copy: {} ({}) -> {} ({})",
            resolved_src.url.redacted(),
            backend_src.name(),
            resolved_dst.url.redacted(),
            backend_dst.name()
        );
    }

    let conn_opts = ConnectOptions {
        insecure: args.conn_flags.insecure,
    };
    if conn_opts.insecure {
        eprintln!("Warning: --insecure disables TLS certificate verification.");
    }

    let mut conn_src = super::connect_resolved(resolved_src, &conn_opts).await?;
    let mut conn_dst = super::connect_resolved(resolved_dst, &conn_opts).await?;

    // --- progress callback (stderr, every batch) -----------------------
    let progress: Option<Box<dyn Fn(usize) + Send>> = if args.output.verbose {
        let last = Arc::new(AtomicUsize::new(0));
        let l = last.clone();
        Some(Box::new(move |total: usize| {
            l.store(total, Ordering::Relaxed);
            eprintln!("[ferrule] copied {total} rows...");
        }))
    } else {
        None
    };

    let opts = CopyOptions {
        source,
        create_table: args.create_table,
        if_exists,
        atomic: args.atomic,
        batch_size: args.batch,
        bulk_mode,
        verbose: args.output.verbose,
        progress,
    };

    let started = std::time::Instant::now();
    let copied = copy_rows(
        conn_src.as_mut(),
        backend_src,
        conn_dst.as_mut(),
        backend_dst,
        &opts,
    )
    .await
    .map_err(CliError::query)?;
    let elapsed = started.elapsed();

    eprintln!(
        "Copied {} rows: {} -> {} ({:.2?})",
        copied,
        backend_src.name(),
        backend_dst.name(),
        elapsed
    );

    Ok(())
}
