# Schema Introspection

Two commands cover the bulk of "what's in this database?" workflows:
`ferrule tables` lists tables, and `ferrule describe` shows column
shapes for one of them. Both work the same way against every backend.

## When to use them

- **Exploring an unfamiliar database.** Drop in, list tables,
  describe the ones that look interesting. Same flow on Postgres,
  MySQL, MSSQL, SQLite, or Oracle.
- **Scripted schema checks.** CI step that asserts a table has the
  columns it should, or a pre-deploy guard that fails if a migration
  is missing.
- **ORM / codegen scaffolding.** Generate model definitions from
  live introspection rather than hand-rolled SQL.
- **Onboarding.** A new engineer can `tables` + `describe` their way
  around without learning each backend's metadata catalog by heart.

For the deepest backend-specific introspection (Postgres `\d+` with
indexes, constraints, and triggers; MySQL `SHOW CREATE TABLE`; MSSQL
sys catalogs; etc.) reach for the native shell. Ferrule's surface is
the cross-backend subset that's worth unifying.

## List tables

```bash
ferrule tables demo
```

JSON output is the default:

```json
[
  {"table_name": "customers"},
  {"table_name": "events"},
  {"table_name": "schema_migrations"}
]
```

For a terminal-friendly view, add `--format table`:

```bash
ferrule tables demo --format table
```

The list comes from the backend's metadata catalog: `pg_catalog`
(plus `information_schema`) on Postgres, `information_schema` on
MySQL, `sys.tables` on MSSQL, `sqlite_schema` (formerly
`sqlite_master`) on SQLite, `ALL_TABLES` on Oracle. Views and
materialized views are excluded — only base tables.

## Describe a table

```bash
ferrule describe demo customers
```

```json
[
  {
    "column_name": "id",
    "data_type": "integer",
    "is_nullable": "NO",
    "column_default": "nextval('customers_id_seq'::regclass)"
  },
  {
    "column_name": "name",
    "data_type": "text",
    "is_nullable": "YES"
  },
  {
    "column_name": "signed_up",
    "data_type": "timestamp with time zone",
    "is_nullable": "YES",
    "column_default": "now()"
  }
]
```

Column-level fields are taken from the backend's catalog. Coverage
varies by backend — `column_default` is reliably populated on
Postgres / MySQL / MSSQL, less so on SQLite (older versions don't
expose it via `pragma_table_info`).

## Scripted use with `jq`

Because the default format is JSON, downstream shell scripts compose
naturally:

```bash
# All NOT NULL columns in `customers`
ferrule describe demo customers \
  | jq '.[] | select(.is_nullable == "NO") | .column_name'

# Tables in the `audit` schema (Postgres-style)
ferrule tables demo \
  | jq -r '.[].table_name | select(startswith("audit_"))'

# Quick "does this table have an index_id column?" check
ferrule describe demo events | jq -e '.[] | select(.column_name == "index_id")' \
  > /dev/null && echo "yes" || echo "no"
```

If you need Postgres-style schema-qualified names, use the SQL form
directly — the `tables` command lists the current schema's tables,
not all schemas.

## What you won't get

By design, `tables` and `describe` expose only a backend-agnostic
subset. The following do not unify well across backends, so ferrule
doesn't try:

- **Indexes, constraints, foreign keys.** Use the native shell or
  a direct SQL query.
- **Enum variants** (Postgres `CREATE TYPE … AS ENUM`, MySQL inline
  `ENUM`).
- **Composite types and ranges** (Postgres-specific).
- **Generated / computed column expressions** beyond the bare
  `column_default` field.
- **Triggers, stored procedures, sequences.** Query the relevant
  catalog (`pg_catalog.pg_proc`, `INFORMATION_SCHEMA.ROUTINES`,
  etc.) with `ferrule query` for now.

If a future workflow needs structured access to one of these, file
an issue — the `Value` enum has the headroom (e.g., `Json` for
constraint detail), the question is which subset stays cross-
backend.

## REPL equivalents

Inside `ferrule repl`, the meta-commands `\dt` and `\d <table>` do
the same thing as the CLI variants:

```text
> \dt
> \d customers
```

See [Interactive REPL](repl.md).

## Reference

Both commands accept the standard output flags (`--format`,
`--output`, `--limit`, `--offset`, `--timing`, `--verbose`) plus
the connection flags (`--insecure`, `--daemon`). See
[CLI Reference](reference.md) for the full list.
