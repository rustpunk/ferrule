# How Ferrule Thinks

Ferrule is built around a few small abstractions. Once you have them, the
rest of the docs make sense at a glance. This chapter is the conceptual
"map" of the tool — read it once and refer back as needed.

## The unified `Value` type

Every supported backend speaks a different SQL dialect with different
native types: PostgreSQL has `JSONB` and `UUID`, MySQL has its own `JSON`,
MSSQL has `UNIQUEIDENTIFIER` and `DATETIMEOFFSET`, SQLite has dynamic
typing, Oracle has `NUMBER(1)` standing in for booleans. If you wrote a
query against each one, you'd get five different shapes back.

Ferrule maps every backend's native types into a single `Value` enum:

```text
Null | Bool | Int64 | Float64 | Decimal | String | Bytes
| Date | Time | DateTime | DateTimeTz | Json | Uuid | Array
```

Output formatters (`table`, `json`, `csv`, `yaml`, `raw`) only ever see
`Value` — they never touch driver-specific types. The practical
consequence: a JSON dump from PostgreSQL and a JSON dump from MySQL look
the same. A CSV from MSSQL pipes into the same `awk` script as a CSV from
SQLite. You can switch backends without rewriting downstream tooling.

The exact backend → `Value` mapping is documented in
[Backends](backends.md). When a native type doesn't have a clean `Value`
equivalent (Postgres `int4range`, custom enums, composite types),
ferrule falls back to `String` with the value rendered as the driver
returns it.

## Connection resolution

Most ferrule commands take a `<CONNECTION>` argument. That argument is
resolved in three steps:

1. **Raw URL.** If `<CONNECTION>` parses as a database URL
   (`postgres://...`, `mysql://...`, `sqlite::memory:`, etc.), ferrule
   uses it directly.
2. **Profile from `.ferrule.toml`.** If not a URL, ferrule looks up the
   name in the loaded `[connection.<name>]` profile of the discovered
   config file. Profiles can declare a `password_url` and other defaults.
3. **Registry from `connections.toml`.** If no matching profile exists,
   ferrule falls back to the per-user registry at
   `~/.config/ferrule/connections.toml` (the file managed by
   `ferrule conn add` / `list` / `remove`).

If none of those match, ferrule exits with a usage error and lists the
available profiles so you can spot a typo.

> **Note:** profiles take precedence over the registry. If you have
> both a registry entry named `prod` and a `[connection.prod]` block in
> `.ferrule.toml`, the profile wins. This lets a project-local
> `.ferrule.toml` shadow a global registry entry without modifying it.

## The credential stack

Once ferrule has a URL, it needs a password. URLs that include a password
inline (`postgres://user:secret@host/db`) skip this entirely — but
inline passwords leak through process listings and shell history, so
keep them out of saved profiles. See [Security](security.md) for the
full rationale.

When the URL has no password, ferrule walks the **hasp** credential
stack in order. The first source that returns a value wins:

1. **`--password` CLI flag.** Explicit, ephemeral, and intentionally
   loud. Useful for one-off debugging; bad for daily use.
2. **`password_url` from the active profile.** Whatever the
   `password_url` in `.ferrule.toml` resolves to, via the
   [hasp](https://crates.io/crates/hasp) URL scheme — `env://`,
   `keyring://`, or `file://`.
3. **`FERRULE_<NAME>_PASSWORD` env var.** Legacy fallback.
   `production` → `FERRULE_PRODUCTION_PASSWORD`. Hyphens become
   underscores: `local-db` → `FERRULE_LOCAL_DB_PASSWORD`.
4. **OS keyring at `keyring://ferrule/<name>`.** Whatever
   `ferrule conn set-password <name>` stored. macOS Keychain, Windows
   Credential Manager, or libsecret on Linux.
5. **Interactive prompt.** Only fires if stdin is a TTY.
6. **Fail.** Exit code 2 with a diagnostic listing every step it tried.

This is the canonical home for the credential stack. The
[Connections](connections.md) and [Security](security.md) chapters
expand on the *why* of each step.

## Output format selection

Ferrule supports five output formats: `table`, `json`, `csv`, `yaml`,
`raw`. The format is chosen in this order:

1. `--format <fmt>` on the command line.
2. `default.format` in the loaded `.ferrule.toml`.
3. The built-in default, which is **`json`**.

The built-in default is JSON whether or not stdout is a TTY. If you
want pretty tables in your terminal, either pass `--format table` per
command or set `format = "table"` under `[default]` in your config.

When writing to a file with `--output`, the format applies to the file
contents.

## Multi-statement semantics

Some backends (PostgreSQL, MSSQL) accept multiple `;`-separated
statements in a single round-trip. Ferrule supports this for those
backends:

```bash
ferrule query prod "
  INSERT INTO logs (msg) VALUES ('startup');
  SELECT COUNT(*) FROM logs;
"
```

A few rules:

- **`--limit` and `--offset` are not allowed** with multi-statement
  SQL. The default limit (`1000` from `[default]`) counts. Set
  `--limit 0` to disable paging for batches.
- **Each result is reported separately.** Result sets print to stdout;
  DML row counts ("`N rows affected`") print to stderr.
- **Backends that don't support batches** (MySQL, SQLite, Oracle in
  the current driver layer) reject the SQL — split it into separate
  `ferrule query` calls or wrap it in a stored procedure.

## Where to next

- New users: [Quick Start](quickstart.md) for a guided first query.
- Curating connections: [Connections](connections.md).
- Picking the right secret store: [Security](security.md).
- Looking up a flag: [CLI Reference](reference.md).
- Things going wrong: [Troubleshooting](troubleshooting.md).
