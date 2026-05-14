# Cross-DB Copy

`ferrule copy <SRC> <DST>` streams rows from one database into another,
translating column types via ferrule's unified `Value` enum. Postgres
→ SQLite snapshots, MySQL → MSSQL exports, anything → anything — one
command, no intermediate file, no third tool.

## What it does

Source and destination can be any pair of supported backends
(Postgres, MySQL, MSSQL, SQLite, opt-in Oracle). For each batch:

1. SELECT one page from the source, paged via `LIMIT/OFFSET` or the
   dialect equivalent (same machinery as `ferrule dump`).
2. Translate the source column types into a target-side DDL when
   `--create-table` is set (see "Type translation" below).
3. INSERT the page into the destination, with per-backend literal
   quoting from `ferrule_core::params::render_value`.

The first SELECT establishes the column shape. Subsequent SELECTs
keep paging until a partial page is returned.

## Conflict handling

The default is **non-destructive**: a copy into a non-empty existing
target table errors out before any INSERT runs (and before the source
SELECT is even issued).

| Target state | Default behavior |
|---|---|
| Doesn't exist + `--create-table` | Create + insert |
| Doesn't exist, no `--create-table` | Usage error (exit 2) |
| Exists, empty | Insert (treated as fresh) |
| Exists, non-empty | **Error (exit 4)** with hint to pass `--if-exists` |

Override the default with `--if-exists <strategy>`:

- **`error`** *(default)* — refuse if target is non-empty. Source is
  never touched.
- **`append`** — INSERT alongside existing rows. UNIQUE / PK conflicts
  surface as driver errors and abort the run.
- **`truncate`** — `DELETE FROM <tbl>` then INSERT. Destructive,
  requires `--yes` when stdin is a TTY. The DELETE and the first batch
  run inside the same transaction so a transient first-INSERT failure
  cannot leave the target wiped + empty.
- **`skip`** — INSERT new rows; silently drop rows whose primary key
  already exists on the destination. Per-backend codegen:
  - PG / SQLite: `INSERT … ON CONFLICT (pk) DO NOTHING`
  - MySQL: `INSERT IGNORE INTO …`
  - MSSQL / Oracle: `MERGE … WHEN NOT MATCHED THEN INSERT`
- **`upsert`** — INSERT new rows; UPDATE every non-PK column when the
  primary key already exists. Per-backend codegen:
  - PG / SQLite: `INSERT … ON CONFLICT (pk) DO UPDATE SET col = EXCLUDED.col, …`
  - MySQL: `INSERT … ON DUPLICATE KEY UPDATE col = VALUES(col), …`
  - MSSQL / Oracle: full `MERGE` with both `WHEN MATCHED THEN UPDATE`
    and `WHEN NOT MATCHED THEN INSERT` branches.

