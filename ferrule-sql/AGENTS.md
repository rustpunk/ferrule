# ferrule-sql Agent Guide

## Purpose

Embeddable SQL core with synchronous public API, URL/backend dispatch, neutral values, backend drivers, streaming cursors, copy/write paths, proxy, and optional SSH tunnel support.

## Responsibilities

- Own `DatabaseUrl`, `Backend`, `Value`, `Row`, `ColumnInfo`, `Connection`, `RowCursor`, `SizeGuards`.
- Implement backend drivers under `src/backends/`.
- Keep credential resolution outside this crate.
- Preserve bounded-memory read/write paths.

## Entry Points

- Public exports: `src/lib.rs`.
- Public connection contract: `src/connection.rs`.
- Runtime wrapper: `src/sync.rs`.
- Connection dispatch: `src/backend.rs`.
- Streaming: `src/stream.rs`.
- Copy/write: `src/copy.rs`, `src/write.rs`.

## Module Map

- `backends/`: Postgres, MySQL, MSSQL, SQLite, Oracle.
- `proxy.rs`, `tunnel.rs`: network transport helpers.
- `guard.rs`: memory caps.
- `query_builder.rs`, `render.rs`, `transaction.rs`, `url.rs`, `value.rs`: supporting primitives.

## Dependency Rules

- Do not add `ferrule-config`, prompting, keyring, or CLI dependencies here.
- Default features stay empty unless explicitly approved.
- Respect `deny.toml` C-free firewall and backend feature gates.
- This crate uses edition 2024 and Rust 1.91.

## Invariants

- Public API stays synchronous.
- `ConnectOptions::password` overrides URL password.
- `query_cursor` borrows the connection until dropped.
- Eager `query` is guarded by `SizeGuards`.
- `BulkUnavailable` can be a fallback signal in auto bulk mode.

## Common Mistakes

- Exposing public async APIs.
- Using eager `query` for unbounded copy/export flows.
- Treating SQLite as native-bulk capable.
- Bypassing quote/render/copy builders.
- Ignoring cursor lifetime or nested-runtime constraints.

## Local Commands

- `cargo test -p ferrule-sql --features sqlite`
- `cargo run -p ferrule-sql --example embed --features sqlite`
- Backend-specific: `cargo test -p ferrule-sql --features postgres|mysql|mssql|oracle|ssh`

## Documentation Updates

Update `doc/ai/10_ARCHITECTURE.md`, `30_DESIGN_RULES.md`, `40_COMMON_PATTERNS.md`, and `60_PERFORMANCE_NOTES.md` for public API, backend, dependency, or performance changes.

## Approval Gates

Ask before changing public APIs, backend features, dependency pins, runtime model, credential model, or size-guard defaults.

## Evidence

`src/lib.rs`, `src/connection.rs`, `src/sync.rs`, `src/stream.rs`, `src/guard.rs`, `src/copy.rs`, `src/write.rs`, `Cargo.toml`.
