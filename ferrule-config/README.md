# ferrule-config

> Connection registry, profiles, and credential resolution for the [`ferrule`](https://github.com/rustpunk/ferrule) database CLI.

`ferrule-config` is the configuration layer of the `ferrule` workspace. It owns:

- **Connection registry** — named connections persisted to `~/.config/ferrule` and project-local `.ferrule.toml`, with environment-variable interpolation.
- **Profiles** — reusable bundles of connection defaults.
- **Credential resolution** — the layered stack that resolves a connection's
  password in order: CLI flag → `FERRULE_<NAME>_PASSWORD` env var → OS keyring
  → file, backed by [`hasp`](https://crates.io/crates/hasp). Passwords are
  carried as `secrecy::SecretString` (redacted in `Debug`, zeroized on drop).

It is consumed by `ferrule-core` (the resolver) and the `ferrule` CLI. It does
**not** depend on the SQL driver core (`ferrule-sql`), which stays config-free
so it can be embedded without pulling in keyring / file backends.

## License

Licensed under either of MIT or Apache-2.0 (`SPDX: MIT OR Apache-2.0`) at your
option.
