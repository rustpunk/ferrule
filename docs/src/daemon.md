# Connection Pooling Daemon

Most ferrule invocations are one-shot: spin up the runtime, open a
connection, run one query, tear down. For occasional use that's fine
— a few hundred milliseconds of connection setup disappears into the
noise of a human typing. Tight scripts that run hundreds of queries
in a row pay that cost on every call.

The daemon is a long-lived background process that holds open
connections so subsequent commands reuse them.

## When to use it

Reach for `--daemon` when:

- A script (or pipeline, or dashboard refresh) issues many small
  queries to the same database in quick succession.
- You're running an interactive REPL workflow and the connect time
  is noticeable.
- The database itself is slow to authenticate (Oracle in particular
  takes a few seconds to set up a session).

Skip it when:

- You only run ferrule once or twice per shell session — the
  per-invocation overhead is tiny, and the daemon is memory you
  won't reclaim.
- You're on a shared multi-user host. The daemon is per-user and
  doesn't help anyone but you.
- You can't afford the daemon to hold a resolved password in memory
  (see [Security caveats](security.md#caveats)).

## Lifecycle

### Start

```bash
# Foreground (good for debugging — Ctrl-C to stop)
ferrule conn start

# Background — forks and detaches
ferrule conn start --background
```

On Unix, the daemon binds a per-user socket inside the user cache
directory; on Windows it listens on a random localhost TCP port
recorded in a sibling file:

| OS | Endpoint | Discovery file |
|---|---|---|
| Linux | `~/.cache/ferrule/daemon.sock` (Unix domain socket, mode `0600`) | (the socket itself) |
| macOS | `~/Library/Caches/ferrule/daemon.sock` (Unix domain socket, mode `0600`) | (the socket itself) |
| Windows | `127.0.0.1:<random-port>` (TCP, localhost only) | `%LOCALAPPDATA%\ferrule\daemon.port` |

The PID is recorded next to the socket at `daemon.pid` so
`ferrule conn status` can find it. Unix socket permissions are
`0600`, restricting access to the same user.

### Status

```bash
ferrule conn status
```

Reports whether the daemon is running, its PID, and how many
connections it currently holds.

### Stop

```bash
ferrule conn stop
```

Sends a graceful shutdown. In-flight queries are allowed to finish;
new requests are rejected.

### Restart

```bash
ferrule conn restart
```

Equivalent to `stop` followed by `start` (foreground). Useful after
changing config that affects how connections are built (TLS posture,
default profile defaults).

## Routing requests through the daemon

Every command that takes a connection accepts `--daemon`:

```bash
ferrule query prod "SELECT * FROM users LIMIT 10" --daemon
ferrule tables prod --daemon
ferrule describe prod users --daemon
ferrule explain prod "SELECT * FROM users WHERE id = 1" --daemon
ferrule dump prod users --dump-format csv --daemon > users.csv
ferrule watch prod "SELECT COUNT(*) FROM jobs" --interval 2 --daemon
```

The first request to a given connection name opens a real connection
and pools it. Subsequent requests reuse it. Pool keys are based on
the resolved URL (post-credential-stack), so two profiles pointing
at the same DB don't waste a slot.

If the daemon isn't running, `--daemon` exits with a usage error
listing how to start it. There's no implicit "start if missing"
because that would mask configuration mistakes.

## Operational notes

- **No persistent state.** Pools live in the daemon's RAM. A restart
  drops everything; the next request rebuilds.
- **No remote endpoint.** The daemon listens on a per-user Unix
  socket, never on TCP. There is no flag to expose it over the
  network — that's intentional.
- **Connections live for the daemon's lifetime.** There is no idle
  reaper today; pooled connections stay open until they're either
  used (refreshing the last-used timestamp) or the daemon is stopped
  / restarted. If a server-side timeout closes the connection out
  from under the pool, the next request reopens it transparently.
- **One daemon per user, not per database.** Each user gets one
  daemon process that multiplexes all their connections. Two
  ferrule users on the same box have independent daemons and
  independent sockets.

## Troubleshooting

### `daemon is not running`

`ferrule conn start --background` and try again. Confirm with
`ferrule conn status`.

### `failed to connect to daemon socket`

The socket exists but the daemon isn't reachable. Often happens
after a clean reboot if `--background` left a stale socket file.
`ferrule conn restart` clears it.

### Stale connections after DB restart

The daemon doesn't proactively detect that the DB went away. The
first reused connection after a server restart will fail; the
second one (built fresh) succeeds. If you see a single transient
failure after a known DB blip, that's why. `ferrule conn restart`
forces all pools to drop.

### Want to see what it's doing

Run the daemon in the foreground:

```bash
ferrule conn start
```

Logs to stderr in human-readable form. Combine with
`RUST_LOG=ferrule=debug` for verbose output.

See also: [Concepts](concepts.md), [Connections](connections.md),
[Troubleshooting](troubleshooting.md).
