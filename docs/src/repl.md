# Interactive REPL

Launch the REPL with any connection:

```bash
ferrule repl "sqlite::memory:"
ferrule repl production
```

## Features

- **Readline** editing via `rustyline` (arrow keys, history, reverse search).
- **Multi-line SQL** — statements spanning multiple lines are collected until a trailing `;`.
- **History** persisted to `~/.cache/ferrule/history`.
- **Session parameters** (`\param`) and **bookmarks** (`\bookmark`).
- **Watch mode** (`\watch`) for live-query monitoring without leaving the REPL.

## Meta-Commands

Prefix with `\`:

| Command | Description |
|---------|-------------|
| `\q` | Quit REPL |
| `\conn [name]` | Switch connection or show current |
| `\d [table]` | Describe table (list tables if no arg) |
| `\dt [schema]` | List tables |
| `\format [fmt]` | Set output format |
| `\limit [N]` | Set row limit (`0` to clear) |
| `\timing [on\|off]` | Toggle timing display |
| `\verbose [on\|off]` | Toggle verbose logging |
| `\param <name> <value>` | Set session parameter |
| `\param clear` | Clear all parameters |
| `\param list` | List parameters |
| `\bookmark save <name>` | Save last SQL as bookmark |
| `\bookmark list` | List bookmarks |
| `\bookmark run <name>` | Run a bookmark |
| `\bookmark delete <name>` | Delete a bookmark |
| `\explain <sql>` | Explain a query |
| `\explain` | Toggle explain mode |
| `\watch [sql]` | Watch a query (re-executes every 5s) |
| `\dump <table>` | Dump table to stdout |
| `\load <file> INTO <table>` | Load data from a file |
| `\help` | Show help |

## Bookmarks

Bookmarks are saved to `~/.config/ferrule/bookmarks.toml`.

```
> SELECT * FROM users WHERE active = true;
> \bookmark save active-users
Bookmark 'active-users' saved.

> \bookmark list
- active-users

> \bookmark run active-users
```

Bookmarks support positional parameter substitution with `${1}`, `${2}`, etc:

```toml
[by-id]
sql = "SELECT * FROM users WHERE id = ${1};"
connection = "production"
```

```
> \bookmark run by-id 42
```

## Watch Mode

Watch the last query or a new one directly from the REPL:

```
> SELECT COUNT(*) FROM events;
> \watch              # watches the COUNT(*) query
> \watch interval 3   # change interval
> \watch stop         # stop background watch
```

Watch prints a header on each iteration and supports `--diff` mode (only show output when it changes).
