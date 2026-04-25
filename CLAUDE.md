# Ferrule — Rust-native Database Query CLI

> The collar that joins you to your data.

## Project Identity

`ferrule` is a CLI tool for querying relational databases from the terminal. One static binary speaks Postgres, MySQL, MSSQL, and SQLite. Oracle is opt-in. No runtime client libraries required for the default backends.

## Architecture

Three-crate workspace:

- **`ferrule-core`** — Backend drivers (feature-gated), unified `Value`/`Row` types, `DatabaseUrl` parser, result formatters.
- **`ferrule-config`** — Connection registry, credential resolution stack, profiles.
- **`ferrule-cli`** — Binary crate. clap derive-based command tree, dispatch, output routing, exit codes.

## Tech Stack

- **Postgres**: `tokio-postgres` + `rustls` (pure Rust, zero runtime deps)
- **MySQL**: `mysql_async` (pure Rust)
- **MSSQL**: `tiberius` (pure Rust TDS)
- **SQLite**: `rusqlite` with `bundled` (statically linked SQLite)
- **Oracle**: `oracle` crate (ODPI-C, opt-in only; user must install Instant Client)
- **CLI**: `clap` derive API
- **Runtime**: `tokio` `current_thread`
- **Errors**: `thiserror` in libraries, `miette` in CLI
- **Secrets**: `secrecy::SecretString` everywhere. Never raw strings.
- **Formatting**: `tabled` + `serde_json` + `crossterm`

## Critical Conventions

- Zero runtime deps for default features. Oracle alone requires external Instant Client.
- All passwords wrapped in `SecretString` — zeroize on drop, redact in Debug.
- Connection URLs: password component redacted in every log/diagnostic.
- Exit codes: 0=success, 1=usage, 2=connection, 3=query error, 4=no rows (optional).
- `--format table` is default when TTY; `--format json` when piped.
- `--insecure` flag required to disable TLS verification. Warns on stderr.
- `#[tokio::main(flavor = "current_thread")]` in `main.rs`.
- `IndexMap` over `HashMap` where column/row order matters.
- No `unwrap()` in library crates. Return `Result`.
- `#[must_use]` on `Result`-returning functions.

## Build / Test / Lint

```bash
cargo build --workspace
cargo test --workspace
cargo clippy --workspace
cargo doc --workspace --no-deps
```

## How to Test

### SQLite — no setup required

SQLite is the default backend and requires no external services. The integration
tests create `:memory:` or temporary file databases automatically. All `ferrule`
commands work out of the box against `sqlite:///path/to/db`.

### Postgres — start container first

The Postgres backend requires a running Postgres server for runtime validation.
Use the following Docker one-liner:

```bash
docker run -d --name ferrule-pg-test \
  -e POSTGRES_PASSWORD=ferrule \
  -e POSTGRES_USER=ferrule \
  -e POSTGRES_DB=ferrule \
  -p 127.0.0.1:15432:5432 \
  postgres:17-alpine
```

Wait for the container to be ready, then seed it:

```bash
PGPASSWORD=ferrule psql -h 127.0.0.1 -p 15432 -U ferrule -d ferrule -c "
CREATE TABLE test_users (
  id SERIAL PRIMARY KEY,
  name TEXT,
  age INT,
  score NUMERIC(10,2),
  created_at TIMESTAMP WITH TIME ZONE DEFAULT NOW(),
  active BOOLEAN,
  meta JSONB,
  uid UUID DEFAULT gen_random_uuid()
);
INSERT INTO test_users (name, age, score, active, meta)
  VALUES ('Alice', 30, 99.5, true, '{\"role\": \"admin\"}'),
         ('Bob', 25, 88.25, false, '{\"role\": \"user\"}');
"
```

Verify the three backend paths (extended protocol, single DML, multi-statement):

```bash
# 1. Single SELECT — typed values via extended protocol
ferrule query "postgres://ferrule:ferrule@127.0.0.1:15432/ferrule?sslmode=disable" \
  "SELECT * FROM test_users;" --format json

# 2. Single DML — execute path, "N rows affected" on stderr
ferrule query "postgres://ferrule:ferrule@127.0.0.1:15432/ferrule?sslmode=disable" \
  "INSERT INTO test_users (name, age) VALUES ('Charlie', 35);"

# 3. Multi-statement — mixed DML and SELECT
ferrule query "postgres://ferrule:ferrule@127.0.0.1:15432/ferrule?sslmode=disable" \
  "INSERT INTO test_users (name, age) VALUES ('Dave', 40); SELECT COUNT(*) FROM test_users;" \
  --format table

# 4. Auxiliary commands
ferrule tables   "postgres://ferrule:ferrule@127.0.0.1:15432/ferrule?sslmode=disable" --format table
ferrule describe "postgres://ferrule:ferrule@127.0.0.1:15432/ferrule?sslmode=disable" test_users
ferrule conn test "postgres://ferrule:ferrule@127.0.0.1:15432/ferrule?sslmode=disable"
```

Clean up when done:

```bash
docker stop ferrule-pg-test && docker rm ferrule-pg-test
```

### MySQL, MSSQL, Oracle

These backends are currently stubs (`todo!()`) and have no runtime tests yet.
They compile-gate cleanly via Cargo features (`mysql`, `mssql`, `oracle`).

## Wave Structure

- **Wave 1** — Core query flow: `conn`, `query`, `tables`, `describe`, multi-statement, JSON/CSV/table output.
- **Wave 1.5** — Connection pooling daemon, paging, config profiles, env interpolation, `.ferrule.toml`.
- **Wave 2** — Interactive REPL, query bookmarks, parameterized queries, explain, dump/load, watch mode.
- **Wave 3** — Schema diff, migration runner, SSH tunnels, k8s port-forward, daemon mode.

## Oracle Runtime Behavior

- ODPI-C lazily loads `libclntsh` on first Oracle connection attempt.
- If Instant Client is missing: emit `miette` diagnostic with download link and `LD_LIBRARY_PATH` help.
- Non-Oracle connections continue to work normally.

## Credential Resolution Stack

1. `--password` CLI flag
2. `FERRULE_<NAME>_PASSWORD` env var
3. OS keyring (`service=ferrule`, `user=<name>`)
4. Interactive prompt (TTY only)
5. Fail with diagnostic

## Result Type Unification

Every backend maps native types to `ferrule_core::value::Value` enum:

```rust
pub enum Value {
    Null, Bool, Int64, Float64, Decimal, String, Bytes,
    Date, Time, DateTime, DateTimeTz, Json, Uuid, Array,
}
```

The formatter layer never sees driver-specific types.

## Stubbing Policy

This scaffold contains many `todo!()` bodies. Each crate root has `#![allow(dead_code, unused_variables, unused_imports)]` so that `cargo clippy` passes. Do not remove these pragmas until Wave 1 is complete.
