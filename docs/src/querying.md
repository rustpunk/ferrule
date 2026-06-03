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
| Markdown | `--format markdown` (alias `md`) | GFM pipe tables for docs / PR descriptions / issue comments |
| JSONL | `--format jsonl` (alias `ndjson`) | Streaming, one JSON object per line, `jq -c` friendly |
| HTML | `--format html` | Static `<table>` snippets for emails, reports, dashboards |

The default is **JSON**, regardless of whether stdout is a terminal
or a pipe. To get pretty tables interactively, either pass `--format
table` per command, or set `format = "table"` under `[default]` in
`.ferrule.toml` (see [Configuration](configuration.md)).

Write to a file directly with `--output`:

```bash
ferrule query demo "SELECT * FROM events" --format csv --output events.csv
```

## Large results: size guards

The default `table` / `json` rendering buffers the **whole** result in
memory so it can lay out columns. To keep a pathological result from
OOM-killing the process, `ferrule-sql` applies three constant-memory
size guards to every read:

| Guard | Default | What it caps |
|-------|---------|--------------|
| `max_cell_bytes` | 64 MiB | a single value (e.g. a giant `bytea` / `TEXT`) |
| `max_row_bytes` | 256 MiB | one row's summed cell payloads |
| `max_total_buffered_bytes` | 1 GiB | the running total an eager buffered render may accumulate |

When a guard is exceeded, the query fails fast with a structured
diagnostic naming the offending row / column and the cap, instead of
allocating without bound. The error message points you at the options
below.

These are coarse ceilings, not an RSS budget — they are checked
incrementally as rows decode, so the checker itself never allocates
proportional to the data it inspects. Embedders tune them through
`ferrule_sql::SizeGuards` on the connection; a `0` cap disables that
dimension.

### Working past the cap

When a result legitimately exceeds the default ceilings you have two
options:

1. **Raise the relevant cap.** For a known-wide column or a deliberately
   large export, lift `max_cell_bytes` / `max_row_bytes` /
   `max_total_buffered_bytes` (embedders set these on the connection via
   `ferrule_sql::SizeGuards`).
2. **Ingest row-at-a-time instead of buffering.** Embedders that need to
   process an unbounded result under a fixed memory budget use the
   `Connection::query_cursor` streaming API, which pulls from a native
   database cursor at `O(batch)` memory and never materializes the whole
   result — the guards then apply per row, so a single oversized cell
   still fails fast while the stream as a whole is unbounded in length.
   The row-oriented `csv` / `jsonl` formats are the natural sink for that
   streamed output.

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

Backends with native multi-statement support (PostgreSQL, MySQL, MSSQL) or
PL/SQL block support (Oracle) can take several `;`-separated
statements in one round trip:

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
- **SQLite** at the current driver layer doesn't
  multiplex multiple statements per query — split into separate
  `ferrule query` calls or use a stored procedure / script
  invocation specific to that backend.
- **Oracle** now supports semicolon-separated batches including
  anonymous PL/SQL blocks (`BEGIN … END`), control structures
  (`IF … END IF`, `LOOP … END LOOP`, `CASE … END CASE`), and
  mixed DML/DDL. See [Backends](backends.md#multi-statement-support).

## Transactions

Wrap the entire statement batch in a single backend-aware outer
transaction with `--begin`. The batch (whether one statement or many)
commits at the end unless `--rollback` is set.

```bash
# Implicit COMMIT at end (--begin alone implies COMMIT):
ferrule query demo --begin \
  "INSERT INTO orders (user_id, total) VALUES (1, 99.50);
   UPDATE users SET last_order_at = NOW() WHERE id = 1"

# Explicit ROLLBACK — useful for dry-run / read-only snapshot
# semantics even on statements that would otherwise write.
ferrule query demo --begin --rollback \
  "DELETE FROM cache WHERE expires_at < NOW(); SELECT count(*) FROM cache"

# --commit is the explicit, redundant form. It exists for symmetry
# with --rollback when scripts compose flags dynamically.
ferrule query demo --begin --commit "INSERT INTO t VALUES (1)"
```

Semantics:

- The batch runs on a single live connection, so every statement
  (and the BEGIN / COMMIT / ROLLBACK that bracket it) ride the same
  TCP round trip.
- **Inner statement failure**: ferrule best-effort rolls back the
  wrapping transaction, prints
  `[ferrule] inner statement failed — rolled back wrapping transaction`
  on stderr, and surfaces the original SQL error as exit code 4.
- **`--rollback` on success**: ferrule still rolls back and prints
  `[ferrule] explicit ROLLBACK (--rollback)` on stderr; exit code 0
  when SQL + ROLLBACK both succeed.
- `--commit` requires `--begin` (clap exit 2). `--rollback` requires
  `--begin`. `--commit` and `--rollback` conflict (clap exit 2).
- `--begin --daemon` is rejected: the daemon path doesn't guarantee
  per-tick connection affinity, which would silently dissolve the
  transaction across pool checkouts.
- `--begin --watch` is rejected: each watch tick would reopen a
  separate transaction.
- `--begin --bench N` wraps the **whole loop** in ONE outer
  transaction (not N separate ones). Pairs with `--bench --rollback`
  for side-effect-free microbenchmarks.
- **Backend SQL emitted**: `BEGIN` / `COMMIT` / `ROLLBACK` for
  PostgreSQL, MySQL, SQLite. `BEGIN TRANSACTION` /
  `COMMIT TRANSACTION` / `ROLLBACK TRANSACTION` for MSSQL. Oracle
  has implicit transactions, so the BEGIN is a no-op and only
  COMMIT / ROLLBACK are sent.

Read-only `--begin SELECT` is legal — useful for snapshot-isolation
requirements where the entire read must observe one consistent
point-in-time view.

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
