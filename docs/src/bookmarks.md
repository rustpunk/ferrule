# Bookmarks

Bookmarks save frequently-run queries by name so you don't retype them
or maintain a separate scratch directory of `.sql` files. Think shell
aliases, but cross-shell, with positional parameters and a connection
hint baked in.

## Saving bookmarks

```bash
# Simple query
ferrule bookmark add active-users \
  "SELECT * FROM users WHERE active = true;" \
  --connection dev

# With a positional parameter placeholder
ferrule bookmark add user-by-id \
  "SELECT * FROM users WHERE id = ${1};" \
  --connection dev

# Long query — wrap in single quotes (or a heredoc into stdin)
ferrule bookmark add recent-sales '
SELECT product, SUM(amount) AS total
FROM orders
WHERE created_at > now() - interval '"'"'7 days'"'"'
GROUP BY product
ORDER BY total DESC;
' --connection production
```

The `--connection` hint is optional but recommended. Without it,
`bookmark run` requires a `--connection` flag at every call.

## Naming convention

Plain names work fine:

```bash
ferrule bookmark add count-all "SELECT COUNT(*) FROM ${1};"
```

Dotted names are treated as **connection hints** — the first segment
suggests the connection to use, so a single `bookmark run` works
without `--connection`:

```bash
# The "pg." prefix tells `run` to look for a connection named `pg`
ferrule bookmark add pg.select_users "SELECT id, name, email FROM users;"
```

When the prefix doesn't match a saved connection, ferrule falls back
to the explicit `--connection` flag or the `[connection.default]`
profile.

## Listing bookmarks

```bash
ferrule bookmark list
```

```text
Name             | SQL
--------------------------------------------------------------
active-users     | SELECT * FROM users WHERE active = true;
user-by-id       | SELECT * FROM users WHERE id = ${1};
recent-sales     | SELECT product, SUM(amount) as total FR...
pg.select_users  | SELECT id, name, email FROM users;
```

## Running bookmarks

```bash
# Run a simple bookmark
ferrule bookmark run active-users

# Run with positional parameters
ferrule bookmark run user-by-id 42

# Override the format
ferrule bookmark run recent-sales --format table

# Override the connection
ferrule bookmark run pg.select_users --connection staging

# Combine with paging
ferrule bookmark run active-users --limit 10 --offset 20

# Edit before running — opens your $EDITOR
ferrule bookmark run active-users --edit
```

`${1}`, `${2}`, … are replaced with the positional arguments you
pass after the name. Missing parameters leave the placeholder
intact (which the database will then reject).

> **Positional only.** Bookmarks support `${1}` / `${2}` / etc., not
> the named `${name}` placeholders that `ferrule query --param` uses.
> Named params are CLI-only because they require explicit `name=value`
> mapping at call time.

> **`--edit`** opens the bookmarked SQL in your `$EDITOR`.  You can modify
> the statement before it runs.  This is useful for ad-hoc tweaks without
> creating a new bookmark.

## Common patterns

### Per-environment health checks

```bash
ferrule bookmark add prod.health    "SELECT 1;" --connection prod
ferrule bookmark add staging.health "SELECT 1;" --connection staging
ferrule bookmark add dev.health     "SELECT 1;" --connection dev

for env in prod staging dev; do
  ferrule bookmark run "$env.health" --format raw && echo "$env: ok" || echo "$env: down"
done
```

### Daily KPI dashboard line

```bash
ferrule bookmark add daily-signups \
  "SELECT COUNT(*) FROM users WHERE created_at::date = current_date;" \
  --connection prod
```

Drop it in a cron / launchd / Task Scheduler job piping to your
notification channel of choice.

### Parameterized lookup-by-id

```bash
ferrule bookmark add user-by-email \
  "SELECT * FROM users WHERE email = ${1};" \
  --connection prod

ferrule bookmark run user-by-email "'alice@example.com'"
```

Note the quotes: bookmark substitution is text-level. Wrapping the
argument in single quotes makes it a string literal in SQL.

### Export of the day

```bash
ferrule bookmark run user-growth --format csv > "growth-$(date +%F).csv"
```

## Deleting

```bash
ferrule bookmark delete user-by-id
```

## Where bookmarks live

```text
~/.config/ferrule/bookmarks.toml
```

The file format is plain TOML and safe to commit if you want to
version-control your team's saved queries:

```toml
[active-users]
sql = "SELECT * FROM users WHERE active = true;"
connection = "dev"

[user-by-id]
sql = "SELECT * FROM users WHERE id = ${1};"
connection = "dev"

["pg.select_users"]
sql = "SELECT id, name, email FROM users;"
```

(Quoted keys are required when the bookmark name contains a `.`.)

## Bookmarks in the REPL

The same bookmark file is shared with the REPL meta-commands:

```text
> SELECT * FROM users WHERE active = true;
> \bookmark save active-users
Bookmark 'active-users' saved.

> \bookmark list
- active-users
- daily-metrics

> \bookmark run active-users
> \bookmark delete active-users
```

See [Interactive REPL](repl.md) for the full meta-command surface.
