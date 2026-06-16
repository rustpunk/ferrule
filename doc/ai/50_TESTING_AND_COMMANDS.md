# Testing And Commands

## Required Tools

- Inferred: Rust toolchain compatible with workspace and `ferrule-sql` MSRVs.
- Inferred: `rustfmt` and `clippy`.
- Inferred: sibling `../hasp` checkout at `../../hasp/crates/hasp` relative to crate manifests.
- Inferred on Linux for all-features/CI parity: `libdbus-1-dev`, `pkg-config`, `libssl-dev`.
- Inferred for supply-chain gate: `cargo-deny`.

## Commands Run In This Session

| Command | Status | Notes |
| --- | --- | --- |
| `git status --short` | Verified | Showed pre-existing untracked `.claude/`. |
| `find . -maxdepth 3 -type f ...` | Verified | Inventory only. |
| `cargo metadata --no-deps --format-version 1` | Verified | Confirmed workspace packages/features/targets. |
| `rg ...` and `sed ...` discovery commands | Verified | Read-only inspection. |
| `git diff --stat` | Verified | Empty because new docs are untracked and not staged. |
| `git diff -- AGENTS.md doc/ai` | Verified | Empty for the same untracked-file reason. |
| `git diff --no-index --stat ...` | Verified | Used to summarize untracked new documentation. |
| `git diff --no-index --check ...` | Verified | No whitespace warnings after cleanup. |
| `rg` placeholder/link/stale-reference checks | Verified | No placeholders or markdown links found; stale references only appear as documented stale evidence. |
| `rg -n "[ \t]+$" ...` | Verified | No trailing whitespace found. |

## Fast Check Command

- Inferred: `cargo test -p ferrule-sql --features sqlite`
- Inferred: `cargo test -p ferrule-core`
- Inferred: `cargo test -p ferrule-config`
- Inferred: `cargo test -p ferrule-cli`
- Verified for docs-only syntax: `git diff --check` should be run before completion.

## Full Test Command

- Inferred from CI: `cargo test --workspace`
- Inferred from CI: `cargo test --workspace --all-features`

## Formatting

- Inferred from CI: `cargo fmt --all --check`

## Linting

- Inferred from CI: `cargo clippy --workspace --all-targets -- -D warnings`
- Inferred from CI: `cargo clippy --workspace --all-features --all-targets -- -D warnings`

## Docs

- Inferred from CI: `RUSTDOCFLAGS=-D warnings cargo doc --workspace --no-deps`
- Inferred for product docs: `mdbook build docs` if mdBook is installed and product docs are changed.

## Example And Demo Commands

- Inferred: `cargo run -p ferrule-sql --example embed --features sqlite`
- Inferred: `cargo build -p ferrule-cli --features ferrule-cli/ssh`
- Inferred: `cargo build -p ferrule-cli --features ferrule-cli/tui`

## Benchmark And Performance Commands

- Inferred: no Cargo bench target found.
- Inferred from CLI code/docs: `ferrule query <connection> <sql> --bench N --bench-warmup M`.

## CI And Supply Chain

- Inferred from `.github/workflows/ci.yml`: `cargo deny check`.
- Inferred from CI C-free firewall:
  `cargo tree -p ferrule-sql --no-default-features --features postgres,mysql -i <banned-crate>`.

## Commands Agents Should Run Before Claiming Success

- Docs-only: `git diff --check`, plus link/stale-reference search.
- Rust source changes: relevant focused package tests, then workspace format/lint/test depending on blast radius.
- Dependency/backend changes: all-features clippy/test, docs, cargo-deny, and C-free cargo-tree checks.
- CLI command changes: `cargo test -p ferrule-cli` and targeted command module tests.

## Expensive Or Environment-Dependent Commands

- `cargo test --workspace --all-features`: may compile optional backends and require system libraries.
- Backend integration tests: may need local Postgres/MySQL/MSSQL/Oracle/SSH containers and Oracle Instant Client.
- `cargo deny check`: requires cargo-deny installed and `../hasp` available.
- Product smoke commands may start Docker services; do not run without approval.

## Troubleshooting Notes

- If Cargo cannot read `../../hasp/crates/hasp/Cargo.toml`, clone the sibling `hasp` repo as documented in `CLAUDE.md` and CI comments.
- If all-features builds fail on Linux for keyring/MSSQL-related native libraries, compare CI package install notes.
- If docs contradict manifests, prefer current `Cargo.toml`, `cargo metadata`, and source files.
