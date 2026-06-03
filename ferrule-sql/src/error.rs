use thiserror::Error;

/// Errors originating in the `ferrule-sql` driver and write-path layer.
///
/// Every backend method, URL parse, connect dispatch, transaction
/// helper, and copy routine returns this type. Variant names and tuple
/// shapes are load-bearing across the workspace (the CLI pattern-matches
/// [`SqlError::QueryFailed`] in several hot paths), so preserve them when
/// editing.
///
/// `RegistryError` is registry/CLI-level rather than driver-level; it
/// rides along here as a deliberate minimal-diff choice during the
/// `ferrule-core` -> `ferrule-sql` extraction and is a candidate for a
/// later relocation to a core-side error type.
#[derive(Error, Debug)]
pub enum SqlError {
    #[error("unsupported URL scheme: {0}")]
    UnsupportedScheme(String),

    #[error("invalid connection URL: {0}")]
    InvalidUrl(String),

    #[error("backend '{0}' is not enabled — recompile with the appropriate feature")]
    BackendDisabled(String),

    #[error("connection failed: {0}")]
    ConnectionFailed(String),

    #[error("query failed: {0}")]
    QueryFailed(String),

    /// Backend-native bulk-load path is not usable at runtime (server
    /// config, missing capability, permission denied, target relation
    /// is not a base table, etc.). Callers may retry on the generic
    /// INSERT path. The string is intended for stderr only; do not
    /// match on it.
    #[error("bulk path unavailable: {0}")]
    BulkUnavailable(String),

    #[error("TLS error: {0}")]
    TlsError(String),

    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    /// Host key mismatch during SSH tunnel setup (always fatal).
    #[cfg(feature = "ssh")]
    #[error("SSH host key mismatch for {host}:{port}")]
    SshHostKeyMismatch { host: String, port: u16 },

    /// Unknown host during SSH tunnel setup (can be TOFU-prompted
    /// interactively by the CLI layer).
    #[cfg(feature = "ssh")]
    #[error("SSH unknown host {host}:{port}")]
    SshUnknownHost {
        host: String,
        port: u16,
        algorithm: String,
        fingerprint: String,
        /// Boxed so this (otherwise large) variant doesn't inflate the
        /// whole `SqlError` enum — every synchronous backend method
        /// returns `Result<_, SqlError>` by value, so an oversized error
        /// variant would bloat every `Ok` path too.
        key: Box<russh::keys::ssh_key::PublicKey>,
    },

    #[error("timeout")]
    Timeout,

    /// A single cell exceeded the configured `max_cell_bytes` guard.
    ///
    /// Raised by the read paths (streaming cursor and eager `query`)
    /// **before** the offending value is retained, so the guard caps
    /// peak memory at one over-budget cell rather than letting a
    /// pathological `bytea` / `TEXT` blow the heap. `row` is the
    /// 0-based row ordinal within the current result; `column` names the
    /// offending column; `size` and `cap` are byte counts.
    #[error(
        "cell too large: row {row}, column {column:?} is {size} bytes, \
         exceeds the {cap}-byte cap (raise SizeGuards::max_cell_bytes, or \
         stream the column out via --format csv/jsonl)"
    )]
    CellTooLarge {
        row: u64,
        column: String,
        size: usize,
        cap: usize,
    },

    /// A whole row's measured byte size exceeded `max_row_bytes`.
    ///
    /// Like [`CellTooLarge`](Self::CellTooLarge) this fires before the
    /// row is appended to any buffer, so an unexpectedly wide row fails
    /// fast instead of being materialized. `row` is the 0-based ordinal;
    /// `size`/`cap` are byte counts.
    #[error(
        "row too large: row {row} is {size} bytes, exceeds the \
         {cap}-byte cap (raise SizeGuards::max_row_bytes, or stream via \
         --format csv/jsonl)"
    )]
    RowTooLarge { row: u64, size: usize, cap: usize },

    /// The running total of buffered row bytes crossed
    /// `max_total_buffered_bytes` while materializing an eager result.
    ///
    /// This is the guard the CLI's eager table path relies on: rather
    /// than collect an unbounded `Vec<Row>` for a huge table, the eager
    /// `query` accumulates a byte tally and aborts once it crosses the
    /// cap. `rows_buffered` is how many rows had been accumulated; `cap`
    /// is the byte ceiling.
    #[error(
        "result too large: buffered {rows_buffered} rows exceeding the \
         {cap}-byte total cap (raise SizeGuards::max_total_buffered_bytes, \
         or stream the result via the cursor API / --format csv/jsonl)"
    )]
    BufferTooLarge { rows_buffered: u64, cap: usize },

    #[error("registry error: {0}")]
    RegistryError(String),
}
