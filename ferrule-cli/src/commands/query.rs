use super::{QueryArgs, WatchArgs};
use crate::bench::BenchSummary;
use crate::error::CliError;
use ferrule_config::profile::GlobalConfig;
use ferrule_core::backend::Backend;
use ferrule_core::connection::{ConnectOptions, Connection, QueryResult, StatementResult};
use ferrule_core::explain::{explain_sql, is_modifying, ExplainOutput};
use ferrule_core::formatter::{format_result, OutputFormat};
use ferrule_core::{infer_type, parse_param, substitute, ParameterSet};

/// Hints threaded into `run_bench` so the bench loop runs inside the
/// same wrapping transaction as the script-mode path. `begin` is the
/// caller's `outer_tx_opened` (already accounts for `--begin` and the
/// best-effort BEGIN result); `rollback` mirrors the CLI flag.
struct BenchTxnHints {
    /// Caller already issued BEGIN and the txn is open.
    begin: bool,
    /// Force ROLLBACK at end of bench instead of COMMIT.
    rollback: bool,
    backend: Backend,
}

/// Apply an optional JMESPath filter to a rendered JSON string.
///
/// When `filter` is `None` the input string is returned unchanged. When set,
/// the string is parsed, filtered, and re-serialized (pretty). Any JMESPath
/// or JSON error is mapped to `CliError::Query` so the binary exits with
/// code 3 — the same class as a SQL execution failure.
fn maybe_apply_filter(rendered: String, filter: Option<&str>) -> Result<String, CliError> {
    let Some(expr) = filter else {
        return Ok(rendered);
    };
    let parsed: serde_json::Value = serde_json::from_str(&rendered).map_err(|e| {
        CliError::query(ferrule_core::CoreError::QueryFailed(format!(
            "filter expects JSON output but rendered output is not valid JSON: {e}"
        )))
    })?;
    let filtered = crate::output::apply_filter(parsed, expr)
        .map_err(|e| CliError::query(ferrule_core::CoreError::QueryFailed(e)))?;
    serde_json::to_string_pretty(&filtered)
        .map_err(|e| CliError::query(ferrule_core::CoreError::QueryFailed(e.to_string())))
}

fn render_query_result(
    result: &QueryResult,
    format: OutputFormat,
    limit: Option<usize>,
    offset: Option<usize>,
) -> Result<String, CliError> {
    let mut qr = result.clone();

    // Apply client-side offset
    if let Some(off) = offset {
        if off >= qr.rows.len() {
            qr.rows.clear();
        } else {
            qr.rows = qr.rows.split_off(off);
        }
    }

    // Apply client-side limit
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
    offset: Option<usize>,
) -> Result<String, CliError> {
    match result {
        StatementResult::Query(qr) => render_query_result(qr, format, limit, offset),
        StatementResult::Summary(s) => {
            Ok(format!("{} rows affected", s.rows_affected.unwrap_or(0)))
        }
    }
}

fn print_explain_payload(payload: &str, out: ExplainOutput) {
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
        _ => println!("{}", payload),
    }
}

