# Quick Start

## First Query

```bash
# SQLite works without any setup
ferrule query "sqlite::memory:" "SELECT 1 + 1 AS answer;"
```

Output (TTY defaults to table format):
```
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

Ferrule resolves passwords via a stack:

1. `--password` CLI flag
2. `FERRULE_<NAME>_PASSWORD` environment variable
3. OS keyring (`keyring://ferrule/<name>`)
4. Interactive prompt (TTY only)
5. Fail with diagnostic

```bash
# Set via environment
FERRULE_PRODUCTION_PASSWORD=secret ferrule query production "SELECT 1;"

# Store in OS keyring
ferrule conn set-password production
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
