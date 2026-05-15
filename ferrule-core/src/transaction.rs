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
use crate::error::CoreError;

/// Open a target-side transaction. Returns `true` if the BEGIN
/// succeeded, `false` if the backend rejected the statement (best-
/// effort: the caller proceeds without a wrapping transaction).
#[must_use]
pub async fn begin_transaction(conn: &mut dyn Connection, backend: Backend) -> bool {
    let stmt = match backend {
        #[cfg(feature = "mssql")]
        Backend::MsSql => "BEGIN TRANSACTION",
        // Oracle starts implicit transactions; an explicit BEGIN here
        // would parse as a PL/SQL block. Skip the statement; the
        // wrapping COMMIT at the end still terminates the implicit txn.
        #[cfg(feature = "oracle")]
        Backend::Oracle => return true,
        _ => "BEGIN",
    };
    conn.execute(stmt).await.is_ok()
}

#[must_use = "commit_transaction returns a CoreError on wire failure that the caller must surface"]
pub async fn commit_transaction(
    conn: &mut dyn Connection,
    backend: Backend,
) -> Result<(), CoreError> {
    let stmt = match backend {
        #[cfg(feature = "mssql")]
        Backend::MsSql => "COMMIT TRANSACTION",
        _ => "COMMIT",
    };
    conn.execute(stmt).await.map(|_| ())
}

#[must_use = "rollback_transaction returns a CoreError on wire failure; best-effort callers should still `let _ =`"]
pub async fn rollback_transaction(
    conn: &mut dyn Connection,
    backend: Backend,
) -> Result<(), CoreError> {
    let stmt = match backend {
        #[cfg(feature = "mssql")]
        Backend::MsSql => "ROLLBACK TRANSACTION",
        _ => "ROLLBACK",
    };
    conn.execute(stmt).await.map(|_| ())
}
