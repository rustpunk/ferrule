# Ferrule

> The collar that joins you to your data.

Ferrule is a Rust-native command-line tool for querying relational
databases. One static binary speaks **PostgreSQL**, **MySQL**, **MSSQL**,
and **SQLite**. Oracle is optional via a feature flag.

## Why ferrule?

Most database CLIs are tied to one engine — `psql` for Postgres,
`mysql` for MySQL, `sqlcmd` for MSSQL, `sqlite3` for SQLite. Each is
excellent at its own backend, but moving between them means learning
five output conventions, five auth quirks, and five sets of
introspection commands.

Ferrule trades some backend-specific depth for **a single consistent
interface across all of them**:

- **One static binary, zero runtime dependencies** for the four default
  backends. No `libpq`, no `libmysqlclient`, no `unixodbc`. The binary
  drops onto a fresh box and works.
- **A unified type system.** Every backend maps native types into a
  single `Value` enum, so a JSON dump from Postgres and a JSON dump
  from MySQL look identical to your downstream tools. See
  [Concepts](concepts.md#the-unified-value-type).
- **Pipe-friendly defaults.** JSON output by default, with `--format
  table` for human-readable terminal use, plus CSV / YAML / raw for
  pipelines.
- **Same command surface across backends.** `ferrule query`,
  `ferrule tables`, `ferrule describe`, `ferrule explain` all work the
  same way against any supported backend.
- **Real credential hygiene.** Passwords resolved through OS keyring,
  Docker secrets, env vars, or interactive prompt — never required in
  URLs, never logged. See [Security](security.md).

## When to reach for something else

Ferrule isn't trying to replace the native shells when you need their
specialty features. Reach for:

- **`psql`** — when you need backslash commands like `\dt+`,
  `\copy`, or psql variables; or when you want the deepest Postgres
  introspection.
- **`mysql` / `mysqlsh`** — for MySQL-specific tooling like
  `mysqldump`, X Protocol, or replication management.
- **`sqlcmd` / `sqlpackage`** — for MSSQL deployment artifacts and
  T-SQL debugging.
- **A GUI (DBeaver, TablePlus, DataGrip)** — when you want a visual
  ER diagram, an inline result-set editor, or stored-procedure
  authoring with syntax help.
- **`usql`** — if you specifically need the Go ecosystem and a built-
  in ORM-like layer.

Ferrule is for **terminal-first, scriptable, multi-backend work**:
ad-hoc queries during oncall, sanity checks across staging and
production replicas, fixture export/import, daily KPI bookmarks,
small data pipelines.

## One-minute demo

```bash
# SQLite works out of the box — no setup
ferrule query "sqlite::memory:" "SELECT 1 + 1 AS answer;" --format table

# Postgres (requires running server)
ferrule query "postgres://user:pass@localhost/db" "SELECT * FROM users;"

# Save a connection, then reuse the name
ferrule conn add prod "postgres://app@db.example.com/myapp"
ferrule conn set-password prod
ferrule query prod "SELECT COUNT(*) FROM orders;"

# Interactive REPL
ferrule repl prod
> SELECT count(*) FROM orders WHERE created_at > now() - interval '1 day';
> \format table
> \q
```

## Where to next

- **New here?** Start with [Installation](installation.md) and the
  [Quick Start](quickstart.md).
- **Want the mental model?** Read [How Ferrule Thinks](concepts.md) —
  it's the conceptual map for everything else.
- **Looking up a flag?** Jump to the [CLI Reference](reference.md).
- **Something broken?** Try [Troubleshooting](troubleshooting.md).
