# Connections

## Connection URLs

Ferrule accepts either a raw database URL or a saved connection name.

### URL Formats

| Backend | URL Pattern |
|---------|-------------|
| PostgreSQL | `postgres://user:pass@host:port/db?sslmode=disable` |
| MySQL | `mysql://user:pass@host:port/db` |
| MSSQL | `mssql://user:pass@host:port/db?trustServerCertificate=true` |
| SQLite | `sqlite:///path/to/db` or `sqlite::memory:` |
| Oracle | `oracle://user:pass@host:port/service_name` |

### URL Safety

Passwords are **redacted** in all logs and diagnostics:

```bash
# Verbose output shows the URL with password masked
ferrule --verbose query production "SELECT 1"
# [ferrule] Resolved URL: postgres://user:***@host/db
```

## Connection Registry

Saved connections live in `~/.config/ferrule/connections.toml`:

```toml
[default]
url = "postgres://user@localhost/mydb"

[staging]
url = "mysql://user@staging.internal/app"
```

Commands:

```bash
ferrule conn add staging "mysql://user@staging.internal/app"
ferrule conn list
ferrule conn test staging
ferrule conn remove staging
```

## Credential Resolution

When a connection URL does not contain a password, Ferrule attempts to resolve one:

1. **Explicit** — `--password` flag passed on the command line.
2. **Environment** — `FERRULE_<NAME>_PASSWORD` (e.g. `FERRULE_PRODUCTION_PASSWORD`).
3. **OS Keyring** — `keyring://ferrule/<name>` via the `keyring` crate.
4. **Interactive prompt** — asks on TTY if all above fail.
5. **Fail** — exits with code 2 and a `miette` diagnostic.

### Storing Passwords in the Keyring

```bash
# Store the current password for 'production'
ferrule conn set-password production
# Password: ••••••••

# Remove it
ferrule conn delete-password production
```

## Configuration Profiles

Project-local `.ferrule.toml` files can define profiles:

```toml
[connection.production]
url = "postgres://user@db.example.com/app"
```

Used as:

```bash
ferrule query production "SELECT 1;"
```