async fn run_bench(
    conn: &mut dyn Connection,
    sql: &str,
    n: u32,
    warmup: u32,
    csv_output: Option<&str>,
    txn: BenchTxnHints,
) -> Result<(), CliError> {
    if n == 0 {
        return Err(CliError::usage("--bench N requires N >= 1"));
    }
    let mut summary = BenchSummary::new(warmup as usize);
    for i in 0..(warmup + n) {
        let start = std::time::Instant::now();
        // Mirror the regular dispatch triage so DML and SELECT both work.
        let iter_result: Result<(), CliError> = match conn.query(sql).await {
            Ok(_) => Ok(()),
            Err(ferrule_core::CoreError::QueryFailed(_)) => match conn.execute(sql).await {
                Ok(_) => Ok(()),
                Err(_) => conn
                    .execute_multi(sql)
                    .await
                    .map(|_| ())
                    .map_err(CliError::query),
            },
            Err(e) => Err(CliError::query(e)),
        };
        if let Err(e) = iter_result {
            if txn.begin {
                let _ =
                    ferrule_core::transaction::rollback_transaction(conn, txn.backend).await;
                eprintln!("[ferrule] inner statement failed — rolled back wrapping transaction");
            }
            return Err(e);
        }
        let elapsed = start.elapsed();
        if i >= warmup {
            summary.push(elapsed);
        }
    }

    let width = crossterm::terminal::size()
        .map(|(c, _)| (c.saturating_sub(28) as usize).max(20))
        .unwrap_or(40);
    print!("{}", summary.render(width));

    if let Some(path) = csv_output {
        tokio::fs::write(path, summary.to_csv())
            .await
            .map_err(CliError::Io)?;
        eprintln!("Wrote {} samples to {}", summary.n(), path);
    }

    // Stash the rollup in a thread-local that the dispatch hook in
    // main.rs reads off the end of the run() so the history table shows
    // one record per bench, not N. This avoids threading a return-value
    // channel through every command's signature.
    crate::bench::record_last(summary.history_sql(sql), summary.n() as i64);

    if txn.begin {
        if txn.rollback {
            let _ = ferrule_core::transaction::rollback_transaction(conn, txn.backend).await;
            eprintln!("[ferrule] explicit ROLLBACK (--rollback)");
        } else {
            ferrule_core::transaction::commit_transaction(conn, txn.backend)
                .await
                .map_err(CliError::query)?;
        }
    }

    Ok(())
}

