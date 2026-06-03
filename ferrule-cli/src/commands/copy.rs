use super::CopyArgs;
use crate::error::CliError;
use ferrule_config::profile::GlobalConfig;
use ferrule_sql::backend::Backend;
use ferrule_sql::connection::ConnectOptions;
use ferrule_sql::copy::{
    copy_all_tables, copy_rows, AllTablesOptions, BulkMode, CopyFormat, CopyOptions, CopySource,
    IfExists,
};

// `BulkMode` itself comes from ferrule_sql; the CLI wraps it in
// `BulkNativeMode` so clap can enumerate values in --help and reject
// bad inputs with a real usage error instead of routing them through
// the runtime parser.
use is_terminal::IsTerminal;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

pub async fn run(args: CopyArgs, global_config: &GlobalConfig) -> Result<(), CliError> {
    // --- arg validation ------------------------------------------------
    // Surface the most specific error first: --include / --exclude /
    // --no-fk-check are only meaningful in --all-tables mode.
    if !args.all_tables && (!args.include.is_empty() || !args.exclude.is_empty()) {
        return Err(CliError::usage(
            "--include / --exclude require --all-tables.",
        ));
    }
    if !args.all_tables && args.no_fk_check {
        return Err(CliError::usage("--no-fk-check requires --all-tables."));
    }

    let source = if args.all_tables {
        // Belt-and-braces — clap conflicts_with should catch these.
        if args.table.is_some() || args.query.is_some() || args.into.is_some() {
            return Err(CliError::usage(
                "--all-tables is mutually exclusive with --table, --query, and --into.",
            ));
        }
        None
    } else {
        Some(
            match (args.table.clone(), args.query.clone(), args.into.clone()) {
                (Some(t), None, _) => CopySource::Table(t),
                (None, Some(q), Some(i)) => CopySource::Query { sql: q, into: i },
                (None, Some(_), None) => {
                    return Err(CliError::usage(
                        "--query requires --into NAME for the target table.",
                    ));
                }
                (None, None, _) => {
                    return Err(CliError::usage(
                        "Pass --table NAME, --query SQL --into NAME, or --all-tables.",
                    ));
                }
                (Some(_), Some(_), _) => {
                    // clap's conflicts_with should catch this, but belt-and-braces.
                    return Err(CliError::usage(
                        "--table and --query are mutually exclusive.",
                    ));
                }
            },
        )
    };

    let if_exists = IfExists::parse(&args.if_exists).ok_or_else(|| {
        CliError::usage(format!(
            "Unknown --if-exists strategy '{}'. Use: error, append, truncate, skip, upsert.",
            args.if_exists
        ))
    })?;

    // clap's ValueEnum derive on `BulkNativeMode` already rejects
    // invalid input with a usage error before we get here, so this
    // conversion is infallible.
    let bulk_mode: BulkMode = args.bulk_native.into();
    let copy_format: CopyFormat = args.copy_format.into();
    // --copy-format binary is only meaningful when the bulk path is
    // selected (the generic INSERT path doesn't use COPY). Surface
    // the misconfiguration up front rather than silently dropping.
    if copy_format == CopyFormat::Binary && bulk_mode == BulkMode::Off {
        return Err(CliError::usage(
            "--copy-format binary requires --bulk-native auto or on; \
             the generic INSERT path does not use COPY.",
        ));
    }

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
    // Per-side `--src-*` / `--dst-*` flags override the unsuffixed
    // shared shortcut. Setting both for the same field is a usage
    // error (no silent merge). Booleans use the same rule: setting
    // `--insecure` and `--src-insecure` together is rejected because
    // the latter is redundant with the former.
    let src_ssh_tunnel = merge_per_side_str(
        args.src_ssh_tunnel.as_deref(),
        args.conn_flags.ssh_tunnel.as_deref(),
        "--ssh-tunnel",
        "--src-ssh-tunnel",
    )?;
    let src_ssh_key = merge_per_side_str(
        args.src_ssh_key.as_deref(),
        args.conn_flags.ssh_key.as_deref(),
        "--ssh-key",
        "--src-ssh-key",
    )?;
    let src_proxy_url = merge_per_side_str(
        args.src_proxy_url.as_deref(),
        args.conn_flags.proxy_url.as_deref(),
        "--proxy-url",
        "--src-proxy-url",
    )?;
    let src_insecure = merge_per_side_bool(
        args.src_insecure,
        args.conn_flags.insecure,
        "--insecure",
        "--src-insecure",
    )?;

    let dst_ssh_tunnel = merge_per_side_str(
        args.dst_ssh_tunnel.as_deref(),
        args.conn_flags.ssh_tunnel.as_deref(),
        "--ssh-tunnel",
        "--dst-ssh-tunnel",
    )?;
    let dst_ssh_key = merge_per_side_str(
        args.dst_ssh_key.as_deref(),
        args.conn_flags.ssh_key.as_deref(),
        "--ssh-key",
        "--dst-ssh-key",
    )?;
    let dst_proxy_url = merge_per_side_str(
        args.dst_proxy_url.as_deref(),
        args.conn_flags.proxy_url.as_deref(),
        "--proxy-url",
        "--dst-proxy-url",
    )?;
    let dst_insecure = merge_per_side_bool(
        args.dst_insecure,
        args.conn_flags.insecure,
        "--insecure",
        "--dst-insecure",
    )?;

    let resolved_src = super::resolve_connection(
        &args.source,
        args.password_src,
        src_ssh_tunnel.as_deref(),
        src_ssh_key.as_deref(),
        src_proxy_url.as_deref(),
        global_config,
    )
    .await?;
    let resolved_dst = super::resolve_connection(
        &args.dest,
        args.password_dst,
        dst_ssh_tunnel.as_deref(),
        dst_ssh_key.as_deref(),
        dst_proxy_url.as_deref(),
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

    let conn_opts_src = ConnectOptions {
        insecure: src_insecure,
        password: None,
    };
    let conn_opts_dst = ConnectOptions {
        insecure: dst_insecure,
        password: None,
    };
    if conn_opts_src.insecure && conn_opts_dst.insecure {
        eprintln!("Warning: --insecure disables TLS certificate verification.");
    } else if conn_opts_src.insecure {
        eprintln!("Warning: TLS certificate verification disabled on source.");
    } else if conn_opts_dst.insecure {
        eprintln!("Warning: TLS certificate verification disabled on destination.");
    }

    let mut conn_src = super::connect_resolved(resolved_src, &conn_opts_src).await?;
    let mut conn_dst = super::connect_resolved(resolved_dst, &conn_opts_dst).await?;

    let started = std::time::Instant::now();
    let copied = if args.all_tables {
        let all_opts = AllTablesOptions {
            include: args.include.clone(),
            exclude: args.exclude.clone(),
            if_exists,
            atomic: args.atomic,
            batch_size: args.batch,
            bulk_mode,
            copy_format,
            verbose: args.output.verbose,
            create_table: args.create_table,
            preserve_pk: args.preserve_pk,
            conflict_key: args.key.clone(),
            no_fk_check: args.no_fk_check,
        };
        copy_all_tables(
            conn_src.as_mut(),
            backend_src,
            conn_dst.as_mut(),
            backend_dst,
            &all_opts,
        )
        .await
        .map_err(CliError::query)?
    } else {
        // --- progress callback (stderr, every batch) ------------------
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
            source: source.expect("source resolved in the non-all-tables branch"),
            create_table: args.create_table,
            preserve_pk: args.preserve_pk,
            if_exists,
            conflict_key: args.key.clone(),
            atomic: args.atomic,
            batch_size: args.batch,
            bulk_mode,
            copy_format,
            verbose: args.output.verbose,
            progress,
        };

        copy_rows(
            conn_src.as_mut(),
            backend_src,
            conn_dst.as_mut(),
            backend_dst,
            &opts,
        )
        .await
        .map_err(CliError::query)?
    };
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

/// Pick the per-side override over the shared shortcut, erroring when
/// both are set (no silent merge — the user expressed two conflicting
/// intents). Returns the resolved value, or `None` if neither side set
/// the flag (profile defaults still apply downstream).
fn merge_per_side_str(
    side: Option<&str>,
    shared: Option<&str>,
    shared_flag: &str,
    side_flag: &str,
) -> Result<Option<String>, CliError> {
    match (side, shared) {
        (Some(_), Some(_)) => Err(CliError::usage(format!(
            "Cannot combine {shared_flag} and {side_flag}: pick one. \
             The unsuffixed form applies to both sides; the per-side \
             form is a source/destination override."
        ))),
        (Some(v), None) => Ok(Some(v.to_string())),
        (None, Some(v)) => Ok(Some(v.to_string())),
        (None, None) => Ok(None),
    }
}

/// Same shape as [`merge_per_side_str`] but for boolean flags. Both
/// `true` is rejected so the user expresses intent unambiguously.
fn merge_per_side_bool(
    side: bool,
    shared: bool,
    shared_flag: &str,
    side_flag: &str,
) -> Result<bool, CliError> {
    if side && shared {
        return Err(CliError::usage(format!(
            "Cannot combine {shared_flag} and {side_flag}: the \
             unsuffixed form already applies to both sides."
        )));
    }
    Ok(side || shared)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merge_per_side_str_rejects_both_set() {
        let err = merge_per_side_str(Some("u@a"), Some("u@b"), "--ssh-tunnel", "--src-ssh-tunnel")
            .expect_err("expected usage error");
        let msg = format!("{err}");
        assert!(
            msg.contains("--ssh-tunnel"),
            "msg should name both flags: {msg}"
        );
        assert!(
            msg.contains("--src-ssh-tunnel"),
            "msg should name both flags: {msg}"
        );
    }

    #[test]
    fn merge_per_side_str_prefers_side_when_only_side_set() {
        let v =
            merge_per_side_str(Some("u@a"), None, "--ssh-tunnel", "--src-ssh-tunnel").expect("ok");
        assert_eq!(v, Some("u@a".to_string()));
    }

    #[test]
    fn merge_per_side_str_falls_back_to_shared() {
        let v =
            merge_per_side_str(None, Some("u@a"), "--ssh-tunnel", "--src-ssh-tunnel").expect("ok");
        assert_eq!(v, Some("u@a".to_string()));
    }

    #[test]
    fn merge_per_side_str_returns_none_when_neither_set() {
        let v = merge_per_side_str(None, None, "--ssh-tunnel", "--src-ssh-tunnel").expect("ok");
        assert_eq!(v, None);
    }

    #[test]
    fn merge_per_side_bool_rejects_both_true() {
        let err = merge_per_side_bool(true, true, "--insecure", "--src-insecure")
            .expect_err("expected usage error");
        let msg = format!("{err}");
        assert!(
            msg.contains("--insecure"),
            "msg should name both flags: {msg}"
        );
        assert!(
            msg.contains("--src-insecure"),
            "msg should name both flags: {msg}"
        );
    }

    #[test]
    fn merge_per_side_bool_logical_or_when_one_set() {
        assert!(merge_per_side_bool(true, false, "--insecure", "--src-insecure").unwrap());
        assert!(merge_per_side_bool(false, true, "--insecure", "--src-insecure").unwrap());
        assert!(!merge_per_side_bool(false, false, "--insecure", "--src-insecure").unwrap());
    }
}
