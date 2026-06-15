# Read This First

## Purpose

This file is the entry point for AI agents working in Ferrule. It explains the status of this documentation, how to read it, and the minimum context required before editing.

## Status

Initial documentation created from read-only repository inventory and read-only subagent discovery on 2026-06-15. Claims are evidence-backed where possible and marked when inferred or uncertain.

## Confidence Terms

- Verified: directly observed in current repository files, metadata, CI config, or commands run in this session.
- Strong inference: not stated as a single rule, but strongly supported by repeated code, comments, tests, or config.
- Hypothesis: plausible but not fully proven by current evidence.
- Open question: unresolved issue that needs maintainer input or a targeted code/doc audit.

## Recommended Reading Order

- Any task: root `AGENTS.md`, then this file.
- CLI behavior: `ferrule-cli/AGENTS.md`, `10_ARCHITECTURE.md`, `30_DESIGN_RULES.md`, `50_TESTING_AND_COMMANDS.md`.
- SQL core/backend behavior: `ferrule-sql/AGENTS.md`, `10_ARCHITECTURE.md`, `40_COMMON_PATTERNS.md`, `60_PERFORMANCE_NOTES.md`.
- Config/credentials/profiles: `ferrule-config/AGENTS.md`, `30_DESIGN_RULES.md`, `70_GLOSSARY.md`.
- Formatting/dump/load/migrate/EXPLAIN/params: `ferrule-core/AGENTS.md`, `40_COMMON_PATTERNS.md`.
- Documentation work: `docs/AGENTS.md`, `20_PROJECT_MAP.md`, `AI_CHANGELOG.md`.

## Minimum Checklist Before Editing Code

- Read root `AGENTS.md`.
- Read the local `AGENTS.md` for the crate or docs area being changed.
- Check `git status --short`.
- Confirm whether the task allows source edits, dependency edits, lockfile edits, commits, pushes, or service startup.
- Identify the relevant package command, not only workspace-wide commands.
- Search for existing patterns before introducing a new one.
- If touching public APIs, backend features, credentials, telemetry, or exit codes, ask before broadening scope.

## Repository Memory Model

- Root `AGENTS.md` is concise auto-loaded guidance.
- Local `AGENTS.md` files specialize rules for crate/doc roots.
- `doc/ai/*.md` is durable, detailed project memory.
- `AI_CHANGELOG.md` records architecture facts and future changes.
- Product docs under `docs/src/` explain user behavior; they are not always canonical for current code if manifests or source disagree.
- Generated `docs/book/` is output, not source.

## Rules For Future AI Agents

- Do not modify application/source code for documentation-only tasks.
- Do not modify `Cargo.lock` unless explicitly approved.
- Do not add dependencies without explicit approval.
- Do not push.
- Do not create a commit unless explicitly approved.
- Prefer verified facts over broad summaries.
- Delete weak claims rather than making them sound confident.
- Mark stale or historical docs as context, not current truth.
- Keep `doc/ai` updated when architecture, command policy, or local invariants change.

## Definition Of Done

- The change is scoped to the user request.
- All touched docs are internally consistent.
- Claims cite evidence by file, symbol, command, or config.
- Commands run are listed as Verified; commands not run are listed as Inferred.
- Open questions are captured in `80_OPEN_QUESTIONS.md`.
- No generated docs or unrelated files were modified.

## Documentation Map

- `README.md`: product-facing overview.
- `CLAUDE.md`: older agent/project notes; useful but partially stale.
- `docs/src/`: mdBook product/user docs.
- `docs/internal/`: planning, backlog, handoffs, and historical implementation notes.
- `doc/ai/`: durable AI onboarding.
- Local `AGENTS.md`: auto-loadable local rules.

## When To Update Which Doc

- New architecture boundary: update `10_ARCHITECTURE.md`, `30_DESIGN_RULES.md`, `AI_CHANGELOG.md`, and any relevant local `AGENTS.md`.
- New crate/module/service: update `20_PROJECT_MAP.md` and `90_LOCAL_AGENT_PLAN.md`.
- New repeated pattern: update `40_COMMON_PATTERNS.md`.
- New command or CI gate: update `50_TESTING_AND_COMMANDS.md`.
- Performance-sensitive change: update `60_PERFORMANCE_NOTES.md`.
- New term: update `70_GLOSSARY.md`.
- Unresolved risk: update `80_OPEN_QUESTIONS.md`.

## Known Limitations

- This initial pass did not execute full Cargo test/lint/doc commands.
- Backend integration behavior can depend on local DB services, Oracle Instant Client, SSH setup, and sibling `../hasp`.
- Older docs and internal plans are mixed current/historical evidence.
- Some product docs may lag code, especially after the `ferrule-sql` extraction.

## First Prompt For A New Codex Session

Read `AGENTS.md`, `doc/ai/00_READ_THIS_FIRST.md`, and the local `AGENTS.md` for the area you will edit. Confirm scope, inspect current `git status --short`, and preserve the repository boundaries documented in `doc/ai/30_DESIGN_RULES.md`.
