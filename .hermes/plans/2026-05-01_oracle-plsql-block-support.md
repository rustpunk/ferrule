# Plan — Oracle PL/SQL Block Support in `split_oracle_statements`

**Goal**  
Extend `ferrule-core/src/backends/oracle.rs` so that `execute_multi` correctly
keeps PL/SQL blocks together when splitting a SQL script on semicolons.
After implementation, move the corresponding entry from
`docs/internal/IDEAS.md` to `docs/internal/IDEAS.archive.md`.

---

## Current context

`split_oracle_statements` (refactored in the last audit cycle) already skips
single-quoted strings (`''` escaped), `--` line comments, and `/* */` block
comments.  It does **not** recognise `BEGIN`/`END` boundaries, so a script
such as:

```sql
BEGIN
  INSERT INTO t VALUES (1);
  NULL;
END;
SELECT 1 FROM DUAL;
```
is chopped into five fragments instead of two statements.

The scanner is a byte-level state machine operating on `&str::as_bytes()`.
Extending it to track block/control-structure depth keeps the zero-allocation,
no-regex approach already used for strings and comments.

---

## Proposed approach

1. **Add keyword detection helpers**  
   `matches_keyword(bytes, at, keyword)` — case-insensitive keyword match with
   word-boundary guards (preceding / following chars must not be alphanumeric
   or `_`).  
   `is_non_block_end(bytes, at)` — after a bare `END`, inspect the next
   non-whitespace token; returns `true` for `END IF`, `END LOOP`, and
   `END CASE` (which do **not** close a `BEGIN` block).

2. **Add depth counters**  
   - `block_depth` — incremented by `DECLARE`, `BEGIN`; decremented by bare
     `END` (but not `END IF`/`END LOOP`/`END CASE`).
   - `if_depth` — incremented by `IF`; decremented by `END IF`.
   - `loop_depth` — incremented by `LOOP`, `FOR`, `WHILE`; decremented by
     `END LOOP`.
   - `case_stmt_depth` — incremented by `CASE`; decremented by `END CASE`.
   - `case_expr_depth` — incremented by `CASE`; decremented by bare `END`
     (used for `CASE … END` expressions inside assignments, etc.).

   A semicolon is only a statement terminator when **all** depths are zero.

3. **Update `execute_multi` signature / contract**  
   No change to the public API.  The splitter returns `Result<Vec<&str>, String>`
   exactly as before.

4. **Unit-test matrix**  
   Cover anonymous blocks, `DECLARE` sections, nested blocks, control
   structures inside blocks, case expressions inside blocks, mixed case, and
   `BEGIN`/`END` inside string literals (which must be ignored).  No Oracle
   container required — all tests exercise the splitter directly.

---

## Step-by-step plan

| Step | Task | Files | Time |
|---|---|---|---|
| 1 | Read current `split_oracle_statements` and `execute_multi` to confirm offsets | `ferrule-core/src/backends/oracle.rs` | 5 min |
| 2 | Implement `matches_keyword` and `is_non_block_end` as private `fn` directly below `split_oracle_statements` | `ferrule-core/src/backends/oracle.rs` | 15 min |
| 3 | Extend the scanner arms with depth counters (`block_depth`, `if_depth`, `loop_depth`, `case_stmt_depth`, `case_expr_depth`) | `ferrule-core/src/backends/oracle.rs` | 20 min |
| 4 | Write unit tests (see matrix below) in the existing `tests` module | `ferrule-core/src/backends/oracle.rs` | 25 min |
| 5 | `cargo check -p ferrule-core --features oracle --no-default-features` | – | 5 min |
| 6 | `cargo test -p ferrule-core --features oracle -- split_oracle_statements` | – | 10 min |
| 7 | Run full workspace lint: `cargo clippy --workspace -- -D warnings` + `cargo fmt --all` | – | 10 min |
| 8 | Move the "Oracle PL/SQL block support" entry from `docs/internal/IDEAS.md` to `docs/internal/IDEAS.archive.md`; add a one-line archive note with the date and PR/issue reference | `docs/internal/IDEAS.md`, `docs/internal/IDEAS.archive.md` | 10 min |

