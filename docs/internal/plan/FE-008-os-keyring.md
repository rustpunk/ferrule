# Plan: FE-008 — OS Keyring Credential Store

**Target:** `ferrule-config/src/credentials.rs`  
**Crate:** `ferrule-config`  
**Feature:** default  
**Estimate:** Small  
**Reference Implementation:** `ferrule-config/src/credentials.rs`

---

## Why This Matters

`connections.toml` stores URLs in plain text. Even with password redaction, users often paste full URLs into the registry. The OS keyring (macOS Keychain, Linux Secret Service / libsecret, Windows Credential Manager) is the standard place to store credentials safely. Ferrule should read from it before falling back to the interactive prompt.

---

## Architecture

```
ferrule-config/src/credentials.rs
├─ resolve_password(name: &str) -> Option<SecretString>
│   ├─ explicit CLI flag
│   ├─ FERRULE_{NAME}_PASSWORD env var
│   ├─ keyring::Entry::new("ferrule", name)
│   └─ interactive prompt
├─ set_keyring_password(name: &str, password: &SecretString) -> Result<(), ConfigError>
└─ delete_keyring_password(name: &str) -> Result<(), ConfigError>

ferrule-cli/src/commands/conn.rs
├─ ConnCommand::SetPassword { name }
└─ ConnCommand::DeletePassword { name }
```

---

## Implementation Checklist

1. **Keyring Crate**
   - Add `keyring = "3"` to `ferrule-config/Cargo.toml`
   - The `keyring` crate abstracts macOS/Windows/Linux backends automatically

2. **Credential Resolution Stack Update**
   - Current stack in `ferrule-cli/src/commands/mod.rs`:
     1. Explicit `--password`
     2. `FERRULE_{NAME}_PASSWORD`
     3. Interactive prompt
   - Insert keyring as step 3, moving prompt to step 4:
     1. Explicit `--password`
     2. `FERRULE_{NAME}_PASSWORD`
     3. `keyring::Entry::new("ferrule", name).get_password()`
     4. Interactive prompt (TTY only)
     5. Fail with diagnostic

3. **Keyring Management Commands**
   - `ferrule conn set-password <name>` — prompts for password, stores in keyring
   - `ferrule conn delete-password <name>` — removes keyring entry
   - Both fail gracefully if no keyring backend is available (e.g. headless Linux without D-Bus)

4. **Error Handling**
   - `ConfigError::KeyringError(String)` for keyring-specific failures
   - If keyring read fails with `NoEntry`, continue to interactive prompt
   - If keyring backend is missing entirely, log a one-line warning and continue

5. **Verification**
   - [ ] `cargo build --workspace` ✅
   - [ ] `cargo clippy --workspace` ✅
   - [ ] `cargo test --workspace` ✅ (mock keyring backend for CI)
   - [ ] No `todo!()` remaining

---

## Integration Tests

```bash
# Store password
ferrule conn set-password production
# (prompts for password)

# Verify query works without --password
ferrule query production "SELECT 1"

# Cleanup
ferrule conn delete-password production
```

Unit tests (mock backend):

```rust
#[test]
fn test_keyring_roundtrip() {
    // keyring crate provides mock backend for testing
    let entry = keyring::Entry::new("ferrule", "test").unwrap();
    entry.set_password("secret").unwrap();
    assert_eq!(entry.get_password().unwrap(), "secret");
}
```

---

## Cargo.toml Additions

```toml
# ferrule-config/Cargo.toml
[dependencies]
keyring = "3"
```

---

## Risks & Gotchas

1. **Headless Linux CI** — Secret Service requires D-Bus. The `keyring` crate falls back to a file-based keyring (`keyring-rs` file backend) if no secret service is available, but this may store passwords in plaintext. Document that ferrule warns when using the file fallback.
2. **macOS Keychain popups** — First access triggers a system dialog. Document this in README so users aren't surprised.
3. **Windows service accounts** — Credential Manager requires an interactive session. On Windows Server Core or headless mode, keyring may fail. Fall back to env var in that case.
4. **Name normalization** — Keyring entries use the raw connection name. Ensure `ferrule conn set-password "my-db"` and `ferrule query "my-db"` resolve the same keyring entry after name normalization (case, dashes).

---

## Related Files

- `ferrule-config/src/credentials.rs` — Credential resolution logic
- `ferrule-cli/src/commands/mod.rs` — `resolve_password_stack()`
- `ferrule-cli/src/commands/conn.rs` — New `SetPassword` / `DeletePassword` subcommands
- `ferrule-config/src/error.rs` — New `KeyringError` variant

---

*Plan generated after Wave 1 completion.*
