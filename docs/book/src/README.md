# Ferrule

> The collar that joins you to your data.

Ferrule is a Rust-native command-line tool for querying relational databases. One static binary speaks **PostgreSQL**, **MySQL**, **MSSQL**, and **SQLite**. Oracle is optional via feature flag.

## Features

- **Zero runtime dependencies** for default backends (no `libpq`, `libmysqlclient`, or `unixodbc` required).
- **Unified type system** — every backend maps native types to a single `Value` enum.
- **Multiple output formats** — table, JSON, CSV, YAML, raw.
- **Interactive REPL** with history, bookmarks, query parameters, and watch mode.
- **Schema introspection** — `tables` and `describe` against any backend.
- **Server-side paging** with `--limit` and `--offset`.
- **Connection registry** — save named connections in `~/.config/ferrule/`.
- **EXPLAIN** execution plans across all backends.
- **Dump / Load** data to/from CSV and JSON.
- **Watch mode** for monitoring queries in real time.

## One-minute Demo

```bash
# SQLite works out of the box
ferrule query "sqlite::memory:" "SELECT 1 + 1 AS answer;"

# Postgres (requires running server)
ferrule query "postgres://user:pass@localhost/db" "SELECT * FROM users;" --format json

# Interactive REPL
ferrule repl "sqlite::memory:"
> SELECT random() % 100 AS lucky_number;
> \format json
> SELECT * FROM sqlite_master;
> \q
```
