# Plan: FE-004 — Config Profiles & `.ferrule.toml`

**Target:** `ferrule-config/src/profile.rs`, `ferrule-cli/src/commands/mod.rs`  
**Crate:** `ferrule-config`, `ferrule-cli`  
**Feature:** default  
**Estimate:** Small  
**Reference Implementation:** `ferrule-config/src/profile.rs` (scaffold exists)

---

## Why This Matters

Wave 1 hard-codes defaults (format=table/json, limit=none, timeout=30s). Users need a way to override these globally and per-connection without typing flags every time. A `.ferrule.toml` in the project root or `~/.config/ferrule/` provides the standard Rust-CLI experience.

---

## Architecture

```
ferrule-config/src/profile.rs
├─ GlobalConfig { default, connection: IndexMap<...> }
├─ DefaultProfile  { format, limit, timeout }
└─ ConnectionProfile { url, headers }

ferrule-config/src/lib.rs
├─ load_global_config() -> Result<GlobalConfig, ConfigError>
└─ merge_with_args(args, config) -> EffectiveConfig

ferrule-cli/src/commands/mod.rs
└─ OutputFlags gains --config override
```

---

## Implementation Checklist

1. **Config Discovery**
   - Search order (first wins):
     1. `--config <path>` CLI flag
     2. `./.ferrule.toml` (project-local)
     3. `~/.config/ferrule/ferrule.toml` (user-global)
   - Add `load_global_config()` in `ferrule-config/src/lib.rs`

2. **TOML Schema**
   ```toml
   [default]
   format = "table"      # json | csv | yaml | raw
   limit = 1000
   timeout = 30

   [connection.production]
   url = "postgres://..."
   ```

3. **Profile Integration**
   - `ferrule-config/src/profile.rs` — finish `GlobalConfig` deserialization
   - Add `ConfigError::ProfileNotFound` variant

4. **CLI Wiring**
   - `OutputFlags` adds `--config`
   - `resolve_connection()` loads profile URL when `connection` matches a profile name
   - Default format/limit fetched from `GlobalConfig` if flag not provided

5. **Verification**
   - [ ] `cargo build --workspace` ✅
   - [ ] `cargo clippy --workspace` ✅
   - [ ] `cargo test --workspace` ✅ (unit tests for profile loading/merging)
   - [ ] No `todo!()` remaining

---

## Integration Tests

```rust
#[test]
fn test_load_global_config() {
    let config = GlobalConfig::load_from("testdata/.ferrule.toml").unwrap();
    assert_eq!(config.default.format, "json");
    assert_eq!(config.default.limit, 500);
}
```

---

## Cargo.toml

No new dependencies. `toml` and `serde` already present in `ferrule-config`.

---

## Risks & Gotchas

1. **TOML + `IndexMap`** — `serde` deserializes maps as `HashMap` by default. Ensure `IndexMap` deserializer is used so connection order is preserved.
2. **Profile vs registry name collision** — A connection name may exist in both `connections.toml` (registry) and `.ferrule.toml` (profile). Define precedence: CLI flag > env > profile > registry.
3. **Breaking change** — Changing default `limit` from `None` to `1000` alters current behavior. Document in CHANGELOG.

---

## Related Files

- `ferrule-config/src/profile.rs` — Global config structs
- `ferrule-config/src/registry.rs` — Connection registry (collaborates on URL resolution)
- `ferrule-cli/src/commands/mod.rs` — CLI argument definitions
- `ferrule-cli/src/output.rs` — Default format detection (TTY vs pipe)

---

*Plan generated after Wave 1 completion.*
