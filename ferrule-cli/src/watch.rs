use crate::commands::{resolve_connection, ConnectionFlags};
use crate::error::CliError;
use ferrule_config::profile::GlobalConfig;
use ferrule_core::backend::connect;
use ferrule_core::connection::{ConnectOptions, StatementResult};
use ferrule_core::formatter::{format_result, OutputFormat};
use is_terminal::IsTerminal;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::time::Instant;

/// Options that drive a single watch loop.
pub struct WatchOptions {
    pub connection: String,
    pub sql: String,
    pub interval_secs: Arc<AtomicU64>,
    pub max_iterations: Option<u64>,
    pub diff: bool,
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

    while running.load(Ordering::Relaxed) {
        iteration += 1;

        if let Some(max) = opts.max_iterations {
            if iteration > max {
                break;
            }
        }

        let now = chrono::Local::now();
        let header = format!(
            "─── Iteration {}   {}   ({}s interval) ───",
            iteration,
            now.format("%Y-%m-%d %H:%M:%S %Z"),
            opts.interval_secs.load(Ordering::Relaxed),
        );

        // Resolve connection each iteration
        let url = match resolve_connection(&opts.connection, None, &opts.global_config).await {
            Ok(u) => u,
            Err(e) => {
                let _guard = opts.print_lock.lock();
                eprintln!("[watch] connection error: {e}");
                tokio::time::sleep(Duration::from_secs(1)).await;
                continue;
            }
        };

        let backend = match ferrule_core::Backend::from_scheme(url.scheme()) {
            Some(b) => b,
            None => {
                let _guard = opts.print_lock.lock();
                eprintln!("[watch] unsupported scheme: {}", url.scheme());
                tokio::time::sleep(Duration::from_secs(1)).await;
                continue;
            }
        };

        let sql = match ferrule_core::apply_paging(&opts.sql, opts.limit, opts.offset, backend) {
            Ok(s) => s,
            Err(e) => {
                let _guard = opts.print_lock.lock();
                eprintln!("[watch] paging error: {e}");
                tokio::time::sleep(Duration::from_secs(1)).await;
                continue;
            }
        };

        if opts.verbose {
            let _guard = opts.print_lock.lock();
            eprintln!("[watch] SQL: {sql}");
        }

        let conn_start = Instant::now();
        let mut conn = match connect(
            &url,
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
                tokio::time::sleep(Duration::from_secs(1)).await;
                continue;
            }
        };
        let conn_time = conn_start.elapsed();

        let query_start = Instant::now();
        let results = match conn.query(&sql).await {
            Ok(qr) => vec![StatementResult::Query(qr)],
            Err(ferrule_core::CoreError::QueryFailed(_)) => match conn.execute(&sql).await {
                Ok(summary) => vec![StatementResult::Summary(summary)],
                Err(_) => match conn.execute_multi(&sql).await {
                    Ok(r) => r,
                    Err(e) => {
                        let _guard = opts.print_lock.lock();
                        eprintln!("[watch] query error: {e}");
                        tokio::time::sleep(Duration::from_secs(1)).await;
                        continue;
                    }
                },
            },
            Err(e) => {
                let _guard = opts.print_lock.lock();
                eprintln!("[watch] query error: {e}");
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

        let interval_secs = opts.interval_secs.load(Ordering::Relaxed);
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_secs(interval_secs)) => {}
            _ = tokio::signal::ctrl_c() => {
                let _guard = opts.print_lock.lock();
                println!("\n[watch] stopped.");
                break;
            }
        }
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
