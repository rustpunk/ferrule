# Glossary

| Term | Meaning | Where It Appears | Related Terms | Confidence |
| --- | --- | --- | --- | --- |
| Ferrule | Database query CLI and workspace name | `README.md`, `ferrule-cli` | CLI, workspace | High |
| `ferrule` | Binary name | `ferrule-cli/Cargo.toml`, `main.rs` | CLI | High |
| `ferrule-sql` | Embeddable synchronous SQL core crate | `ferrule-sql/src/lib.rs` | `Connection`, `DatabaseUrl` | High |
| `ferrule-core` | CLI-support library above SQL core | `ferrule-core/src/lib.rs` | formatter, dump, migrate | High |
| `ferrule-config` | Config/profile/registry/credential crate | `ferrule-config/src/lib.rs` | `GlobalConfig`, `ConnectionRegistry` | High |
| `ferrule-cli` | Binary crate | `ferrule-cli/src/main.rs` | commands, daemon, REPL | High |
| `DatabaseUrl` | Parsed database URL abstraction | `ferrule-sql/src/url.rs` | `Backend`, redaction | High |
| `Connection` | Public blocking database connection trait | `ferrule-sql/src/connection.rs` | `AsyncConnection`, `SyncConnection` | High |
| `AsyncConnection` | Crate-private async driver trait | `ferrule-sql/src/connection.rs` | backend modules | High |
| `SyncConnection` | Blocking wrapper around async backend | `ferrule-sql/src/sync.rs` | private runtime | High |
| `RowCursor` | Streaming row cursor | `ferrule-sql/src/stream.rs` | `query_cursor`, batch | High |
| `SizeGuards` | Read memory safety caps | `ferrule-sql/src/guard.rs` | `CellTooLarge`, `BufferTooLarge` | High |
| `Value` | Neutral SQL value enum | `ferrule-sql/src/value.rs` | `Row`, `TypeHint` | High |
| `ColumnInfo` | Column metadata | `ferrule-sql/src/value.rs` | `TypeHint` | High |
| `BulkUnavailable` | Bulk path can fall back in auto mode | `ferrule-sql/src/error.rs` | `BulkMode` | High |
| `BulkMode` | Copy native bulk behavior: off/auto/on | `ferrule-sql/src/copy.rs`, CLI args | `CopyFormat` | High |
| `CopyFormat` | Postgres COPY text/binary selection | `ferrule-sql/src/copy.rs`, CLI args | `--copy-format` | High |
| `GlobalConfig` | Loaded TOML config model | `ferrule-config/src/profile.rs` | profiles, defaults | High |
| `ConnectionRegistry` | Named connection registry | `ferrule-config/src/registry.rs` | `connections.toml` | High |
| `BookmarkStore` | Stored SQL bookmark registry | `ferrule-config/src/bookmarks.rs` | bookmark params | High |
| `hasp` | Sibling credential-resolution dependency | `ferrule-config/Cargo.toml`, `ferrule-cli/Cargo.toml` | keyring, env, file | High |
| `CliError` | CLI error category enum | `ferrule-cli/src/error.rs` | exit codes | High |
| Result notable | Successful command with gate-worthy result, exit 1 | `ferrule-cli/src/error.rs` | `--fail-on-empty`, diff | High |
| C-free firewall | Dependency policy banning certain C/system TLS crates in checked graph | `deny.toml`, CI | `cargo deny`, `cargo tree` | High |
| mdBook | Product documentation tool | `docs/book.toml`, `docs/src` | `docs/book` | High |
| `reserve/` | Name-reservation placeholder, not active root workspace member | `reserve/README.md`, root `Cargo.toml` | workspace | Medium |
