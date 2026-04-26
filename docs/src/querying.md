# Querying Data

`ferrule query` is the workhorse: it takes a connection and a SQL
statement, executes it, and prints the result.

```bash
ferrule query "sqlite::memory:" "SELECT 1 + 1 AS answer;"
```

Connection can be a raw URL, a profile from `.ferrule.toml`, or a
saved registry name (see [Connections](connections.md) for the full
resolution order).

## Where the SQL comes from

Three options, mutually exclusive:

```bash
# 1. Inline as the second argument
ferrule query demo "SELECT * FROM users LIMIT 10;"

# 2. From a file
ferrule query demo --file queries/users_active.sql

# 3. From stdin (good for `cat … | ferrule …` pipelines)
echo "SELECT 1;" | ferrule query demo --stdin
```

## Output formats

| Format | Flag | Best for |
|---|---|---|
| Table | `--format table` | Human-readable terminal output |
| JSON | `--format json` | Piping to `jq`, APIs, structured tooling |
| CSV | `--format csv` | Spreadsheets, data pipelines |
| YAML | `--format yaml` | Human-readable structured data, config-y outputs |
| Raw | `--format raw` | Tab-delimited; minimal noise for shell scripts |

The default is **JSON**, regardless of whether stdout is a terminal
or a pipe. To get pretty tables interactively, either pass `--format
table` per command, or set `format = "table"` under `[default]` in
`.ferrule.toml` (see [Configuration](configuration.md)).

Write to a file directly with `--output`:

```bash
ferrule query demo "SELECT * FROM events" --format csv --output events.csv
```

## Paging

`--limit` and `--offset` push paging down to the server when possible:

```bash
# Postgres, MySQL, SQLite — emits LIMIT N OFFSET M
ferrule query demo "SELECT * FROM users ORDER BY id" --limit 25 --offset 50

# MSSQL — emits OFFSET M ROWS FETCH NEXT N ROWS ONLY
ferrule query mssql_db "SELECT * FROM users ORDER BY id" --limit 25 --offset 50
```

A few quirks worth knowing:

- **There is a default limit.** The shipped default in `[default]` is
  `limit = 1000`. If you don't pass `--limit` and don't override the
  default, ferrule still appends `LIMIT 1000`. To disable globally,
  set `limit = 0` in `[default]`.
- **`--limit 0` disables paging for the current call.** Useful when
  you really want every row.
- **Multi-statement batches reject `--limit` / `--offset`.** Ferrule
  has no safe way to inject paging into one statement of many. Pass
  `--limit 0` (or set the global default to 0) for batches.
- **Auto-injection is single-statement only.** If your SQL already has
  `LIMIT` / `OFFSET`, ferrule does not stack a second one — it just
  uses what you wrote.

## Multi-statement batches

Backends with native multi-statement support (PostgreSQL, MSSQL) can
take several `;`-separated statements in one round trip:

```bash
ferrule query demo --limit 0 "
  INSERT INTO logs (msg) VALUES ('startup');
  SELECT COUNT(*) FROM logs;
"
```

Behavior:

- **Result sets** print to stdout, one per `SELECT`, prefixed with
  a `-- Result set N` separator.
- **DML row counts** print to stderr as `-- Statement N: K rows
  affected`.
- **MySQL, SQLite, and Oracle** at the current driver layer don't
  multiplex multiple statements per query — split into separate
  `ferrule query` calls or use a stored procedure / script
  invocation specific to that backend.

## Parameterized queries

Use `${name}` placeholders and pass values via `--param`:

```bash
ferrule query demo \
  "SELECT * FROM events WHERE severity = \${sev} AND created_at > \${date}" \
  --param "sev=error" \
  --param "date=2026-01-01"
```

Type inference rules:

- `true` / `false` → boolean.
- An integer (`-42`, `0`, `1000`) → `Int64`.
- A decimal (`3.14`, `-0.5`) → `Float64`.
- Anything else → `String` (properly quoted for the backend's
  dialect).

Backend-specific placeholder forms (`$1` for Postgres, `?` for
MySQL, etc.) are handled internally — you always write `${name}`
in the SQL, and ferrule rewrites it.

### Loading many parameters from JSON

```bash
cat > params.json <<'JSON'
{
  "id": 42,
  "name": "Alice",
  "active": true
}
JSON

ferrule query demo 'SELECT * FROM users WHERE id = ${id} AND name = ${name}' \
  --param-file params.json
```

`--param-file` and inline `--param` flags can be combined; later
flags override earlier ones for the same key.

## Dry run

Preview the substituted SQL and the resolved (redacted) URL without
opening a connection or executing anything:

```bash
ferrule query demo 'SELECT * FROM users WHERE id = ${id}' \
  --param "id=42" --dry-run
# [ferrule] Would execute against postgres://ferrule:***@127.0.0.1:15432/ferrule
# [ferrule] SQL: SELECT * FROM users WHERE id = 42
```

Use `--dry-run` to confirm parameter substitution and URL resolution
when debugging a ferrule invocation.

## TLS verification (`--insecure`)

`--insecure` disables both certificate-chain *and* hostname
verification. It applies to the current call only and prints a
warning to stderr:

```bash
ferrule query mssql_demo "SELECT 1;" --insecure
# Warning: --insecure disables TLS certificate verification.
```

For the narrower MSSQL-specific case, append
`?trustServerCertificate=true` to the URL — it accepts the self-
signed cert without disabling hostname checks. See
[Security](security.md#tls-posture-per-backend) for the full
breakdown.

## Useful flag combinations

```bash
# See where time is being spent
ferrule query demo "SELECT * FROM big_table" --timing

# Echo the resolved (redacted) URL and SQL before executing
ferrule query demo "SELECT 1" --verbose

# Get JSON with timing info for benchmarking
ferrule query demo "SELECT * FROM events" --format json --timing

# Pipe to a file with explicit format
ferrule query demo "SELECT * FROM events" --format csv --output events.csv

# Route through the connection-pooling daemon (after `ferrule conn start`)
ferrule query demo "SELECT 1" --daemon
```

See also: [Schema Introspection](schema.md), [Bookmarks](bookmarks.md),
[Reference](reference.md) for the full flag list.
