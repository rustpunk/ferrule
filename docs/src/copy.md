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

The `skip` (`INSERT … ON CONFLICT DO NOTHING` / `INSERT IGNORE` /
`MERGE … WHEN NOT MATCHED`) and `upsert` strategies are tracked as a
backlog enhancement — they require detecting the target's primary key
per backend.

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

## Limits (Phase 1)

- **Generic INSERT path only.** Backend-native bulk loaders
  (Postgres `COPY FROM STDIN`, MySQL `LOAD DATA`, MSSQL `BULK INSERT`,
  Oracle direct-path) are tracked as a Phase 2 enhancement.
- **One table at a time.** Schema-level copy with FK ordering is
  Phase 2.
- **`error` / `append` / `truncate` strategies only.** `skip` and
  `upsert` are Phase 2.
- **Shared connection flags.** `--ssh-tunnel`, `--ssh-key`,
  `--proxy-url`, and `--insecure` apply to *both* source and target
  in Phase 1. Per-side `--src-*` / `--dst-*` flags are tracked as a
  backlog enhancement; today, source and target must be reachable
  through the same tunnel/proxy.
- **No daemon routing.** Use direct connections for both sides.
