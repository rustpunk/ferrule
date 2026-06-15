# Local Agent Plan

## Recommended Local `AGENTS.md` Locations

| Location | Priority | Why Local Guidance Is Needed | Rules To Include | Local Commands | Risks Without Guidance | Evidence | Confidence |
| --- | --- | --- | --- | --- | --- | --- | --- |
| `ferrule-sql/AGENTS.md` | High | Public embeddable API, backend drivers, own edition/MSRV, dependency firewall, performance-sensitive streaming/copy paths | synchronous public API, no config/credential deps, feature-gated backends, size guards, bulk fallback semantics | `cargo test -p ferrule-sql --features sqlite`; backend feature tests as needed | async API leakage, dependency drift, OOM-prone eager queries, backend feature mistakes | `ferrule-sql/src/lib.rs`, `Cargo.toml`, subagent report | High |
| `ferrule-core/AGENTS.md` | High | Cross-crate support layer where source/docs are easy to confuse after SQL extraction | keep driver primitives in `ferrule-sql`, use formatter/dump/load/migrate/params/redaction boundaries, beware stale older docs | `cargo test -p ferrule-core`; `cargo test -p ferrule-core --features sqlite` | moving backend code back into core, unsupported formatter/dump claims | `ferrule-core/src/lib.rs`, subagent report | High |
| `ferrule-config/AGENTS.md` | High | Credentials/profile registry code has security-sensitive behavior and `hasp` dependency | use `SecretString`, preserve `deny_unknown_fields`, keep profile precedence, do not confuse registry/config/bookmarks | `cargo test -p ferrule-config` | raw secret handling, config docs mismatch, breaking credential resolution | `ferrule-config/src/*.rs`, subagent report | High |
| `ferrule-cli/AGENTS.md` | High | User-facing command behavior, exit codes, telemetry/cache, daemon/runtime/TUI constraints | no outer runtime, explicit `CliError`, best-effort history/cache, SSH/daemon incompatibilities, ratatui pin | `cargo test -p ferrule-cli`; `cargo test -p ferrule-cli --all-features` | exit-code regressions, password leaks, nested runtimes, cache failures becoming fatal | `ferrule-cli/src/main.rs`, `error.rs`, subagent report | High |
| `docs/AGENTS.md` | High | Product docs source/output split and historical internal docs require careful handling | edit `docs/src`, not `docs/book`; treat `docs/internal` as context; sync docs with manifests/source | markdown/link checks; `mdbook build docs` if approved/available | generated output edits, stale docs treated as source of truth | `docs/book.toml`, docs/CI report | High |

## Suggested Creation Batches

1. Batch 1: root `AGENTS.md`, all `doc/ai/*.md`, and crate-root local `AGENTS.md` files.
2. Batch 2: `docs/AGENTS.md` to protect product docs and generated output.
3. Future batch: only add deeper local `AGENTS.md` files if a source-change task repeatedly needs narrower rules.

## Probably Unnecessary Local `AGENTS.md` Locations

- `ferrule-cli/src/commands/`: current crate-level CLI guidance can cover command modules; add only if command-specific churn grows.
- `ferrule-cli/src/tui/`: optional TUI is important, but crate-level guidance can cover it for now.
- `ferrule-sql/src/backends/`: backend rules are shared; add backend-specific guidance later only if backend work becomes frequent.
- `docs/src/`: covered by `docs/AGENTS.md`.
- `docs/internal/`: covered by `docs/AGENTS.md`; do not imply internal plans are canonical.
- `examples/`: low complexity.
- `reserve/`: not a root workspace member; local guidance would overemphasize it.
- `target/`, `reserve/target/`, `.claude/`: do not add guidance; these are generated or external worktree state.
