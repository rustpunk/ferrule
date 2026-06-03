//! Backend-aware transaction-control helpers.
//!
//! Lifted from `copy.rs` so that callers outside the copy module
//! (notably the CLI `query` command's `--begin`/`--commit`/`--rollback`
//! wiring) can drive the same BEGIN / COMMIT / ROLLBACK statements
//! without depending on copy.rs internals.
//!
//! The string-statement chosen per backend mirrors what `copy.rs`
//! already issues; Oracle has no explicit BEGIN (implicit txn), so
//! [`begin_transaction`] is a noop that still returns `true` so the
//! caller's wrapping COMMIT/ROLLBACK at the end terminates the
//! implicit transaction.

use crate::backend::Backend;
use crate::connection::Connection;
use crate::error::SqlError;

/// SQL string the [`begin_transaction`] dispatcher will send for a
/// given backend. `None` means "skip the wire round-trip" — Oracle
/// uses this because issuing an explicit `BEGIN` would parse as a
/// PL/SQL block.
fn begin_sql_for(backend: Backend) -> Option<&'static str> {
    match backend {
        #[cfg(feature = "mssql")]
        Backend::MsSql => Some("BEGIN TRANSACTION"),
        #[cfg(feature = "oracle")]
        Backend::Oracle => None,
        _ => Some("BEGIN"),
    }
}

/// SQL string for the [`commit_transaction`] dispatcher.
fn commit_sql_for(backend: Backend) -> &'static str {
    match backend {
        #[cfg(feature = "mssql")]
        Backend::MsSql => "COMMIT TRANSACTION",
        _ => "COMMIT",
    }
}

/// SQL string for the [`rollback_transaction`] dispatcher.
fn rollback_sql_for(backend: Backend) -> &'static str {
    match backend {
        #[cfg(feature = "mssql")]
        Backend::MsSql => "ROLLBACK TRANSACTION",
        _ => "ROLLBACK",
    }
}

/// Open a target-side transaction. Returns `true` if the BEGIN
/// succeeded, `false` if the backend rejected the statement (best-
/// effort: the caller proceeds without a wrapping transaction).
///
/// **Blocking:** issues at most one synchronous `BEGIN` round-trip.
#[must_use]
pub fn begin_transaction(conn: &mut dyn Connection, backend: Backend) -> bool {
    match begin_sql_for(backend) {
        None => true,
        Some(stmt) => conn.execute(stmt).is_ok(),
    }
}

/// Commit the wrapping transaction. **Blocking:** one synchronous
/// `COMMIT` round-trip.
#[must_use = "commit_transaction returns a SqlError on wire failure that the caller must surface"]
pub fn commit_transaction(conn: &mut dyn Connection, backend: Backend) -> Result<(), SqlError> {
    conn.execute(commit_sql_for(backend)).map(|_| ())
}

/// Roll back the wrapping transaction. **Blocking:** one synchronous
/// `ROLLBACK` round-trip.
#[must_use = "rollback_transaction returns a SqlError on wire failure; best-effort callers should still `let _ =`"]
pub fn rollback_transaction(conn: &mut dyn Connection, backend: Backend) -> Result<(), SqlError> {
    conn.execute(rollback_sql_for(backend)).map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn statements_per_backend() {
        // Default backends: BEGIN / COMMIT / ROLLBACK.
        #[cfg(feature = "postgres")]
        {
            let b = Backend::Postgres;
            assert_eq!(begin_sql_for(b), Some("BEGIN"));
            assert_eq!(commit_sql_for(b), "COMMIT");
            assert_eq!(rollback_sql_for(b), "ROLLBACK");
        }
        #[cfg(feature = "mysql")]
        {
            let b = Backend::MySql;
            assert_eq!(begin_sql_for(b), Some("BEGIN"));
            assert_eq!(commit_sql_for(b), "COMMIT");
            assert_eq!(rollback_sql_for(b), "ROLLBACK");
        }
        #[cfg(feature = "sqlite")]
        {
            let b = Backend::Sqlite;
            assert_eq!(begin_sql_for(b), Some("BEGIN"));
            assert_eq!(commit_sql_for(b), "COMMIT");
            assert_eq!(rollback_sql_for(b), "ROLLBACK");
        }

        // MSSQL uses the "TRANSACTION" suffix.
        #[cfg(feature = "mssql")]
        {
            assert_eq!(begin_sql_for(Backend::MsSql), Some("BEGIN TRANSACTION"));
            assert_eq!(commit_sql_for(Backend::MsSql), "COMMIT TRANSACTION");
            assert_eq!(rollback_sql_for(Backend::MsSql), "ROLLBACK TRANSACTION");
        }

        // Oracle: BEGIN is a noop (None); COMMIT/ROLLBACK still send.
        #[cfg(feature = "oracle")]
        {
            assert_eq!(begin_sql_for(Backend::Oracle), None);
            assert_eq!(commit_sql_for(Backend::Oracle), "COMMIT");
            assert_eq!(rollback_sql_for(Backend::Oracle), "ROLLBACK");
        }
    }
}
