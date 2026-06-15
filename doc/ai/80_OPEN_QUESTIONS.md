# Open Questions

## 1. Should `CLAUDE.md` Be Updated To Four-Crate Architecture?

- Priority: High.
- Why it matters: `CLAUDE.md` says "Three-crate workspace", but root `Cargo.toml` and source show four crates with `ferrule-sql` owning backend drivers.
- Files/modules: `CLAUDE.md`, `Cargo.toml`, `ferrule-sql/`, `ferrule-core/`.
- Suggested resolution: Maintainer-approved docs update outside this AI onboarding task.

## 2. Is README "Zero Runtime Deps" Wording Precise Enough?

- Priority: High.
- Why it matters: README says default install includes MSSQL and zero runtime deps, while CI/deny notes all-features MSSQL pulls native-tls/OpenSSL and Oracle requires Instant Client at runtime.
- Files/modules: `README.md`, `deny.toml`, `.github/workflows/ci.yml`, `ferrule-cli/Cargo.toml`, `ferrule-sql/Cargo.toml`.
- Suggested resolution: Audit current backend dependency/runtime behavior and align product docs.

## 3. Should Product Docs Include Current Profile Fields?

- Priority: Medium.
- Why it matters: code accepts `ssh_*` and `proxy_url` in profiles, while some docs may list a smaller field set.
- Files/modules: `docs/src/configuration.md`, `docs/src/connections.md`, `docs/src/ssh-tunnels.md`, `docs/src/proxy.md`, `ferrule-config/src/profile.rs`.
- Suggested resolution: Compare docs against `ConnectionProfile` and update product docs.

## 4. Should Explicit Missing Config Path Return Defaults?

- Priority: Medium.
- Why it matters: `GlobalConfig::load(Some(nonexistent))` returning defaults is tested behavior but may surprise users.
- Files/modules: `ferrule-config/src/profile.rs`, CLI config loading in `ferrule-cli/src/main.rs`.
- Suggested resolution: Maintainer decision; if behavior stays, document clearly.

## 5. Is `ConfigError::ProfileNotFound` Still Needed?

- Priority: Low.
- Why it matters: explorer did not find usage.
- Files/modules: `ferrule-config/src/error.rs`, resolver/config code.
- Suggested resolution: Targeted dead-code audit in source-change task.

## 6. Should `ferrule-sql` Keep Crate-Level `allow(dead_code, unused_variables, unused_imports)`?

- Priority: Medium.
- Why it matters: could hide cleanup debt in an embeddable public core.
- Files/modules: `ferrule-sql/src/lib.rs`.
- Suggested resolution: Source audit with clippy/test evidence; do not remove in documentation-only work.

## 7. Is TUI Long-Query Blocking Accepted?

- Priority: Medium.
- Why it matters: optional TUI may freeze on long synchronous query work.
- Files/modules: `ferrule-cli/src/tui/*`, `ferrule-cli/src/commands/tui` if present.
- Suggested resolution: Product/architecture decision before async/background TUI query refactor.

## 8. Which `docs/internal` Plans Are Still Active?

- Priority: Low.
- Why it matters: internal plans/handoffs mix completed, stale, and active context.
- Files/modules: `docs/internal/BUGS.md`, `docs/internal/IDEAS.md`, `docs/internal/plan/*.md`, `docs/internal/handoffs/*.md`.
- Suggested resolution: Maintainer triage and labels/status markers.
