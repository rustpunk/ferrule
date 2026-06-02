use crate::backend::Backend;
use crate::error::SqlError;

/// Apply dialect-specific LIMIT / OFFSET paging to a SQL statement.
///
/// Returns the original SQL unchanged if:
/// - both `limit` and `offset` are `None`
/// - the SQL already contains a `LIMIT` or `OFFSET` or `FETCH` clause
/// - the SQL contains multiple statements (`;`)
///
/// # Dialects
///
/// | Backend | Syntax |
/// |---------|--------|
/// | Postgres, SQLite, MySQL | `LIMIT {n} OFFSET {m}` |
/// | MSSQL | `OFFSET {m} ROWS FETCH NEXT {n} ROWS ONLY` |
/// | Oracle (12c+) | `OFFSET {m} ROWS FETCH NEXT {n} ROWS ONLY` |
///
/// For MSSQL and Oracle, if no `ORDER BY` is present, a synthetic
/// `ORDER BY (SELECT NULL)` / `ORDER BY 1` is injected and a warning
/// can be emitted by the caller.
pub fn apply_paging(
    sql: &str,
    limit: Option<usize>,
    offset: Option<usize>,
    backend: Backend,
) -> Result<String, SqlError> {
    let limit = limit.filter(|n| *n > 0);
    let offset = offset.filter(|n| *n > 0);

    if limit.is_none() && offset.is_none() {
        return Ok(sql.to_string());
    }

    let trimmed = sql.trim().trim_end_matches(';').trim();
    if trimmed.is_empty() {
        return Ok(sql.to_string());
    }

    // Refuse multi-statement paging (only if semicolons remain after stripping trailing)
    if trimmed.contains(';') {
        return Err(SqlError::QueryFailed(
            "Multi-statement SQL does not support --limit / --offset.".into(),
        ));
    }

    // Only apply paging to SELECT-like statements
    let first_word = trimmed
        .split_whitespace()
        .next()
        .unwrap_or("")
        .to_uppercase();
    if first_word != "SELECT" && first_word != "WITH" {
        return Ok(sql.to_string());
    }

    // Check for existing paging clauses (case-insensitive word boundary)
    let upper = trimmed.to_uppercase();
    if has_existing_paging(&upper) {
        return Ok(sql.to_string());
    }

    let limit_val = limit.unwrap_or(0);
    let offset_val = offset.unwrap_or(0);

    let paged = match backend {
        #[cfg(feature = "mssql")]
        Backend::MsSql => build_mssql_paging(sql, &upper, limit_val, offset_val)?,
        #[cfg(feature = "oracle")]
        Backend::Oracle => build_oracle_paging(sql, &upper, limit_val, offset_val)?,
        #[cfg(any(feature = "postgres", feature = "mysql", feature = "sqlite"))]
        _ => build_limit_offset_paging(sql, limit_val, offset_val),
        #[allow(unreachable_patterns)]
        _ => build_limit_offset_paging(sql, limit_val, offset_val),
    };

    Ok(paged)
}

fn has_existing_paging(upper: &str) -> bool {
    // Look for LIMIT, OFFSET, or FETCH keywords as whole words
    upper.split_whitespace().any(|word| {
        word == "LIMIT"
            || word == "OFFSET"
            || word == "FETCH"
            || word == "TOP"
            || word.starts_with("LIMIT(")
            || word.starts_with("LIMIT(")
    })
}

fn build_limit_offset_paging(sql: &str, limit: usize, offset: usize) -> String {
    let trimmed = sql.trim().trim_end_matches(';').trim();
    let mut clauses = Vec::new();
    if limit > 0 {
        clauses.push(format!("LIMIT {limit}"));
    } else if offset > 0 {
        // Some backends require LIMIT when OFFSET is present
        clauses.push("LIMIT 18446744073709551615".to_string());
    }
    if offset > 0 {
        clauses.push(format!("OFFSET {offset}"));
    }
    if clauses.is_empty() {
        trimmed.to_string()
    } else {
        format!("{} {}", trimmed, clauses.join(" "))
    }
}

