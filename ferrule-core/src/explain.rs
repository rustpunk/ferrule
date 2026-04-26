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
/// Checks the first keyword (case‑insensitive) against a blocklist.
pub fn is_modifying(sql: &str) -> bool {
    let first = sql
        .split_whitespace()
        .next()
        .unwrap_or("")
        .to_ascii_uppercase();
    matches!(
        first.as_str(),
        "INSERT" | "UPDATE" | "DELETE" | "CREATE" | "DROP" | "ALTER" | "TRUNCATE" | "MERGE"
    )
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
