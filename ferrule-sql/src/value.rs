use chrono::{DateTime, NaiveDate, NaiveDateTime, NaiveTime, Utc};
use serde::{Deserialize, Serialize};
use std::fmt;

/// A type hint used by formatters to choose presentation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TypeHint {
    Null,
    Bool,
    Int64,
    Float64,
    Decimal,
    String,
    Bytes,
    Date,
    Time,
    DateTime,
    DateTimeTz,
    Json,
    Uuid,
    Array,
    Other,
}

/// Metadata about a single column.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ColumnInfo {
    pub name: String,
    pub type_hint: TypeHint,
    pub nullable: bool,
}

/// A unified value type across all backends.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Value {
    Null,
    Bool(bool),
    Int64(i64),
    Float64(f64),
    // Decimal stored as string to avoid needing rust_decimal in public API for now.
    Decimal(String),
    String(String),
    Bytes(Vec<u8>),
    Date(NaiveDate),
    Time(NaiveTime),
    DateTime(NaiveDateTime),
    DateTimeTz(DateTime<Utc>),
    Json(serde_json::Value),
    Uuid(String),
    Array(Vec<Value>),
}

impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Value::Null => write!(f, "NULL"),
            Value::Bool(v) => write!(f, "{v}"),
            Value::Int64(v) => write!(f, "{v}"),
            Value::Float64(v) => write!(f, "{v}"),
            Value::Decimal(v) => write!(f, "{v}"),
            Value::String(v) => write!(f, "{v}"),
            Value::Bytes(v) => write!(f, "<{} bytes>", v.len()),
            Value::Date(v) => write!(f, "{v}"),
            Value::Time(v) => write!(f, "{v}"),
            Value::DateTime(v) => write!(f, "{v}"),
            Value::DateTimeTz(v) => write!(f, "{v}"),
            Value::Json(v) => write!(f, "{v}"),
            Value::Uuid(v) => write!(f, "{v}"),
            Value::Array(v) => {
                let items: Vec<String> = v.iter().map(|i| i.to_string()).collect();
                write!(f, "[{}]", items.join(", "))
            }
        }
    }
}

impl Value {
    /// Approximate in-memory byte footprint of this value's *payload*,
    /// used by the `ferrule-sql` size guards (`max_cell_bytes` /
    /// `max_row_bytes` / `max_total_buffered_bytes`) to fail fast on a
    /// pathological cell before it is retained.
    ///
    /// This counts the bytes the payload actually owns on the heap (the
    /// `String` / `Bytes` / `Json` / nested-`Array` contents), not the
    /// fixed 24-ish-byte `Value` enum discriminant+inline scalars. The
    /// goal is a cheap, monotonic proxy for "how much memory does
    /// keeping this cell cost" — exact accounting is unnecessary because
    /// the guards are coarse ceilings, not an RSS budget. `Json` is
    /// measured by its serialized length (a tight upper bound on the
    /// retained `serde_json::Value` tree); `Array` recurses so a deeply
    /// nested array is charged for its whole subtree.
    #[must_use]
    pub fn byte_size(&self) -> usize {
        match self {
            Value::Null
            | Value::Bool(_)
            | Value::Int64(_)
            | Value::Float64(_)
            | Value::Date(_)
            | Value::Time(_)
            | Value::DateTime(_)
            | Value::DateTimeTz(_) => std::mem::size_of::<Value>(),
            Value::Decimal(s) | Value::String(s) | Value::Uuid(s) => s.len(),
            Value::Bytes(b) => b.len(),
            Value::Json(j) => json_byte_size(j),
            Value::Array(items) => items.iter().map(Value::byte_size).sum(),
        }
    }
}

/// Serialized-length proxy for a `serde_json::Value`'s retained size.
///
/// `serde_json::to_string(...).len()` would allocate; this walks the
/// tree counting characters instead, so measuring a giant JSON cell for
/// a size-guard check never doubles its memory cost.
fn json_byte_size(v: &serde_json::Value) -> usize {
    match v {
        serde_json::Value::Null => 4,
        serde_json::Value::Bool(_) => 5,
        serde_json::Value::Number(n) => n.to_string().len(),
        serde_json::Value::String(s) => s.len() + 2,
        serde_json::Value::Array(a) => {
            2 + a.iter().map(json_byte_size).sum::<usize>() + a.len().saturating_sub(1)
        }
        serde_json::Value::Object(o) => {
            2 + o
                .iter()
                .map(|(k, val)| k.len() + 3 + json_byte_size(val))
                .sum::<usize>()
                + o.len().saturating_sub(1)
        }
    }
}

/// A row is just a vector of values, indexed by column position.
pub type Row = Vec<Value>;
