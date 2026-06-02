use ferrule_sql::render_value;
use ferrule_sql::value::{TypeHint, Value};
use ferrule_sql::{Backend, Connection, SqlError};

/// Supported load formats.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoadFormat {
    Csv,
    Json,
}

impl LoadFormat {
    pub fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "csv" => Some(Self::Csv),
            "json" => Some(Self::Json),
            _ => None,
        }
    }
}

/// Options for a load operation.
#[derive(Debug, Clone)]
pub struct LoadOptions {
    pub format: LoadFormat,
    pub table: String,
    pub create_table: bool,
    pub batch_size: usize,
}

impl Default for LoadOptions {
    fn default() -> Self {
        Self {
            format: LoadFormat::Csv,
            table: String::new(),
            create_table: false,
            batch_size: 1000,
        }
    }
}

/// Load data from a reader (CSV or JSON) into a table.
pub async fn load_data(
    conn: &mut dyn Connection,
    data: &str,
    backend: Backend,
    opts: &LoadOptions,
) -> Result<usize, SqlError> {
    match opts.format {
        LoadFormat::Csv => load_csv(conn, data, backend, opts).await,
        LoadFormat::Json => load_json(conn, data, backend, opts).await,
    }
}

async fn load_csv(
    conn: &mut dyn Connection,
    data: &str,
    backend: Backend,
    opts: &LoadOptions,
) -> Result<usize, SqlError> {
    let mut rdr = csv::Reader::from_reader(data.as_bytes());
    let headers: Vec<String> = rdr
        .headers()
        .map_err(|e| SqlError::QueryFailed(e.to_string()))?
        .iter()
        .map(|s| s.to_string())
        .collect();
    let quoted_table = quote_identifier(&opts.table);
    let quoted_cols: Vec<String> = headers.iter().map(|h| quote_identifier(h)).collect();
    let cols = quoted_cols.join(", ");

    let mut total = 0usize;
    let mut batch = Vec::new();
    for result in rdr.records() {
        let record = result.map_err(|e| SqlError::QueryFailed(e.to_string()))?;
        let values: Vec<String> = record
            .iter()
            .map(|s| render_value(&Value::String(s.to_string()), backend))
            .collect();
        batch.push(format!("({})", values.join(", ")));
        if batch.len() >= opts.batch_size {
            let sql = format!(
                "INSERT INTO {quoted_table} ({cols}) VALUES {};",
                batch.join(", ")
            );
            conn.execute(&sql).await?;
            total += batch.len();
            batch.clear();
        }
    }
    if !batch.is_empty() {
        let sql = format!(
            "INSERT INTO {quoted_table} ({cols}) VALUES {};",
            batch.join(", ")
        );
        conn.execute(&sql).await?;
        total += batch.len();
    }
    Ok(total)
}

async fn load_json(
    conn: &mut dyn Connection,
    data: &str,
    backend: Backend,
    opts: &LoadOptions,
) -> Result<usize, SqlError> {
    let arr: Vec<serde_json::Value> =
        serde_json::from_str(data).map_err(|e| SqlError::QueryFailed(e.to_string()))?;
    if arr.is_empty() {
        return Ok(0);
    }

    // Infer columns from first object
    let first = arr[0]
        .as_object()
        .ok_or_else(|| SqlError::QueryFailed("JSON array must contain objects".into()))?;
    let columns: Vec<String> = first.keys().cloned().collect();
    let quoted_table = quote_identifier(&opts.table);
    let quoted_cols: Vec<String> = columns.iter().map(|c| quote_identifier(c)).collect();
    let cols = quoted_cols.join(", ");

    if opts.create_table {
        let schema = infer_schema(&arr, backend);
        let create = build_create_table(&opts.table, &schema, backend);
        conn.execute(&create).await?;
    }

    let mut total = 0usize;
    let mut batch = Vec::new();
    for obj in &arr {
        if let Some(map) = obj.as_object() {
            let values: Vec<String> = columns
                .iter()
                .map(|c| {
                    let val = map.get(c).cloned().unwrap_or(serde_json::Value::Null);
                    render_value(&json_to_value(&val), backend)
                })
                .collect();
            batch.push(format!("({})", values.join(", ")));
            if batch.len() >= opts.batch_size {
                let sql = format!(
                    "INSERT INTO {quoted_table} ({cols}) VALUES {};",
                    batch.join(", ")
                );
                conn.execute(&sql).await?;
                total += batch.len();
                batch.clear();
            }
        }
    }
    if !batch.is_empty() {
        let sql = format!(
            "INSERT INTO {quoted_table} ({cols}) VALUES {};",
            batch.join(", ")
        );
        conn.execute(&sql).await?;
        total += batch.len();
    }
    Ok(total)
}

