# Connections

## Raw Connection URLs

Pass a database URL directly as the first argument to any `query`, `tables`, `describe`, `dump`, `watch` command:

```bash
# SQLite (bundled — no external server)
ferrule query "sqlite::memory:" "SELECT 1;"
ferrule query "sqlite:///home/me/data/logs.db" "SELECT COUNT(*) FROM events;"

# PostgreSQL
ferrule query "postgres://ferrule:ferrule@localhost:5432/myapp?sslmode=disable" "SELECT * FROM users;"

# MySQL
ferrule query "mysql://root:pass@localhost:3306/myapp" "SHOW TABLES;"

# MSSQL  
ferrule query "mssql://sa:pass@127.0.0.1:1433/myapp?trustServerCertificate=true" "SELECT 1;"
```

### URL Formats

| Backend | URL Pattern | Notes |
|---------|-------------|-------|
| PostgreSQL | `postgres://user:pass@host:port/db?sslmode=disable` | `sslmode` values: `prefer`, `require`, `disable` |
| MySQL | `mysql://user:pass@host:port/db` | |
| MSSQL | `mssql://user:pass@host:port/db?trustServerCertificate=true` | Set `trustServerCertificate=true` for self-signed certs |
| SQLite | `sqlite:///absolute/path` or `sqlite::memory:` | `:memory:` does not work after `://`; use the form above |
| Oracle | `oracle://user:pass@host:port/service_name` | Requires `--features oracle` |

### URL Safety

**Never embed passwords in URLs.** Even when saved, connection registry entries are stored as plain TOML. Always omit the password from the URL and let Ferrule resolve it through the credential stack.

Passwords are **redacted** in all logs, error messages, and verbose diagnostics:

```bash
ferrule query --verbose "postgres://user:secret@host/db" "SELECT 1"
# [ferrule] Resolved URL: postgres://user:***@host/db
```

## Security Recommendations

Ferrule resolves passwords through the `hasp` unified secret stack. The options below are listed from **most secure** to **least secure**.

### Recommended: `file://` (Docker / Kubernetes secrets)

Mount secrets as files. This is the most secure option because the secret is never exposed in environment variables (visible via `/proc/<pid>/environ`) or shell history.

```toml
[connection.production]
url = "postgres://app@db.example.com/myapp"
password_url = "file:///run/secrets/db_password"
```

Use `?raw=true` if the file must be read verbatim without trimming the trailing newline.

### Recommended: `keyring://` (OS keyring)

Store passwords in the OS-native credential store (macOS Keychain, Windows Credential Manager, Linux Secret Service). Secrets are encrypted at rest and isolated from other processes.

```toml
[connection.production]
url = "postgres://app@db.example.com/myapp"
password_url = "keyring://ferrule/production"
```

To store a password interactively:

```bash
ferrule conn set-password production
```

### Acceptable for development: `env://`

Environment variables are convenient but visible to any process running as the same user via `/proc/<pid>/environ`.

```toml
[connection.staging]
url = "mysql://user@staging.internal/app"
password_url = "env://STAGING_DB_PASSWORD"
```

### Avoid: `--password` flag

The `--password` flag leaks the secret into shell history (`~/.bash_history`, `~/.zsh_history`) and process listings (`ps`). Only use it for one-off debugging or in CI pipelines where the secret is injected as an ephemeral variable.

```bash
# Leaks to history — do not use for real secrets
ferrule query production "SELECT 1;" --password "my-secret"
```

### Avoid: passwords in URLs

URLs containing passwords are written to the connections registry (`connections.toml`) in plain text and may appear in process listings.

```bash
# Bad — password ends up in plain-text TOML
ferrule conn add production "postgres://user:secret@host/db"
```

## Password Resolution Stack

If a connection URL does not contain a password, Ferrule resolves one automatically. The numeric order below is the exact fallback order; each step is attempted only if the previous one yielded no password.

### 1. Explicit `--password` flag (_least secure_)

```bash
ferrule query production "SELECT * FROM users;" --password "my-secret"
```

### 2. `password_url` from profile

If `.ferrule.toml` defines a `password_url`, Ferrule resolves it via `hasp` first.

```toml
[connection.production]
url = "postgres://app@prod.example.com/myapp"
password_url = "keyring://ferrule/production"
```

Supported `hasp` URL schemes:
- `env://VAR_NAME` — environment variable
- `keyring://service/account` — OS keyring
- `file:///path/to/secret` — file on disk (trims trailing newline by default)

### 3. Per-connection environment variable (legacy)

```bash
export FERRULE_PRODUCTION_PASSWORD="my-secret"
ferrule query production "SELECT * FROM users;"
```

The variable name is derived from the connection name:
- `production` → `FERRULE_PRODUCTION_PASSWORD`
- `local-db` → `FERRULE_LOCAL_DB_PASSWORD`

### 4. OS Keyring fallback

If no `password_url` is configured and the env var is unset, Ferrule falls back to the OS keyring at `keyring://ferrule/<name>`.

```bash
# Store password without putting it on disk
ferrule conn set-password production
# Password: ••••••••

# Remove from keyring
ferrule conn delete-password production
```

Passwords are stored as `service=ferrule`, `account=<name>` in the OS keyring.

### 5. Interactive prompt (TTY only)

```text
$ ferrule query production "SELECT * FROM users;"
Password for 'production': ••••••••
```

### 6. Fail with diagnostic

If all five steps fail, Ferrule exits with code 2:

```text
ferrule::connection
  × Could not resolve password for 'production'
  ├─ No --password flag
  ├─ password_url not configured
  ├─ FERRULE_PRODUCTION_PASSWORD is unset
  ├─ keyring://ferrule/production: not found
  └─ Terminal is not interactive
```

## Project-Local Profiles

For team-shared settings, add a `.ferrule.toml` to your project root:

```toml
[default]
format = "json"
limit = 100

[connection.production]
url = "postgres://app@${DB_HOST:-db.example.com}/myapp"
```

Ferrule discovers `.ferrule.toml` automatically. The URL above uses optional environment interpolation — if `DB_HOST` is unset, it falls back to `db.example.com`.

Named connections from the registry take precedence over `.ferrule.toml` profiles if they share a name.
