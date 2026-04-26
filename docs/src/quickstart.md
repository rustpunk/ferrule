# Quick Start

## First Query

```bash
# SQLite works without any setup
ferrule query "sqlite::memory:" "SELECT 1 + 1 AS answer;"
```

Output (TTY defaults to table format):

```text
 answer
--------
 42
```

## Save a Named Connection

Typing full URLs repeatedly is tedious. Save the ones you use often:

```bash
ferrule conn add production "postgres://user@db.example.com/app"

# Now use the name instead of the full URL
ferrule query production "SELECT * FROM customers LIMIT 5;"
ferrule tables production
ferrule repl production
```

## Pipe-Friendly Defaults

When stdout is not a TTY, output defaults to JSON:

```bash
ferrule query "sqlite::memory:" "SELECT 1" | jq '.[]."1"'
# > 1
```

## Save a Bookmark

For queries you run all the time:

```bash
ferrule bookmark add daily-count "SELECT COUNT(*) FROM events;" --connection production

ferrule bookmark run daily-count
```

## Password Resolution

Ferrule resolves passwords via the `hasp` unified secret stack. For daily use, prefer the most secure option available in your environment:

1. **`file://`** — mount secrets as files (Docker / Kubernetes). Not visible in `/proc/<pid>/environ`.
2. **`keyring://`** — OS keyring. Encrypted at rest, isolated from other processes.
3. **`env://`** — environment variable. Convenient but visible to other processes as the same user.
4. **`FERRULE_<NAME>_PASSWORD`** — legacy env var fallback.
5. **Interactive prompt** — TTY only; secret never touches disk or env.
6. **`--password`** — **least secure**; leaks to shell history (`~/.bash_history`) and `ps`.

### Recommended: `file://` in production

```toml
[connection.production]
url = "postgres://app@db.example.com/myapp"
password_url = "file:///run/secrets/db_password"
```

### Recommended: `keyring://` on workstations

```bash
# Store in OS keyring once
ferrule conn set-password production

# Use from now on
ferrule query production "SELECT 1;"
```

### One-off debugging (avoid for real secrets)

```bash
# Leaks to shell history — only use for ephemeral testing
ferrule query production "SELECT 1;" --password "my-secret"
```

## JSON Output with Paging

```bash
ferrule query production "SELECT * FROM events" \
  --format json --limit 50 --offset 100
```

## File Output

```bash
ferrule query production "SELECT * FROM events" --output events.json
```

## Explore Schema

```bash
# List tables
ferrule tables production

# Describe a table
ferrule describe production events
```
