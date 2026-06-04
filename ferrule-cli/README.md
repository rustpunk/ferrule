# ferrule

> The collar that joins you to your data.

`ferrule` is a single, fast, statically linked CLI for querying relational
databases. Register named connections, run SQL from the command line or
interactively, and get beautifully formatted results — with zero runtime
dependencies for Postgres, MySQL, MSSQL, and SQLite. Oracle support is an
opt-in feature for users who can install the Oracle Instant Client.

## Install

```bash
# Postgres + MySQL + MSSQL + SQLite (default)
cargo install ferrule

# Add Oracle and/or SSH-tunnel support
cargo install ferrule --features all
```

## Quick start

```bash
ferrule conn add prod "postgres://user@host/db"   # save a named connection
ferrule query prod "SELECT id, name FROM users LIMIT 10"
ferrule tables prod                               # list tables
ferrule describe prod users                       # inspect a table
```

`--format` renders `table` (default on a TTY), `json` (default when piped),
`jsonl`, `csv`, `yaml`, `markdown`, or `html`. See the
[repository](https://github.com/rustpunk/ferrule) for the full command tree
(REPL, dump/load, cross-DB copy, schema diff, query telemetry, SSH tunnels,
result cache, and more).

## Built on the ferrule libraries

`ferrule` is the reference binary implementation over a stack of reusable,
independently published libraries — depend on them directly to power your own
CLI or UI:

- [`ferrule-sql`](https://crates.io/crates/ferrule-sql) — embeddable,
  synchronous, bounded-memory SQL driver core (neutral `Value`/`Row` types,
  URL parser, per-backend drivers, streaming cursors, cross-backend copy).
- [`ferrule-core`](https://crates.io/crates/ferrule-core) — output formatters
  and the credential-resolution glue.
- [`ferrule-config`](https://crates.io/crates/ferrule-config) — connection
  registry, profiles, and the layered credential stack.

## License

Licensed under either of MIT or Apache-2.0 (`SPDX: MIT OR Apache-2.0`) at your
option.
