# ferrule-core Agent Guide

## Purpose

CLI-support library above `ferrule-sql`: formatting, dump/load, migrations, EXPLAIN wrapping, parameter substitution, SQL redaction, and connection resolution.

## Responsibilities

- Format `QueryResult` values.
- Dump and load table/query data.
- Run migrations and verify checksums.
- Build backend-specific EXPLAIN SQL.
- Resolve connections using config/registry/credentials.
- Redact SQL secrets before telemetry/history.

## Entry Points

- Public exports: `src/lib.rs`.
- Formatting: `src/formatter.rs`.
- Dump/load: `src/dump.rs`, `src/load.rs`.
- Migrations: `src/migrate.rs`.
- EXPLAIN: `src/explain.rs`.
- Params/redaction/resolution: `src/params.rs`, `src/redact.rs`, `src/resolver.rs`.

## Dependency Rules

- Driver primitives and backend implementations belong in `ferrule-sql`.
- This crate may depend on `ferrule-sql` and `ferrule-config`.
- Backend features forward to `ferrule-sql`; keep forwarding explicit.

## Invariants

- Deterministic dumps require stable ordering; raw deterministic query dumps require `ORDER BY`.
- Migration versions derive from filename text before `_`.
- Migration checksums use SHA-256.
- `EXPLAIN ANALYZE` must not run modifying SQL.
- Redaction is conservative scanning, not a full SQL parser.

## Common Mistakes

- Reintroducing backend drivers here.
- Claiming dump is fully constant-memory without auditing final output buffering.
- Treating MySQL/Oracle DDL rollback semantics as equivalent to Postgres/SQLite without backend-specific verification.
- Treating `redact_sql` as exhaustive secret detection.

## Local Commands

- `cargo test -p ferrule-core`
- `cargo test -p ferrule-core --features sqlite`
- `cargo test -p ferrule-core --all-features`

## Documentation Updates

Update `doc/ai/40_COMMON_PATTERNS.md` when adding reusable formatting/dump/load/migrate patterns. Update `80_OPEN_QUESTIONS.md` if source and product docs disagree.

## Approval Gates

Ask before moving responsibilities between `ferrule-core` and `ferrule-sql`, changing migration semantics, or broadening redaction behavior.

## Evidence

`src/lib.rs`, `src/dump.rs`, `src/formatter.rs`, `src/load.rs`, `src/migrate.rs`, `src/explain.rs`, `src/params.rs`, `src/redact.rs`, `src/resolver.rs`.
