# Design Rules

## Core Design Philosophy

- Verified: `ferrule-sql` is an embeddable synchronous SQL core with bounded-memory streaming paths. Evidence: `ferrule-sql/src/lib.rs`, `connection.rs`, `stream.rs`, `guard.rs`.
- Strong inference: The CLI is a thin command/UX layer that should not absorb driver or neutral SQL primitives. Evidence: crate split and `ferrule-core/src/lib.rs`.

## Dependency Direction Rules

- Verified: `ferrule-sql` must not depend on `ferrule-config` or credential prompting/keyring libraries.
- Verified: `ferrule-core` may depend on `ferrule-sql` and `ferrule-config`.
- Verified: `ferrule-cli` may depend on all workspace crates.
- Verified: `ferrule-config` and `ferrule-cli` depend on sibling `../../hasp/crates/hasp`; local builds require `../hasp`.
- Verified: `ferrule-sql` default features are empty.
- Verified: `ferrule-sql` uses edition 2024 and MSRV 1.91; other workspace crates use edition 2021 and workspace rust-version 1.75.

## Public API Rules

- Verified: Keep `ferrule-sql` public API synchronous; do not expose public async functions without approval.
- Verified: `query_cursor` borrows the connection for cursor lifetime; do not design APIs that require using the same connection concurrently while a cursor is live.
- Strong inference: New backend functionality should expose neutral data through `Value`, `Row`, `ColumnInfo`, `QueryResult`, and `Connection`.
- Verified: `ConnectOptions::password` takes precedence over URL password.

## Error Handling Rules

- Verified: CLI error categories are explicit and map to stable exit codes in `ferrule-cli/src/error.rs`.
- Verified: Do not add blanket `From<SqlError>` conversions to `CliError`; call sites choose connection/query/usage semantics.
- Verified: `BulkUnavailable` is distinct from fatal SQL errors because `BulkMode::Auto` can fall back to generic insert paths.
- Strong inference: Result-notable exit code 1 is success-with-gate-worthy-result, not an ordinary error.

## State, Ownership, And Concurrency Rules

- Verified: Normal CLI dispatch intentionally avoids a top-level Tokio runtime.
- Verified: `ferrule-sql` connection handles own private current-thread runtimes.
- Verified: daemon/watch/signal paths create runtimes at edges.
- Strong inference: Async hosts embedding `ferrule-sql` should use blocking threads rather than nesting runtimes.

## Testing Rules

- Verified: CI requires format, clippy default/all-features, build, test default/all-features, and docs.
- Strong inference: Focused package tests are appropriate while developing, but workspace tests are expected before broad success claims.
- Strong inference: Backend integration tests may skip or require containers/Oracle client/SSH setup; document environment assumptions.

## Performance Rules

- Verified: Use `query_cursor`/streaming paths for large reads.
- Verified: Eager `query` is protected by `SizeGuards` but still materializes rows.
- Verified: `write_rows` and copy paths batch writes.
- Strong inference: Native bulk paths are performance features, but conflict/upsert modes can force generic SQL.

## Documentation Rules

- Verified: edit mdBook source under `docs/src`, not generated `docs/book`.
- Strong inference: `docs/internal` is planning/historical context; confirm against current manifests/source.
- Verified: update `doc/ai/AI_CHANGELOG.md` when architecture facts change.

## Never Do This Unless Explicitly Approved

- Add dependencies or change backend dependency features.
- Modify `Cargo.lock`.
- Change CLI exit codes or error classification.
- Add credential resolution to `ferrule-sql`.
- Expose public async APIs from `ferrule-sql`.
- Store raw passwords in history/cache/logs/docs examples.
- Edit generated `docs/book`.
- Treat `reserve/` as an active workspace crate.

## Ask The Human Before Changing These Areas

- Public `ferrule-sql` API.
- Backend feature matrix or C-free dependency firewall.
- Password, keyring, SSH, proxy, history, cache, or telemetry semantics.
- CLI command names, aliases, flags, or exit codes.
- `hasp` path dependency.
- Rust edition/MSRV.
