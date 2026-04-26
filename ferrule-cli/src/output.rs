use ferrule_core::formatter::OutputFormat;
use is_terminal::IsTerminal;

/// Determine the default output format based on TTY detection.
pub fn default_format() -> OutputFormat {
    if std::io::stdout().is_terminal() {
        OutputFormat::Table
    } else {
        OutputFormat::Json
    }
}

/// Apply a JMESPath expression to a JSON value.
///
/// Returns the filtered JSON. Errors are returned as `String` so callers can
/// wrap them in their own error type with the right exit-code category.
pub fn apply_filter(
    json: serde_json::Value,
    expr: &str,
) -> Result<serde_json::Value, String> {
    let compiled =
        jmespath::compile(expr).map_err(|e| format!("invalid JMESPath expression: {e}"))?;
    let result = compiled
        .search(json)
        .map_err(|e| format!("JMESPath evaluation failed: {e}"))?;
    serde_json::to_value(&*result)
        .map_err(|e| format!("failed to serialize JMESPath result: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn apply_filter_field_projection() {
        let data = json!([
            {"name": "Alice", "age": 30},
            {"name": "Bob",   "age": 25}
        ]);
        let result = apply_filter(data, "[].name").expect("filter should compile and run");
        assert_eq!(result, json!(["Alice", "Bob"]));
    }

    #[test]
    fn apply_filter_predicate() {
        let data = json!([
            {"name": "Alice", "age": 30},
            {"name": "Bob",   "age": 25},
            {"name": "Carol", "age": 35}
        ]);
        let result = apply_filter(data, "[?age > `28`].name")
            .expect("filter should compile and run");
        assert_eq!(result, json!(["Alice", "Carol"]));
    }

    #[test]
    fn apply_filter_pipe_length() {
        let data = json!([
            {"name": "Alice"},
            {"name": "Bob"},
            {"name": "Carol"}
        ]);
        let result =
            apply_filter(data, "length(@)").expect("filter should compile and run");
        assert_eq!(result, json!(3));
    }

    #[test]
    fn apply_filter_invalid_expression_errors() {
        let data = json!([{"name": "Alice"}]);
        let err = apply_filter(data, "[[[").expect_err("malformed expression");
        assert!(
            err.contains("invalid JMESPath expression"),
            "expected parse error, got: {err}"
        );
    }

    #[test]
    fn apply_filter_no_match_returns_null() {
        let data = json!([{"name": "Alice"}]);
        let result =
            apply_filter(data, "missing_field").expect("expression should run even on miss");
        assert_eq!(result, serde_json::Value::Null);
    }
}
