# ferrule-config Agent Guide

## Purpose

Configuration, profile, registry, credential, bookmark, and parser crate.

## Responsibilities

- Load global config from explicit path, `./.ferrule.toml`, or user config dir.
- Manage connection profiles and defaults.
- Manage saved connection registry.
- Resolve credentials through `hasp`.
- Manage bookmarks and bookmark params.
- Parse duration and size strings.

## Entry Points

- Public exports: `src/lib.rs`.
- Profiles/config: `src/profile.rs`.
- Registry: `src/registry.rs`.
- Credentials: `src/credentials.rs`.
- Bookmarks: `src/bookmarks.rs`.
- Parsers/errors: `src/parse.rs`, `src/error.rs`.

## Dependency Rules

- Keep `SecretString` for credentials.
- Keep `hasp` path dependency behavior aligned with workspace/CI.
- Preserve `IndexMap` where order is user-visible.

## Invariants

- Config structs use `serde(deny_unknown_fields)`.
- Profile names take precedence over registry names in resolver behavior.
- `parse_duration("500")` is not globally valid; slow-log threshold has special bare-millisecond behavior.
- Unknown env vars in interpolation remain literal unless fallback syntax is used.

## Common Mistakes

- Confusing global `.ferrule.toml` with `connections.toml`.
- Logging or storing raw passwords.
- Assuming product docs list every current profile field.
- Treating missing explicit config path behavior as accidental without checking tests.

## Local Commands

- `cargo test -p ferrule-config`

## Documentation Updates

Update `doc/ai/30_DESIGN_RULES.md`, `70_GLOSSARY.md`, and `80_OPEN_QUESTIONS.md` when profile, registry, credential, or parser behavior changes.

## Unclear / Ask Human

Ask before changing credential precedence, config discovery, missing-config behavior, or `hasp` usage.

## Evidence

`src/lib.rs`, `src/profile.rs`, `src/registry.rs`, `src/credentials.rs`, `src/bookmarks.rs`, `src/parse.rs`, `Cargo.toml`.