`skip` and `upsert` require a declared primary key on the destination
table. If the destination table has no PK, the copy hard-errors before
any source SELECT runs, pointing at the future `--key COL[,COL...]`
override (tracked under issue #43). PK columns are resolved on the
destination via the per-backend introspection method
(`Connection::primary_key`); cross-backend copies may need an explicit
`SELECT col AS "DEST_NAME" …` alias when source and destination disagree
on identifier case (Oracle uppercases unquoted identifiers).

Conflict resolution always runs through the generic INSERT path: the
native bulk loaders (`COPY`, `BULK INSERT`, `LOAD DATA`, `Batch`) carry
no MERGE / ON CONFLICT semantics. Passing `--bulk-native=auto` or
`--bulk-native=on` alongside `--if-exists skip|upsert` emits a one-line
stderr notice and silently degrades the bulk path for that copy.

## Atomicity

By default each batch (`--batch`, default 1000) is committed
independently. Progress survives mid-copy failure; partial state is
visible on the target. This matches `ferrule load`'s semantics.

`--atomic` wraps the entire copy in a single target-side transaction.
Recommended for snapshots; avoid for million-row migrations because
the target holds locks for the full duration.

## Examples

```bash
# Snapshot prod Postgres into a local SQLite file.
ferrule copy \
  "postgres://ferrule:ferrule@127.0.0.1:15432/ferrule?sslmode=disable" \
  "sqlite:///tmp/snap.db" \
  --table test_users --create-table

# Refresh: clear the destination and reload (interactive confirmation).
ferrule copy prod-pg snap-sqlite --table test_users --if-exists truncate --yes

# Project a subset via --query.
ferrule copy prod-mysql warehouse-mssql \
  --query "SELECT id, name FROM users WHERE active = 1" \
  --into active_users --create-table

# Schema-level refresh: copy every table from prod into a fresh
# SQLite snapshot, FK-ordered. --include narrows the selection;
# --exclude trims the noise.
ferrule copy prod-pg snap-sqlite --all-tables --create-table \
  --include 'app_*' --exclude '*_audit'

# Atomic snapshot, all-or-nothing on the target.
ferrule copy prod-pg snap-sqlite \
  --table test_users --create-table --atomic
```

## Type translation

When `--create-table` is set, ferrule issues a
`CREATE TABLE IF NOT EXISTS` against the destination, mapping each
source column's `TypeHint` to a destination type:

| `TypeHint`   | Postgres        | MySQL         | MSSQL              | SQLite | Oracle              |
|--------------|-----------------|---------------|--------------------|--------|---------------------|
| `Bool`       | BOOLEAN         | TINYINT(1)    | BIT                | INTEGER| NUMBER(1)           |
| `Int64`      | BIGINT          | BIGINT        | BIGINT             | INTEGER| NUMBER(19)          |
| `Float64`    | DOUBLE PRECISION| DOUBLE        | FLOAT              | REAL   | BINARY_DOUBLE       |
| `Decimal`    | NUMERIC         | DECIMAL(38,10)| DECIMAL(38,10)     | NUMERIC| NUMBER              |
| `String`     | TEXT            | TEXT          | NVARCHAR(MAX)      | TEXT   | CLOB                |
| `Bytes`      | BYTEA           | LONGBLOB      | VARBINARY(MAX)     | BLOB   | BLOB                |
| `Date`       | DATE            | DATE          | DATE               | TEXT   | DATE                |
| `Time`       | TIME            | TIME          | TIME               | TEXT   | TIMESTAMP           |
| `DateTime`   | TIMESTAMP       | DATETIME      | DATETIME2          | TEXT   | TIMESTAMP           |
| `DateTimeTz` | TIMESTAMPTZ     | DATETIME      | DATETIMEOFFSET     | TEXT   | TIMESTAMP WITH TZ   |
| `Json`       | JSONB           | JSON          | NVARCHAR(MAX)      | TEXT   | CLOB                |
| `Uuid`       | UUID            | CHAR(36)      | UNIQUEIDENTIFIER   | TEXT   | RAW(16)             |
| `Array`      | JSONB           | JSON          | NVARCHAR(MAX)      | TEXT   | CLOB                |

The mapping favours portability over fidelity. `Decimal` collapses to
`(38,10)` precision on backends that need it; `Array` is stored as
JSON-ish text on every backend except Postgres / MySQL where the
native type carries it. SQLite uses dynamic typing, so most types
collapse to its five storage classes.

`NOT NULL` is preserved per source column metadata. Primary keys,
indexes, defaults, and check constraints are *not* copied — Phase 1
focuses on data movement.

## Native bulk paths (`--bulk-native`)

By default, `ferrule copy` builds one multi-row INSERT per page and
sends it via the standard driver path. That works everywhere but is
5–50× slower than each backend's native bulk loader for
million-row migrations.

Pass `--bulk-native <mode>` to opt into the native path:

| Mode    | Behavior                                                                                                                  |
|---------|---------------------------------------------------------------------------------------------------------------------------|
| `off`   | *(default)* Always use the generic multi-row INSERT path. v1 baseline.                                                    |
| `auto`  | Try the native path; on `BulkUnavailable` emit one stderr warning and fall back to generic INSERT for that batch.         |
| `on`    | Require the native path. `BulkUnavailable` surfaces as a hard error referencing `--bulk-native`.                          |

The flag is destination-only: each backend ships a native path
*if it has one*. SQLite stays on the generic path always (its
bottleneck is fsync, not parse/plan).

| Destination | Native path                                              | Common `BulkUnavailable` triggers                                                          |
|-------------|----------------------------------------------------------|--------------------------------------------------------------------------------------------|
| Postgres    | `COPY <tbl> (cols) FROM STDIN WITH (FORMAT TEXT)`        | Target is a VIEW / materialized view / foreign table.                                      |
| MSSQL       | `tiberius::Client::bulk_insert` (TDS bulk-load token)    | Target is not a base table; `Invalid object name`.                                         |
| MySQL       | `LOAD DATA LOCAL INFILE` via a per-call infile handler   | Server-side `local_infile=OFF` (default in MySQL 8.0+ — set `local_infile=ON` to enable).  |
| Oracle      | `oracle::Batch` (array DML via ODPI-C)                   | `ORA-01031` insufficient privilege; `ORA-00942` table does not exist; Instant Client missing.|
| SQLite      | No native loader. `--bulk-native=on` returns a hard error.| —                                                                                          |

### Format choices

- **Postgres** uses `FORMAT TEXT`, not `BINARY`. Binary payloads can
  be *larger* than text on int-heavy schemas (4-byte length prefixes
  inflate small ints), and most of the win over INSERT comes from
  skipping parse/plan, not from the wire format. Binary COPY is
  tracked as a follow-up under issue #36.
- **MySQL** ships UTF-8 tab/newline-delimited with backslash escapes
  matching `ESCAPED BY '\\'`. The per-call local infile handler is
  installed only for the duration of one `bulk_insert_rows` call —
  a hostile `LOAD DATA LOCAL INFILE '/etc/passwd'` typed into
  `ferrule query` on the same connection fails immediately because
  there is no handler installed at that moment.
- **Oracle** uses array DML (safe default — respects triggers,
  constraints, indexes). The faster but more invasive
  `INSERT /*+ APPEND */` direct-path is opt-in and tracked under
  issue #37; on this path bulk loads bypass triggers and require
  exclusive locks until commit.

### What surfaces in `--bulk-native=auto`

A successful bulk batch in `auto` mode prints nothing extra. On
fallback, you'll see one stderr line per affected batch:

```
[ferrule] bulk: <backend> path unavailable: <reason>; using generic INSERT
```

Use `--verbose` to additionally log one line per successful bulk batch.

## Schema-level copy (`--all-tables`)

`ferrule copy <SRC> <DST> --all-tables` discovers every table on the
source, orders them so foreign-key parents load before children, and
copies each one through the same per-table pipeline `--table` uses.
This is the canonical "refresh dev from prod" workflow.

```bash
ferrule copy prod-pg snap-sqlite --all-tables --create-table
ferrule copy prod-pg snap-sqlite --all-tables --if-exists truncate --yes
```

### Filtering

Two repeatable glob flags narrow which tables get copied. Globs use
shell-style `*` and `?`, matched case-sensitively against the
identifier shape the source returns:

```bash
# Only app_* tables, skipping the *_audit log shadows.
ferrule copy prod-pg snap-sqlite --all-tables --create-table \
  --include 'app_*' --exclude '*_audit'
```

- `--include` defaults to "everything"; multiple `--include` patterns
  OR together.
- `--exclude` is always applied after the include filter.
- Tables referenced by foreign keys but excluded from the selection
  are simply dropped from the dependency graph — the copy will not
  block waiting for them.

### Order, cycles, and `--no-fk-check`

The load order is computed via Kahn's algorithm over the
destination-aware view of the FK graph (`Connection::list_foreign_keys`).
Independent tables retain their `list_tables` order so successive
runs are deterministic.

Foreign-key *cycles* hard-error before any copy starts, with the
cycle path in the message. Pass `--no-fk-check` to copy in
discovery order anyway — useful when:

- the destination has deferrable FKs (Postgres `DEFERRABLE INITIALLY
  DEFERRED`) or FK enforcement is off (SQLite default;
  `SET session_replication_role = replica` on Postgres);
- you plan to drop and recreate constraints after the copy;
- the cycle is across self-referential columns you'll backfill later.

Self-referential foreign keys (`tree.parent_id REFERENCES tree(id)`)
do *not* count as cycles and are loaded in a single pass.

### Per-table progress

With `--verbose`, ferrule prints one line at the start and end of
each table:

```
[ferrule] [3/12] copying users…
[ferrule] [3/12] users: 4523 rows
```

### Combining strategies

`--all-tables` honours every other copy flag:

- `--if-exists truncate --yes` clears each destination table once at
  the top of its own copy — `--yes` is consulted once for the run,
  not per table.
- `--if-exists skip|upsert` requires a PK per table; tables without
  a PK hard-error at their own step (see [Conflict handling]
  (#conflict-handling)).
- `--bulk-native auto|on` applies per table; `skip|upsert` still
  force the generic path table-by-table.
- `--atomic` wraps *each table* in its own transaction. A
  cross-table single transaction is not in this release (it would
  require deferrable FK support on the destination).

## Limits

- **Composite-key / unique-index conflict resolution.** `skip` /
  `upsert` use the destination's declared primary key. A user-supplied
  `--key COL[,COL...]` override (for PK-less tables and conflicts on
  unique indexes) is tracked under #43.
- **Shared connection flags.** `--ssh-tunnel`, `--ssh-key`,
  `--proxy-url`, and `--insecure` apply to *both* source and target
  in this release. Per-side `--src-*` / `--dst-*` flags are tracked
  under #44.
- **No daemon routing for `copy`.** Use direct connections for both
  sides. Tracked under #10.
