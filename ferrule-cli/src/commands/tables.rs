use super::TablesArgs;
use crate::error::CliError;
use ferrule_core::backend::connect;
use ferrule_core::connection::{ConnectOptions, QueryResult};
use ferrule_core::formatter::{OutputFormat, format_result};
use ferrule_core::value::{ColumnInfo, TypeHint, Value};

pub async fn run(args: TablesArgs) -> Result<(), CliError> {
    let format = args
        .output
        .format
        .as_deref()
        .and_then(OutputFormat::parse)
        .unwrap_or_else(crate::output::default_format);

    let total_start = std::time::Instant::now();

    let url = super::resolve_connection(&args.connection, None).await?;

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

    let query_start = std::time::Instant::now();
    let names = conn
        .list_tables(None)
        .await
        .map_err(CliError::query)?;
    let query_time = query_start.elapsed();

    let mut result = QueryResult {
        columns: vec![ColumnInfo {
            name: "table_name".to_string(),
            type_hint: TypeHint::String,
            nullable: true,
        }],
        rows: names
            .into_iter()
            .map(|n| vec![Value::String(n)])
            .collect(),
    };

    if let Some(limit) = args.output.limit {
        if result.rows.len() > limit {
            result.rows.truncate(limit);
        }
    }

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
