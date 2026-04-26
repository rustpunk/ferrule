use crate::backend::Backend;
use crate::error::CoreError;
use crate::value::Value;
use indexmap::IndexMap;
use std::str::FromStr;

/// A named set of runtime parameters.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ParameterSet {
    pub map: IndexMap<String, Value>,
}

impl ParameterSet {
    /// Insert or overwrite a parameter.
    pub fn set(&mut self, name: String, value: Value) {
        self.map.insert(name, value);
    }

    /// Remove a parameter.
    pub fn clear(&mut self) {
        self.map.clear();
    }
}

/// Substitute `${name}` placeholders in SQL with values from `params`.
///
/// One pass only — no recursive substitution.
/// Missing parameters return `CoreError::QueryFailed`.
pub fn substitute(sql: &str, params: &ParameterSet, backend: Backend) -> Result<String, CoreError> {
    let mut result = String::with_capacity(sql.len());
    let mut chars = sql.chars().peekable();

    while let Some(ch) = chars.next() {
        if ch == '$' && chars.next_if_eq(&'{').is_some() {
            let name: String = chars.by_ref().take_while(|c| *c != '}').collect();
            if name.is_empty() {
                result.push_str("${}");
                continue;
            }
            match params.map.get(&name) {
                Some(value) => result.push_str(&render_value(value, backend)),
                None => {
                    return Err(CoreError::QueryFailed(format!(
                        "Missing parameter: {}",
                        name
                    )));
                }
            }
        } else {
            result.push(ch);
        }
    }

    Ok(result)
}

/// Render a `Value` into a SQL literal suitable for inline substitution.
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
            _ => {
                if *b {
                    "TRUE".to_string()
                } else {
                    "FALSE".to_string()
                }
            }
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

/// Quote a string for SQL: wrap in single quotes, escape `'` as `''`.
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

/// Parse a `NAME=VALUE` string, splitting at the first `=`.
pub fn parse_param(s: &str) -> Result<(String, String), CoreError> {
    let pos = s.find('=').ok_or_else(|| {
        CoreError::QueryFailed(format!(
            "Invalid parameter format '{}', expected NAME=VALUE",
            s
        ))
    })?;
    let (name, value) = s.split_at(pos);
    Ok((name.to_string(), value[1..].to_string()))
}

/// Infer a `Value` type from a raw string.
///
/// * `"true"` / `"false"` (case‑insensitive) → `Value::Bool`
/// * `"-42"` → `Value::Int64`
/// * `"3.14"` → `Value::Float64`
/// * anything else → `Value::String`
pub fn infer_type(v: &str) -> Value {
    let trimmed = v.trim();
    if trimmed.eq_ignore_ascii_case("true") {
        return Value::Bool(true);
    }
    if trimmed.eq_ignore_ascii_case("false") {
        return Value::Bool(false);
    }
    if trimmed.bytes().all(|b| b.is_ascii_digit() || b == b'-') && trimmed.len() > 1
        || trimmed.bytes().all(|b| b.is_ascii_digit()) && !trimmed.is_empty()
    {
        if let Ok(i) = i64::from_str(trimmed) {
            return Value::Int64(i);
        }
    }
    if trimmed.bytes().filter(|b| *b == b'.').count() == 1 {
        if let Ok(f) = f64::from_str(trimmed) {
            return Value::Float64(f);
        }
    }
    Value::String(trimmed.to_string())
}

