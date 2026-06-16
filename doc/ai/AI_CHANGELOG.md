# AI Changelog

## Purpose

This file is durable architecture/change memory for future agents. Update it when architecture facts, public API boundaries, command policies, dependency policies, or local agent guidance change.

## 2026-06-15 Initial AI Onboarding Set

Created the initial `doc/ai` documentation set and root/local `AGENTS.md` guidance from read-only repository discovery and read-only subsystem subagents.

### Major Architecture Facts Discovered

- The active root workspace has four members: `ferrule-sql`, `ferrule-core`, `ferrule-config`, and `ferrule-cli`.
- `ferrule-sql` is the embeddable SQL core with synchronous public API, backend drivers, streaming cursors, size guards, copy, write, proxy, and SSH transport support.
- `ferrule-core` is now a CLI-support layer above `ferrule-sql`, not the backend-driver crate.
- `ferrule-config` owns profile/registry/bookmark/credential config and depends on sibling `hasp`.
- `ferrule-cli` owns the user-facing binary command tree, error categories, history/cache, daemon, REPL, watch, SSH flags/keys, and optional TUI.
- CI runs format, clippy default/all-features, build, tests default/all-features, docs with warnings denied, cargo-deny, and a C-free cargo-tree firewall.
- Product docs source is `docs/src`; `docs/book` is generated.

### Open Question Routing

Current unresolved questions are tracked in `doc/ai/80_OPEN_QUESTIONS.md`. Keep this changelog focused on dated evidence, resolved uncertainty, and factual documentation maintenance history.
### Update Instructions

Future agents should append new dated entries when:

- crates, modules, or public API boundaries change;
- command, CI, or verification policy changes;
- dependency or feature policy changes;
- local `AGENTS.md` rules are added or removed;
- an open question is resolved.

Do not invent past decisions. If evidence is weak, link to `80_OPEN_QUESTIONS.md` instead of treating it as settled history.
