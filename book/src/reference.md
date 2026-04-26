# Reference

## Exit Codes

| Code | Meaning |
|------|---------|
| 0 | Success |
| 1 | Usage error (invalid CLI arguments, malformed SQL) |
| 2 | Connection error (network, auth, backend unavailable) |
| 3 | Query error (syntax error, constraint violation) |
| 4 | No rows returned (optional, used by `--expect-rows`) |

## Environment Variables

| Variable | Purpose |
|----------|---------|
| `FERRULE_<NAME>_PASSWORD` | Password for connection `<NAME>` |
| `FERRULE_CONFIG` | Path to global config file override |
| `RUST_LOG` | Enable debug logging from underlying crates |

## CLI Quick Reference

```bash
# Connections
ferrule conn add <name> <url>
ferrule conn list
ferrule conn remove <name>
ferrule conn test <name>
ferrule conn set-password <name>
ferrule conn delete-password <name>

# Querying
ferrule query <connection> <sql> [options]
ferrule explain <connection> <sql>
ferrule tables <connection>
ferrule describe <connection> <table>

# Data movement
ferrule dump <connection> <table> [--dump-format <fmt>]
ferrule load <connection> <file> --table <table> [--create-table]

# Monitoring
ferrule watch <connection> <sql> [--interval <secs>] [--diff] [--max-iterations <N>]

# Interactive
ferrule repl <connection>

# Bookmarks
ferrule bookmark add <name> --sql <sql> [--connection <conn>]
ferrule bookmark list
ferrule bookmark run <name> [param1] [param2] ...
ferrule bookmark delete <name>

# Options common to query/dump/explain/watch/tables/describe
--format <fmt>       Output format (table, json, csv, yaml, raw)
--limit <N>            Server-side row limit
--offset <N>           Skip N rows
--timing               Show timing diagnostics
--verbose              Show SQL and resolved URLs
--insecure             Disable TLS verification
--password <pwd>      Explicit password
```

## File Locations

| File | Path |
|------|------|
| Config | `~/.config/ferrule/ferrule.toml` |
| Connections | `~/.config/ferrule/connections.toml` |
| Bookmarks | `~/.config/ferrule/bookmarks.toml` |
| History | `~/.cache/ferrule/history` |
