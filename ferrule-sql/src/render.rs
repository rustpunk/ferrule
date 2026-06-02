//! SQL-literal rendering helpers for inline value substitution.
//!
//! These functions turn a neutral [`Value`] into a backend-appropriate
//! SQL literal that can be spliced directly into a statement string.
//! They are the lowest layer of the parameter-substitution and
//! write-path machinery: callers higher up (parameter substitution in
//! `ferrule-core`, the `copy` write path here) compose them into full
//! statements. The rendering is purely string-building and performs no
//! I/O, so it is synchronous and allocation-bounded by the input value.

use crate::backend::Backend;
use crate::value::Value;

/// Render a `Value` into a SQL literal suitable for inline substitution.
///
/// Backend-aware only for booleans, where dialects diverge (Oracle has
/// no native boolean and uses `1`/`0`; the rest use `TRUE`/`FALSE`).
/// Numeric and decimal values pass through unquoted; strings and the
/// catch-all `other` arm are single-quote-escaped via [`quote_string`];
/// raw bytes render as a backend-portable `X'..'` hex literal.
pub fn render_value(value: &Value, backend: Backend) -> String {
    match value {
        Value::Null => "NULL".to_string(),
        Value::Bool(b) => match backend {
            #[cfg(feature = "oracle")]
            Backend::Oracle => {
                if *b {
                    "1".to_string()
                } else {
                    "0".to_string()
                }
            }
            #[cfg(feature = "postgres")]
            Backend::Postgres => bool_literal(*b),
            #[cfg(feature = "mysql")]
            Backend::MySql => bool_literal(*b),
            #[cfg(feature = "mssql")]
            Backend::MsSql => bool_literal(*b),
            #[cfg(feature = "sqlite")]
            Backend::Sqlite => bool_literal(*b),
        },
        Value::Int64(i) => i.to_string(),
        Value::Float64(f) => f.to_string(),
        Value::Decimal(d) => d.clone(),
        Value::String(s) => quote_string(s),
        Value::Bytes(_b) => {
            // Bytes in parameter substitution are rare; render as a hex literal.
            // This is best-effort and backend-specific.
            let hex: String = _b.iter().map(|b| format!("{:02x}", b)).collect();
            format!("X'{}'", hex)
        }
        other => quote_string(&other.to_string()),
    }
}

/// Render a boolean as an ANSI SQL literal (`TRUE` / `FALSE`).
///
/// Private because Oracle does not use it (it renders `1`/`0` in
/// [`render_value`]); kept narrow to the dialects that accept the
/// keyword form.
fn bool_literal(b: bool) -> String {
    if b {
        "TRUE".to_string()
    } else {
        "FALSE".to_string()
    }
}

/// Quote a string for SQL: wrap in single quotes, escape `'` as `''`.
///
/// This is the SQL-standard single-quote escaping accepted by every
/// supported backend; it guards against injection through string
/// values when building literals inline rather than via bind
/// parameters.
pub fn quote_string(v: &str) -> String {
    let mut out = String::with_capacity(v.len() + 2);
    out.push('\'');
    for ch in v.chars() {
        if ch == '\'' {
            out.push('\'');
            out.push('\'');
        } else {
            out.push(ch);
        }
    }
    out.push('\'');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_quote_escape() {
        assert_eq!(quote_string("O'Brien"), "'O''Brien'");
        assert_eq!(quote_string("hello"), "'hello'");
        assert_eq!(quote_string("it's a test"), "'it''s a test'");
    }
}
