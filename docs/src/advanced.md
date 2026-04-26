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
or `MERGE`. `WITH cte AS (UPDATE ...) ...` (Postgres data-modifying
CTEs) is *not* detected — be careful.

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

## Watch mode

Re-execute a query at fixed intervals.

### When it's useful

- Watching a job queue drain.
- Verifying a deploy that changes a counter.
- Polling a long-running migration's progress.
- Smoke-testing during incident response without typing `\!\!` in
  `psql` over and over.

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

### Notes

- Ctrl-C exits cleanly — the in-flight query is allowed to finish.
- The backend connection is reused across iterations, so cost-per-
  iteration is roughly one round-trip plus the query itself. Pair
  with the [connection-pooling daemon](daemon.md) for tight loops.
- Use `--format raw` or `--format json` for clean output that's
  easy to diff visually or pipe into another tool.
