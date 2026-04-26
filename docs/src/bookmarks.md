# Bookmarks

Bookmarks let you save frequently-run queries so you don't have to retype them (or maintain a separate SQL script directory).

## Saving Bookmarks

```bash
# Save a simple query
ferrule bookmark add active-users "SELECT * FROM users WHERE active = true;" --connection dev

# Save with positional parameter placeholders
ferrule bookmark add user-by-id "SELECT * FROM users WHERE id = ${1};" --connection dev

# Long query — wrap in quotes
ferrule bookmark add recent-sales '
SELECT product, SUM(amount) as total
FROM orders
WHERE created_at > now() - interval '"'"'7 days'"'"'
GROUP BY product
ORDER BY total DESC;
' --connection production
```

The `--connection` hint is optional but recommended — without it, `bookmark run` will require a `--connection` flag every time.

## Naming Convention

Plain names work:
```bash
ferrule bookmark add count-all "SELECT COUNT(*) FROM ${1};"
```

Dotted names are treated as connection hints — the first segment suggests the connection to use:
```bash
# pg.select_users → auto-uses connection named "pg"
ferrule bookmark add pg.select_users "SELECT id, name, email FROM users;"

# When a dotted name doesn't match a saved connection, Ferrule falls back
# to requiring --connection or the default profile
```

## Listing Bookmarks

```bash
ferrule bookmark list
```

Output:
```
Name             | SQL
--------------------------------------------------------------
active-users     | SELECT * FROM users WHERE active = true;
user-by-id       | SELECT * FROM users WHERE id = ${1};
recent-sales     | SELECT product, SUM(amount) as total FR...
pg.select_users  | SELECT id, name, email FROM users;
```

## Running Bookmarks

```bash
# Run a simple bookmark
ferrule bookmark run active-users

# Run a bookmark with positional parameters
ferrule bookmark run user-by-id 42

# Run with a different format than the global default
ferrule bookmark run recent-sales --format table

# Override the suggested connection
ferrule bookmark run pg.select_users --connection staging

# Combine with output paging
ferrule bookmark run active-users --limit 10 --offset 20
```

Parameter substitution replaces `${1}`, `${2}`, etc. with the provided arguments. Missing parameters leave the placeholder intact.

## Deleting Bookmarks

```bash
ferrule bookmark delete user-by-id
```

## Where Bookmarks Are Stored

```
~/.config/ferrule/bookmarks.toml
```

Format:

```toml
[active-users]
sql = "SELECT * FROM users WHERE active = true;"
connection = "dev"

[user-by-id]
sql = "SELECT * FROM users WHERE id = ${1};"
connection = "dev"
```

## Workflow Example

Bookmarks shine in day-to-day workflows. Here's a typical pattern:

```bash
# Morning standup — quick metrics
ferrule bookmark run daily-metrics

# On-call alert — replication lag
ferrule bookmark run check-lag

# Customer support — lookup by email
ferrule bookmark run user-by-email 'alice@example.com'

# End of sprint — export user growth chart
ferrule bookmark run user-growth --format csv > sprint_growth.csv
```

## Using Bookmarks in the REPL

Bookmarks also work interactively from within the REPL:

```
> SELECT * FROM users WHERE active = true;
> \bookmark save active-users
Bookmark 'active-users' saved.

> \bookmark list
- active-users
- daily-metrics

> \bookmark run active-users
> \bookmark delete active-users
```

REPL bookmarks are saved to the same `~/.config/ferrule/bookmarks.toml` file as CLI bookmarks.
