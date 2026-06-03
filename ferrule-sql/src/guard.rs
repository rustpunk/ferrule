//! Constant-memory size guards for the read paths.
//!
//! A pathological result — one 512 MB `bytea`/`TEXT` cell, an
//! unexpectedly wide row, or a huge table the CLI tries to buffer whole
//! for table rendering — can OOM-kill the process. These guards turn
//! that into a clean, *bounded* failure: each cell and row is measured
//! against a cap **before** it is retained, and the eager `query` path
//! keeps a running byte tally so it aborts long before an unbounded
//! `Vec<Row>` exhausts memory.
//!
//! **Constant-memory by construction.** This is deliberately *not* an
//! RSS-accounting budget. [`Value::byte_size`](crate::value::Value::byte_size)
//! is a cheap monotonic proxy; the guards are coarse ceilings checked
//! incrementally so the checker itself never allocates proportional to
//! the data it inspects. A `0` cap means "unlimited" for that dimension.

use crate::error::SqlError;
use crate::value::{ColumnInfo, Value};

/// Per-connection size ceilings applied to every read.
///
/// All three caps are byte counts; `0` disables that dimension. The
/// defaults are generous enough that ordinary OLTP rows never trip them
/// yet still cap a runaway result well short of an OOM:
///
/// - `max_cell_bytes` (default 64 MiB) — a single value's payload.
/// - `max_row_bytes` (default 256 MiB) — one row's summed cell payloads.
/// - `max_total_buffered_bytes` (default 1 GiB) — the running total an
///   eager [`query`](crate::Connection::query) is allowed to materialize
///   before failing with [`SqlError::BufferTooLarge`]. The streaming
///   cursor does **not** apply this total (it never buffers the whole
///   result); it applies only the per-cell and per-row caps.
///
/// Construct with [`SizeGuards::unlimited`] to opt out entirely, or
/// tweak individual fields. The guards are copied into each connection
/// at connect time and consulted on every row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SizeGuards {
    /// Max payload bytes for a single cell; `0` = unlimited.
    pub max_cell_bytes: usize,
    /// Max summed payload bytes for one row; `0` = unlimited.
    pub max_row_bytes: usize,
    /// Max running total an eager `query` may buffer; `0` = unlimited.
    /// Ignored by the streaming cursor, which is bounded by design.
    pub max_total_buffered_bytes: usize,
}

impl Default for SizeGuards {
    fn default() -> Self {
        Self {
            max_cell_bytes: 64 * 1024 * 1024,
            max_row_bytes: 256 * 1024 * 1024,
            max_total_buffered_bytes: 1024 * 1024 * 1024,
        }
    }
}

impl SizeGuards {
    /// Disable every guard. Use only when the caller has its own bound
    /// on result size and wants ferrule-sql to impose none.
    #[must_use]
    pub fn unlimited() -> Self {
        Self {
            max_cell_bytes: 0,
            max_row_bytes: 0,
            max_total_buffered_bytes: 0,
        }
    }

    /// Check one fully-decoded row against the per-cell and per-row
    /// caps. Returns a structured [`SqlError`] naming the offending
    /// row/column the instant a cap is crossed, so the caller never
    /// retains an over-budget row.
    ///
    /// `row_ordinal` is the 0-based position of this row within the
    /// current result (used only for diagnostics). `columns` supplies
    /// the column names for the [`SqlError::CellTooLarge`] message; when
    /// shorter than the row (or empty) the column index is rendered
    /// instead. This performs no allocation proportional to row size —
    /// it sums precomputed [`Value::byte_size`] values.
    ///
    /// [`Value::byte_size`]: crate::value::Value::byte_size
    pub fn check_row(
        &self,
        row_ordinal: u64,
        row: &[Value],
        columns: &[ColumnInfo],
    ) -> Result<(), SqlError> {
        let mut row_total: usize = 0;
        for (i, cell) in row.iter().enumerate() {
            let size = cell.byte_size();
            if self.max_cell_bytes != 0 && size > self.max_cell_bytes {
                return Err(SqlError::CellTooLarge {
                    row: row_ordinal,
                    column: column_label(columns, i),
                    size,
                    cap: self.max_cell_bytes,
                });
            }
            row_total = row_total.saturating_add(size);
            if self.max_row_bytes != 0 && row_total > self.max_row_bytes {
                return Err(SqlError::RowTooLarge {
                    row: row_ordinal,
                    size: row_total,
                    cap: self.max_row_bytes,
                });
            }
        }
        Ok(())
    }

    /// True when this configuration buffers an eager result under a
    /// finite total cap. The eager `query` path uses this to decide
    /// whether to maintain the running byte tally at all.
    #[must_use]
    pub fn caps_total(&self) -> bool {
        self.max_total_buffered_bytes != 0
    }
}

/// Render a column label for a [`SqlError::CellTooLarge`] message:
/// the declared column name when available, otherwise the bare index.
fn column_label(columns: &[ColumnInfo], idx: usize) -> String {
    columns
        .get(idx)
        .map(|c| c.name.clone())
        .unwrap_or_else(|| format!("#{idx}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::value::TypeHint;

    fn col(name: &str) -> ColumnInfo {
        ColumnInfo {
            name: name.to_string(),
            type_hint: TypeHint::Other,
            nullable: true,
        }
    }

    #[test]
    fn unlimited_passes_everything() {
        let g = SizeGuards::unlimited();
        let row = vec![Value::Bytes(vec![0u8; 10_000_000]), Value::Int64(1)];
        g.check_row(0, &row, &[col("blob"), col("n")])
            .expect("unlimited guards never trip");
    }

    #[test]
    fn oversized_cell_fails_with_column_name() {
        let g = SizeGuards {
            max_cell_bytes: 1024,
            max_row_bytes: 0,
            max_total_buffered_bytes: 0,
        };
        let row = vec![Value::Int64(7), Value::String("x".repeat(2048))];
        let err = g
            .check_row(3, &row, &[col("id"), col("payload")])
            .expect_err("oversized cell must fail");
        match err {
            SqlError::CellTooLarge {
                row,
                column,
                size,
                cap,
            } => {
                assert_eq!(row, 3);
                assert_eq!(column, "payload");
                assert_eq!(size, 2048);
                assert_eq!(cap, 1024);
            }
            other => panic!("expected CellTooLarge, got {other:?}"),
        }
    }

    #[test]
    fn oversized_row_fails_even_when_each_cell_fits() {
        // Each cell is 600 bytes (under the 1000-byte cell cap) but the
        // row sum (1200) crosses the 1000-byte row cap.
        let g = SizeGuards {
            max_cell_bytes: 1000,
            max_row_bytes: 1000,
            max_total_buffered_bytes: 0,
        };
        let row = vec![
            Value::String("a".repeat(600)),
            Value::String("b".repeat(600)),
        ];
        let err = g
            .check_row(0, &row, &[col("a"), col("b")])
            .expect_err("oversized row must fail");
        assert!(matches!(err, SqlError::RowTooLarge { row: 0, .. }));
    }

    #[test]
    fn cell_label_falls_back_to_index_without_metadata() {
        let g = SizeGuards {
            max_cell_bytes: 8,
            max_row_bytes: 0,
            max_total_buffered_bytes: 0,
        };
        let row = vec![Value::String("too long".repeat(4))];
        let err = g.check_row(0, &row, &[]).expect_err("must fail");
        match err {
            SqlError::CellTooLarge { column, .. } => assert_eq!(column, "#0"),
            other => panic!("expected CellTooLarge, got {other:?}"),
        }
    }
}
