use crate::connection::QueryResult;
use crate::error::CoreError;
use crate::value::Value;
use tabled::builder::Builder;
use tabled::settings::Style;

/// Supported output formats.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputFormat {
    Table,
    Json,
    Csv,
    Yaml,
    Raw,
}

impl OutputFormat {
    /// Parse from a string argument.
    pub fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "table" => Some(Self::Table),
            "json" => Some(Self::Json),
            "csv" => Some(Self::Csv),
            "yaml" => Some(Self::Yaml),
            "raw" => Some(Self::Raw),
            _ => None,
        }
    }
}

/// Render a `QueryResult` into the requested output format.
pub fn format_result(result: &QueryResult, format: OutputFormat) -> Result<String, CoreError> {
    match format {
        OutputFormat::Table => format_table(result),
        OutputFormat::Json => format_json(result),
        OutputFormat::Csv => format_csv(result),
        OutputFormat::Yaml => format_yaml(result),
        OutputFormat::Raw => format_raw(result),
    }
}

fn format_table(result: &QueryResult) -> Result<String, CoreError> {
    let mut builder = Builder::default();
    let headers: Vec<String> = result.columns.iter().map(|c| c.name.clone()).collect();
    builder.push_record(headers);
    for row in &result.rows {
        let cells: Vec<String> = row.iter().map(cell_string).collect();
        builder.push_record(cells);
    }
    let mut table = builder.build();
    table.with(Style::modern());
    Ok(table.to_string())
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

fn format_csv(result: &QueryResult) -> Result<String, CoreError> {
    let mut wtr = csv::Writer::from_writer(Vec::new());
    let headers: Vec<&str> = result.columns.iter().map(|c| c.name.as_str()).collect();
    wtr.write_record(&headers)
        .map_err(|e| CoreError::QueryFailed(e.to_string()))?;
    for row in &result.rows {
        let cells: Vec<String> = row.iter().map(cell_string).collect();
        wtr.write_record(&cells)
            .map_err(|e| CoreError::QueryFailed(e.to_string()))?;
    }
    wtr.into_inner()
        .map(|v| String::from_utf8_lossy(&v).into_owned())
        .map_err(|e| CoreError::QueryFailed(e.to_string()))
}

fn format_yaml(result: &QueryResult) -> Result<String, CoreError> {
    let mut out = Vec::with_capacity(result.rows.len());
    for row in &result.rows {
        let mut obj = serde_json::Map::new();
        for (col, val) in result.columns.iter().zip(row.iter()) {
            obj.insert(col.name.clone(), json_value(val));
        }
        out.push(serde_json::Value::Object(obj));
    }
    serde_saphyr::to_string(&out).map_err(|e| CoreError::QueryFailed(e.to_string()))
}

fn format_raw(result: &QueryResult) -> Result<String, CoreError> {
    let mut lines = Vec::new();
    for row in &result.rows {
        let cells: Vec<String> = row.iter().map(cell_string).collect();
        lines.push(cells.join("\t"));
    }
    Ok(lines.join("\n"))
}

fn cell_string(v: &Value) -> String {
    match v {
        Value::Null => "NULL".to_string(),
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
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
        Value::Bytes(b) => serde_json::Value::String(format!("<{} bytes>", b.len())),
        Value::Date(d) => serde_json::Value::String(d.to_string()),
        Value::Time(t) => serde_json::Value::String(t.to_string()),
        Value::DateTime(dt) => serde_json::Value::String(dt.to_string()),
        Value::DateTimeTz(dt) => serde_json::Value::String(dt.to_rfc3339()),
        Value::Json(j) => j.clone(),
        Value::Uuid(u) => serde_json::Value::String(u.clone()),
        Value::Array(a) => serde_json::Value::Array(a.iter().map(json_value).collect()),
    }
}
