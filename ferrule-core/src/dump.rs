use crate::backend::Backend;
use crate::connection::{Connection, QueryResult};
use crate::error::CoreError;
use crate::params::render_value;
use crate::value::Value;

/// Supported dump formats.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DumpFormat {
    Csv,
    Json,
    Sql,
}

impl DumpFormat {
    pub fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "csv" => Some(Self::Csv),
            "json" => Some(Self::Json),
            "sql" => Some(Self::Sql),
            _ => None,
        }
    }
}

/// Options for a dump operation.
#[derive(Debug, Clone)]
pub struct DumpOptions {
    pub format: DumpFormat,
    pub batch_size: usize,
    pub schema: Option<String>,
}

impl Default for DumpOptions {
    fn default() -> Self {
        Self {
            format: DumpFormat::Csv,
            batch_size: 1000,
            schema: None,
        }
    }
}

/// Dump an entire table using server‑side paging.
pub async fn dump_table(
    conn: &mut dyn Connection,
    table: &str,
    backend: Backend,
    opts: &DumpOptions,
) -> Result<String, CoreError> {
    let quoted_table = quote_identifier(table);
    let sql = format!("SELECT * FROM {quoted_table}");
    dump_query(conn, &sql, backend, opts, Some(table)).await
}

/// Dump the results of an arbitrary SELECT query.
pub async fn dump_query(
    conn: &mut dyn Connection,
    sql: &str,
    backend: Backend,
    opts: &DumpOptions,
    table_name: Option<&str>,
) -> Result<String, CoreError> {
    let mut all_results = Vec::new();
    let mut offset = 0usize;
    loop {
        let paged =
            crate::query_builder::apply_paging(sql, Some(opts.batch_size), Some(offset), backend)?;
        let page = conn.query(&paged).await?;
        if page.rows.is_empty() {
            break;
        }
        all_results.push(page);
        let fetched = all_results.last().unwrap().rows.len();
        offset += fetched;
        if fetched < opts.batch_size {
            break;
        }
    }

    // Merge pages into one QueryResult
    let mut columns = Vec::new();
    let mut rows = Vec::new();
    for page in all_results {
        if columns.is_empty() {
            columns = page.columns;
        }
        rows.extend(page.rows);
    }
    let result = QueryResult { columns, rows };

    match opts.format {
        DumpFormat::Csv => format_csv(&result),
        DumpFormat::Json => format_json(&result),
        DumpFormat::Sql => {
            let table = table_name.unwrap_or("dumped_table");
            format_sql(&result, table, backend, opts.batch_size)
        }
    }
}

fn format_csv(result: &QueryResult) -> Result<String, CoreError> {
    let mut wtr = csv::Writer::from_writer(Vec::new());
    let headers: Vec<&str> = result.columns.iter().map(|c| c.name.as_str()).collect();
    wtr.write_record(&headers)
        .map_err(|e| CoreError::QueryFailed(e.to_string()))?;
    for row in &result.rows {
        let cells: Vec<String> = row.iter().map(value_to_csv_cell).collect();
        wtr.write_record(&cells)
            .map_err(|e| CoreError::QueryFailed(e.to_string()))?;
    }
    wtr.into_inner()
        .map(|v| String::from_utf8_lossy(&v).into_owned())
        .map_err(|e| CoreError::QueryFailed(e.to_string()))
}

fn value_to_csv_cell(v: &Value) -> String {
    match v {
        Value::Null => String::new(),
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

fn format_json(result: &QueryResult) -> Result<String, CoreError> {
    let mut out = Vec::with_capacity(result.rows.len());
    for row in &result.rows {
        let mut obj = serde_json::Map::new();
        for (col, val) in result.columns.iter().zip(row.iter()) {
            obj.insert(col.name.clone(), json_value(val));
        }
        out.push(serde_json::Value::Object(obj));
    }
    serde_json::to_string_pretty(&out).map_err(|e| CoreError::QueryFailed(e.to_string()))
}

fn json_value(v: &Value) -> serde_json::Value {
    match v {
        Value::Null => serde_json::Value::Null,
        Value::Bool(b) => serde_json::Value::Bool(*b),
        Value::Int64(i) => serde_json::Value::Number((*i).into()),
        Value::Float64(f) => serde_json::Value::Number(
            serde_json::Number::from_f64(*f).unwrap_or_else(|| serde_json::Number::from(0)),
        ),
        Value::Decimal(d) => serde_json::Value::String(d.clone()),
        Value::String(s) => serde_json::Value::String(s.clone()),
        Value::Bytes(_b) => serde_json::Value::String(format!("<{} bytes>", _b.len())),
        Value::Date(d) => serde_json::Value::String(d.to_string()),
        Value::Time(t) => serde_json::Value::String(t.to_string()),
        Value::DateTime(dt) => serde_json::Value::String(dt.to_string()),
        Value::DateTimeTz(dt) => serde_json::Value::String(dt.to_rfc3339()),
        Value::Json(j) => j.clone(),
        Value::Uuid(u) => serde_json::Value::String(u.clone()),
        Value::Array(a) => serde_json::Value::Array(a.iter().map(json_value).collect()),
    }
}

fn format_sql(
    result: &QueryResult,
    table: &str,
    backend: Backend,
    batch_size: usize,
) -> Result<String, CoreError> {
    let mut lines = Vec::new();
    let quoted_table = quote_identifier(table);
    let col_names: Vec<String> = result
        .columns
        .iter()
        .map(|c| quote_identifier(&c.name))
        .collect();
    let cols = col_names.join(", ");

    let mut batch = Vec::new();
    for row in &result.rows {
        let values: Vec<String> = row.iter().map(|v| render_value(v, backend)).collect();
        batch.push(format!("({})", values.join(", ")));
        if batch.len() >= batch_size {
            lines.push(format!(
                "INSERT INTO {quoted_table} ({cols}) VALUES {};",
                batch.join(", ")
            ));
            batch.clear();
        }
    }
    if !batch.is_empty() {
        lines.push(format!(
            "INSERT INTO {quoted_table} ({cols}) VALUES {};",
            batch.join(", ")
        ));
    }

    Ok(lines.join("\n"))
}

/// SQL-standard identifier quoting: wraps in double quotes and doubles
/// any embedded double-quote characters.
fn quote_identifier(id: &str) -> String {
    format!("\"{}\"", id.replace('"', "\"\""))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quote_identifier_wraps_in_double_quotes() {
        assert_eq!(quote_identifier("users"), "\"users\"");
    }

    #[test]
    fn quote_identifier_escapes_embedded_quotes() {
        assert_eq!(quote_identifier("a\"b"), "\"a\"\"b\"");
        assert_eq!(quote_identifier("\"\""), "\"\"\"\"\"\"");
    }

    #[test]
    fn quote_identifier_preserves_other_chars() {
        assert_eq!(quote_identifier("col with space"), "\"col with space\"");
        assert_eq!(quote_identifier("snake_case_99"), "\"snake_case_99\"");
    }
}
