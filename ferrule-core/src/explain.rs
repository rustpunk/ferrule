use crate::backend::Backend;
use crate::error::CoreError;

/// Tag indicating the expected output format of an EXPLAIN plan.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExplainOutput {
    Json,
    Text,
    Xml,
}

/// Return the EXPLAIN-wrapped SQL and the expected output format.
///
/// When `analyze` is `true` and the statement is non-modifying, the
/// backend-specific "actual execution" variant is used. For modifying
/// statements `ANALYZE` is stripped to avoid side effects.
pub fn explain_sql(
    sql: &str,
    backend: Backend,
    analyze: bool,
) -> Result<(String, ExplainOutput), CoreError> {
    let trimmed = sql.trim();
    if trimmed.is_empty() {
        return Err(CoreError::QueryFailed("Empty SQL for EXPLAIN".into()));
    }

    let modifying = is_modifying(trimmed);
    let safe = modifying || !analyze;

    match backend {
        #[cfg(feature = "postgres")]
        Backend::Postgres => {
            if safe {
                Ok((
                    format!("EXPLAIN (FORMAT JSON, COSTS) {}", trimmed),
                    ExplainOutput::Json,
                ))
            } else {
                Ok((
                    format!(
                        "EXPLAIN (FORMAT JSON, ANALYZE, BUFFERS, TIMING, COSTS) {}",
                        trimmed
                    ),
                    ExplainOutput::Json,
                ))
            }
        }
        #[cfg(feature = "mysql")]
        Backend::MySql => {
            // MySQL 8 EXPLAIN on DML does not execute; safe and analyze are the same.
            Ok((
                format!("EXPLAIN FORMAT=JSON {}", trimmed),
                ExplainOutput::Json,
            ))
        }
        #[cfg(feature = "sqlite")]
        Backend::Sqlite => Ok((
            format!("EXPLAIN QUERY PLAN {}", trimmed),
            ExplainOutput::Text,
        )),
        #[cfg(feature = "mssql")]
        Backend::MsSql => {
            if safe {
                Ok((
                    format!("SET SHOWPLAN_XML ON; {}; SET SHOWPLAN_XML OFF;", trimmed),
                    ExplainOutput::Xml,
                ))
            } else {
                Ok((
                    format!(
                        "SET STATISTICS XML ON; {}; SET STATISTICS XML OFF;",
                        trimmed
                    ),
                    ExplainOutput::Xml,
                ))
            }
        }
        #[cfg(feature = "oracle")]
        Backend::Oracle => {
            let display_opts = if safe { "" } else { "'','','ALLSTATS LAST'" };
            let wrapped = format!(
                "EXPLAIN PLAN FOR {}; SELECT * FROM TABLE(DBMS_XPLAN.DISPLAY('','',{}));",
                trimmed, display_opts
            );
            Ok((wrapped, ExplainOutput::Text))
        }
        #[allow(unreachable_patterns)]
        _ => {
            // Fallback for any backend that gets here without a specific impl.
            Ok((format!("EXPLAIN {}", trimmed), ExplainOutput::Text))
        }
    }
}

/// Detect whether a SQL statement is modifying (DML/DDL).
///
/// A statement is considered modifying if a DML/DDL keyword appears at
/// top-level (parenthesis depth ≤ 0). This correctly handles `WITH`
/// CTEs that are followed by `INSERT` / `UPDATE` / `DELETE` / `MERGE`
/// — the bug that the previous first-token-only check missed.
pub fn is_modifying(sql: &str) -> bool {
    let mut parenthesis_depth = 0i32;
    for token in sql.split_whitespace() {
        for ch in token.chars() {
            if ch == '(' {
                parenthesis_depth += 1;
            } else if ch == ')' {
                parenthesis_depth -= 1;
            }
        }
        if parenthesis_depth <= 0 {
            let upper = token.to_ascii_uppercase();
            if matches!(
                upper.as_str(),
                "INSERT" | "UPDATE" | "DELETE" | "CREATE" | "DROP" | "ALTER" | "TRUNCATE" | "MERGE"
            ) {
                return true;
            }
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_modifying_insert() {
        assert!(is_modifying("INSERT INTO t VALUES (1)"));
    }

    #[test]
    fn test_is_modifying_update() {
        assert!(is_modifying("update t set x=1"));
    }

    #[test]
    fn test_is_modifying_delete() {
        assert!(is_modifying("  DELETE FROM t"));
    }

    #[test]
    fn test_is_modifying_create() {
        assert!(is_modifying("CREATE TABLE t (id INT)"));
    }

    #[test]
    fn test_is_modifying_not_select() {
        assert!(!is_modifying("SELECT * FROM t"));
    }

    #[test]
    fn test_is_modifying_not_with() {
        assert!(!is_modifying("WITH cte AS (SELECT 1) SELECT * FROM cte"));
    }

    #[test]
    fn test_is_modifying_with_insert() {
        assert!(is_modifying(
            "WITH cte AS (SELECT 1) INSERT INTO t VALUES (1)"
        ));
    }

    #[test]
    fn test_is_modifying_with_update() {
        assert!(is_modifying("WITH cte AS (SELECT 1) UPDATE t SET x = 1"));
    }

    #[test]
    fn test_is_modifying_with_merge() {
        assert!(is_modifying(
            "WITH src AS (SELECT * FROM stg) MERGE INTO t USING src ON t.id = src.id"
        ));
    }

    #[test]
    fn test_is_modifying_with_delete() {
        assert!(is_modifying(
            "WITH cte AS (SELECT id FROM t) DELETE FROM t WHERE id IN (SELECT id FROM cte)"
        ));
    }

    #[test]
    fn test_is_modifying_subselect_not_flagged() {
        // Sub-selects inside parentheses should not be flagged
        assert!(!is_modifying(
            "SELECT * FROM t WHERE id IN (SELECT id FROM u)"
        ));
    }

    #[test]
    fn test_is_modifying_cte_in_subquery_not_flagged() {
        // WITH inside a subquery should not be flagged
        assert!(!is_modifying(
            "SELECT * FROM (WITH cte AS (SELECT 1) SELECT * FROM cte) AS sub"
        ));
    }

    #[cfg(feature = "postgres")]
    #[test]
    fn test_postgres_explain_wrap() {
        let (sql, out) = explain_sql("SELECT 1", Backend::Postgres, false).unwrap();
        assert!(sql.contains("EXPLAIN (FORMAT JSON, COSTS)"));
        assert_eq!(out, ExplainOutput::Json);
    }

    #[cfg(feature = "postgres")]
    #[test]
    fn test_postgres_explain_analyze() {
        let (sql, out) = explain_sql("SELECT 1", Backend::Postgres, true).unwrap();
        assert!(sql.contains("ANALYZE"));
        assert_eq!(out, ExplainOutput::Json);
    }

    #[cfg(feature = "postgres")]
    #[test]
    fn test_safe_explain_for_modifying() {
        let (sql, _out) = explain_sql("INSERT INTO t VALUES (1)", Backend::Postgres, true).unwrap();
        assert!(!sql.contains("ANALYZE"));
        assert!(sql.contains("EXPLAIN (FORMAT JSON, COSTS)"));
    }

    #[cfg(feature = "sqlite")]
    #[test]
    fn test_sqlite_explain_wrap() {
        let (sql, out) = explain_sql("SELECT 1", Backend::Sqlite, false).unwrap();
        assert!(sql.contains("EXPLAIN QUERY PLAN"));
        assert_eq!(out, ExplainOutput::Text);
    }
}
