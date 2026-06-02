use super::{
    check_daemon_ssh_compat, connect_resolved, resolve_connection, ConnectionFlags, OutputFlags,
};
use crate::error::CliError;
use clap::Args;
use ferrule_config::profile::GlobalConfig;
use ferrule_sql::connection::ConnectOptions;
use ferrule_sql::Backend;
use std::io::Write;

#[derive(Args, Clone, Debug)]
pub struct ExportArgs {
    /// Connection name or raw URL
    pub connection: String,

    /// SQL SELECT statement to export
    pub sql: String,

    /// Output file (stdout if omitted)
    #[arg(long, value_name = "PATH")]
    pub file: Option<String>,

    /// Export format: csv, json, sql, jsonl
    #[arg(short, long, value_name = "FORMAT", default_value = "csv")]
    pub format: String,

    /// Page size for server-side streaming
    #[arg(long, value_name = "N", default_value = "1000")]
    pub page_size: usize,

    /// Schema name (for SQL insert target)
    #[arg(long)]
    pub schema: Option<String>,

    /// Table name (for SQL insert target; defaults to `exported`)
    #[arg(long, value_name = "TABLE")]
    pub table: Option<String>,

    #[command(flatten)]
    pub output: OutputFlags,

    #[command(flatten)]
    pub conn_flags: ConnectionFlags,

    /// Exit with code 1 ("notable result", GNU diff convention) when
    /// the export produces zero rows. Useful for scripted exports
    /// that should alert when nothing matched.
    #[arg(long)]
    pub fail_on_empty: bool,
}

/// Supported export formats.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ExportFormat {
    Csv,
    Json,
    Jsonl,
    Sql,
}

impl ExportFormat {
    fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "csv" => Some(Self::Csv),
            "json" => Some(Self::Json),
            "jsonl" => Some(Self::Jsonl),
            "sql" => Some(Self::Sql),
            _ => None,
        }
    }
}

pub async fn run(args: ExportArgs, global_config: &GlobalConfig) -> Result<(), CliError> {
    let format = ExportFormat::parse(&args.format)
        .ok_or_else(|| CliError::usage(format!("Unknown export format: {}", args.format)))?;

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

    let backend = Backend::from_scheme(resolved.url.scheme())
        .ok_or_else(|| CliError::usage(format!("Unsupported scheme: {}", resolved.url.scheme())))?;

    let mut conn = connect_resolved(
        resolved,
        &ConnectOptions {
            insecure: args.conn_flags.insecure,
        },
    )
    .await?;

    // Set up output writer
    let mut stdout_guard;
    let mut file_guard;
    let writer: &mut dyn Write = match &args.file {
        Some(path) => {
            file_guard = std::fs::File::create(path).map_err(CliError::Io)?;
            &mut file_guard
        }
        None => {
            stdout_guard = std::io::stdout();
            &mut stdout_guard
        }
    };

    // Page through the query result and stream to output
    let sql = args.sql.trim().trim_end_matches(';').trim();
    if sql.is_empty() {
        return Err(CliError::usage("SQL statement is empty"));
    }

    let mut offset = 0usize;
    let mut page_num = 0usize;
    let mut total_rows = 0usize;
    let mut csv_header_done = false;
    let mut json_started = false;

    loop {
        let paged = ferrule_sql::apply_paging(sql, Some(args.page_size), Some(offset), backend)
            .map_err(|e| CliError::usage(e.to_string()))?;

        let result = conn.query(&paged).await.map_err(CliError::query)?;
        if result.rows.is_empty() {
            break;
        }

        page_num += 1;
        total_rows += result.rows.len();

        match format {
            ExportFormat::Csv => {
                if !csv_header_done {
                    let headers: Vec<&str> =
                        result.columns.iter().map(|c| c.name.as_str()).collect();
                    let line = csv_line(&headers.to_vec()) + "\n";
                    writer.write_all(line.as_bytes()).map_err(CliError::Io)?;
                    csv_header_done = true;
                }
                for row in &result.rows {
                    let cells: Vec<String> = row.iter().map(value_to_csv_cell).collect();
                    let line = csv_line(&cells) + "\n";
                    writer.write_all(line.as_bytes()).map_err(CliError::Io)?;
                }
            }
            ExportFormat::Json => {
                if !json_started {
                    writer.write_all(b"[").map_err(CliError::Io)?;
                    json_started = true;
                }
                for (i, row) in result.rows.iter().enumerate() {
                    if page_num > 1 || i > 0 {
                        writer.write_all(b",").map_err(CliError::Io)?;
                    }
                    let obj = row_to_json_object(&result.columns, row);
                    let line = serde_json::to_string(&obj)
                        .map_err(|e| CliError::Io(std::io::Error::other(e)))?;
                    writer.write_all(line.as_bytes()).map_err(CliError::Io)?;
                }
            }
            ExportFormat::Jsonl => {
                for row in &result.rows {
                    let obj = row_to_json_object(&result.columns, row);
                    let mut line = serde_json::to_string(&obj)
                        .map_err(|e| CliError::Io(std::io::Error::other(e)))?;
                    line.push('\n');
                    writer.write_all(line.as_bytes()).map_err(CliError::Io)?;
                }
            }
            ExportFormat::Sql => {
                let table = args.table.as_deref().unwrap_or("exported");
                let quoted_table = quote_identifier(table);
                let col_names: Vec<String> = result
                    .columns
                    .iter()
                    .map(|c| quote_identifier(&c.name))
                    .collect();
                let cols = col_names.join(", ");
                let mut batch = Vec::new();
                for row in &result.rows {
                    let values: Vec<String> = row
                        .iter()
                        .map(|v| ferrule_sql::render_value(v, backend))
                        .collect();
                    batch.push(format!("({})", values.join(", ")));
                    if batch.len() >= args.page_size {
                        let stmt = format!(
                            "INSERT INTO {quoted_table} ({cols}) VALUES {};\n",
                            batch.join(", ")
                        );
                        writer.write_all(stmt.as_bytes()).map_err(CliError::Io)?;
                        batch.clear();
                    }
                }
                if !batch.is_empty() {
                    let stmt = format!(
                        "INSERT INTO {quoted_table} ({cols}) VALUES {};\n",
                        batch.join(", ")
                    );
                    writer.write_all(stmt.as_bytes()).map_err(CliError::Io)?;
                }
            }
        }

        if result.rows.len() < args.page_size {
            break;
        }
        offset += result.rows.len();
    }

    // Close JSON array
    if format == ExportFormat::Json {
        writer.write_all(b"]\n").map_err(CliError::Io)?;
    }

    if args.output.verbose {
        eprintln!("[export] {} rows written", total_rows);
    }

    if args.fail_on_empty && total_rows == 0 {
        return Err(CliError::result_notable(
            "export produced no rows (--fail-on-empty)",
        ));
    }

    Ok(())
}

