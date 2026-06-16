# docs Agent Guide

## Purpose

Product and internal documentation for Ferrule.

## Responsibilities

- Maintain mdBook source under `docs/src`.
- Keep generated output under `docs/book` out of manual edits.
- Treat `docs/internal` as planning, backlog, handoff, and historical context.
- Keep product docs aligned with current manifests and source.

## Entry Points

- mdBook config: `book.toml`.
- mdBook source index: `src/SUMMARY.md`.
- Product docs: `src/*.md`.
- Internal context: `internal/`.
- Generated output: `book/`.

## Dependency Rules

Docs changes must not add dependencies or edit lockfiles without approval.

## Invariants

- Edit `docs/src`, not `docs/book`.
- Verify docs against current `Cargo.toml`, `cargo metadata`, and source when in doubt.
- Do not treat old plans or handoffs as current implementation without checking source.

## Common Mistakes

- Hand-editing generated HTML.
- Trusting `ferrule-design.md` or old internal plans over current manifests.
- Treating `reserve/` as active implementation.
- Updating product docs without updating `doc/ai` when architecture facts change.

## Local Commands

- `mdbook build docs` if mdBook is installed and product docs changed.
- `git diff --check` for whitespace/patch sanity.

## Documentation Updates

Update `doc/ai/AI_CHANGELOG.md` and relevant `doc/ai/*.md` when product-doc work discovers architecture drift or resolves open questions.

## Approval Gates

Ask before deleting historical internal docs, regenerating `docs/book`, or making product claims that conflict with current code.

## Evidence

`book.toml`, `src/SUMMARY.md`, `docs/book/`, `docs/internal/`, `.github/workflows/ci.yml`.
