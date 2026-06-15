# Project Map

## Workspace

Verified in `Cargo.toml`: active workspace members are `ferrule-sql`, `ferrule-core`, `ferrule-config`, and `ferrule-cli`.

| Path | Purpose | Important Files | Internal Dependencies | Confidence |
| --- | --- | --- | --- | --- |
| `ferrule-sql/` | Embeddable SQL core and backend drivers | `src/lib.rs`, `connection.rs`, `sync.rs`, `stream.rs`, `guard.rs`, `copy.rs`, `write.rs`, `backends/*.rs`, `examples/embed.rs` | None on other Ferrule crates | High |
| `ferrule-core/` | CLI-support library above SQL core | `src/lib.rs`, `formatter.rs`, `dump.rs`, `load.rs`, `migrate.rs`, `explain.rs`, `params.rs`, `redact.rs`, `resolver.rs` | `ferrule-sql`, `ferrule-config` | High |
| `ferrule-config/` | Profiles, registry, credentials, bookmarks, parsing | `src/lib.rs`, `profile.rs`, `registry.rs`, `credentials.rs`, `bookmarks.rs`, `parse.rs` | `hasp` sibling path dependency | High |
| `ferrule-cli/` | `ferrule` binary and user-facing commands | `src/main.rs`, `commands/*.rs`, `daemon.rs`, `history.rs`, `cache.rs`, `bench.rs`, `repl.rs`, `watch.rs`, `tui/*` | All workspace crates | High |
| `docs/src/` | mdBook product docs source | `SUMMARY.md`, `configuration.md`, `connections.md`, `security.md`, `reference.md` | N/A | High |
| `docs/book/` | Generated mdBook output | HTML/CSS/JS files | Generated from `docs/src` | High |
| `docs/internal/` | Planning, bugs, ideas, prompts, handoffs | `BUGS.md`, `IDEAS.md`, `plan/*.md`, `handoffs/*.md` | N/A | Medium |
| `.github/workflows/` | CI | `ci.yml` | Cargo, `hasp`, cargo-deny | High |
| `examples/` | Example config files | `config.toml`, `connections.toml` | N/A | High |
| `reserve/` | Name-reservation placeholder, not root workspace member | `reserve/Cargo.toml`, `reserve/README.md` | N/A | Medium |

## External Dependencies With Architectural Weight

- `tokio`: async runtime hidden behind `ferrule-sql` sync API and edge runtimes in CLI daemon/watch.
- `secrecy`: password redaction and zeroize-on-drop handling.
- `hasp`: credential resolution path dependency at `../../hasp/crates/hasp`.
- `rusqlite`: SQLite backend plus CLI history/cache stores.
- `clap`: CLI command surface.
- `miette`: CLI diagnostics.
- `tabled`, `serde_json`, `serde-saphyr`, `csv`: output and data formatting.
- `tokio-postgres`, `mysql_async`, `tiberius`, `oracle`: backend drivers.
- `russh`: optional SSH tunnel transport.
- `ratatui`: optional TUI feature.

## Tests, Examples, And Benches

- Verified: inline Rust tests are present throughout `ferrule-sql`, `ferrule-core`, `ferrule-config`, and `ferrule-cli`.
- Verified: `ferrule-sql/examples/embed.rs` demonstrates embedding.
- Verified: no explicit `[[bench]]` target was found; `ferrule-cli/src/bench.rs` implements CLI `--bench` mode.
- Strong inference: backend integration tests depend on fixed localhost services and skip when unavailable, based on `CLAUDE.md` and CI comments.

## CI And Deployment Config

- `.github/workflows/ci.yml`: format, clippy default/all-features, build, test default/all-features, docs with rustdoc warnings denied, cargo-deny, and C-free cargo-tree check.
- `deny.toml`: C-free firewall, license/advisory/source policy.
- `docs/book.toml`: mdBook config.

## Evidence

- `Cargo.toml`: workspace members, workspace package/dependency versions.
- `cargo metadata --no-deps --format-version 1`: confirmed packages, targets, features, dependencies.
- `ferrule-sql/src/lib.rs`, `ferrule-core/src/lib.rs`, `ferrule-config/src/lib.rs`, `ferrule-cli/src/main.rs`: subsystem entry points.
- `.github/workflows/ci.yml`, `deny.toml`, `docs/book.toml`: CI/docs policy.
