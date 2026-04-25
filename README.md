# Ferrule

> The collar that joins you to your data.

`ferrule` is a single, fast, statically linked CLI for querying relational databases. Register named connections, run SQL from the command line or interactively, and get beautifully formatted results — with zero runtime dependencies for Postgres, MySQL, MSSQL, and SQLite. Oracle support is available as an opt-in feature for users who can install the Oracle Instant Client.

## Status

🚧 Alpha / Work in Progress

## Features

- **One binary, many databases** — Postgres, MySQL, MSSQL, SQLite out of the box. Oracle via opt-in feature.
- **Pure Rust** — Native async drivers for Postgres (`tokio-postgres`), MySQL (`mysql_async`), MSSQL (`tiberius`), SQLite (`rusqlite`). No libpq, no libmysqlclient, no Microsoft ODBC drivers.
- **Beautiful output** — Auto-detect TTY and render formatted tables, or stream JSON/CSV/YAML for piping.
- **Connection registry** — Save named connections, resolve passwords from env vars, OS keyring, or interactive prompt.
- **Zero runtime deps (default)** — `cargo install ferrule` produces a ~15–25 MB static binary.

## Quick Start

```bash
# Install (Postgres + MySQL + MSSQL + SQLite)
cargo install ferrule

# Add a connection
ferrule conn add prod "postgres://user@host/db"

# Run a query
ferrule query prod "SELECT id, name FROM users LIMIT 10"

# List tables
ferrule tables prod

# Describe a table
ferrule describe prod users
```

## Oracle Support

```bash
# Build with Oracle support
cargo install ferrule --features oracle

# Ensure Oracle Instant Client is on your library path
export LD_LIBRARY_PATH=/opt/oracle/instantclient:$LD_LIBRARY_PATH

ferrule conn add legacy "oracle://user@host/SERVICE"
ferrule query legacy "SELECT * FROM employees"
```

## Etymology

A *ferrule* is the metal band or collar that strengthens the joint between a handle and a blade — or, in computing, the metal sleeve around a fiber-optic cable. This tool is the collar that joins you to your data.

## License

MIT OR Apache-2.0