/// Encode a slice of strings as a single CSV line, quoting fields
/// that contain commas, quotes, or newlines.
fn csv_line(fields: &[impl AsRef<str>]) -> String {
    let mut line = String::new();
    for (i, field) in fields.iter().enumerate() {
        if i > 0 {
            line.push(',');
        }
        let text = field.as_ref();
        let needs_quote =
            text.contains(',') || text.contains('"') || text.contains('\n') || text.contains('\r');
        if needs_quote {
            line.push('"');
            line.push_str(&text.replace('"', "\"\""));
            line.push('"');
        } else {
            line.push_str(text);
        }
    }
    line
}

fn value_to_csv_cell(v: &ferrule_sql::value::Value) -> String {
    match v {
        ferrule_sql::value::Value::Null => String::new(),
        ferrule_sql::value::Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

fn row_to_json_object(
    columns: &[ferrule_sql::value::ColumnInfo],
    row: &ferrule_sql::value::Row,
) -> serde_json::Map<String, serde_json::Value> {
    let mut obj = serde_json::Map::new();
    for (col, val) in columns.iter().zip(row.iter()) {
        obj.insert(col.name.clone(), value_to_json(val));
    }
    obj
}

fn value_to_json(v: &ferrule_sql::value::Value) -> serde_json::Value {
    use ferrule_sql::value::Value;
    match v {
        Value::Null => serde_json::Value::Null,
        Value::Bool(b) => serde_json::Value::Bool(*b),
        Value::Int64(i) => serde_json::Value::Number((*i).into()),
        Value::Float64(f) => serde_json::Value::Number(
            serde_json::Number::from_f64(*f).unwrap_or_else(|| serde_json::Number::from(0)),
        ),
        Value::Decimal(d) => serde_json::Value::String(d.clone()),
        Value::String(s) => serde_json::Value::String(s.clone()),
        Value::Bytes(b) => serde_json::Value::String(format!("<{} bytes>", b.len())),
        Value::Date(d) => serde_json::Value::String(d.to_string()),
        Value::Time(t) => serde_json::Value::String(t.to_string()),
        Value::DateTime(dt) => serde_json::Value::String(dt.to_string()),
        Value::DateTimeTz(dt) => serde_json::Value::String(dt.to_rfc3339()),
        Value::Json(j) => j.clone(),
        Value::Uuid(u) => serde_json::Value::String(u.clone()),
        Value::Array(a) => serde_json::Value::Array(a.iter().map(value_to_json).collect()),
    }
}

fn quote_identifier(id: &str) -> String {
    format!("\"{}\"", id.replace('"', "\"\""))
}
