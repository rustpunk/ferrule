# Performance Notes

## `ferrule-sql` Streaming Reads

- Area/module: `ferrule-sql/src/stream.rs`, backend `query_stream` implementations.
- Why sensitive: result sets can be large.
- Existing choices: `RowCursor::next_batch`, `DEFAULT_CURSOR_CAPACITY = 1024`, connection borrow for cursor lifetime.
- Avoid: replacing streaming with eager `query` in large flows.
- Hooks: cursor tests in backend modules, including SQLite/Postgres cursor tests.
- Confidence: High.
- Evidence: `ferrule-sql/src/lib.rs`, `stream.rs`, backend tests.

## Size Guards

- Area/module: `ferrule-sql/src/guard.rs`, `value.rs`.
- Why sensitive: prevents OOM on large cells/rows/eager results.
- Existing choices: per-cell, per-row, and total eager buffer caps; zero means unlimited.
- Avoid: disabling guards by default or ignoring guard errors.
- Hooks: `guard.rs` tests for oversized cells/rows and unlimited guards.
- Confidence: High.
- Evidence: `SizeGuards` docs and tests.

## Copy And Batched Writes

- Area/module: `ferrule-sql/src/copy.rs`, `write.rs`, backend bulk loaders.
- Why sensitive: cross-DB copy can move many rows.
- Existing choices: batch size defaults, native bulk modes, generic fallback, FK topological ordering for all-table copy.
- Avoid: forcing native bulk where conflict/upsert semantics require generic SQL; treating `BulkUnavailable` as fatal in auto mode.
- Hooks: many copy/write tests, backend bulk tests.
- Confidence: High.
- Evidence: `BulkMode`, `CopyFormat`, `write_rows`, `copy_all_tables`, copy tests.

## Postgres COPY Text/Binary

- Area/module: `ferrule-sql/src/backends/postgres.rs`, copy APIs.
- Why sensitive: binary COPY can help numeric/timestamp/UUID-heavy schemas.
- Existing choices: `CopyFormat::Text` and `CopyFormat::Binary`.
- Avoid: using binary mode generically for all data without measurement.
- Hooks: Postgres binary copy tests.
- Confidence: Medium.
- Evidence: CLI copy-format comments and Postgres tests.

## CLI Daemon Pool

- Area/module: `ferrule-cli/src/daemon.rs`.
- Why sensitive: amortizes connection setup and must avoid nested runtime/blocking issues.
- Existing choices: `DashMap`, per-connection `Mutex`, `spawn_blocking`.
- Avoid: sharing connections concurrently without locking; adding nested runtimes.
- Hooks: daemon code and CLI resolver behavior.
- Confidence: Medium.
- Evidence: CLI explorer report and `daemon.rs` module.

## History And Cache Stores

- Area/module: `ferrule-cli/src/history.rs`, `cache.rs`.
- Why sensitive: runs around normal command execution.
- Existing choices: SQLite stores, busy timeout, pruning, best-effort failures.
- Avoid: making cache/history failures fatal; recording unredacted metadata.
- Hooks: history/cache tests.
- Confidence: High.
- Evidence: `record_dispatch`, `HistoryDb`, `CacheDb` tests.

## Dump/Formatter Buffering

- Area/module: `ferrule-core/src/dump.rs`, `formatter.rs`.
- Why sensitive: final output may be accumulated into strings even when data fetch is batched.
- Existing choices: dump fetches cursor batches but can buffer final output.
- Avoid: claiming dump/export is fully constant-memory without auditing output sink behavior.
- Hooks: dump/formatter tests.
- Confidence: Medium.
- Evidence: `dump_query`, formatter tests, internal bug notes.

## Optional TUI

- Area/module: `ferrule-cli/src/tui/*`.
- Why sensitive: long queries can block event-loop responsiveness.
- Existing choices: optional `tui` feature, pure app/input/result modules with terminal edges.
- Avoid: running long synchronous DB work on the event loop without a deliberate design.
- Hooks: TUI pure-module tests.
- Confidence: Medium.
- Evidence: CLI explorer report and TUI module tests.
