# Common Patterns

## Synchronous Public API Over Private Runtime

- Where: `ferrule-sql/src/lib.rs`, `connection.rs`, `sync.rs`, `ferrule-cli/src/main.rs`.
- Why it seems to exist: embedders can use a blocking API without managing async runtime details.
- Copy correctly: keep async drivers behind internal traits/wrappers; avoid top-level CLI runtime nesting.
- Common mistakes: adding public async methods; calling blocking connection methods inside an existing current-thread runtime.
- Evidence: crate-level docs in `ferrule-sql/src/lib.rs`; no-outer-runtime comment in `ferrule-cli/src/main.rs`.

## Caller-Resolved Credentials

- Where: `ferrule-core/src/resolver.rs`, `ferrule-config/src/credentials.rs`, `ferrule-sql/src/connection.rs`.
- Why it seems to exist: `ferrule-sql` remains embeddable and config-free.
- Copy correctly: resolve credentials in config/core/CLI layers, pass `SecretString` in `ConnectOptions`.
- Common mistakes: adding keyring, prompt, or `ferrule-config` dependencies to `ferrule-sql`; logging `expose_secret()` values.
- Evidence: `ConnectOptions::effective_password`, `resolve_password_stack`, resolver comments.

## Explicit CLI Error Categories

- Where: `ferrule-cli/src/error.rs`, command handlers.
- Why it seems to exist: stable exit codes distinguish usage, connection, query, and notable-result outcomes.
- Copy correctly: convert errors at call sites with `CliError::connection`, `CliError::query`, `CliError::usage`, or `CliError::result_notable`.
- Common mistakes: blanket `From<SqlError>` or collapsing connection failures into query failures.
- Evidence: `CliError` comments and `exit` module.

## Best-Effort Telemetry And Cache

- Where: `ferrule-cli/src/main.rs`, `history.rs`, `cache.rs`, `commands/query.rs`.
- Why it seems to exist: user commands should not fail because history/cache side effects fail.
- Copy correctly: swallow telemetry/cache failures after preserving the main command result; redact connection and SQL metadata first.
- Common mistakes: making cache writes fatal; recording `history` reads recursively; storing unredacted URLs.
- Evidence: `record_dispatch`, `HistoryDb`, `CacheDb`, `Snapshot::capture`.

## Feature-Gated Backend Surface

- Where: `ferrule-sql/Cargo.toml`, `ferrule-core/Cargo.toml`, `ferrule-cli/Cargo.toml`.
- Why it seems to exist: keep default/backend capability explicit and allow opt-in Oracle/SSH/TUI behavior.
- Copy correctly: add backend features in `ferrule-sql` first, forward through `ferrule-core`, expose CLI features only if needed.
- Common mistakes: enabling default features casually; folding `tui` into `all`; changing C-free pins without checking `deny.toml`.
- Evidence: feature tables in crate manifests.

## Bounded Reads And Batched Writes

- Where: `ferrule-sql/src/stream.rs`, `guard.rs`, `copy.rs`, `write.rs`, backend `query_stream` implementations.
- Why it seems to exist: database results can be large; failure should be bounded instead of OOM-prone.
- Copy correctly: use `query_cursor` for large results, respect `SizeGuards`, batch writes through existing helpers.
- Common mistakes: replacing cursor streaming with eager `query` for dump/export/copy flows; disabling guards without caller bounds.
- Evidence: `RowCursor`, `SizeGuards`, `DEFAULT_WRITE_BATCH`, copy/write tests.

## Ordered Maps For User-Visible Order

- Where: workspace deps include `indexmap`; config/profile/bookmark and row/column-sensitive code use ordered structures.
- Why it seems to exist: output and configuration order can be user-visible.
- Copy correctly: prefer `IndexMap` when column, config, registry, or bookmark order should be stable.
- Common mistakes: replacing ordered maps with `HashMap` where order matters.
- Evidence: workspace dependency `indexmap`; config structs and SQL value/order handling.

## Generated Documentation Boundary

- Where: `docs/book.toml`, `docs/src/`, `docs/book/`.
- Why it seems to exist: mdBook source/output split.
- Copy correctly: edit `docs/src` and regenerate outside this documentation-only task if requested.
- Common mistakes: hand-editing HTML under `docs/book`.
- Evidence: `docs/book.toml` sets `src = "src"`.
