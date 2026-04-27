# CLI Reference

Quick lookup for commands, flags, exit codes, and file locations.
For conceptual context, start with [Concepts](concepts.md).

## Commands

```bash
ferrule [--config <path>] <subcommand> [args...]
```

Aliases: `ferrule conn` for `ferrule connection`, `ferrule q` for
`ferrule query`, `ferrule r` for `ferrule repl`.

### `query` — execute SQL

```bash
ferrule query <connection> [<sql>]
  [--file <path>] [--stdin]
  [--param NAME=VALUE]... [--param-file <json>]
  [--explain] [--dry-run]
  [--format <fmt>] [--output <file>]
  [--limit <n>] [--offset <n>]
  [--timing] [--verbose]
  [--insecure] [--daemon]
  [--password <pwd>]
```

See [Querying Data](querying.md).

### `tables` / `describe` — schema introspection

```bash
ferrule tables   <connection> [output flags] [connection flags]
ferrule describe <connection> <table> [output flags] [connection flags]
```

See [Schema Introspection](schema.md).

### `explain` — execution plans

```bash
ferrule explain <connection> <sql>
  [--analyze]
  [--format <fmt>] [--output <file>]
  [--timing] [--verbose]
  [--insecure] [--daemon]
```

See [EXPLAIN, Dump, and Watch](advanced.md#explain).

### `dump` / `load` — data movement

```bash
ferrule dump <connection> <table>
  [--file <path>]
  [--dump-format csv|json|sql]
  [--schema <name>]
  [--limit <n>] [--offset <n>]
  [--timing] [--verbose]
  [--insecure] [--daemon]

ferrule load <connection> <file>
  [--table <name>]
  [--format csv|json]
  [--create-table]
  [--insecure] [--daemon]
```

See [Dump and Load](advanced.md#dump-and-load).

### `watch` — periodic re-execution

```bash
ferrule watch <connection> <sql>
  [--interval <seconds>]
  [--max-iterations <n>]
  [--diff]
  [--format <fmt>] [--output <file>]
  [--timing] [--verbose]
  [--insecure] [--daemon]
  [--password <pwd>]
```

See [Watch mode](advanced.md#watch-mode).

### `repl` — interactive shell

```bash
ferrule repl [<connection>]
  [--format <fmt>] [--output <file>]
  [--limit <n>] [--offset <n>]
  [--timing] [--verbose]
  [--insecure] [--daemon]
```

See [Interactive REPL](repl.md).

### `bookmark` — saved queries

```bash
ferrule bookmark add <name> <sql> [--connection <name>]
ferrule bookmark list
ferrule bookmark run <name> [<arg>...]
  [--connection <name>]
  [--format <fmt>] [--output <file>]
  [--limit <n>] [--offset <n>]
  [--timing] [--verbose]
  [--insecure] [--daemon]
ferrule bookmark delete <name>
```

See [Bookmarks](bookmarks.md).

### `connection` (alias `conn`) — registry and daemon

```bash
ferrule conn add <name> <url>
ferrule conn list
ferrule conn remove <name>
ferrule conn test <name> [--insecure] [--daemon]

ferrule conn set-password <name>      # interactive prompt; stores in OS keyring
ferrule conn delete-password <name>

ferrule conn start [--background]     # connection-pooling daemon
ferrule conn stop
ferrule conn status
ferrule conn restart
```

See [Connections](connections.md) and [Daemon](daemon.md).

## Flags reference

### Output flags

Available on `query`, `tables`, `describe`, `explain`, `dump`,
`watch`, `repl`, `bookmark run`.

| Flag | Description |
|---|---|
| `-f, --format <fmt>` | One of `table`, `json`, `csv`, `yaml`, `raw`. Default: `json` |
| `-o, --output <file>` | Write results to a file (otherwise stdout) |
| `-n, --limit <n>` | Cap the number of rows returned. `0` disables paging for the call |
| `--offset <n>` | Skip the first `<n>` rows |
| `--timing` | Print connect/query/format timing to stderr |
| `-v, --verbose` | Echo resolved (redacted) URL and SQL to stderr |

### Connection flags

Available on every command that opens a connection.

| Flag | Description |
|---|---|
| `--insecure` | Disable TLS certificate-chain *and* hostname verification. Prints a warning to stderr. See [Security](security.md#tls-posture-per-backend) |
| `--daemon` | Route the request through the per-user connection-pooling daemon (must be running — `ferrule conn start`). See [Daemon](daemon.md) |

### Password flag (`query` and `watch` only)

| Flag | Description |
|---|---|
| `-p, --password <pwd>` | Pass the password explicitly. **Insecure** — leaks to shell history. Prefer `password_url` in [Configuration](configuration.md#password_url) |

### Query-only flags

| Flag | Description |
|---|---|
| `--file <path>` | Read SQL from a file |
| `--stdin` | Read SQL from stdin |
| `--param NAME=VALUE` | Set a `${NAME}` placeholder. Repeat for multiple parameters |
| `--param-file <path>` | Load parameters from a JSON object file |
| `--explain` | Wrap the SQL in EXPLAIN before sending |
| `--dry-run` | Print the resolved SQL and URL; do not connect |

### Explain-only flags

| Flag | Description |
|---|---|
| `--analyze` | Execute the statement and collect runtime statistics. Silently downgraded for INSERT / UPDATE / DELETE / DDL to avoid side effects |

### Dump-only flags

| Flag | Description |
|---|---|
| `--file <path>` | Output file (stdout if omitted) |
| `--dump-format <fmt>` | One of `csv`, `json`, `sql`. Default: `csv`. Distinct from `--format` |
| `--schema <name>` | Schema name; affects qualified table names in SQL dumps |

### Load-only flags

| Flag | Description |
|---|---|
| `-t, --table <name>` | Target table (inferred from file stem if omitted) |
| `-f, --format <fmt>` | `csv` or `json` (inferred from extension if omitted) |
| `--create-table` | Create the target table from the JSON schema before loading. JSON only |

### Watch-only flags

| Flag | Description |
|---|---|
| `-i, --interval <seconds>` | Polling interval. Default: `5`. Minimum: `1` |
| `--max-iterations <n>` | Stop after `<n>` iterations |
| `--diff` | Only print output when the result differs from the previous iteration |

### Daemon-only flags

| Flag | Description |
|---|---|
| `--background` (on `conn start`) | Fork to background and detach |

## Exit codes

| Code | Meaning |
|---|---|
| 0 | Success |
| 1 | Notable result — `ferrule diff` found schema differences. Reserved for future `--expect-rows`-style assertions and any future check / validate / lint commands. Follows the GNU `diff` / `grep` / `kubectl diff` convention: the command ran correctly and produced a result you likely want to gate on. **Not an error.** |
| 2 | Usage error (invalid CLI arguments, malformed SQL parsing). Matches clap's default exit code for argument-parse failures |
| 3 | Connection error (network, TLS, authentication, backend unavailable) |
| 4 | Query error (SQL syntax, constraint violation, schema mismatch) |

## Environment variables

| Variable | Effect |
|---|---|
| `FERRULE_<NAME>_PASSWORD` | Legacy password fallback for connection `<name>` (uppercased; hyphens become underscores). `production` → `FERRULE_PRODUCTION_PASSWORD` |
| `RUST_LOG` | Enable structured logging from ferrule and driver crates. `RUST_LOG=ferrule=debug` is a useful starting point |

`FERRULE_CONFIG` is **not** currently a recognized override; pass
`--config` on the command line.

## File locations

| File | Purpose | Path |
|---|---|---|
| Global config | Per-user defaults, connection profiles | `~/.config/ferrule/ferrule.toml` |
| Project config | Project-local overrides | `./.ferrule.toml` |
| Connections registry | Saved name → URL (managed by `ferrule conn add`) | `~/.config/ferrule/connections.toml` |
| Bookmarks | Saved query library | `~/.config/ferrule/bookmarks.toml` |
| REPL history | Persistent history file | `~/.cache/ferrule/history` |
| Daemon socket (Unix) | Per-user IPC for `--daemon` mode | `~/.cache/ferrule/daemon.sock` |
| Daemon PID | PID file for the running daemon | `~/.cache/ferrule/daemon.pid` |
| Daemon port (Windows) | TCP port file (Unix uses sockets) | `%LOCALAPPDATA%\ferrule\daemon.port` |

Paths use `dirs::config_dir()` and `dirs::cache_dir()` — replace
`~/.config` and `~/.cache` with the platform equivalent on macOS
(`~/Library/Application Support/`, `~/Library/Caches/`) and Windows
(`%APPDATA%`, `%LOCALAPPDATA%`).

## Type mapping

The canonical type-mapping table lives in [Backends](backends.md#type-mapping).

## Credential resolution

The canonical credential resolution stack lives in
[Concepts](concepts.md#the-credential-stack); the security
trade-offs of each scheme live in [Security](security.md).
