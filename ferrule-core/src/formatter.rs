use crate::connection::QueryResult;
use crate::error::CoreError;
use crate::value::Value;
use std::borrow::Cow;
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
    Markdown,
    Jsonl,
    Html,
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
            "markdown" | "md" => Some(Self::Markdown),
            "jsonl" | "ndjson" => Some(Self::Jsonl),
            "html" => Some(Self::Html),
            _ => None,
        }
    }
}

/// Optional formatter configuration. Reserved for future extensions
/// such as a custom NULL marker (issue #52); v1 callers can pass
/// [`FormatOptions::default()`] via [`format_result`] without losing
/// functionality.
#[derive(Debug, Clone, Default)]
pub struct FormatOptions {
    /// Override for NULL rendering. v1 honours this in future-extensible
    /// formats only — markdown/HTML render empty cells; JSONL always
    /// emits JSON null (changing it would produce invalid JSON).
    pub null_marker: Option<String>,
}

/// Render a `QueryResult` into the requested output format using default options.
#[must_use = "the formatted output is the function's only product"]
pub fn format_result(result: &QueryResult, format: OutputFormat) -> Result<String, CoreError> {
    format_result_with(result, format, &FormatOptions::default())
}

/// Render a `QueryResult` into the requested output format with explicit
/// [`FormatOptions`]. Entry point for callers that need to override
/// formatter behaviour (e.g. custom NULL marker — issue #52).
#[must_use = "the formatted output is the function's only product"]
pub fn format_result_with(
    result: &QueryResult,
    format: OutputFormat,
    _opts: &FormatOptions,
) -> Result<String, CoreError> {
    match format {
        OutputFormat::Table => format_table(result),
        OutputFormat::Json => format_json(result),
        OutputFormat::Csv => format_csv(result),
        OutputFormat::Yaml => format_yaml(result),
        OutputFormat::Raw => format_raw(result),
        OutputFormat::Markdown => format_markdown(result),
        OutputFormat::Jsonl => format_jsonl(result),
        OutputFormat::Html => format_html(result),
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

fn format_markdown(result: &QueryResult) -> Result<String, CoreError> {
    if result.columns.is_empty() {
        return Ok("(no columns)\n".into());
    }
    let mut out = String::new();
    // Header row.
    out.push_str("| ");
    let headers: Vec<String> = result
        .columns
        .iter()
        .map(|c| escape_md_cell(&c.name))
        .collect();
    out.push_str(&headers.join(" | "));
    out.push_str(" |\n");
    // Separator row.
    out.push_str("| ");
    let seps: Vec<&str> = result.columns.iter().map(|_| "---").collect();
    out.push_str(&seps.join(" | "));
    out.push_str(" |\n");
    // Data rows.
    for row in &result.rows {
        out.push_str("| ");
        let cells: Vec<String> = row
            .iter()
            .map(|v| {
                if matches!(v, Value::Null) {
                    String::new()
                } else {
                    escape_md_cell(&cell_string(v))
                }
            })
            .collect();
        out.push_str(&cells.join(" | "));
        out.push_str(" |\n");
    }
    Ok(out)
}

fn escape_md_cell(s: &str) -> String {
    // 1. Escape pipes (must come before newline → <br> so we don't accidentally
    //    escape pipes introduced by <br> — there aren't any, but the order keeps
    //    the transformation easy to reason about).
    let mut out = s.replace('|', "\\|");
    // 2. Newlines → <br>. Replace \r\n BEFORE \n so we don't leave a stray \r.
    out = out.replace("\r\n", "<br>").replace('\n', "<br>");
    // 3. Leading / trailing space runs → &nbsp; per space (preserve whitespace
    //    that GFM would otherwise collapse).
    let trimmed = out.trim_matches(' ');
    let leading = out.len() - out.trim_start_matches(' ').len();
    let trailing = out.len() - out.trim_end_matches(' ').len();
    let mut padded = String::with_capacity(out.len() + (leading + trailing) * 5);
    for _ in 0..leading {
        padded.push_str("&nbsp;");
    }
    padded.push_str(trimmed);
    for _ in 0..trailing {
        padded.push_str("&nbsp;");
    }
    padded
}

fn format_jsonl(result: &QueryResult) -> Result<String, CoreError> {
    if result.rows.is_empty() {
        return Ok(String::new());
    }
    let mut out = String::new();
    for row in &result.rows {
        let mut obj = serde_json::Map::new();
        for (col, val) in result.columns.iter().zip(row.iter()) {
            obj.insert(col.name.clone(), json_value(val));
        }
        let line = serde_json::to_string(&serde_json::Value::Object(obj))
            .map_err(|e| CoreError::QueryFailed(e.to_string()))?;
        out.push_str(&line);
        out.push('\n');
    }
    Ok(out)
}

fn format_html(result: &QueryResult) -> Result<String, CoreError> {
    let mut out = String::new();
    out.push_str("<table>\n<thead>\n<tr>\n");
    for col in &result.columns {
        out.push_str("<th>");
        out.push_str(&html_escape(&col.name));
        out.push_str("</th>\n");
    }
    out.push_str("</tr>\n</thead>\n<tbody>\n");
    for row in &result.rows {
        out.push_str("<tr>\n");
        for v in row {
            if matches!(v, Value::Null) {
                out.push_str("<td></td>\n");
            } else {
                out.push_str("<td>");
                out.push_str(&html_escape(&cell_string(v)));
                out.push_str("</td>\n");
            }
        }
        out.push_str("</tr>\n");
    }
    out.push_str("</tbody>\n</table>\n");
    Ok(out)
}

fn html_escape(s: &str) -> Cow<'_, str> {
    if !s
        .bytes()
        .any(|b| matches!(b, b'&' | b'<' | b'>' | b'"' | b'\''))
    {
        return Cow::Borrowed(s);
    }
    let mut out = String::with_capacity(s.len() + 8);
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            other => out.push(other),
        }
    }
    Cow::Owned(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::value::{ColumnInfo, TypeHint, Value};

    fn col(name: &str) -> ColumnInfo {
        ColumnInfo {
            name: name.into(),
            type_hint: TypeHint::String,
            nullable: true,
        }
    }

    fn qr(cols: Vec<&str>, rows: Vec<Vec<Value>>) -> QueryResult {
        QueryResult {
            columns: cols.into_iter().map(col).collect(),
            rows,
        }
    }

    #[test]
    fn parse_markdown_and_aliases() {
        assert_eq!(OutputFormat::parse("markdown"), Some(OutputFormat::Markdown));
        assert_eq!(OutputFormat::parse("md"), Some(OutputFormat::Markdown));
        assert_eq!(OutputFormat::parse("MD"), Some(OutputFormat::Markdown));
        assert_eq!(OutputFormat::parse("jsonl"), Some(OutputFormat::Jsonl));
        assert_eq!(OutputFormat::parse("ndjson"), Some(OutputFormat::Jsonl));
        assert_eq!(OutputFormat::parse("JSONL"), Some(OutputFormat::Jsonl));
        assert_eq!(OutputFormat::parse("html"), Some(OutputFormat::Html));
        assert_eq!(OutputFormat::parse("HTML"), Some(OutputFormat::Html));
    }

    #[test]
    fn parse_unknown_still_none() {
        assert_eq!(OutputFormat::parse("xml"), None);
    }

    #[test]
    fn markdown_happy_path() {
        let result = qr(
            vec!["id", "name"],
            vec![
                vec![Value::Int64(1), Value::String("alice".into())],
                vec![Value::Int64(2), Value::String("bob".into())],
                vec![Value::Int64(3), Value::String("carol".into())],
            ],
        );
        let out = format_result(&result, OutputFormat::Markdown).unwrap();
        assert_eq!(
            out,
            "| id | name |\n| --- | --- |\n| 1 | alice |\n| 2 | bob |\n| 3 | carol |\n",
        );
    }

    #[test]
    fn markdown_escapes_pipe_and_newline() {
        let result = qr(
            vec!["c"],
            vec![vec![Value::String("a|b\nc".into())]],
        );
        let out = format_result(&result, OutputFormat::Markdown).unwrap();
        assert!(out.contains("a\\|b<br>c"));
        // Also verify \r\n becomes a single <br> (not <br>\r or \r<br>).
        let result_crlf = qr(
            vec!["c"],
            vec![vec![Value::String("a\r\nb".into())]],
        );
        let out_crlf = format_result(&result_crlf, OutputFormat::Markdown).unwrap();
        assert!(out_crlf.contains("a<br>b"));
        assert!(!out_crlf.contains('\r'));
    }

    #[test]
    fn markdown_empty_columns_emits_note() {
        let result = qr(vec![], vec![]);
        let out = format_result(&result, OutputFormat::Markdown).unwrap();
        assert_eq!(out, "(no columns)\n");
    }

    #[test]
    fn markdown_zero_rows_emits_header_only() {
        let result = qr(vec!["a", "b"], vec![]);
        let out = format_result(&result, OutputFormat::Markdown).unwrap();
        assert_eq!(out, "| a | b |\n| --- | --- |\n");
    }

    #[test]
    fn jsonl_each_line_parses_independently() {
        let result = qr(
            vec!["id", "name"],
            vec![
                vec![Value::Int64(1), Value::String("alice".into())],
                vec![Value::Int64(2), Value::Null],
                vec![Value::Int64(3), Value::String("carol".into())],
            ],
        );
        let out = format_result(&result, OutputFormat::Jsonl).unwrap();
        // Exactly N newlines for N rows.
        assert_eq!(out.matches('\n').count(), 3);
        // Each line is valid JSON.
        for line in out.lines() {
            let parsed: serde_json::Value = serde_json::from_str(line).unwrap();
            assert!(parsed.is_object());
        }
        // NULL → JSON null.
        assert!(out.contains("\"name\":null"));
    }

    #[test]
    fn jsonl_zero_rows_returns_empty_string() {
        let result = qr(vec!["id"], vec![]);
        let out = format_result(&result, OutputFormat::Jsonl).unwrap();
        assert_eq!(out, "");
    }

    #[test]
    fn html_escapes_owasp_set() {
        let result = qr(
            vec!["c"],
            vec![vec![Value::String(
                "<script>alert('Tom & Jerry')</script>".into(),
            )]],
        );
        let out = format_result(&result, OutputFormat::Html).unwrap();
        assert!(!out.contains("<script>"));
        assert!(out.contains("&lt;script&gt;"));
        assert!(out.contains("&amp;"));
        assert!(out.contains("&#39;"));
    }

    #[test]
    fn html_table_shape() {
        let result = qr(
            vec!["id", "name"],
            vec![
                vec![Value::Int64(1), Value::String("alice".into())],
                vec![Value::Int64(2), Value::Null],
            ],
        );
        let out = format_result(&result, OutputFormat::Html).unwrap();
        assert_eq!(out.matches("<table>").count(), 1);
        assert_eq!(out.matches("<thead>").count(), 1);
        assert_eq!(out.matches("<tbody>").count(), 1);
        // NULL → empty <td></td>.
        assert!(out.contains("<td></td>"));
        // Final \n after </table>.
        assert!(out.ends_with("</table>\n"));
    }
}
