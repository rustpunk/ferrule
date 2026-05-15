use super::{QueryArgs, WatchArgs};
use crate::bench::BenchSummary;
use crate::cache::{self, CacheDb, CacheKey};
use crate::error::CliError;
use ferrule_config::profile::GlobalConfig;
use ferrule_core::backend::Backend;
use ferrule_core::connection::{ConnectOptions, Connection, QueryResult, StatementResult};
use ferrule_core::explain::{explain_sql, is_modifying, ExplainOutput};
use ferrule_core::formatter::{format_result, OutputFormat};
use ferrule_core::{infer_type, parse_param, substitute, ParameterSet};
use std::time::Duration;

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

    // ----- Result cache (R5 / #5) -----
    //
    // Lookup BEFORE `connect_resolved` so a cached hit can serve users
    // even when the database itself is unavailable. Bypass rules cover
    // every CLI flag that conflicts with the cache contract (bench,
    // explain, watch, dry_run, daemon, modifying SQL, --no-cache, or
    // `default_ttl = "0"` / empty). Failures here NEVER block the
    // user's query — every error is swallowed at the dispatch
    // boundary and surfaced only under `--verbose`.
    let cache_bypass = args.no_cache
        || args.bench.is_some()
        || args.explain
        || args.watch
        || args.watch_file.is_some()
        || args.dry_run
        || args.conn_flags.daemon
        || is_modifying(&sql)
        || (args.cache.is_none()
            && (global_config.cache.default_ttl == "0"
                || global_config.cache.default_ttl.is_empty()));
    // Split into two `Option`s rather than a single tuple because
    // lookup needs `&CacheDb` and insert needs `&mut CacheDb`; threading
    // a tuple would force a borrow split at every call site.
    let (mut cache_db, cache_key): (Option<CacheDb>, Option<CacheKey>) = if cache_bypass {
        (None, None)
    } else {
        match CacheDb::maybe_open(&global_config.cache) {
            Ok(Some(db)) => (
                Some(db),
                Some(cache::cache_key(&args.connection, &sql, &param_set)),
            ),
            // Cache disabled (env kill switch or config), OR open
            // failed — either way, fall through to the real query.
            Ok(None) => (None, None),
            Err(_) => {
                if args.output.verbose {
                    eprintln!("[ferrule] cache: open error (continuing without cache)");
                }
                (None, None)
            }
        }
    };
    if let (Some(db), Some(key)) = (cache_db.as_ref(), cache_key.as_ref()) {
        let t = std::time::Instant::now();
        match db.lookup(key) {
            Ok(Some(cached)) => {
                let lookup_micros = t.elapsed().as_micros() as u64;
                let rendered =
                    render_query_result(&cached.result, format, limit, offset)?;
                let filtered = maybe_apply_filter(rendered, args.filter.as_deref())?;
                println!("{}", filtered);
                if let Some(path) = args.output.output.as_deref() {
                    let again =
                        render_query_result(&cached.result, format, limit, offset)?;
                    tokio::fs::write(path, again).await.map_err(CliError::Io)?;
                    eprintln!("Wrote to {}", path);
                }
                cache::record_last(cache::CacheHitInfo {
                    hit: true,
                    key: key.0.clone(),
                    lookup_micros,
                });
                if args.output.verbose {
                    eprintln!(
                        "[ferrule] cache hit (key={}\u{2026}, age={}s)",
                        &key.0[..8.min(key.0.len())],
                        cached.age_secs
                    );
                }
                if args.fail_on_empty {
                    let synthetic =
                        StatementResult::Query(cached.result.clone());
                    check_fail_on_empty(&[synthetic])?;
                }
                return Ok(());
            }
            Ok(None) => {
                if args.output.verbose {
                    eprintln!("[ferrule] cache miss");
                }
            }
            Err(_) => {
                if args.output.verbose {
                    eprintln!("[ferrule] cache: lookup error (continuing)");
                }
            }
        }
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

    // Cache insert: only for successful, single-statement SELECT
    // results. Multi-statement batches, DML, and modifying SQL are
    // already filtered out by the bypass rules + the `results.len()`
    // / variant checks. Errors here are swallowed.
    if let (Some(db), Some(key)) = (cache_db.as_mut(), cache_key.as_ref()) {
        if results.len() == 1 {
            if let StatementResult::Query(ref qr) = results[0] {
                if !is_modifying(&sql) {
                    let ttl_secs = match args.cache.as_deref() {
                        Some(d) => cache::parse_duration_secs(d)?,
                        None => cache::parse_duration_secs(&global_config.cache.default_ttl)
                            .unwrap_or(300),
                    };
                    if ttl_secs > 0 {
                        let preview_end = sql.len().min(200);
                        let redacted = ferrule_core::DatabaseUrl::parse(&args.connection)
                            .map(|u| u.redacted())
                            .unwrap_or_else(|_| args.connection.clone());
                        let meta = cache::CacheMeta {
                            conn_redacted: &redacted,
                            sql_preview: &sql[..preview_end],
                        };
                        let insert_res =
                            db.insert(key, qr, Duration::from_secs(ttl_secs), &meta);
                        match insert_res {
                            Ok(()) => {
                                // Open-loop prune (mirrors history.rs::record).
                                // Failures here are non-fatal.
                                let _ = db.prune(&global_config.cache);
                                cache::record_last(cache::CacheHitInfo {
                                    hit: false,
                                    key: key.0.clone(),
                                    lookup_micros: 0,
                                });
                                if args.output.verbose {
                                    eprintln!(
                                        "[ferrule] cache miss; inserted (ttl={}s)",
                                        ttl_secs
                                    );
                                }
                            }
                            Err(_) => {
                                if args.output.verbose {
                                    eprintln!(
                                        "[ferrule] cache: insert error (continuing)"
                                    );
                                }
                            }
                        }
                    }
                }
            }
        }
    }

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
            cache: None,
            no_cache: false,
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

    /// Build a GlobalConfig with `[cache] path` pointed at an isolated
    /// tempdir so the per-test cache file doesn't bleed into the user
    /// data dir or other tests. Returns the config + the directory
    /// guard so the caller controls lifetime.
    fn cache_test_config(tmp: &tempfile::TempDir) -> GlobalConfig {
        let cache_path = tmp.path().join("results.db");
        GlobalConfig {
            cache: ferrule_config::CacheConfig {
                enabled: true,
                default_ttl: "5m".into(),
                max_age_days: 0,
                max_rows: 0,
                path: Some(cache_path.to_string_lossy().into_owned()),
            },
            ..GlobalConfig::default()
        }
    }

    fn cache_count(cache_path: &std::path::Path) -> i64 {
        if !cache_path.exists() {
            return 0;
        }
        let conn = rusqlite::Connection::open(cache_path).unwrap();
        conn.query_row("SELECT COUNT(*) FROM cache", [], |r| r.get::<_, i64>(0))
            .unwrap_or(0)
    }

    // 18. --bench bypasses the cache: no record_last should fire, no
    // row should land in the cache db.
    #[tokio::test(flavor = "current_thread")]
    async fn bench_bypasses_cache() {
        let tmp = tempfile::tempdir().unwrap();
        let db_path = tmp.path().join("data.db");
        let url = format!("sqlite://{}", db_path.display());
        let global = cache_test_config(&tmp);
        let cache_path = tmp.path().join("results.db");
        // Drain any stale state from prior tests.
        let _ = cache::take_last();

        let mut args = synth_query_args(&url, "SELECT 1");
        args.bench = Some(1);
        args.bench_warmup = 0;
        // Run; the synth args target an on-disk sqlite that connect
        // will create automatically.
        run(args, &global).await.expect("bench run must succeed");
        assert!(
            cache::take_last().is_none(),
            "--bench must not record a cache hit/miss event"
        );
        assert_eq!(
            cache_count(&cache_path),
            0,
            "--bench must not insert into the cache"
        );
    }

    // 19. is_modifying SQL bypasses insert: cache row count stays 0.
    #[tokio::test(flavor = "current_thread")]
    async fn is_modifying_bypasses_insert() {
        let tmp = tempfile::tempdir().unwrap();
        let db_path = tmp.path().join("data.db");
        let url = format!("sqlite://{}", db_path.display());
        let global = cache_test_config(&tmp);
        let cache_path = tmp.path().join("results.db");
        // Seed table.
        {
            let conn = rusqlite::Connection::open(&db_path).unwrap();
            conn.execute_batch("CREATE TABLE t(x INTEGER); INSERT INTO t VALUES (1);")
                .unwrap();
        }
        let _ = cache::take_last();

        let mut args = synth_query_args(&url, "INSERT INTO t VALUES (2)");
        args.cache = Some("5m".into());
        run(args, &global)
            .await
            .expect("insert run must succeed");
        assert_eq!(
            cache_count(&cache_path),
            0,
            "is_modifying SQL must NOT insert into the cache"
        );
    }
}
