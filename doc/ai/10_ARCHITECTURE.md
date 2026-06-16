# Architecture

## Project Overview

Verified: Ferrule is a Rust Cargo workspace for a database query CLI and an embeddable SQL core. The product-facing `README.md` describes `ferrule` as a CLI for querying relational databases, and root `Cargo.toml` lists four workspace members: `ferrule-sql`, `ferrule-core`, `ferrule-config`, and `ferrule-cli`.

## Major Subsystems

### `ferrule-sql`

Verified: Embeddable SQL core. It owns `DatabaseUrl`, `Backend`, neutral `Value`/`Row`/`ColumnInfo` types, synchronous `Connection`, backend implementations, proxy/SSH transport types, `RowCursor`, `SizeGuards`, copy, and batched write APIs. Evidence: `ferrule-sql/src/lib.rs`, `ferrule-sql/src/connection.rs`, `ferrule-sql/src/backends/*.rs`.

### `ferrule-core`

Verified: CLI-support layer above `ferrule-sql`. It owns result formatting, dump/load, migrations, EXPLAIN wrapping, parameter substitution, redaction, and connection resolution. Evidence: `ferrule-core/src/lib.rs`.

### `ferrule-config`

Verified: Configuration, connection registry, credential resolution, profiles, bookmarks, and parsing. Evidence: `ferrule-config/src/lib.rs`, `profile.rs`, `registry.rs`, `credentials.rs`, `bookmarks.rs`, `parse.rs`.

### `ferrule-cli`

Verified: Binary crate for `ferrule`, clap command tree, output, daemon, REPL, watch, history/cache/bench, SSH key/flag handling, optional TUI. Evidence: `ferrule-cli/src/main.rs`, `ferrule-cli/src/commands/mod.rs`.

### Product Docs And CI

Verified: Product docs are mdBook source in `docs/src`; generated output is `docs/book`. CI in `.github/workflows/ci.yml` runs format, clippy, build, test, docs, `cargo deny`, and a cargo-tree C-free firewall.

## Data And Control Flow

Strong inference:

1. User invokes `ferrule` CLI.
2. `ferrule-cli/src/main.rs` parses `Cli` and dispatches a `Commands` variant.
3. Command handlers load `GlobalConfig`, resolve connection names or URLs through `ferrule-core::resolver` and `ferrule-config`.
4. Resolved `DatabaseUrl`, password `SecretString`, SSH config, and proxy config are passed to `ferrule-sql`.
5. `ferrule-sql` dispatches by URL scheme/backend feature, builds a synchronous `Connection`, and runs query/execute/cursor/copy/write operations.
6. `ferrule-core` formats or transforms results.
7. `ferrule-cli` writes output, maps errors to stable exit codes, and records best-effort history/cache metadata.

## Important Boundaries

- Verified: `ferrule-sql` intentionally has no `ferrule-config` dependency and no credential resolution.
- Verified: `ferrule-core` depends on `ferrule-sql` and `ferrule-config`.
- Verified: `ferrule-cli` depends on all workspace crates and owns command dispatch.
- Verified: Product docs source is `docs/src`; `docs/book` is generated.
- Strong inference: `docs/internal` is context/planning, not canonical behavior when source and manifests disagree.

## Public API Surfaces

- CLI: `ferrule` binary and subcommands in `ferrule-cli/src/main.rs`.
- SQL library: `ferrule_sql::{connect, Backend, DatabaseUrl, ConnectOptions, Connection, RowCursor, SizeGuards, Value, write_rows, copy_rows, copy_all_tables}`.
- Core support: `ferrule_core::{format_result, dump_query, dump_table, load_data, explain_sql, substitute, redact_sql}`.
- Config: `ferrule_config::{GlobalConfig, ConnectionRegistry, BookmarkStore, resolve_password_stack}`.

## Ownership, State, And Concurrency

- Verified: `ferrule-sql` public API is synchronous; async drivers are hidden behind private current-thread runtimes.
- Verified: Normal CLI dispatch avoids an outer Tokio runtime to prevent nested runtime errors.
- Verified: Daemon/watch/signal paths create local runtimes at edges.
- Strong inference: `query_cursor` is the intended path for large reads because eager `query` buffers rows under `SizeGuards`.
- Verified: `ferrule-cli` history and cache are best-effort; failures are swallowed for normal command success.

## Configuration, Serialization, And Resource Loading

- Verified: `ferrule-config` loads optional TOML config from explicit path, `./.ferrule.toml`, then user config dir.
- Verified: config/profile structs use `serde(deny_unknown_fields)`.
- Verified: registry and bookmarks are TOML-backed.
- Verified: history/cache use SQLite via `rusqlite`.
- Verified: `hasp` is a sibling path dependency used for credential resolution.

## Error Handling

- Verified: `ferrule-sql` returns `SqlError`.
- Verified: `ferrule-cli` maps errors to `CliError` categories and stable exit codes: notable result 1, usage 2, connection 3, query 4.
- Verified: CLI call sites intentionally choose error category; library errors are not blanket-converted.

## Extension Boundaries

- Backend features in `ferrule-sql`: `postgres`, `mysql`, `mssql`, `sqlite`, `oracle`, `ssh`.
- CLI features in `ferrule-cli`: default backends, opt-in `oracle`, opt-in `ssh`, optional `tui`.
- Strong inference: New backend work should start in `ferrule-sql`, then flow through `ferrule-core` and CLI only where needed.

## Open Question Routing

Current unresolved architecture questions are tracked in `doc/ai/80_OPEN_QUESTIONS.md`. Check that registry before changing crate-boundary docs, README dependency claims, or `docs/internal` interpretation.