#[cfg(feature = "mssql")]
fn build_mssql_paging(
    sql: &str,
    upper: &str,
    limit: usize,
    offset: usize,
) -> Result<String, SqlError> {
    let needs_order_by = !upper.contains("ORDER BY");
    let trimmed = sql.trim().trim_end_matches(';').trim();
    let mut result = trimmed.to_string();
    if needs_order_by {
        result.push_str(" ORDER BY (SELECT NULL)");
    }
    result.push_str(&format!(" OFFSET {offset} ROWS"));
    if limit > 0 {
        result.push_str(&format!(" FETCH NEXT {limit} ROWS ONLY"));
    }
    Ok(result)
}

#[cfg(feature = "oracle")]
fn build_oracle_paging(
    sql: &str,
    upper: &str,
    limit: usize,
    offset: usize,
) -> Result<String, SqlError> {
    let needs_order_by = !upper.contains("ORDER BY");
    let trimmed = sql.trim().trim_end_matches(';').trim();
    let mut result = trimmed.to_string();
    if needs_order_by {
        result.push_str(" ORDER BY 1");
    }
    result.push_str(&format!(" OFFSET {offset} ROWS"));
    if limit > 0 {
        result.push_str(&format!(" FETCH NEXT {limit} ROWS ONLY"));
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(feature = "postgres")]
    #[test]
    fn test_no_paging_needed() {
        let sql = apply_paging("SELECT 1", None, None, Backend::Postgres).unwrap();
        assert_eq!(sql, "SELECT 1");
    }

    #[cfg(feature = "postgres")]
    #[test]
    fn test_postgres_limit() {
        let sql = apply_paging("SELECT 1", Some(10), None, Backend::Postgres).unwrap();
        assert_eq!(sql, "SELECT 1 LIMIT 10");
    }

    #[cfg(feature = "postgres")]
    #[test]
    fn test_postgres_limit_offset() {
        let sql = apply_paging("SELECT 1", Some(10), Some(5), Backend::Postgres).unwrap();
        assert_eq!(sql, "SELECT 1 LIMIT 10 OFFSET 5");
    }

    #[cfg(feature = "postgres")]
    #[test]
    fn test_postgres_offset_only() {
        let sql = apply_paging("SELECT 1", None, Some(20), Backend::Postgres).unwrap();
        assert_eq!(sql, "SELECT 1 LIMIT 18446744073709551615 OFFSET 20");
    }

    #[cfg(feature = "postgres")]
    #[test]
    fn test_existing_limit_skipped() {
        let sql = apply_paging("SELECT 1 LIMIT 5", Some(10), None, Backend::Postgres).unwrap();
        assert_eq!(sql, "SELECT 1 LIMIT 5");
    }

    #[cfg(feature = "mssql")]
    #[test]
    fn test_mssql_paging() {
        let sql = apply_paging("SELECT * FROM t", Some(10), Some(5), Backend::MsSql).unwrap();
        assert_eq!(
            sql,
            "SELECT * FROM t ORDER BY (SELECT NULL) OFFSET 5 ROWS FETCH NEXT 10 ROWS ONLY"
        );
    }

    #[cfg(feature = "mssql")]
    #[test]
    fn test_mssql_paging_with_order_by() {
        let sql = apply_paging(
            "SELECT * FROM t ORDER BY id",
            Some(10),
            Some(5),
            Backend::MsSql,
        )
        .unwrap();
        assert_eq!(
            sql,
            "SELECT * FROM t ORDER BY id OFFSET 5 ROWS FETCH NEXT 10 ROWS ONLY"
        );
    }

    #[cfg(feature = "oracle")]
    #[test]
    fn test_oracle_paging() {
        let sql = apply_paging("SELECT * FROM t", Some(10), Some(5), Backend::Oracle).unwrap();
        assert_eq!(
            sql,
            "SELECT * FROM t ORDER BY 1 OFFSET 5 ROWS FETCH NEXT 10 ROWS ONLY"
        );
    }

    #[cfg(feature = "postgres")]
    #[test]
    fn test_multistatement_rejected() {
        let result = apply_paging("SELECT 1; SELECT 2", Some(10), None, Backend::Postgres);
        assert!(result.is_err());
    }
}
