# Plan: FE-006 — LIMIT / OFFSET Paging

**Target:** `ferrule-cli/src/commands/query.rs`, `ferrule-cli/src/commands/tables.rs`  
**Crate:** `ferrule-cli`  
**Feature:** default  
**Estimate:** Small  
**Reference Implementation:** `ferrule-cli/src/commands/query.rs`

---

## Why This Matters

Users already pass `--limit` but it truncates *after* fetching all rows. For large tables this wastes bandwidth and memory. Server-side paging pushes `LIMIT`/`OFFSET` into the SQL so the database only returns the requested window.

---

## Architecture

```
ferrule-core/src/query_builder.rs   (NEW)
├─ struct Page { limit: usize, offset: usize }
├─ fn apply_paging(sql: &str, page: Page, backend: Backend) -> String
└─ dialect helpers for LIMIT/OFFSET syntax

ferrule-cli/src/commands/query.rs
└─ inject paging into SQL before execution (when --offset or --limit present)

ferrule-cli/src/commands/tables.rs
└─ same injection for list_tables result set
```

---

## Implementation Checklist

1. **Query Builder Module**
   - New file: `ferrule-core/src/query_builder.rs`
   - `apply_paging(sql: &str, limit: Option<usize>, offset: Option<usize>, backend: Backend) -> String`
   - Detect if SQL already contains `LIMIT`/`OFFSET` (case-insensitive regex) — skip injection to avoid double-clauses

2. **Dialect Mapping**

   | Backend | Syntax |
   |---------|--------|
   | Postgres | `LIMIT {n} OFFSET {m}` |
   | MySQL | `LIMIT {m}, {n}` or `LIMIT {n} OFFSET {m}` |
   | SQLite | `LIMIT {n} OFFSET {m}` |
   | MSSQL | `OFFSET {m} ROWS FETCH NEXT {n} ROWS ONLY` (requires ORDER BY) |
   | Oracle | `OFFSET {m} ROWS FETCH NEXT {n} ROWS ONLY` (12c+) |

3. **MSSQL / Oracle Caveat**
   - If user SQL lacks `ORDER BY`, append `ORDER BY (SELECT NULL)` for MSSQL or `ORDER BY 1` for Oracle
   - Emit a warning on stderr about the synthetic ORDER BY

4. **CLI Args**
   - `OutputFlags` already has `--limit`
   - Add `--offset` to `OutputFlags`

5. **Tables Command**
   - `list_tables()` returns all names; client-side paging is acceptable for now because `information_schema.tables` is small
   - Add `--offset` support to `TablesArgs` (reuses `OutputFlags`)

6. **Verification**
   - [ ] `cargo build --workspace` ✅
   - [ ] `cargo clippy --workspace` ✅
   - [ ] `cargo test --workspace` ✅ (unit tests for each dialect)
   - [ ] No `todo!()` remaining

---

## Integration Tests

```bash
# Postgres
ferrule query "postgres://..." "SELECT * FROM test_users" --limit 1 --offset 1

# MSSQL (synthetic ORDER BY warning)
ferrule query "mssql://..." "SELECT * FROM test_users" --limit 1 --offset 1
```

Unit tests:

```rust
#[test]
fn test_postgres_paging() {
    let sql = apply_paging("SELECT 1", Page { limit: 10, offset: 5 }, Backend::Postgres);
    assert_eq!(sql, "SELECT 1 LIMIT 10 OFFSET 5");
}
```

---

## Cargo.toml

No new dependencies. Regex not needed — use `sql.find("LIMIT")` / `sql.find("offset")` in a simple pre-check.

---

## Risks & Gotchas

1. **Double LIMIT** — If user already wrote `LIMIT 5` and we append another, the query is invalid. The presence check MUST be robust (word boundary check).
2. **MSSQL requires ORDER BY** — `OFFSET/FETCH` is syntax-error without it. Silently injecting `ORDER BY (SELECT NULL)` is safe but may confuse users. Warn on stderr.
3. **Oracle 11g** — `OFFSET/FETCH` is 12c+. For 11g fallback, use `ROWNUM` wrapping. Detect via URL param `oracle_version=11` or just document the limitation.
4. **Multi-statement batches** — `apply_paging` should only touch the final SELECT. For now, refuse paging on strings containing `;` and emit a clear error.

---

## Related Files

- `ferrule-cli/src/commands/query.rs` — SQL execution pipeline
- `ferrule-cli/src/commands/tables.rs` — Table listing pipeline
- `ferrule-cli/src/commands/mod.rs` — `OutputFlags` definition
- `ferrule-core/src/backend.rs` — `Backend` enum for dialect selection

---

*Plan generated after Wave 1 completion.*
