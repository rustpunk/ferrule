# Plan: Ferrule Workflow Automation (Preflight, Config Sentinel, Gap Hunter)

**Goal:** Eliminate three daily friction points in the Ferrule development workflow by shipping small CLI subcommands that gate what developers should already be running manually.

**Current Context:**
- Ferrule is a three-crate Rust workspace (`ferrule-core`, `ferrule-config`, `ferrule-cli`).
- Build/test ritual from `CLAUDE.md`: `cargo fmt --all`, `cargo clippy --workspace -- -D warnings`, `cargo test --workspace`, `cargo check --benches --workspace`, `cargo deny check`.
- Config profiles use `toml` deserialization **without** `deny_unknown_fields` — typos in `.ferrule.toml` silently default to `None`.
- `ferrule-config` has zero tests across its 6 source files.
- `spall-core/src/loader.rs` has stale Wave 1 URL rejection logic despite Wave 3 completion.

---

## Step 1: `ferrule preflight` — Pre-commit Gauntlet

**What it does:** One subcommand that runs the full manual pre-commit ritual, fails fast, and prints a unified report.

**Files to change:**
- `ferrule-cli/src/commands/preflight.rs` — new file
- `ferrule-cli/src/commands/mod.rs` — register PreflightArgs + pub use
- `ferrule-cli/src/main.rs` — add Commands::Preflight variant

**Tests:**
- `ferrule-cli/tests/integration_preflight.rs` — exit-code assertions for each failure path

**Implementation approach:**
1. `PreflightArgs` with `--stages` (default: `fmt,clippy,test,benches,deny`), `--fix` flag.
2. `tokio::process::Command` spawns each stage sequentially.
3. Progress output with `crossterm` styling: `[✓] fmt`, `[✗] clippy — 3 warnings`, etc.
4. Exit code: 0 = all green, 1 = any stage failed.

**Open question:** Should `--fix` run `cargo clippy --fix` and `cargo fmt` automatically, or just suggest the commands? → **Decision:** suggest only; fixes are dangerous without review.

---

## Step 2: `ferrule config check` — Typo Sentinel  

**What it does:** Validates `.ferrule.toml` (or explicit path) with `deny_unknown_fields` enabled via a temporary strict parse.

**Files to change:**
- `ferrule-config/src/profile.rs` — add `check` module or free function
- `ferrule-cli/src/commands/mod.rs` — register CheckConfigArgs
- `ferrule-cli/src/main.rs` — add Commands::CheckConfig variant

**Implementation approach:**
1. Deserialize the file with `serde::Deserialize` into a strict mirror struct that has `#[serde(deny_unknown_fields)]`.
2. On success: print "Valid" and list discovered profiles + connections.
3. On error: print the `serde::de::Error` path so "ssh_hots" → "unknown field `ssh_hots`" surfaces immediately.

**Dependency addition:** none — uses existing `toml` + `serde` in `ferrule-config`.

**Tests:**
- `ferrule-config/tests/strict_config_check.rs` — pass/fail TOML fixtures in `tests/fixtures/config/`.

---

## Step 3: `ferrule test --watch --gap` (Zero-Coverage Hunter)

**What it does:** Watches `ferrule-core/src/**/*.rs`; on change, identifies which backend module has zero tests, and offers to scaffold a test skeleton.

**Files to change:**
- `ferrule-cli/src/commands/test.rs` — new file (exists? check)
- `ferrule-cli/src/commands/mod.rs` — register TestArgs
- `ferrule-cli/src/main.rs` — add variant
- If `ferrule-cli/src/commands/test.rs` already exists, extend it.

**Implementation approach:**
1. `--watch` mode uses `notify` crate (already in `ferrule-cli` deps).
2. `--gap` scans `ferrule-core/src/**/*.rs` using a glob + regex heuristic for `mod tests` blocks.
3. Report: file path + suggested `#[cfg(test)]` skeleton with `async fn test_connect` stubs.

**Open question:** Is `--gap` better as a standalone `ferrule gap` subcommand rather than a `test --gap` flag? → **Decision:** keep it as `test --gap` for discoverability, but also support `ferrule test --gap-only`.

---

## Risks / Tradeoffs

- Preflight may duplicate CI logic; gate it behind a `ci` feature if binary size matters.
- Config sentinel requires keeping the strict mirror struct in sync with profile.rs — add a CI check or codegen note.
- Gap hunter globbing may hit edge cases; pin to `src/**/*.rs` and ignore `tests/` inside.

## Validation (post-implementation)

```bash
# After each step:
cargo build --workspace
cargo test --workspace
cargo clippy --workspace -- -D warnings
# Preflight should now catch what clippy missed.
```
