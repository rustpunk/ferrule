# Interactive REPL

Launch with any connection — a raw URL, a profile name, or a saved
registry entry:

```bash
ferrule repl "sqlite::memory:"
ferrule repl demo
ferrule repl prod
```

The REPL is a TTY-only experience. If you need batch input from a
script, use `ferrule query --stdin` instead (see
[Querying](querying.md)).

## What you get

- **Readline editing** via `rustyline` — arrow keys, line editing,
  Ctrl-R reverse history search, Ctrl-A / Ctrl-E for line endpoints.
- **Multi-line SQL.** Statements that span multiple lines are
  collected until a trailing `;`. Cancel a partially-typed
  statement with Ctrl-C.
- **Persistent history** at `~/.cache/ferrule/history`. Shared
  across sessions for the current user.
- **Session parameters** (`\param`) and **bookmarks**
  (`\bookmark`) work the same as their CLI counterparts and share
  the same bookmark file.
- **Watch mode** (`\watch`) re-runs the previous query on a
  configurable interval without leaving the REPL.

## Meta-commands

Backslash prefix:

| Command | Description |
|---|---|
| `\q` | Quit the REPL |
| `\conn [name]` | Switch connection or show current |
| `\d [table]` | Describe table; without an argument, behaves like `\dt` |
| `\dt [schema]` | List tables |
| `\format [fmt]` | Set output format (`table`, `json`, `csv`, `yaml`, `raw`) |
| `\limit [N]` | Set row limit (`0` to clear) |
| `\timing [on\|off]` | Toggle timing display (per-query connect/query/format breakdown) |
| `\verbose [on\|off]` | Toggle verbose logging (resolved URL + SQL) |
| `\param <name> <value>` | Set a session parameter for `${name}` placeholders |
| `\param clear` | Clear all session parameters |
| `\param list` | List currently-set parameters |
| `\bookmark save <name>` | Save the *previous* SQL as a bookmark |
| `\bookmark list` | List saved bookmarks |
| `\bookmark run <name> [args…]` | Run a bookmark with positional params |
| `\bookmark delete <name>` | Delete a bookmark |
| `\g` | Re-run the previous SQL statement |
| `\explain` | Toggle EXPLAIN-mode (wraps every executed query) |
| `\explain on\|off\|toggle` | Set EXPLAIN-mode explicitly |
| `\explain <sql>` | Explain a single query (one-shot) |
| `\watch` | Re-run previous SQL every 5s until Ctrl+C |
| `\watch <secs>` | Re-run previous SQL every `<secs>` seconds |
| `\watch <sql>` | Watch the given SQL (5s default) |
| `\dump <table>` | Dump a table to stdout |
| `\load <file> INTO <table>` | Load a CSV/JSON file into a table |
| `\help` | Show in-REPL help |

## Example session

```text
$ ferrule repl demo

Connected to postgres://ferrule:***@127.0.0.1:15432/ferrule
Type \help for commands, \q to quit.

ferrule> \format table
ferrule> SELECT id, name, signed_up FROM customers ORDER BY id;
┌────┬───────┬───────────────────────────────┐
│ id │ name  │ signed_up                     │
├────┼───────┼───────────────────────────────┤
│ 1  │ Alice │ 2026-04-26 18:01:23.456+00:00 │
│ 2  │ Bob   │ 2026-04-26 18:01:23.456+00:00 │
│ 3  │ Carol │ 2026-04-26 18:01:23.456+00:00 │
└────┴───────┴───────────────────────────────┘

ferrule> \timing on
Timing: on

ferrule> SELECT COUNT(*) FROM customers;
┌──────┐
│ count│
├──────┤
│ 3    │
└──────┘
[ferrule] connect: 0ms (pooled)
[ferrule] query:   2ms
[ferrule] total:   2ms

ferrule> \bookmark save customer-count
Bookmark 'customer-count' saved.

ferrule> \param email 'alice@example.com'
ferrule> SELECT * FROM customers WHERE name = ${email};
(...)

ferrule> \q
```

## Bookmarks in the REPL

REPL bookmarks share the `~/.config/ferrule/bookmarks.toml` file
with CLI bookmarks. Saving from the REPL captures the *previous*
SQL statement as the bookmark body:

```text
ferrule> SELECT * FROM users WHERE active = true;
ferrule> \bookmark save active-users
Bookmark 'active-users' saved.

ferrule> \bookmark list
- active-users
- customer-count

ferrule> \bookmark run active-users
```

Positional parameters work the same as on the CLI:

```text
ferrule> \bookmark run user-by-id 42
```

See [Bookmarks](bookmarks.md) for the full surface.

## Watch mode inside the REPL

Re-run the previous query on a fixed cadence without leaving the
REPL:

```text
ferrule> SELECT COUNT(*) FROM events;
ferrule> \watch                  # repeats the COUNT(*) every 5s
ferrule> \watch 3                # repeats the COUNT(*) every 3s
ferrule> \watch SELECT now();    # watch a different SQL at 5s default
```

Press `Ctrl+C` to exit the watch loop and return to the prompt
without leaving the REPL. Intervals are integer seconds (u64);
sub-second intervals are not supported in v1.

If `\explain` mode is on, the watched SQL is wrapped through
`EXPLAIN` on each iteration so plan changes can be tracked
visually.

Mid-loop control (`\watch interval N` / `\watch stop`) is reserved
for a future release; issuing them outside a running loop prints a
polite error. Watch prints a header on each iteration. The `--diff`
mode (only print on change) is also supported in watch loops; see
[Advanced Features](advanced.md#watch-mode).

## When to leave the REPL

Switch to one-shot `ferrule query` invocations when:

- You're scripting (no TTY).
- You need `--output FILE` to write to a file.
- You want predictable exit codes for use in `&&` chains.

Switch *to* the REPL when:

- You're iterating on a query and want history + multi-line editing.
- You want timing or verbose mode toggled per-statement.
- You want to alternate between SQL and `\d` / `\dt` exploration.

## History maintenance

Persistent history at `~/.cache/ferrule/history` grows unbounded.
If it gets unwieldy, truncate it manually:

```bash
tail -n 1000 ~/.cache/ferrule/history > ~/.cache/ferrule/history.new
mv ~/.cache/ferrule/history.new ~/.cache/ferrule/history
```

Or delete it entirely to start fresh — the file is recreated on
the next REPL launch.
