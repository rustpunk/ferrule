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
| `FERRULE_CONFIG` | Path to config file override |
| `RUST_LOG` | Enable debug logging from underlying crates |

## CLI Quick Reference

### Connections

```bash
ferrule conn add <name> <url>
ferrule conn list
ferrule conn remove <name>
ferrule conn test <name>
ferrule conn set-password <name>
ferrule conn delete-password <name>
```

### Querying

```bash
ferrule query <connection> <sql> [options]
ferrule explain <connection> <sql>
ferrule tables <connection>
ferrule describe <connection> <table>
```

### Bookmarks

```bash
ferrule bookmark add <name> <sql> [--connection <conn>]
ferrule bookmark list
ferrule bookmark run <name> [param1] [param2] ... [--connection <conn>] [--format <fmt>]
ferrule bookmark delete <name>
```

Bookmark names can be dotted (`pg.select_users`) — the first segment suggests the connection to use.

### Data Movement

```bash
ferrule dump <connection> <table> [--dump-format <fmt>] [--file <path>] [--schema <schema>]
ferrule load <connection> <file> --table <table> [--create-table]
```

### Monitoring

```bash
ferrule watch <connection> <sql> [--interval <secs>] [--diff] [--max-iterations <N>]
```

### Interactive

```bash
ferrule repl <connection>
```

## Common Options (for `query`, `tables`, `describe`, `dump`, `explain`, `watch`)

| Flag | Description |
|------|-------------|
| `-f, --format <fmt>` | Output format: `table`, `json`, `csv`, `yaml`, `raw` |
| `-n, --limit <N>` | Server-side row limit (`0` to disable) |
| `--offset <N>` | Skip N rows |
| `--timing` | Show timing diagnostics |
| `-v, --verbose` | Show resolved URL and SQL |
| `--insecure` | Disable TLS verification |
| `-p, --password <pwd>` | Explicit password |
| `--output <FILE>` | Write results to a file |
| `--daemon` | Route through connection-pooling daemon |

## File Locations

| File | Purpose | Path |
|------|---------|------|
| Global config | Per-user defaults, connection profiles | `~/.config/ferrule/ferrule.toml` |
| Connections | Saved name → URL registry | `~/.config/ferrule/connections.toml` |
| Bookmarks | Saved query library | `~/.config/ferrule/bookmarks.toml` |
| History | REPL command history | `~/.cache/ferrule/history` |

## Type Reference

Every backend maps native types to a unified `Value` enum:

| Ferrule `Value` | Postgres | MySQL | MSSQL | SQLite | Oracle |
|----------------|----------|-------|-------|--------|--------|
| `Bool` | `BOOLEAN` | `BOOLEAN` | `BIT` | `INTEGER` | `NUMBER(1)` |
| `Int64` | `BIGINT` | `BIGINT` | `BIGINT` | `INTEGER` | `NUMBER` |
| `Float64` | `DOUBLE` | `DOUBLE` | `FLOAT` | `REAL` | `BINARY_FLOAT` |
| `Decimal` | `NUMERIC` | `DECIMAL` | `DECIMAL` | `NUMERIC` | `NUMBER` |
| `String` | `TEXT` | `VARCHAR` | `NVARCHAR` | `TEXT` | `VARCHAR2` |
| `Bytes` | `BYTEA` | `BLOB` | `VARBINARY` | `BLOB` | `RAW` |
| `Date` | `DATE` | `DATE` | `DATE` | `TEXT` | `DATE` |
| `DateTime` | `TIMESTAMP` | `DATETIME` | `DATETIME2` | `TEXT` | `TIMESTAMP` |
| `DateTimeTz` | `TIMESTAMPTZ` | `TIMESTAMP` | `DATETIMEOFFSET` | `TEXT` | `TIMESTAMP WITH TIME ZONE` |
| `Json` | `JSONB` | `JSON` | `NVARCHAR(MAX)` | `TEXT` | `CLOB` |
| `Uuid` | `UUID` | `CHAR(36)` | `UNIQUEIDENTIFIER` | `TEXT` | `RAW(16)` |
