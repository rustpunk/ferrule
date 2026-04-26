# Plan: FE-007 — Environment Variable Interpolation

**Target:** `ferrule-config/src/registry.rs`  
**Crate:** `ferrule-config`  
**Feature:** default  
**Estimate:** Small  
**Reference Implementation:** `ferrule-config/src/registry.rs`

---

## Why This Matters

Connection URLs often contain sensitive tokens or hostnames that vary across environments (dev, staging, prod). Hard-coding them in `connections.toml` forces users to maintain separate files per environment. `${VAR}` interpolation lets a single `connections.toml` drive CI, local dev, and production without modification.

---

## Architecture

```
ferrule-config/src/registry.rs
├─ interpolate_env_vars(input: &str) -> String
└─ load_default() calls interpolate_env_vars on each entry.url before parsing
```

---

## Implementation Checklist

1. **Interpolation Engine**
   - Pattern: `${VAR}` or `${VAR:-default}` (Bourne-shell style)
   - Also support `$VAR` for simple cases (optional)
   - Recursive substitution: `${DB_HOST}` resolves first, then `${PORT}` inside the default
   - Leave unknown variables intact (don't error) so partial URLs can be validated later

2. **Registry Integration**
   - In `ConnectionRegistry::load_default()`, after `toml::from_str`, iterate `registry.entries` and call `entry.url = interpolate_env_vars(&entry.url)`
   - Add `interpolate_env_vars` as a public helper for other consumers

3. **Security**
   - Interpolation happens *before* URL parsing, so `DatabaseUrl` validation still applies
   - Never log interpolated URLs at `info` level — the resolved URL may contain secrets
   - `redacted()` continues to mask passwords even after interpolation

4. **CLI/Profile Extension**
   - Apply same interpolation to `url` fields inside `.ferrule.toml` connection profiles (FE-004)
   - Share `interpolate_env_vars` between registry and profile loaders

5. **Verification**
   - [ ] `cargo build --workspace` ✅
   - [ ] `cargo clippy --workspace` ✅
   - [ ] `cargo test --workspace` ✅ (unit tests for substitution patterns)
   - [ ] No `todo!()` remaining

---

## Integration Tests

```rust
#[test]
fn test_interpolate_basic() {
    std::env::set_var("FERRULE_DB", "mydb");
    assert_eq!(
        interpolate_env_vars("postgres://u@h/${FERRULE_DB}"),
        "postgres://u@h/mydb"
    );
}

#[test]
fn test_interpolate_default() {
    std::env::remove_var("MISSING");
    assert_eq!(
        interpolate_env_vars("host=${MISSING:-localhost}"),
        "host=localhost"
    );
}
```

---

## Cargo.toml

No new dependencies. Implement with `std::env` and `regex` (already in dependency tree via other crates). Alternatively, a small hand-rolled parser avoids regex entirely:

```rust
fn interpolate_env_vars(input: &str) -> String {
    let mut out = String::new();
    let mut chars = input.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '$' {
            if chars.next_if_eq(&'{').is_some() {
                let var = chars.by_ref().take_while(|c| *c != '}').collect::<String>();
                let val = std::env::var(&var).unwrap_or_default();
                out.push_str(&val);
            }
        } else {
            out.push(ch);
        }
    }
    out
}
```

---

## Risks & Gotchas

1. **Escaping `$`** — Users who have `$` in passwords need a way to escape. Support `$$` → literal `$`.
2. **Default values with `:`** — `${VAR:-a:b}` ends at first `}` *outside* quotes. Keep parser simple: no nested braces, split on `:-`.
3. **Circular references** — `${A:-${B}}` where `B` contains `${A}`. Cap recursion depth at 10 and return the unresolved string.
4. **Windows env var syntax** — `%VAR%` is Windows native. Document that ferrule uses Unix `${VAR}` everywhere for consistency.

---

## Related Files

- `ferrule-config/src/registry.rs` — Load & interpolate URLs
- `ferrule-config/src/profile.rs` — Apply to `.ferrule.toml` profiles (FE-004)
- `ferrule-core/src/url.rs` — `DatabaseUrl::parse` assumes fully resolved string

---

*Plan generated after Wave 1 completion.*