fn json_to_value(v: &serde_json::Value) -> Value {
    match v {
        serde_json::Value::Null => Value::Null,
        serde_json::Value::Bool(b) => Value::Bool(*b),
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                Value::Int64(i)
            } else if let Some(f) = n.as_f64() {
                if f.fract() == 0.0 && f >= i64::MIN as f64 && f <= i64::MAX as f64 {
                    Value::Int64(f as i64)
                } else {
                    Value::Float64(f)
                }
            } else {
                Value::String(n.to_string())
            }
        }
        serde_json::Value::String(s) => Value::String(s.clone()),
        serde_json::Value::Array(a) => Value::Array(a.iter().map(json_to_value).collect()),
        serde_json::Value::Object(_) => Value::String(v.to_string()),
    }
}

/// Infer a schema from a slice of JSON objects.
pub fn infer_schema(objects: &[serde_json::Value], backend: Backend) -> Vec<(String, TypeHint)> {
    let mut schema = Vec::new();
    if objects.is_empty() {
        return schema;
    }
    if let Some(first) = objects[0].as_object() {
        for (key, val) in first {
            let hint = infer_json_type(val, backend);
            schema.push((key.clone(), hint));
        }
    }
    schema
}

#[cfg_attr(not(feature = "oracle"), allow(unused_variables))]
fn infer_json_type(val: &serde_json::Value, backend: Backend) -> TypeHint {
    match val {
        serde_json::Value::Null => TypeHint::String,
        serde_json::Value::Bool(_) => {
            #[cfg(feature = "oracle")]
            if matches!(backend, Backend::Oracle) {
                return TypeHint::Int64;
            }
            TypeHint::Bool
        }
        serde_json::Value::Number(n) => {
            if let Some(_i) = n.as_i64() {
                TypeHint::Int64
            } else {
                TypeHint::Float64
            }
        }
        serde_json::Value::String(_) => TypeHint::String,
        serde_json::Value::Array(_) => TypeHint::Array,
        serde_json::Value::Object(_) => TypeHint::Json,
    }
}

fn build_create_table(table: &str, schema: &[(String, TypeHint)], backend: Backend) -> String {
    let quoted_table = quote_identifier(table);
    let cols: Vec<String> = schema
        .iter()
        .map(|(name, hint)| {
            let quoted_name = quote_identifier(name);
            let sql_type = type_hint_to_sql(hint, backend);
            format!("{} {}", quoted_name, sql_type)
        })
        .collect();
    format!("CREATE TABLE {quoted_table} ({});", cols.join(", "))
}

#[cfg_attr(not(feature = "oracle"), allow(unused_variables))]
fn type_hint_to_sql(hint: &TypeHint, backend: Backend) -> &'static str {
    match hint {
        TypeHint::Int64 => "INTEGER",
        TypeHint::Float64 | TypeHint::Decimal => "NUMERIC(18,6)",
        TypeHint::Bool => {
            #[cfg(feature = "oracle")]
            if matches!(backend, Backend::Oracle) {
                return "NUMBER(1)";
            }
            "BOOLEAN"
        }
        TypeHint::Json => {
            #[cfg(feature = "oracle")]
            if matches!(backend, Backend::Oracle) {
                return "CLOB";
            }
            "TEXT"
        }
        TypeHint::String | TypeHint::Null | TypeHint::Uuid => {
            #[cfg(feature = "oracle")]
            if matches!(backend, Backend::Oracle) {
                return "VARCHAR2(4000)";
            }
            "TEXT"
        }
        _ => {
            #[cfg(feature = "oracle")]
            if matches!(backend, Backend::Oracle) {
                return "VARCHAR2(4000)";
            }
            "TEXT"
        }
    }
}

fn quote_identifier(id: &str) -> String {
    format!("\"{}\"", id.replace('\"', "\"\""))
}