pub async fn run(args: QueryArgs, global_config: &GlobalConfig) -> Result<(), CliError> {
    // Validate --filter precondition before resolving format.
    // Filter operates on JSON, so it implies --format json.
    let format = if args.filter.is_some() {
        match args.output.format.as_deref() {
            None => OutputFormat::Json,
            Some(s) if s.eq_ignore_ascii_case("json") => OutputFormat::Json,
            Some(other) => {
                return Err(CliError::usage(format!(
                    "--filter requires --format json (got --format {other})"
                )));
            }
        }
    } else {
        args.output.resolve_format(global_config)
    };

    if args.filter.is_some() && args.explain {
        return Err(CliError::usage(
            "--filter cannot be combined with --explain.",
        ));
    }

    if args.begin && args.conn_flags.daemon {
        return Err(CliError::usage(
            "--begin cannot be combined with --daemon (the daemon path does not guarantee transaction affinity).",
        ));
    }
    if args.begin && (args.watch || args.watch_file.is_some()) {
        return Err(CliError::usage(
            "--begin cannot be combined with --watch (each tick reopens the transaction).",
        ));
    }

    if args.fail_on_empty {
        if args.explain {
            return Err(CliError::usage(
                "--fail-on-empty cannot be combined with --explain.",
            ));
        }
        if args.conn_flags.daemon {
            return Err(CliError::usage(
                "--fail-on-empty cannot be combined with --daemon (the daemon path returns a pre-rendered payload).",
            ));
        }
        if args.watch || args.watch_file.is_some() {
            return Err(CliError::usage(
                "--fail-on-empty cannot be combined with --watch.",
            ));
        }
        if args.bench.is_some() {
            return Err(CliError::usage(
                "--fail-on-empty cannot be combined with --bench.",
            ));
        }
    }

    let limit = args.output.resolve_limit(global_config);
    let offset = args.output.offset;

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

    // Build parameter set from --param-file and --param
    let mut param_set = ParameterSet::default();
    if let Some(ref path) = args.param_file {
        let path = std::path::Path::new(path);
        let file_set = ferrule_core::load_from_json(path).map_err(CliError::query)?;
        for (k, v) in file_set.map {
            param_set.set(k, v);
        }
    }
    for p in &args.params {
        let (name, value) = parse_param(p).map_err(CliError::query)?;
        param_set.set(name, infer_type(&value));
    }

    if let Some(file_path) = args.watch_file {
        let watch_args = WatchArgs {
            connection: args.connection,
            sql,
            interval: args.watch_interval,
            max_iterations: None,
            diff: false,
            exit_on_error: false,
            bell: false,
            output: args.output,
            conn_flags: args.conn_flags,
            password: args.password,
            file_path: Some(file_path),
        };
        return crate::commands::watch::run(watch_args, global_config).await;
    }

    if args.watch {
        let watch_args = WatchArgs {
            connection: args.connection,
            sql,
            interval: args.watch_interval,
            max_iterations: None,
            diff: false,
            exit_on_error: false,
            bell: false,
            output: args.output,
            conn_flags: args.conn_flags,
            password: args.password,
            file_path: None,
        };
        return crate::commands::watch::run(watch_args, global_config).await;
    }

    if args.dry_run {
        println!("-- Dry run");
        println!("-- Connection: {}", args.connection);
        println!("{}", sql);
        return Ok(());
    }

    let resolved = super::resolve_connection(
        &args.connection,
        args.password,
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

    let backend = ferrule_core::Backend::from_scheme(resolved.url.scheme())
        .ok_or_else(|| CliError::usage(format!("Unsupported scheme: {}", resolved.url.scheme())))?;

    // Substitute parameters into SQL before paging
    let sql = substitute(&sql, &param_set, backend).map_err(CliError::query)?;

    if args.output.verbose && !param_set.map.is_empty() {
        eprintln!("[ferrule] substituted SQL: {}", sql);
    }

    if args.explain {
        let (wrapped, out, _is_multi) =
            explain_sql(&sql, backend, false).map_err(CliError::query)?;
        if is_modifying(&sql) {
            eprintln!(
                "Warning: EXPLAIN on modifying statement uses estimated plan (ANALYZE disabled)."
            );
        }
        if args.conn_flags.daemon {
            eprintln!("[ferrule] Routing via daemon...");
            let payload = crate::daemon::daemon_query(
                &wrapped,
                &resolved.url,
                args.conn_flags.insecure,
                format,
                None,
                None,
            )
            .await?;
            print_explain_payload(&payload, out);
            return Ok(());
        }
        let opts = ConnectOptions {
            insecure: args.conn_flags.insecure,
        };
        let mut conn = super::connect_resolved(resolved, &opts).await?;
        let result = conn.query(&wrapped).await.map_err(CliError::query)?;
        let rendered = format_result(&result, format).map_err(CliError::query)?;
        print_explain_payload(&rendered, out);
        return Ok(());
    }

    // Route through daemon if requested
    if args.conn_flags.daemon {
        eprintln!("[ferrule] Routing via daemon...");
        let payload = crate::daemon::daemon_query(
            &sql,
            &resolved.url,
            args.conn_flags.insecure,
            format,
            limit,
            offset,
        )
        .await?;
        let payload = maybe_apply_filter(payload, args.filter.as_deref())?;
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

    // Wrap the entire statement batch in a single outer transaction when
    // `--begin` is set. Oracle's begin is a noop (implicit txn) but
    // still returns `true` so the wrapping COMMIT/ROLLBACK terminates
    // the implicit transaction.
    let outer_tx_opened = if args.begin {
        ferrule_core::transaction::begin_transaction(&mut *conn, backend).await
    } else {
        false
    };

    // Inject server-side paging into the SQL
    let sql = ferrule_core::apply_paging(&sql, limit, offset, backend).map_err(CliError::query)?;

    if (limit.is_some() || offset.is_some()) && args.output.verbose {
        eprintln!("[ferrule] Paged SQL: {}", sql);
    }

    // --bench mode: loop the existing conn.query()/execute() triage N+K
    // times, drop K warmup samples, render histogram. Connect cost was
    // taken above, outside the loop. One RunRecord is emitted by the
    // dispatch hook at the end — not N.
    if let Some(n) = args.bench {
        return run_bench(
            &mut *conn,
            &sql,
            n,
            args.bench_warmup,
            args.bench_output.as_deref(),
            BenchTxnHints {
                begin: outer_tx_opened,
                rollback: args.rollback,
                backend,
            },
        )
        .await;
    }

    let query_start = std::time::Instant::now();
    let dispatch_result: Result<Vec<StatementResult>, CliError> = match conn.query(&sql).await {
        Ok(qr) => Ok(vec![StatementResult::Query(qr)]),
        Err(ferrule_core::CoreError::QueryFailed(_)) => match conn.execute(&sql).await {
            Ok(summary) => Ok(vec![StatementResult::Summary(summary)]),
            Err(_) => conn.execute_multi(&sql).await.map_err(CliError::query),
        },
        Err(e) => Err(CliError::query(e)),
    };

    // If inner failed AND we opened the wrapping transaction, best-effort
    // roll back before surfacing the original error.
    let results = match dispatch_result {
        Ok(r) => r,
        Err(e) => {
            if outer_tx_opened {
                let _ =
                    ferrule_core::transaction::rollback_transaction(&mut *conn, backend).await;
                eprintln!("[ferrule] inner statement failed — rolled back wrapping transaction");
            }
            return Err(e);
        }
    };
    let query_time = query_start.elapsed();

    let format_start = std::time::Instant::now();

    if results.len() == 1 {
        let rendered = render_single_result(&results[0], format, limit, offset)?;
        match &results[0] {
            StatementResult::Query(_) => {
                let filtered = maybe_apply_filter(rendered, args.filter.as_deref())?;
                println!("{}", filtered);
            }
            StatementResult::Summary(_) => {
                if args.filter.is_some() {
                    return Err(CliError::usage(
                        "--filter requires a SELECT-style query that returns rows.",
                    ));
                }
                eprintln!("{}", rendered);
            }
        }
    } else {
        if args.filter.is_some() {
            return Err(CliError::usage(
                "--filter cannot be applied to multi-statement queries.",
            ));
        }
        for (i, result) in results.iter().enumerate() {
            match result {
                StatementResult::Query(_) => {
                    let rendered = render_single_result(result, format, limit, offset)?;
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
        let rendered = render_single_result(&results[0], format, limit, offset)?;
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

    if outer_tx_opened {
        if args.rollback {
            let _ = ferrule_core::transaction::rollback_transaction(&mut *conn, backend).await;
            eprintln!("[ferrule] explicit ROLLBACK (--rollback)");
        } else {
            ferrule_core::transaction::commit_transaction(&mut *conn, backend)
                .await
                .map_err(CliError::query)?;
        }
    }

    if args.fail_on_empty {
        check_fail_on_empty(&results)?;
    }

    Ok(())
}

/// `--fail-on-empty`: gate the exit code on row count without aborting
/// the print path. Returns the `ResultNotable` variant (exit 1) when
/// no rows came back; multi-statement batches gate on the first SELECT
/// result. DML-only batches are a usage error.
fn check_fail_on_empty(results: &[StatementResult]) -> Result<(), CliError> {
    for r in results {
        if let StatementResult::Query(qr) = r {
            return if qr.rows.is_empty() {
                Err(CliError::result_notable(
                    "query returned no rows (--fail-on-empty)",
                ))
            } else {
                Ok(())
            };
        }
    }
    Err(CliError::usage(
        "--fail-on-empty requires a query that returns rows; got DML/DDL only.",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ferrule_core::connection::ExecutionSummary;
    use ferrule_core::value::{ColumnInfo, TypeHint, Value};

    fn col(name: &str) -> ColumnInfo {
        ColumnInfo {
            name: name.into(),
            type_hint: TypeHint::Int64,
            nullable: false,
        }
    }

    #[test]
    fn fail_on_empty_zero_rows_returns_notable() {
        let qr = QueryResult {
            columns: vec![col("c")],
            rows: vec![],
        };
        let err = check_fail_on_empty(&[StatementResult::Query(qr)]).unwrap_err();
        assert!(matches!(err, CliError::ResultNotable(_)));
        assert_eq!(err.exit_code(), 1);
    }

    #[test]
    fn fail_on_empty_with_rows_returns_ok() {
        let qr = QueryResult {
            columns: vec![col("c")],
            rows: vec![vec![Value::Int64(1)]],
        };
        assert!(check_fail_on_empty(&[StatementResult::Query(qr)]).is_ok());
    }

    #[test]
    fn fail_on_empty_dml_only_is_usage_error() {
        let err = check_fail_on_empty(&[StatementResult::Summary(ExecutionSummary {
            rows_affected: Some(5),
            command_tag: None,
        })])
        .unwrap_err();
        assert!(matches!(err, CliError::Usage(_)));
        assert_eq!(err.exit_code(), 2);
    }

    /// Parse `["query", ...]` style argv into [`QueryArgs`] for clap
    /// validation tests. Mirrors how `Commands::Query(QueryArgs)` in
    /// `main.rs` flattens the subcommand, but stays inside this crate.
    #[derive(clap::Parser, Debug)]
    struct TestQueryCli {
        #[command(flatten)]
        args: QueryArgs,
    }

    #[test]
    fn commit_without_begin_clap_rejects() {
        use clap::Parser;
        let err = TestQueryCli::try_parse_from(["query", "sqlite://x", "SELECT 1", "--commit"])
            .expect_err("--commit alone must require --begin");
        assert_eq!(
            err.kind(),
            clap::error::ErrorKind::MissingRequiredArgument,
            "expected MissingRequiredArgument, got {:?}",
            err.kind()
        );
    }

    #[test]
    fn commit_with_rollback_clap_rejects() {
        use clap::Parser;
        let err = TestQueryCli::try_parse_from([
            "query",
            "sqlite://x",
            "SELECT 1",
            "--begin",
            "--commit",
            "--rollback",
        ])
        .expect_err("--commit and --rollback together must conflict");
        assert_eq!(
            err.kind(),
            clap::error::ErrorKind::ArgumentConflict,
            "expected ArgumentConflict, got {:?}",
            err.kind()
        );
    }

    fn synth_query_args(connection: &str, sql: &str) -> QueryArgs {
        QueryArgs {
            connection: connection.into(),
            sql: Some(sql.into()),
            file: None,
            stdin: false,
            params: Vec::new(),
            param_file: None,
            explain: false,
            output: super::super::OutputFlags {
                format: None,
                output: None,
                limit: None,
                offset: None,
                timing: false,
                verbose: false,
            },
            conn_flags: super::super::ConnectionFlags {
                insecure: false,
                daemon: false,
                ssh_tunnel: None,
                ssh_key: None,
                proxy_url: None,
            },
            password: None,
            filter: None,
            dry_run: false,
            watch_file: None,
            watch: false,
            watch_interval: 5,
            bench: None,
            bench_warmup: 1,
            bench_output: None,
            fail_on_empty: false,
            begin: false,
            commit: false,
            rollback: false,
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn begin_with_daemon_usage_error() {
        let mut args = synth_query_args("sqlite://x", "SELECT 1");
        args.begin = true;
        args.conn_flags.daemon = true;
        let global = GlobalConfig::default();
        let err = run(args, &global)
            .await
            .expect_err("--begin --daemon should be a usage error");
        assert!(matches!(err, CliError::Usage(_)), "got: {:?}", err);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn begin_with_watch_usage_error() {
        let mut args = synth_query_args("sqlite://x", "SELECT 1");
        args.begin = true;
        args.watch = true;
        let global = GlobalConfig::default();
        let err = run(args, &global)
            .await
            .expect_err("--begin --watch should be a usage error");
        assert!(matches!(err, CliError::Usage(_)), "got: {:?}", err);
    }

    #[test]
    fn fail_on_empty_multi_statement_gates_on_first_select() {
        let qr_empty = QueryResult {
            columns: vec![col("c")],
            rows: vec![],
        };
        let qr_nonempty = QueryResult {
            columns: vec![col("c")],
            rows: vec![vec![Value::Int64(1)]],
        };
        // First SELECT is empty → notable regardless of later statements.
        let err = check_fail_on_empty(&[
            StatementResult::Summary(ExecutionSummary {
                rows_affected: Some(1),
                command_tag: None,
            }),
            StatementResult::Query(qr_empty),
            StatementResult::Query(qr_nonempty.clone()),
        ])
        .unwrap_err();
        assert!(matches!(err, CliError::ResultNotable(_)));

        // First SELECT is non-empty → ok.
        assert!(check_fail_on_empty(&[
            StatementResult::Query(qr_nonempty),
            StatementResult::Summary(ExecutionSummary {
                rows_affected: Some(0),
                command_tag: None,
            }),
        ])
        .is_ok());
    }
}
