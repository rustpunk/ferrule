use crate::commands::{
    check_daemon_ssh_compat, connect_resolved, resolve_connection, ConnectionFlags,
};
use crate::error::CliError;
use ferrule_config::profile::GlobalConfig;
use ferrule_sql::connection::{ConnectOptions, StatementResult};
use ferrule_core::formatter::{format_result, OutputFormat};
use is_terminal::IsTerminal;
use notify::{EventKind, RecursiveMode, Watcher};
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use tokio::time::Duration;
use tokio::time::Instant;

/// Options that drive a single watch loop.
pub struct WatchOptions {
    pub connection: String,
    pub sql: String,
    pub file_path: Option<PathBuf>,
    pub interval_secs: Arc<AtomicU64>,
    pub max_iterations: Option<u64>,
    pub diff: bool,
    pub exit_on_error: bool,
    pub bell: bool,
    pub format: OutputFormat,
    pub limit: Option<usize>,
    pub offset: Option<usize>,
    pub timing: bool,
    pub verbose: bool,
    pub conn_flags: ConnectionFlags,
    pub global_config: GlobalConfig,
    pub print_lock: Arc<std::sync::Mutex<()>>,
}

/// Run the watch loop until the `running` flag is cleared.
pub async fn watch_loop(opts: &WatchOptions, running: &AtomicBool) -> Result<(), CliError> {
    let mut iteration = 0u64;
    let mut prev_output: Option<String> = None;
    let is_tty = std::io::stdout().is_terminal();

    // Set up file watcher if watching a file
    let (watch_tx, mut watch_rx) = tokio::sync::mpsc::channel::<()>(1);
    let _watcher = if let Some(ref path) = opts.file_path {
        let path = path.clone();
        let tx = watch_tx.clone();
        let mut watcher =
            notify::recommended_watcher(move |res: Result<notify::Event, notify::Error>| {
                if let Ok(event) = res {
                    if matches!(
                        event.kind,
                        EventKind::Modify(_) | EventKind::Create(_) | EventKind::Remove(_)
                    ) {
                        let _ = tx.try_send(());
                    }
                }
            })
            .map_err(|e| CliError::Io(std::io::Error::other(e.to_string())))?;

        watcher
            .watch(&path, RecursiveMode::NonRecursive)
            .map_err(|e| CliError::Io(std::io::Error::other(e.to_string())))?;
        Some(watcher)
    } else {
        None
    };

    while running.load(Ordering::Relaxed) {
        iteration += 1;

        if let Some(max) = opts.max_iterations {
            if iteration > max {
                break;
            }
        }

        // If watching a file, wait for a change; otherwise use interval
        if opts.file_path.is_some() {
            let timeout = tokio::time::Duration::from_secs(600); // 10 min fallback
            tokio::select! {
                _ = tokio::time::sleep(timeout) => {
                    // Timeout — just re-run on interval as fallback
                }
                _ = watch_rx.recv() => {
                    // File changed — debounce slightly
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
                _ = tokio::signal::ctrl_c() => {
                    let _guard = opts.print_lock.lock();
                    println!("\n[watch] stopped.");
                    break;
                }
            }
        } else {
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_secs(opts.interval_secs.load(Ordering::Relaxed))) => {}
                _ = tokio::signal::ctrl_c() => {
                    let _guard = opts.print_lock.lock();
                    println!("\n[watch] stopped.");
                    break;
                }
            }
        }

        let now = chrono::Local::now();
        let header = format!(
            "─── Iteration {}   {}   ({}s interval) ───",
            iteration,
            now.format("%Y-%m-%d %H:%M:%S %Z"),
            opts.interval_secs.load(Ordering::Relaxed),
        );

        // Re-read SQL from file if watching a file
        let sql = if let Some(ref path) = opts.file_path {
            match tokio::fs::read_to_string(path).await {
                Ok(s) => s,
                Err(e) => {
                    let _guard = opts.print_lock.lock();
                    eprintln!("[watch] read file error: {e}");
                    tokio::time::sleep(Duration::from_secs(1)).await;
                    continue;
                }
            }
        } else {
            opts.sql.clone()
        };

        let trimmed = sql.trim().trim_end_matches(';').trim();
        if trimmed.is_empty() {
            let _guard = opts.print_lock.lock();
            eprintln!("[watch] SQL is empty, skipping.");
            tokio::time::sleep(Duration::from_secs(1)).await;
            continue;
        }

        // Resolve connection each iteration. SSH tunnels are spun
        // up fresh per poll: the previous iteration's session and
        // forwarder were dropped when the previous `conn` went out
        // of scope.
        let resolved = match resolve_connection(
            &opts.connection,
            None,
            opts.conn_flags.ssh_tunnel.as_deref(),
            opts.conn_flags.ssh_key.as_deref(),
            opts.conn_flags.proxy_url.as_deref(),
            &opts.global_config,
        )
        .await
        {
            Ok(r) => r,
            Err(e) => {
                let _guard = opts.print_lock.lock();
                eprintln!("[watch] connection error: {e}");
                if opts.exit_on_error {
                    return Err(CliError::query(ferrule_sql::SqlError::QueryFailed(
                        format!("watch connection error: {e}"),
                    )));
                }
                tokio::time::sleep(Duration::from_secs(1)).await;
                continue;
            }
        };
        if let Err(e) = check_daemon_ssh_compat(opts.conn_flags.daemon, &resolved) {
            let _guard = opts.print_lock.lock();
            eprintln!("[watch] {e}");
            if opts.exit_on_error {
                return Err(CliError::query(ferrule_sql::SqlError::QueryFailed(
                    format!("watch daemon/ssh compatibility: {e}"),
                )));
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
            continue;
        }

        let backend = match ferrule_sql::Backend::from_scheme(resolved.url.scheme()) {
            Some(b) => b,
            None => {
                let _guard = opts.print_lock.lock();
                eprintln!("[watch] unsupported scheme: {}", resolved.url.scheme());
                if opts.exit_on_error {
                    return Err(CliError::usage(format!(
                        "unsupported scheme: {}",
                        resolved.url.scheme()
                    )));
                }
                tokio::time::sleep(Duration::from_secs(1)).await;
                continue;
            }
        };

        let sql = match ferrule_sql::apply_paging(&sql, opts.limit, opts.offset, backend) {
            Ok(s) => s,
            Err(e) => {
                let _guard = opts.print_lock.lock();
                eprintln!("[watch] paging error: {e}");
                if opts.exit_on_error {
                    return Err(CliError::query(ferrule_sql::SqlError::QueryFailed(
                        format!("watch paging error: {e}"),
                    )));
                }
                tokio::time::sleep(Duration::from_secs(1)).await;
                continue;
            }
        };

        if opts.verbose {
            let _guard = opts.print_lock.lock();
            eprintln!("[watch] SQL: {sql}");
        }

        let conn_start = Instant::now();
        let mut conn = match connect_resolved(
            resolved,
            &ConnectOptions {
                insecure: opts.conn_flags.insecure,
            },
        )
        .await
        {
            Ok(c) => c,
            Err(e) => {
                let _guard = opts.print_lock.lock();
                eprintln!("[watch] connection failed: {e}");
                if opts.exit_on_error {
                    return Err(CliError::query(ferrule_sql::SqlError::QueryFailed(
                        format!("watch connection failed: {e}"),
                    )));
                }
                tokio::time::sleep(Duration::from_secs(1)).await;
                continue;
            }
        };
        let conn_time = conn_start.elapsed();

        let query_start = Instant::now();
        let results = match conn.query(&sql).await {
            Ok(qr) => vec![StatementResult::Query(qr)],
            Err(ferrule_sql::SqlError::QueryFailed(_)) => match conn.execute(&sql).await {
                Ok(summary) => vec![StatementResult::Summary(summary)],
                Err(_) => match conn.execute_multi(&sql).await {
                    Ok(r) => r,
                    Err(e) => {
                        let _guard = opts.print_lock.lock();
                        eprintln!("[watch] query error: {e}");
                        if opts.exit_on_error {
                            return Err(CliError::query(ferrule_sql::SqlError::QueryFailed(
                                format!("watch query error: {e}"),
                            )));
                        }
                        tokio::time::sleep(Duration::from_secs(1)).await;
                        continue;
                    }
                },
            },
            Err(e) => {
                let _guard = opts.print_lock.lock();
                eprintln!("[watch] query error: {e}");
                if opts.exit_on_error {
                    return Err(CliError::query(ferrule_sql::SqlError::QueryFailed(
                        format!("watch query error: {e}"),
                    )));
                }
                tokio::time::sleep(Duration::from_secs(1)).await;
                continue;
            }
        };
        let query_time = query_start.elapsed();

        // Render output
        let mut output = String::new();
        if results.len() == 1 {
            let rendered =
                match render_single_result(&results[0], opts.format, opts.limit, opts.offset) {
                    Ok(s) => s,
                    Err(e) => format!("Render error: {e}"),
                };
            output.push_str(&rendered);
        } else {
            for (i, result) in results.iter().enumerate() {
                if i > 0 {
                    output.push('\n');
                }
                match result {
                    StatementResult::Query(_) => {
                        let rendered = match render_single_result(
                            result,
                            opts.format,
                            opts.limit,
                            opts.offset,
                        ) {
                            Ok(s) => s,
                            Err(e) => format!("Render error: {e}"),
                        };
                        output.push_str(&format!("-- Result set {}\n", i + 1));
                        output.push_str(&rendered);
                        output.push('\n');
                    }
                    StatementResult::Summary(s) => {
                        output.push_str(&format!(
                            "-- Statement {}: {} rows affected\n",
                            i + 1,
                            s.rows_affected.unwrap_or(0)
                        ));
                    }
                }
            }
        }

        {
            let _guard = opts.print_lock.lock();
            if is_tty {
                use crossterm::cursor::MoveTo;
                use crossterm::execute;
                use crossterm::terminal::{Clear, ClearType};
                let _ = execute!(std::io::stdout(), Clear(ClearType::All));
                let _ = execute!(std::io::stdout(), MoveTo(0, 0));
            }
            println!("{header}");

            if opts.diff {
                if Some(&output) != prev_output.as_ref() {
                    println!("{output}");
                    if opts.bell {
                        let _ = std::io::stdout().write_all(b"\x07");
                        let _ = std::io::stdout().flush();
                    }
                }
            } else {
                println!("{output}");
            }

            if opts.timing {
                eprintln!(
                    "[watch] timing: connect={:.3}s query={:.3}s",
                    conn_time.as_secs_f64(),
                    query_time.as_secs_f64(),
                );
            }
        }

        prev_output = Some(output);
    }

    Ok(())
}

fn render_single_result(
    result: &StatementResult,
    format: OutputFormat,
    limit: Option<usize>,
    offset: Option<usize>,
) -> Result<String, CliError> {
    match result {
        StatementResult::Query(qr) => {
            let mut qr = qr.clone();
            if let Some(off) = offset {
                if off >= qr.rows.len() {
                    qr.rows.clear();
                } else {
                    qr.rows = qr.rows.split_off(off);
                }
            }
            if let Some(n) = limit {
                if qr.rows.len() > n {
                    qr.rows.truncate(n);
                }
            }
            format_result(&qr, format).map_err(CliError::query)
        }
        StatementResult::Summary(s) => {
            Ok(format!("{} rows affected", s.rows_affected.unwrap_or(0)))
        }
    }
}
