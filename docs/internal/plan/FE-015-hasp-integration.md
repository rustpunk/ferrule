# FE-015: hasp Integration

> Replace the legacy `keyring = "3"` credential stack with `hasp` as a hard dependency of `ferrule-config`.

## Goals

1. Remove the legacy `keyring = "3"` dependency entirely.
2. Delete `resolve_env_password()`, `resolve_keyring_password()`, `set_keyring_password()`, and `delete_keyring_password()` from `ferrule-config/src/credentials.rs`.
3. Rewrite `resolve_password_stack` to use `hasp::Store::get()` for `password_url`, `env://`, and `keyring://` lookups.
4. Add `password_url: Option<String>` to `ConnectionProfile` for `.ferrule.toml` support.
5. Update `ferrule-cli/src/commands/conn.rs` `set_password`/`delete_password` to use hasp-backed helpers.
6. No feature gating — hasp is a hard dependency.
7. Construct `hasp::Store` locally per call (no global cache).

## Context

- `hasp` is buildable and tested at `~/code/rustpunk/hasp/`:
  ```bash
  cargo test -p hasp --no-default-features --features "env,keyring,file"
  ```
- `hasp` returns `secrecy::SecretString` from `Store::get(url)` and accepts `&str` URLs.
- Error taxonomy maps cleanly:
  - `NotFound` → fall through
  - `PermissionDenied` / `AuthenticationFailed` → warn on stderr, fall through
  - `InvalidUrl` / `UrlParse` → hard error (`ConfigError::HaspError`)
  - `Backend { kind: Transient | Throttled, .. }` → warn on stderr, fall through
  - All other errors → warn on stderr, fall through

## Architecture

### Credential resolution order

1. `explicit` (CLI `--password`)
2. `password_url` from `ConnectionProfile` via `hasp::Store::get(password_url)`
3. `env://FERRULE_{NAME}_PASSWORD` via `hasp`
4. `keyring://ferrule/{name}` via `hasp`
5. Interactive TTY prompt (handled in CLI layer)
6. Return `Ok(None)`

### Files to modify

| File | Change |
|------|--------|
| `ferrule-config/Cargo.toml` | Replace `keyring = "3"` with `hasp` path dep; remove `[features]` |
| `ferrule-config/src/error.rs` | Add `HaspError(String)` variant |
| `ferrule-config/src/credentials.rs` | Delete legacy functions; rewrite `resolve_password_stack` using hasp; keep `set_keyring_password`/`delete_keyring_password` as hasp wrappers |
| `ferrule-config/src/profile.rs` | Add `password_url: Option<String>` to `ConnectionProfile` |
| `ferrule-config/src/lib.rs` | Update `pub use credentials::{...}` exports |
| `ferrule-cli/src/commands/mod.rs` | Delete local `resolve_password_stack`; add `prompt_password_interactive` helper; update `resolve_connection` callers to use `ferrule_config::credentials::resolve_password_stack` + prompt |
| `ferrule-cli/src/commands/conn.rs` | No structural change; underlying helpers now use hasp |
| `docs/src/connections.md` | Document `password_url` field |
| `docs/src/configuration.md` | Add `password_url` example |

### Files that must NOT be modified

- `ferrule-core/*`
- Any other `ferrule-cli/src/*.rs` files

## Error handling rules

- `NotFound` → fallthrough
- `PermissionDenied` → `eprintln!("Warning: hasp permission denied for {}")`, fallthrough
- `AuthenticationFailed` → same pattern
- `InvalidUrl` / `UrlParse` → return `Err(ConfigError::HaspError(...))`
- `Backend { Transient | Throttled }` → warn, fallthrough
- All other errors → warn, fallthrough

## Backward compatibility

- Existing `.ferrule.toml` files without `password_url` deserialize as `None` thanks to `#[serde(default)]`.
- Existing `connections.toml` registry entries are unaffected.
- Legacy `FERRULE_{NAME}_PASSWORD` env vars continue to work via `env://`.
- Legacy keyring entries at `service=ferrule, account={name}` continue to work via `keyring://ferrule/{name}`.

## Build verification

```bash
cargo build --workspace
cargo test --workspace
cargo clippy --workspace
cargo doc --workspace --no-deps
cargo fmt --check
```

## Commits

1. `feat(config): replace legacy keyring with hasp unified credential stack`
2. `docs: document password_url in connections and configuration`
