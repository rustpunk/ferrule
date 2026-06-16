# Ferrule Agent Guide

## Project Summary

Ferrule is a Rust workspace for a database query CLI and embeddable SQL core. The binary is `ferrule`; supported backends are Postgres, MySQL, MSSQL, SQLite, and opt-in Oracle, with opt-in SSH tunnel support.

Start with `doc/ai/00_READ_THIS_FIRST.md`, then use the task-specific docs under `doc/ai/`.

## Repository Layout

- `ferrule-sql/`: embeddable SQL core, backend drivers, synchronous `Connection` API, streaming cursors, copy/write paths.
- `ferrule-core/`: CLI-support library for formatting, dump/load, migrations, EXPLAIN, params, redaction, and connection resolution.
- `ferrule-config/`: profiles, registry, credentials, bookmarks, and config parsing.
- `ferrule-cli/`: `ferrule` binary, clap command tree, daemon, REPL, watch, history/cache/bench, optional TUI.
- `docs/src/`: mdBook source. Do not edit generated `docs/book/` by hand.
- `doc/ai/`: durable AI onboarding and architecture memory.

## Design Rules

- Keep driver primitives and backend implementations in `ferrule-sql`; do not move them into `ferrule-core`.
- Keep `ferrule-sql` public APIs synchronous. Do not expose public async APIs without explicit approval.
- Keep credential resolution outside `ferrule-sql`; use caller-provided `SecretString` through `ConnectOptions`.
- Preserve stable CLI error categories and exit codes in `ferrule-cli/src/error.rs`.
- Treat history/cache as best-effort side effects; they must not block successful user commands.
- Preserve bounded-memory paths: prefer `query_cursor`, batching, and existing copy/write helpers for large data flows.
- Respect the C-free dependency firewall in `deny.toml`; ask before changing backend dependency features.

## Commands

Local Cargo commands require the sibling path dependency `../hasp` to exist.

- Format: `cargo fmt --all --check`
- Lint: `cargo clippy --workspace --all-targets -- -D warnings`
- Full lint: `cargo clippy --workspace --all-features --all-targets -- -D warnings`
- Build: `cargo build --workspace`
- Test: `cargo test --workspace`
- Full feature test: `cargo test --workspace --all-features`
- Docs: `RUSTDOCFLAGS=-D warnings cargo doc --workspace --no-deps`
- Supply chain: `cargo deny check`
- Fast docs-only check: `git diff --check`

## Safety Rules

- Do not edit application/source code when the task is documentation-only.
- Do not modify `Cargo.lock`, add dependencies, start services, push, or commit unless explicitly approved.
- Do not edit `.claude/`, `target/`, `reserve/target/`, or generated `docs/book/` unless the task explicitly targets them.
- Do not store or log raw passwords; redact URLs and SQL secrets before telemetry/history/cache metadata.
- Ask before changing dependency features, backend support, CLI exit codes, credential handling, or public APIs.

## Coding Conventions

- Rust workspace uses edition 2021 and MSRV 1.75, except `ferrule-sql`, which uses edition 2024 and MSRV 1.91.
- Prefer `IndexMap` where user-visible order or column order matters.
- Use existing error types and explicit error categorization.
- Keep tests close to modules unless an integration test is clearly needed.

## Documentation Updates

Update `doc/ai/AI_CHANGELOG.md` when architecture, command policy, dependency policy, or public API boundaries change. Update local `AGENTS.md` files when local invariants change.

Definition of done: scoped docs/source updates, matching tests or rationale, relevant commands run or explicitly marked unverified, and no unsupported claims left in AI docs.