/// Load parameters from a JSON file (object mapping name → raw string value).
pub fn load_from_json(path: &std::path::Path) -> Result<ParameterSet, CoreError> {
    let content = std::fs::read_to_string(path).map_err(|e| {
        CoreError::QueryFailed(format!(
            "Cannot read parameter file '{}': {}",
            path.display(),
            e
        ))
    })?;
    let obj: serde_json::Map<String, serde_json::Value> =
        serde_json::from_str(&content).map_err(|e| {
            CoreError::QueryFailed(format!(
                "Invalid JSON in parameter file '{}': {}",
                path.display(),
                e
            ))
        })?;

    let mut set = ParameterSet::default();
    for (key, val) in obj {
        let value = match val {
            serde_json::Value::Bool(b) => Value::Bool(b),
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
            serde_json::Value::String(s) => infer_type(&s),
            other => Value::String(other.to_string()),
        };
        set.set(key, value);
    }
    Ok(set)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_infer_string() {
        assert_eq!(infer_type("Alice"), Value::String("Alice".into()));
    }

    #[test]
    fn test_infer_int() {
        assert_eq!(infer_type("-42"), Value::Int64(-42));
        assert_eq!(infer_type("0"), Value::Int64(0));
    }

    #[test]
    fn test_infer_float() {
        assert_eq!(infer_type("3.14"), Value::Float64(3.14));
    }

    #[test]
    fn test_infer_bool() {
        assert_eq!(infer_type("false"), Value::Bool(false));
        assert_eq!(infer_type("true"), Value::Bool(true));
        assert_eq!(infer_type("TRUE"), Value::Bool(true));
        assert_eq!(infer_type("FALSE"), Value::Bool(false));
    }

    #[test]
    fn test_quote_escape() {
        assert_eq!(quote_string("O'Brien"), "'O''Brien'");
        assert_eq!(quote_string("hello"), "'hello'");
        assert_eq!(quote_string("it's a test"), "'it''s a test'");
    }

    #[test]
    fn test_parse_param() {
        assert_eq!(
            parse_param("name=Alice").unwrap(),
            ("name".into(), "Alice".into())
        );
        assert_eq!(
            parse_param("host=localhost:5432").unwrap(),
            ("host".into(), "localhost:5432".into())
        );
        assert!(parse_param("no_equals").is_err());
    }

    #[test]
    fn test_substitute_completes() {
        let mut params = ParameterSet::default();
        params.set("name".into(), Value::String("Alice".into()));
        params.set("age".into(), Value::Int64(30));
        let sql = substitute(
            "SELECT * FROM t WHERE n = ${name} AND a = ${age}",
            &params,
            Backend::Postgres,
        )
        .unwrap();
        assert_eq!(sql, "SELECT * FROM t WHERE n = 'Alice' AND a = 30");
    }

    #[test]
    fn test_substitute_missing_errors() {
        let params = ParameterSet::default();
        let result = substitute("SELECT ${x}", &params, Backend::Postgres);
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("Missing parameter: x"));
    }

    #[cfg(feature = "oracle")]
    #[test]
    fn test_substitute_oracle_bool() {
        let mut params = ParameterSet::default();
        params.set("active".into(), Value::Bool(true));
        let sql = substitute("SELECT ${active}", &params, Backend::Oracle).unwrap();
        assert_eq!(sql, "SELECT 1");
    }

    #[test]
    fn test_substitute_postgres_bool() {
        let mut params = ParameterSet::default();
        params.set("active".into(), Value::Bool(false));
        let sql = substitute("SELECT ${active}", &params, Backend::Postgres).unwrap();
        assert_eq!(sql, "SELECT FALSE");
    }

    #[test]
    fn test_substitute_no_recursive() {
        let mut params = ParameterSet::default();
        params.set("foo".into(), Value::String("${bar}".into()));
        let sql = substitute("SELECT ${foo}", &params, Backend::Postgres).unwrap();
        assert_eq!(sql, "SELECT '${bar}'");
    }

    #[test]
    fn test_load_from_json() {
        let path = std::env::temp_dir().join("ferrule_test_params.json");
        std::fs::write(
            &path,
            r#"{"name":"Alice","age":30,"active":true,"score":99.5}"#,
        )
        .unwrap();
        let set = load_from_json(&path).unwrap();
        assert_eq!(set.map.get("name"), Some(&Value::String("Alice".into())));
        assert_eq!(set.map.get("age"), Some(&Value::Int64(30)));
        assert_eq!(set.map.get("active"), Some(&Value::Bool(true)));
        assert_eq!(set.map.get("score"), Some(&Value::Float64(99.5)));
        std::fs::remove_file(&path).ok();
    }
}
