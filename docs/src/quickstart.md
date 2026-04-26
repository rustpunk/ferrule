# Quick Start

This walkthrough takes you from a fresh ferrule install to a real
database in under five minutes. SQLite for the warm-up, then a
disposable Postgres in Docker for everything else.

## Step 1 — your first query

SQLite needs no setup. Run:

```bash
ferrule query "sqlite::memory:" "SELECT 1 + 1 AS answer;" --format table
```

Output:

```text
┌────────┐
│ answer │
├────────┤
│ 2      │
└────────┘
```

Drop the `--format table` flag and you'll get JSON instead — that's
the default. Either form is fine for now.

```bash
ferrule query "sqlite::memory:" "SELECT 1 + 1 AS answer;"
# [
#   {
#     "answer": 2
#   }
# ]
```

## Step 2 — a real database

Spin up a throwaway Postgres in Docker. This mirrors the test setup
in `CLAUDE.md`; copy and paste:

```bash
docker run -d --name ferrule-quickstart \
  -e POSTGRES_PASSWORD=ferrule \
  -e POSTGRES_USER=ferrule \
  -e POSTGRES_DB=ferrule \
  -p 127.0.0.1:15432:5432 \
  postgres:17-alpine

# Wait for it to come up (~3 seconds)
until docker exec ferrule-quickstart pg_isready -U ferrule >/dev/null 2>&1; do
  sleep 1
done

# Seed a tiny schema
PGPASSWORD=ferrule psql -h 127.0.0.1 -p 15432 -U ferrule -d ferrule -c "
CREATE TABLE customers (
  id SERIAL PRIMARY KEY,
  name TEXT,
  signed_up TIMESTAMPTZ DEFAULT now()
);
INSERT INTO customers (name) VALUES ('Alice'), ('Bob'), ('Carol');
"
```

Now query it directly:

```bash
ferrule query "postgres://ferrule:ferrule@127.0.0.1:15432/ferrule?sslmode=disable" \
  "SELECT * FROM customers;" --format table
```

Output something like:

```text
┌────┬───────┬───────────────────────────────┐
│ id │ name  │ signed_up                     │
├────┼───────┼───────────────────────────────┤
│ 1  │ Alice │ 2026-04-26 18:01:23.456+00:00 │
│ 2  │ Bob   │ 2026-04-26 18:01:23.456+00:00 │
│ 3  │ Carol │ 2026-04-26 18:01:23.456+00:00 │
└────┴───────┴───────────────────────────────┘
```

When you're done with the container:

```bash
docker stop ferrule-quickstart && docker rm ferrule-quickstart
```

## Step 3 — save a named connection

Typing the full URL every time gets old. Add it to the registry:

```bash
ferrule conn add demo "postgres://ferrule@127.0.0.1:15432/ferrule?sslmode=disable"
ferrule conn set-password demo
# Password: ferrule
```

Now use the name:

```bash
ferrule query demo "SELECT COUNT(*) FROM customers;"
ferrule tables demo
ferrule describe demo customers
ferrule repl demo
```

The password is stored in your OS keyring under
`keyring://ferrule/demo`, never on disk in plaintext.

## Step 4 — pipe-friendly defaults

The default output format is JSON. That's chosen so output piped to
`jq`, `awk`, or another command "just works":

```bash
ferrule query demo "SELECT * FROM customers" | jq '.[].name'
# "Alice"
# "Bob"
# "Carol"
```

If you'd rather see tables in your terminal, either pass `--format
table` per command, or set a project default in `.ferrule.toml`
(see [Configuration](configuration.md)).

## Step 5 — save a bookmark

For queries you run all the time, bookmarks beat shell aliases:

```bash
ferrule bookmark add daily-count \
  "SELECT COUNT(*) FROM customers WHERE signed_up > now() - interval '1 day';" \
  --connection demo

ferrule bookmark run daily-count
```

Positional parameters work too:

```bash
ferrule bookmark add by-name "SELECT * FROM customers WHERE name = ${1};" \
  --connection demo

ferrule bookmark run by-name "'Alice'"
```

## How passwords get resolved

Step 3 stored a password in the keyring. The next time you run
`ferrule query demo "..."` without a password on the URL, ferrule
walks this stack and stops at the first hit:

```text
1. --password CLI flag        (you didn't pass one)
2. password_url in profile    (no .ferrule.toml profile yet)
3. FERRULE_DEMO_PASSWORD env  (unset)
4. keyring://ferrule/demo     ← FOUND (set by `conn set-password`)
5. Interactive prompt
6. Fail
```

This is described in detail in [Concepts](concepts.md#the-credential-stack)
and [Security](security.md). For now: storing in the keyring is the
right default for a workstation.

## Where to next

- [Querying Data](querying.md) — output formats, paging,
  parameterized queries.
- [Connections](connections.md) — profiles, environment
  interpolation, registry vs `.ferrule.toml`.
- [Interactive REPL](repl.md) — meta-commands, watch mode.
- [Concepts](concepts.md) — the abstractions everything is built on.
