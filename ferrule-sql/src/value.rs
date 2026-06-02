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

/// A row is just a vector of values, indexed by column position.
pub type Row = Vec<Value>;
