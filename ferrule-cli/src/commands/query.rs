#![allow(dead_code, unused_variables, unused_imports)]

use super::QueryArgs;
use crate::error::CliError;
use ferrule_core::backend::connect;
use ferrule_core::connection::{ConnectOptions, ExecutionSummary, QueryResult, StatementResult};
use ferrule_core::formatter::{OutputFormat, format_result};

fn render_query_result(
    result: &QueryResult,
    format: OutputFormat,
    limit: Option<usize>,
) -> Result<String, CliError> {
    let mut qr = result.clone();
    if let Some(n) = limit {
        if qr.rows.len() > n {
            qr.rows.truncate(n);
        }
    }
    format_result(&qr, format).map_err(CliError::query)
}

fn render_single_result(
    result: &StatementResult,
    format: OutputFormat,
    limit: Option<usize>,
) -> Result<String, CliError> {
    match result {
        StatementResult::Query(qr) => render_query_result(qr, format, limit),
        StatementResult::Summary(s) => {
            Ok(format!("{} rows affected", s.rows_affected.unwrap_or(0)))
        }
    }
}

pub async fn run(args: QueryArgs) -> Result<(), CliError> {
    let format = args
        .output
        .format
        .as_deref()
        .and_then(OutputFormat::parse)
        .unwrap_or_else(crate::output::default_format);

    let total_start = std::time::Instant::now();

    let sql = if let Some(path) = args.file {
        tokio::fs::read_to_string(path)
            .await
            .map_err(CliError::Io)?
    } else if args.stdin {
        let mut buf = String::new();
        tokio::io::AsyncReadExt::read_to_string(&mut tokio::io::stdin(), &mut buf)
            .await
            .map_err(CliError::Io)?;
        buf
    } else if let Some(sql) = args.sql {
        sql
    } else {
        return Err(CliError::usage(
            "No SQL provided. Use positional argument, --file, or --stdin.",
        ));
    };

    if args.output.verbose {
        eprintln!("[ferrule] SQL: {}", sql);
    }

    if args.dry_run {
        println!("-- Dry run");
        println!("-- Connection: {}", args.connection);
        println!("{}", sql);
        return Ok(());
    }

    let url = super::resolve_connection(&args.connection, args.password).await?;

    if args.output.verbose {
        eprintln!("[ferrule] Resolved URL: {}", url.redacted());
    }

    let opts = ConnectOptions {
        insecure: args.conn_flags.insecure,
    };
    if opts.insecure {
        eprintln!("Warning: --insecure disables TLS certificate verification.");
    }

    let conn_start = std::time::Instant::now();
    let mut conn = connect(&url, &opts).await.map_err(CliError::connection)?;
    let conn_time = conn_start.elapsed();

    // Strategy:
    // 1. Try query() first — gives typed values via extended protocol.
    // 2. If that fails, try execute() — for DML that returns no rows.
    // 3. If both fail, try execute_multi() — handles multi-statement batches.
    let query_start = std::time::Instant::now();
    let results = match conn.query(&sql).await {
        Ok(qr) => vec![StatementResult::Query(qr)],
        Err(ferrule_core::CoreError::QueryFailed(_)) => {
            match conn.execute(&sql).await {
                Ok(summary) => vec![StatementResult::Summary(summary)],
                Err(_) => {
                    conn.execute_multi(&sql)
                        .await
                        .map_err(CliError::query)?
                }
            }
        }
        Err(e) => return Err(CliError::query(e)),
    };
    let query_time = query_start.elapsed();

    let format_start = std::time::Instant::now();

    if results.len() == 1 {
        let rendered = render_single_result(&results[0],
            format,
            args.output.limit,
        )?;
        match &results[0] {
            StatementResult::Query(_) => println!("{}", rendered),
            StatementResult::Summary(_) => eprintln!("{}", rendered),
        }
    } else {
        for (i, result) in results.iter().enumerate() {
            match result {
                StatementResult::Query(_) => {
                    let rendered = render_single_result(
                        result, format, args.output.limit)?;
                    println!("-- Result set {}\n", i + 1);
                    println!("{}", rendered);
                    println!();
                }
                StatementResult::Summary(s) => {
                    eprintln!(
                        "-- Statement {}: {} rows affected\n",
                        i + 1,
                        s.rows_affected.unwrap_or(0)
                    );
                }
            }
        }
    }

    let format_time = format_start.elapsed();

    if let Some(path) = args.output.output {
        eprintln!("Warning: file output with multi-statement uses first result only.");
        let rendered = render_single_result(
            &results[0], format, args.output.limit)?;
        tokio::fs::write(&path, rendered)
            .await
            .map_err(CliError::Io)?;
        eprintln!("Wrote to {}", path);
    }

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
