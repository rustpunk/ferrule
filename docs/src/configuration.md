# Configuration

Ferrule reads optional configuration from a TOML file. Every value is
optional; a fresh install with no config file works fine. Use the
config to set defaults, declare connection profiles, and wire up
credential resolution.

## Discovery order

Ferrule looks for a config file in this order; the first one that
exists wins:

1. `--config <path>` on the command line.
2. `./.ferrule.toml` in the current working directory (project-local).
3. `~/.config/ferrule/ferrule.toml` (or the platform equivalent —
   `dirs::config_dir()`).

If none of those exist, ferrule uses the built-in defaults.

> **Two TOML files, two purposes.** Don't confuse the *config file*
> (`.ferrule.toml` / `ferrule.toml`) with the *connections registry*
> (`~/.config/ferrule/connections.toml`). The registry is managed by
> `ferrule conn add` / `list` / `remove`; it stores raw URLs only.
> The config file holds defaults, profiles, and credential
> declarations. Profile entries take precedence over registry
> entries with the same name.

## Example `.ferrule.toml`

```toml
[default]
format = "table"          # default output format when no --format given
limit = 1000              # default LIMIT applied to single-statement queries
timeout = 30              # connection timeout in seconds (reserved; not yet enforced)

[connection.production]
url = "postgres://app@${DB_HOST:-db.example.com}/myapp"
password_url = "keyring://ferrule/production"

[connection.staging]
url = "mysql://user@staging.internal/app"
password_url = "env://STAGING_DB_PASSWORD"

[connection.local]
url = "sqlite:///./local.db"
```

Once present, queries can use the bare profile name:

```bash
ferrule query production "SELECT 1;"
```

## `[default]` block

| Field | Type | Default | Notes |
|---|---|---|---|
| `format` | string | `"json"` | One of `table` / `json` / `csv` / `yaml` / `raw`. Overridden per-call by `--format`. |
| `limit` | integer | `1000` | Default LIMIT for single-statement queries. Set to `0` to disable. Multi-statement batches reject any non-zero value. |
| `timeout` | integer | `30` | Connection timeout in seconds. Reserved field; the current driver layer doesn't enforce it. |

The `limit = 1000` default catches the "I forgot to add `LIMIT` and
the table has 50M rows" case at low cost. If you mostly run
multi-statement DDL or batch jobs, set `limit = 0` here so you don't
have to pass `--limit 0` on every call.

## `[connection.<name>]` blocks

Each block defines a profile that resolves to a database URL plus
credential metadata.

| Field | Type | Required | Notes |
|---|---|---|---|
| `url` | string | yes | Database URL. May contain `${ENV_VAR}` and `${ENV_VAR:-default}` interpolation. |
| `password_url` | string | no | Hasp URL for credential resolution. See below. |
| `headers` | table | no | Reserved; not yet used by the driver layer. |

### Environment interpolation

`${VAR}` in a URL is replaced with the value of environment variable
`VAR` at config-load time. `${VAR:-fallback}` provides a literal
fallback if `VAR` is unset or empty. `$$` is an escaped literal `$`.

```toml
[connection.production]
url = "postgres://app@${DB_HOST:-db.example.com}:${DB_PORT:-5432}/myapp"
```

Behavior:

- **Unset variables without `:-` fallback** are left as the literal
  `${VAR}` text — ferrule does *not* fail. (This usually causes a
  later parse error; it's loud, not silent.)
- **Empty values are treated like unset** for the `:-` fallback.
- Bare `$VAR` (no braces) is *not* interpolated.

### `password_url`

Tells ferrule where to fetch the connection password via the
[hasp](https://crates.io/crates/hasp) credential resolver.
Schemes:

| Scheme | Example | Notes |
|---|---|---|
| `env://` | `env://STAGING_DB_PASSWORD` | Reads an env var |
| `keyring://` | `keyring://ferrule/production` | OS keyring (`service` / `account`) |
| `file://` | `file:///run/secrets/db_password` | File on disk; trims trailing newline. Append `?raw=true` to disable trimming |

`password_url` is consulted *before* the legacy `FERRULE_<NAME>_PASSWORD`
env var and the implicit `keyring://ferrule/<name>` lookup, so it lets
you point at any secret store you like without renaming env vars.

The full credential stack and security trade-offs live in
[Security](security.md). Quick guide:

- Production with mounted secrets → `file://`.
- Workstation → `keyring://`.
- CI / dev → `env://`.

## Notable defaults that surprise people

- The default output format is **`json`**, not `table`. To get
  pretty tables interactively, set `format = "table"` here or pass
  `--format table` per call.
- The default `limit` is **`1000`**, applied to every single-statement
  query that doesn't override it. Set `limit = 0` if you don't want
  this.
- Profiles take precedence over the registry. If a project-local
  `.ferrule.toml` and `~/.config/ferrule/connections.toml` both
  define `prod`, the profile wins.
- **Typos are now hard errors.** Every `[default]` and `[connection]`
  key is validated; a misspelled field like `ssh_hostt` produces an
  error at config-load time instead of being silently ignored.

## Per-profile output defaults

Output settings under a profile aren't supported yet — the
`[connection.<name>]` block accepts `url`, `password_url`, and
`headers` only. To override defaults for a specific environment,
pass the relevant flags at call time, or maintain a separate
`.ferrule.toml` per project.

## Environment variable overrides

| Variable | Effect |
|---|---|
| `FERRULE_<NAME>_PASSWORD` | Legacy password fallback for connection `<name>` |
| `RUST_LOG` | Enable structured logging from ferrule and the driver crates |

`FERRULE_CONFIG` is **not** currently a recognized override; use
`--config` on the command line.

## Where the registry lives

Separate from the config file:

```text
~/.config/ferrule/connections.toml
```

Format:

```toml
[entries.production]
name = "production"
url = "postgres://app@db.example.com/myapp"

[entries.local]
name = "local"
url = "sqlite:///./local.db"
```

Don't hand-edit this file — use `ferrule conn add` / `remove` /
`list` (see [Connections](connections.md)). The registry is
deliberately simpler than `.ferrule.toml`; if you need
`password_url` or env interpolation, switch to a profile.