Total estimated time: ~100 min.

---

## Files likely to change

- `ferrule-core/src/backends/oracle.rs`  
  - Add `matches_keyword`, `is_block_end`, `is_non_block_end` helpers  
  - Rewrite `split_oracle_statements` loop with depth tracking  
  - Add `#[cfg(test)]` unit tests ~15–20 lines each

- `docs/internal/IDEAS.md` — remove "Oracle PL/SQL block support" section  
- `docs/internal/IDEAS.archive.md` — append archived entry (create file if absent)

---

## Tests / validation

### Unit-test matrix (no Oracle container needed)

| Test name | Input (abbreviated) | Expected statement count |
|---|---|---|
| `test_split_begin_end` | `"BEGIN NULL; END;"` | 1 |
| `test_split_declare_begin_end` | `"DECLARE x INT; BEGIN NULL; END;"` | 1 |
| `test_split_nested_begin` | `"BEGIN BEGIN NULL; END; END;"` | 1 |
| `test_split_end_if_not_block_end` | `"BEGIN IF TRUE THEN NULL; END IF; END;"` | 1 |
| `test_split_end_loop_not_block_end` | `"BEGIN LOOP NULL; END LOOP; END;"` | 1 |
| `test_split_end_case_not_block_end` | `"BEGIN CASE WHEN 1=1 THEN NULL; END CASE; END;"` | 1 |
| `test_split_case_expr_bare_end` | `"BEGIN x := CASE WHEN 1=1 THEN 1 END; END;"` | 1 |
| `test_split_case_insensitive` | `"begin null; end;"` | 1 |
| `test_split_string_ignores_keywords` | `"SELECT 'BEGIN END' FROM DUAL;"` | 1 |
| `test_split_comment_ignores_keywords` | `"/* BEGIN */ SELECT 1;"` | 1 |
| `test_split_multiple_statements` | `"BEGIN NULL; END; SELECT 1;"` | 2 |

### Validation commands

```bash
cargo check -p ferrule-core --features oracle --no-default-features
cargo test -p ferrule-core --features oracle -- split_oracle_statements
cargo clippy --workspace -- -D warnings
cargo fmt --all
cargo test --workspace
```

---

## Risks, trade-offs, and open questions

| Risk | Likelihood | Mitigation |
|---|---|---|
| Double-counting `CASE` (expression vs procedural) | Medium | The `case_expr_depth` vs `case_stmt_depth` heuristic is not perfect.  Document in a comment that complex nested `CASE` mixes may confuse the splitter. |
| `CREATE PROCEDURE … BEGIN … END` still splits on internal `;` before `BEGIN` if `DECLARE` is absent | Low | The `DECLARE` keyword is tracked; `CREATE` + `PROCEDURE` is not.  This is acceptable for the first iteration — stored-unit DDL is usually run as a single statement by the user. |
| Byte-level scanning could mishandle non-ASCII identifiers adjacent to keywords | Very low | Oracle keywords are ASCII; `to_ascii_lowercase()` is safe.  Non-ASCII identifiers use double quotes and are protected by word-boundary checks. |

### Open questions

1. Should `CASE` keyword tracking be omitted in favour of a simpler heuristic
   ("only track `BEGIN/END` and document that `END` inside a CASE expression
   closes the block")?  → **Decision: keep CASE tracking** because the common
   pattern `x := CASE WHEN … END;` inside a block is likely to appear in real
   scripts.
2. Do we need to handle Oracle `
   /` alternative delimiter (SQL*Plus style)?  → **Out of scope for this plan.**
3. Should the splitter live in `ferrule-core` shared code so that `mssql.rs`
   and `mysql.rs` can reuse it?  → **Future refactor**, not part of this plan.

---

## Post-implementation checklist

- [ ] All new tests pass  
- [ ] `cargo clippy --workspace -- -D warnings` is clean  
- [ ] `cargo fmt --all` is a no-op  
- [ ] Entry removed from `docs/internal/IDEAS.md`  
- [ ] Entry appended to `docs/internal/IDEAS.archive.md` with date
  `2026-05-01` and a note referencing the commit / PR  
