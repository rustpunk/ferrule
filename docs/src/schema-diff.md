# Schema Diff

`ferrule diff <conn-a> <conn-b>` compares the table-and-column
inventory of two databases and prints the differences. Like `git
diff` for schemas — useful when migrations have been applied
out-of-order, when staging and prod have drifted, or when porting a
schema between backends.

## What it compares

For each table that exists on both sides:

- Columns present only in A (`-` lines).
- Columns present only in B (`+` lines).
- Columns whose `data_type` differs across the two
  (`~` lines).

For tables that exist on only one side:

- Listed under `Tables only in A` or `Tables only in B`.

The comparison is keyed on table and column *names*. Type comparison
uses the `data_type` string from each backend's
`information_schema.columns` (or its equivalent), so be aware: a
`VARCHAR(255)` on one side and `TEXT` on the other will show as a
type change, even though they often behave identically.

## Cross-backend comparison

The intentional design call: ferrule does *not* normalize data types
across backends. Diffing Postgres → MySQL will produce a lot of `~`
lines because their type vocabularies are different. Diffing Postgres
→ Postgres or MySQL → MySQL is where this command shines.

If you need cross-backend type compatibility, post-process the JSON
output (see `--format json` below) and apply your own type-mapping
rules.

## Basic usage

```bash
# Two named connections from .ferrule.toml
ferrule diff staging prod

# Two raw URLs
ferrule diff \
  "postgres://app:pwd@staging-db/myapp" \
  "postgres://app:pwd@prod-db/myapp"

# Mix: name on the left, URL on the right
ferrule diff staging "postgres://app:pwd@prod-db/myapp"
```

## Diffing a single table

`--table <name>` restricts the diff to one table on each side. Useful
when the inventory is large and you only care about one subject.

```bash
ferrule diff staging prod --table users
```

If the table is missing on either side, the command exits with a
clear message rather than an empty diff.

## Output formats

The default is human-readable text, mirroring `diff` output:

```text
Tables only in A:
  - audit_log

Tables only in B:
  + feature_flags

Table users:
  - middle_name VARCHAR(255)
  + nickname TEXT
  ~ created_at: timestamp -> timestamp with time zone
```

JSON output (`--format json`) is structured — a top-level object with
`tables_only_in_a`, `tables_only_in_b`, and `table_diffs` arrays.
Pipe through `jq` for arbitrary post-processing:

```bash
ferrule diff staging prod --format json \
  | jq '.table_diffs[] | select(.table == "users")'
```

CSV output (`--format csv`) flattens the diff into rows: one per
column-level change, with columns `table`, `change_type`, `column`,
`a_type`, `b_type`. Easy to import into a spreadsheet for review.

## Per-side passwords

The `-p` / `--password` flag from `query` doesn't apply here — diff
takes two connections, so each side gets its own flag:

- `--password-a <pwd>` — overrides the credential stack for
  connection A.
- `--password-b <pwd>` — same for B.

If a password isn't passed on the CLI and isn't in the credential
stack, ferrule falls through to the standard interactive prompt
twice (once per side). For non-interactive contexts, use
`FERRULE_<NAME>_PASSWORD` env vars or the OS keyring.

## Exit codes

`ferrule diff` follows GNU `diff` convention:

- `0` — schemas are identical (no differences in tables or columns).
- `1` — differences were found. Suitable for CI gates: a non-zero
  exit makes the job fail when staging and prod drift.
- `2` — connection or query failure (couldn't read either schema).
- `3` — internal SQL error during introspection.

Note that exit `1` here overlaps with the generic CLI usage error.
This is deliberate — it matches what GNU diff/grep do — but it means
a script can't distinguish "schemas differ" from "you typed the
command wrong" by exit code alone. Read stderr for the diagnostic.

## Wrapping into CI

A common pattern: run `ferrule diff` in CI against the production
database and the migration target, fail the job on any drift.

```bash
#!/bin/sh
set -e

if ! ferrule diff prod migration-target --format json > diff.json; then
  echo "Schemas drifted:"
  jq . diff.json
  exit 1
fi
echo "Schemas match — safe to deploy."
```

The JSON output is stable across runs, so the job's diagnostic stays
attached to the same artifact.

## Limits

- **No constraint, index, or trigger comparison.** Only column
  inventory and types. PRIMARY KEY / FOREIGN KEY differences are
  invisible here. A future Wave is planned to extend this.
- **No row-data comparison.** This is a structure diff only. For
  row-level diffs, dump both tables and use `diff` / `git diff` /
  custom tooling.
- **Type strings are compared verbatim.** `varchar(255)` and
  `character varying(255)` will show as different, even though they
  are the same type — the underlying `data_type` column reports them
  differently across backends.
