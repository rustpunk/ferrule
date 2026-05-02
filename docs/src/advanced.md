# EXPLAIN, Dump, and Watch

Three features that go beyond plain `query`: looking at execution
plans, moving data in and out of tables, and watching a query change
over time. None are essential — but they're the things you'll reach
for once you're past the basics.

## EXPLAIN

`ferrule explain` shows the execution plan for a query without
running it (the `--analyze` variant *does* run it). The output format
varies by backend; ferrule wraps the SQL in the right syntax and
returns whatever the backend produces.

### When to use it

- A query is slower than you expected. EXPLAIN tells you whether
  there's a sequential scan, a missing index, or a join blowing up.
- Before shipping a query you've never seen the plan for. Cheap
  insurance.
- After adding an index — confirm the planner is actually using it
  (it sometimes isn't, especially with low row counts).

### Basic usage

```bash
# Default: estimated plan, no execution
ferrule explain demo "SELECT * FROM customers WHERE name = 'Alice';"

# With actual statistics — runs the query
ferrule explain demo "SELECT * FROM customers WHERE name = 'Alice';" --analyze
```

### `--analyze` and DML

`--analyze` would execute the statement, which is a problem for
`INSERT` / `UPDATE` / `DELETE` / DDL: you'd cause real changes by
asking for a plan. Ferrule detects modifying statements and silently
falls back to the non-executing variant, so `ferrule explain demo
"DELETE FROM users WHERE id = 1" --analyze` is safe to run — you
get the estimated plan, not an actual delete.

A statement is "modifying" if it starts (case-insensitively) with
`INSERT`, `UPDATE`, `DELETE`, `CREATE`, `DROP`, `ALTER`, `TRUNCATE`,
or `MERGE`, or if it contains a data-modifying CTE such as
`WITH cte AS (UPDATE ...) ...`. Ferrule tracks parenthesis depth
while scanning; keywords inside subqueries or CTE bodies are still
caught, so `WITH t AS (INSERT INTO ...) SELECT * FROM t` is correctly
flagged as modifying.

### Per-backend output formats

| Backend | `--analyze` off | `--analyze` on | Format |
|---|---|---|---|
| PostgreSQL | `EXPLAIN (FORMAT JSON, COSTS) …` | `EXPLAIN (FORMAT JSON, ANALYZE, BUFFERS, TIMING, COSTS) …` | JSON |
| MySQL | `EXPLAIN FORMAT=JSON …` | (same — MySQL EXPLAIN doesn't execute) | JSON |
| SQLite | `EXPLAIN QUERY PLAN …` | (same) | Text |
| MSSQL | `SET SHOWPLAN_XML ON; …` | `SET STATISTICS XML ON; …` | XML |
| Oracle | `EXPLAIN PLAN FOR …; SELECT … FROM TABLE(DBMS_XPLAN.DISPLAY('','',''));` | `… DBMS_XPLAN.DISPLAY('','','ALLSTATS LAST')` | Text |

JSON plans pipe naturally into `jq` for filtering (Postgres: pull
out node types and costs; MySQL: walk the nested `query_block`).
The XML and text formats are display-oriented — pretty-print
yourself if you need to compare two plans side by side.

### Example

```bash
ferrule explain demo \
  "SELECT c.id, c.name, COUNT(o.id) AS order_count
   FROM customers c LEFT JOIN orders o ON o.customer_id = c.id
   GROUP BY c.id;" \
  | jq '.[0]["Plan"]["Node Type"]'
# "HashAggregate"
```

## Dump and Load

`ferrule dump` exports a table to CSV / JSON / SQL. `ferrule load`
imports CSV / JSON back into a table.

### When to reach for them

- **Fixture generation.** Pull a sanitized 1000-row sample from
  prod into a JSON fixture for tests.
- **Cross-backend portability.** Dump from MySQL, load into
  Postgres, no manual schema translation needed (within ferrule's
  unified `Value` types).
- **Snapshots** of small to medium tables for offline inspection.

For larger jobs, the native shells win on raw throughput:
`pg_dump` / `pg_restore`, `psql \copy`, `mysqldump`, `bcp`. Ferrule
batches but doesn't attempt to compete with `COPY FROM STDIN`.

### Dump

```bash
# CSV (default)
ferrule dump demo customers --dump-format csv > customers.csv

# JSON — preserves types better than CSV
ferrule dump demo customers --dump-format json > customers.json

# SQL INSERT statements — useful for re-importing into a fresh DB
ferrule dump demo customers --dump-format sql > customers.sql

# Schema-qualified table (Postgres / MSSQL)
ferrule dump demo customers --dump-format sql --schema public > customers.sql

# Stream to a file directly with --file
ferrule dump demo customers --dump-format csv --file customers.csv
```

> **`--dump-format` vs `--format`.** They're separate. `--format`
> controls the *display* format ferrule uses for stderr / output;
> `--dump-format` controls the on-disk dump format. They almost
> never need to match.

### Load

```bash
# CSV — column order from the first row
ferrule load demo events.csv --table events

# JSON array of objects — column names from object keys
ferrule load demo events.json --table events

# Format inferred from the file extension
ferrule load demo events.csv               # CSV inferred
ferrule load demo events.json              # JSON inferred

# Override
ferrule load demo data --table events --format json
```

#### `--create-table` (JSON only)

When the target table doesn't exist, JSON loads can infer a schema
from the first record:

```bash
ferrule load demo new_events.json --table events --create-table
```

The first object in the JSON array becomes the schema template:

- Keys → column names.
- Number values → `Int64` or `Float64` depending on whether the
  literal contains a decimal point.
- Boolean values → `Bool`.
- String values → `String`.
- `null` → nullable column with type from a later record (best
  effort; objects with all-null first records fail).

This is "good enough for fixtures." For production schemas, write
the `CREATE TABLE` by hand.

#### Behavior notes

- **Insertion is batched** (default ~1000 rows per round-trip).
- **Errors abort the load.** Ferrule does not transactionally roll
  back already-loaded rows — be ready for partial state if a
  malformed record sneaks in mid-file.
- **No type coercion.** A CSV with `"true"` in a `Bool` column is
  fine; a CSV with `"yes"` is not. Pre-clean upstream.

## Export

`ferrule export` streams the result of an arbitrary SQL query directly to
a file in CSV, JSON, JSONL, or SQL INSERT format. Unlike `dump`, which
only handles single tables, `export` works with any SELECT statement.

### Basic usage

```bash
ferrule export demo "SELECT * FROM events" --file events.csv
ferrule export demo "SELECT * FROM events" --format json --file events.json
ferrule export demo "SELECT * FROM events" --format jsonl
```

Page-size is tuned for the backend; it fetches rows in chunks to keep
memory usage flat.

```bash
ferrule export demo "SELECT * FROM events" --page-size 5000 --file events.csv
```

### Formats

| Format | Description |
|---|---|
| `csv` | Comma-separated values; newlines inside strings are escaped |
| `json` | One JSON array of objects |
| `jsonl` | One JSON object per line |
| `sql` | `INSERT INTO ... VALUES (...)` statements |

### Notes

- `--file` is optional; without it, the result goes to stdout.
- `--page-size` defaults to 1000 and controls the server-side
  chunk size. Use `--page-size 0` to disable paging (not recommended
  for huge result sets).
- `--limit` and `--offset` are respected just like `query`.

## Watch mode

Re-execute a query at fixed intervals or whenever a watched file changes.

### When it's useful

- Watching a job queue drain.
- Verifying a deploy that changes a counter.
- Polling a long-running migration's progress.
- Smoke-testing during incident response without typing `\!\!` in
  `psql` over and over.
- Re-running a query as you edit a `.sql` file in your editor.

### Basic usage

```bash
# Re-runs every 5 seconds until you Ctrl-C
ferrule watch demo "SELECT COUNT(*) FROM events;"

# Faster cadence
ferrule watch demo "SELECT COUNT(*) FROM events;" --interval 1

# Bounded run — exits after 10 iterations
ferrule watch demo "SELECT NOW();" --interval 1 --max-iterations 10
```

`--interval` is in seconds; the minimum useful value is 1 (the
backend round-trip dominates anything tighter).

### `query --watch` — shorthand

You can also use `--watch` on `query` itself, which delegates to the
same watch loop:

```bash
ferrule query demo "SELECT COUNT(*) FROM events;" --watch
ferrule query demo "SELECT COUNT(*) FROM events;" --watch --watch-interval 2
```

This is identical to `ferrule watch` with the same arguments; it's
just a convenience when you start with a query and realize you want
to keep polling it.

### `--file-path` — watch a file for changes

Instead of polling on an interval, trigger re-execution whenever a
file changes on disk:

```bash
ferrule watch demo --file-path ./query.sql
```

The SQL is re-read from the file every time the filesystem watcher
fires (with a 100ms debounce). Use this when you're editing the query
in an editor and want ferrule to re-run it every time you save.

### `--diff` — only print on change

Without `--diff`, watch prints a header and the full result on every
iteration. With `--diff`, it suppresses output when the result is
identical to the previous one:

```bash
ferrule watch demo "SELECT COUNT(*) FROM events;" --interval 2 --diff
```

That makes it trivial to leave a `watch --diff` running in a corner
of your tmux session and only get noise when something actually
changes.

### `--exit-on-error` — fail fast

By default, watch logs connection or query errors to stderr and
keeps polling. With `--exit-on-error`, the command terminates on
the first failure:

```bash
ferrule watch demo "SELECT COUNT(*) FROM events;" --exit-on-error
```

Useful in CI pipelines or wrapper scripts where a broken connection
should stop the job immediately rather than spamming errors forever.

### `--bell` — terminal bell on change

When paired with `--diff`, rings the terminal bell (ASCII BEL, `\x07`)
whenever the output changes:

```bash
ferrule watch demo "SELECT COUNT(*) FROM events;" --diff --bell
```

Pair with a terminal that flashes the window on bell (e.g. iTerm2
"Flash visual bell") to get passive attention while you work
elsewhere.

### Notes

- Ctrl-C exits cleanly — the in-flight query is allowed to finish.
- The backend connection is reused across iterations, so cost-per-
  iteration is roughly one round-trip plus the query itself. Pair
  with the [connection-pooling daemon](daemon.md) for tight loops.
- Use `--format raw` or `--format json` for clean output that's
  easy to diff visually or pipe into another tool.
