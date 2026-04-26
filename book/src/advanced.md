# Advanced Features

## EXPLAIN

Get execution plans from any backend:

```bash
# Postgres: returns JSON plan
ferrule explain production "SELECT * FROM large_table WHERE status = 'pending'"

# MSSQL: returns graphical plan
ferrule explain mssql_db "SELECT * FROM orders WHERE customer_id = @p1"

# In the REPL, \explain toggles plan mode for every subsequent query
> \explain
Explain mode: on
> SELECT * FROM users;
# shows plan instead of results
```

## Dump and Load

### Dump

Export a table to CSV, JSON, or SQL INSERT statements:

```bash
ferrule dump production users --dump-format csv > users.csv
ferrule dump production users --dump-format json > users.json
ferrule dump production users --dump-format sql > users.sql
```

Dump is batched using server-side paging and works on large tables.

### Load

Import CSV or JSON into a table:

```bash
ferrule load production users.json --table users --create-table
ferrule load production data.csv --table events
```

Load infers CSV columns from the first row and JSON schema from object keys.
With `--create-table`, Ferrule generates a `CREATE TABLE` statement using inferred types.

## Watch Mode

Monitor a query repeatedly with clean screen handling and diff support:

```bash
# Basic watch (re-runs every 5 seconds)
ferrule watch production "SELECT COUNT(*) FROM events" --interval 5

# Only print when result changes
ferrule watch production "SELECT COUNT(*) FROM events" --interval 2 --diff

# Limit iterations
ferrule watch production "SELECT NOW()" --interval 1 --max-iterations 10
```

`Ctrl-C` stops the watch cleanly.
