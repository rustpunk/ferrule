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

Passwords are **redacted** in all logs, error messages, and verbose diagnostics:

```bash
ferrule query --verbose "postgres://user:secret@host/db" "SELECT 1"
# [ferrule] Resolved URL: postgres://user:***@host/db
```

## Saving Named Connections

Typing full URLs repeatedly is tedious and leaks passwords to shell history. Save named connections:

```bash
# Production database
ferrule conn add production "postgres://app@prod.example.com/myapp"

# Local development copy
ferrule conn add dev "postgres://localhost:5432/myapp_dev"

# A read-only replica
ferrule conn add replica "postgres://readonly@replica.internal/myapp"

# Analytics warehouse on a different port
ferrule conn add warehouse "postgres://analytics@warehouse.internal:8432/events"

# Local SQLite file
ferrule conn add local "sqlite:///home/me/projects/myapp/dev.db"
```

### Using Named Connections

Anywhere a connection URL is expected, you can substitute the saved name:

```bash
# Query using a name
ferrule query production "SELECT COUNT(*) FROM orders;"

# List tables
ferrule tables dev

# Describe a table
ferrule describe warehouse events

# Start the REPL on a saved connection
ferrule repl local

# Watch a production metric
ferrule watch replica "SELECT COUNT(*) FROM replication_lag;"
```

### Managing the Registry

```bash
# List all saved connections
ferrule conn list
# production  => postgres://app@prod.example.com/myapp
# dev         => postgres://localhost:5432/myapp_dev
# replica     => postgres://readonly@replica.internal/myapp
# warehouse   => postgres://analytics@warehouse.internal:8432/events
# local       => sqlite:///home/me/projects/myapp/dev.db

# Test connectivity
ferrule conn test production
# Connection 'production' is alive.

# Remove an entry
ferrule conn remove warehouse
```

### Where Connections Are Stored

```
~/.config/ferrule/connections.toml
```

Format:

```toml
[production]
name = "production"
url = "postgres://app@prod.example.com/myapp"

[dev]
name = "dev"
url = "postgres://localhost:5432/myapp_dev"
```

## Password Resolution Stack

When a saved URL does not contain a password, Ferrule resolves one automatically in this order:

### 1. Explicit `--password` flag

```bash
ferrule query production "SELECT * FROM users;" --password "my-secret"
```

### 2. Per-connection environment variable

```bash
export FERRULE_PRODUCTION_PASSWORD="my-secret"
ferrule query production "SELECT * FROM users;"
```

The variable name is derived from the connection name:
- `production` → `FERRULE_PRODUCTION_PASSWORD`
- `local-db` → `FERRULE_LOCAL_DB_PASSWORD`

### 3. OS Keyring

```bash
# Store password without putting it on disk
ferrule conn set-password production
# Password: ••••••••

# Remove from keyring
ferrule conn delete-password production
```

Passwords are stored as `service=ferrule`, `account=<name>` in the OS keyring (macOS Keychain, Windows Credential Manager, Linux Secret Service).

### 4. Interactive prompt (TTY only)

```bash
$ ferrule query production "SELECT * FROM users;"
Password for 'production': ••••••••
```

### 5. Fail with diagnostic

If all four steps fail, Ferrule exits with code 2:

```
ferrule::connection
  × Could not resolve password for 'production'
  ├─ No --password flag
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
