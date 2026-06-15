# ferrule-cli Agent Guide

## Purpose

User-facing `ferrule` binary crate: clap command tree, command dispatch, output, daemon, REPL, watch, history/cache/bench, SSH flags/keys, and optional TUI.

## Responsibilities

- Parse CLI arguments and dispatch commands.
- Map config/env/flags into core/config/sql layers.
- Format and route output.
- Map errors to stable exit codes.
- Record history and manage result cache as best-effort side effects.
- Own daemon, REPL, watch, and TUI user experiences.

## Entry Points

- Binary and command enum: `src/main.rs`.
- Command args/runners: `src/commands/*.rs`.
- Errors: `src/error.rs`.
- History/cache/bench: `src/history.rs`, `src/cache.rs`, `src/bench.rs`.
- Daemon/REPL/watch/TUI: `src/daemon.rs`, `src/repl.rs`, `src/watch.rs`, `src/tui/*`.
- SSH: `src/ssh_flags.rs`, `src/ssh_keys.rs`.

## Dependency Rules

- Normal dispatch must not add a top-level Tokio runtime.
- Keep ratatui/crossterm versions aligned with `Cargo.toml` comments.
- Feature `tui` is separate from `all`; do not fold it in casually.

## Invariants

- CLI error category is explicit at call sites.
- History/cache failures must not fail successful commands.
- Redact URLs and SQL secrets before recording telemetry.
- Reject unsupported `--daemon` combinations such as SSH and TUI daemon paths.
- Cache only successful single-statement SELECT-style results.

## Common Mistakes

- Adding blanket error conversions.
- Making cache/history fatal.
- Recording raw passwords.
- Allowing shared and per-side copy flags to silently merge.
- Assuming `schema --daemon` is implemented.
- Running long TUI queries on the event loop without design approval.

## Local Commands

- `cargo test -p ferrule-cli`
- `cargo test -p ferrule-cli --all-features`
- `cargo build -p ferrule-cli --features ferrule-cli/ssh`
- `cargo build -p ferrule-cli --features ferrule-cli/tui`

## Documentation Updates

Update `doc/ai/50_TESTING_AND_COMMANDS.md` for command/CI changes, `30_DESIGN_RULES.md` for exit/runtime/cache rules, and product docs under `docs/src` for user-facing CLI behavior.

## Unclear / Ask Human

Ask before changing exit codes, command names/aliases, telemetry/cache semantics, daemon protocol, SSH behavior, or TUI architecture.

## Evidence

`Cargo.toml`, `src/main.rs`, `src/error.rs`, `src/commands/mod.rs`, `src/commands/query.rs`, `src/cache.rs`, `src/history.rs`, `src/daemon.rs`, `src/repl.rs`, `src/watch.rs`, `src/tui/*`.
