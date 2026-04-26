# Configuration

## Discovery Order

Ferrule loads configuration from (first found wins):

1. `--config <path>` CLI flag
2. `./.ferrule.toml` (project-local)
3. `~/.config/ferrule/ferrule.toml` (platform-appropriate)

## Example `.ferrule.toml`

```toml
[default]
format = "json"
limit = 1000

[connection.production]
url = "postgres://user@db.example.com/app"
password_url = "file:///run/secrets/db_password"

[connection.staging]
url = "mysql://user@staging.internal/app"
password_url = "env://STAGING_DB_PASSWORD"
```

## Security Notes

### Prefer `file://` for production

In containers, mount secrets as files rather than environment variables. Files are not visible in `/proc/<pid>/environ`, so other processes cannot read them.

```toml
[connection.production]
url = "postgres://user@db.example.com/app"
password_url = "file:///run/secrets/db_password"
```

### Prefer `keyring://` for development workstations

The OS keyring encrypts secrets at rest and isolates them from other processes.

```toml
[connection.production]
url = "postgres://user@db.example.com/app"
password_url = "keyring://ferrule/production"
```

### Avoid hard-coding passwords

Never put passwords directly in the `url` field. The URL is stored in plain-text TOML and may be checked into version control.

```toml
# Bad — password is visible in plain text
url = "postgres://user:secret@db.example.com/app"

# Good — password is resolved at runtime
url = "postgres://user@db.example.com/app"
password_url = "keyring://ferrule/production"
```

## `password_url`

The optional `password_url` field tells Ferrule where to fetch the connection password via `hasp`. It is evaluated before the legacy env-var and keyring fallbacks.

### Docker secrets (recommended for containers)

```toml
[connection.production]
url = "postgres://user@db.example.com/app"
password_url = "file:///run/secrets/db_password"
```

### Team-shared env var

```toml
[connection.staging]
url = "mysql://user@staging.internal/app"
password_url = "env://STAGING_DB_PASSWORD"
```

### OS keyring

```toml
[connection.production]
url = "postgres://user@db.example.com/app"
password_url = "keyring://ferrule/production"
```

## Environment Interpolation

Profile URLs can reference environment variables with `${VAR}`:

```toml
[connection.production]
url = "postgres://user@${DB_HOST}/app"
```

## Per-Profile Defaults

```toml
[connection.readonly]
url = "postgres://readonly@replica.example.com/app"
format = "table"   # override global default for this profile
limit = 50         # narrower default paging
```
