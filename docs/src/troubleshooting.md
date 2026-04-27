# Troubleshooting

When something goes wrong, the fastest path is usually:

1. Re-run with `-v` / `--verbose` to see the resolved URL and SQL.
2. Add `--timing` to see whether you're stuck on connect, query, or
   format.
3. Match the symptom to one of the entries below.

Exit codes (`echo $?`) tell you which class of result you hit:

| Code | Meaning |
|---|---|
| 0 | Success |
| 1 | Notable result — `ferrule diff` found differences, future `--expect-rows`-style assertions. GNU `diff` / `grep` convention. Not an error |
| 2 | Usage / argument error (matches clap's parse-error exit) |
| 3 | Connection error (TLS, auth, network) |
| 4 | Query error (SQL syntax, constraint, schema) |

## Connection errors (exit code 3)

### `connection refused` / `host unreachable`

The TCP connection didn't open. Check:

- Container or DB process is actually running. For the docker setups
  in `CLAUDE.md`: `docker ps | grep ferrule-`.
- Port is published to localhost. The test setups bind to
  `127.0.0.1:15432` / `:13306` / `:11433` / `:11521` — not the
  default ports.
- Firewall isn't dropping it. Try `nc -zv host port` first; if `nc`
  can't connect, ferrule won't either.

### `TLS handshake failed` / `unknown issuer`

The server presented a certificate ferrule's trust store doesn't accept.

- For PostgreSQL with a public cert, prefer `?sslmode=verify-full`
  (or omit `sslmode` to negotiate) — `verify-full` checks the chain
  and the hostname.
- For PostgreSQL with a private CA, install the CA into the system
  trust store (rustls reads `/etc/ssl/certs` on Linux). There is no
  per-connection CA flag yet.
- For MSSQL with a self-signed cert (the docker test image),
  `?trustServerCertificate=true` accepts it. `--insecure` is the
  blanket equivalent.
- Don't reach for `--insecure` until you've checked the cert is what
  you expect: `openssl s_client -connect host:5432 -starttls postgres
  </dev/null 2>/dev/null | openssl x509 -text`.

### `password authentication failed` / `Login failed`

The driver got past TLS and onto auth, then was rejected.

- Re-run with `-v` to see the resolved URL — confirm the user is
  what you expected after profile substitution.
- Try the password manually with the native client (`psql`, `mysql`,
  `sqlcmd`). If the native client also fails, the password is wrong;
  if it succeeds, ferrule resolved a different password than you
  thought.
- Check the credential stack order in [Concepts](concepts.md#the-credential-stack)
  — a stale `FERRULE_<NAME>_PASSWORD` env var beats a freshly stored
  keyring entry.

### `Could not resolve password for '<name>'`

Ferrule walked the entire stack and got nothing. The diagnostic lists
each step and why it failed:

```text
ferrule::connection
  × Could not resolve password for 'production'
  ├─ No --password flag
  ├─ password_url not configured
  ├─ FERRULE_PRODUCTION_PASSWORD is unset
  ├─ keyring://ferrule/production: not found
  └─ Terminal is not interactive
```

Common fixes:

- Add a `password_url` in `.ferrule.toml`, or
- `ferrule conn set-password <name>` to store one in the keyring, or
- Run from an interactive terminal (not `nohup`, not cron).

### Keyring permission denied

The keyring is locked, or there's no Secret Service to talk to.

- **Linux over SSH:** `ssh -Y` forwards the X session and unlocks
  GNOME Keyring; without it, the keyring is locked. Alternatively,
  unlock manually: `gnome-keyring-daemon --unlock < pwfile`.
- **Linux in cron / systemd:** D-Bus user session usually isn't
  running. Switch to `file://` for headless contexts.
- **macOS:** Run `security unlock-keychain` once per session if
  you're invoking from a non-GUI shell.

## Query errors (exit code 4)

### `Multi-statement SQL does not support --limit / --offset`

You sent multiple `;`-separated statements, and `--limit` (or the
default limit from `[default]`) is set. Either:

- Pass `--limit 0` to disable paging for this call, or
- Split the statements into separate `ferrule query` calls, or
- Set `[default] limit = 0` in `.ferrule.toml` if you mostly run
  multi-statement DDL.

The default limit is `1000`, applied even when `--limit` is absent —
that's why this error fires for batches you don't think have a limit.

### `relation "..." does not exist` / `Invalid object name`

Schema, search-path, or case issues.

- Postgres: re-run with explicit schema (`SELECT * FROM
  public.users`) or set the search path:
  `ferrule query prod "SET search_path = my_schema; SELECT * FROM users"`.
- MSSQL: schema-qualified names are `[schema].[table]` or
  `dbo.users`.
- SQLite: tables are case-sensitive but unquoted identifiers are
  case-folded — `SELECT * FROM Users` and `users` may target the
  same table or not, depending on how it was created.

### `JSON parse error` when piping to `jq`

The default output format is JSON — but `--timing`, `--verbose`, and
warnings print to *stderr*, not stdout. If you see them in your `jq`
input, you're capturing both streams. Use `2>/dev/null` or `2>&1 1>&3`
patterns to keep them apart.

## Backend-specific gotchas

### PostgreSQL: `sslmode=require` vs `verify-full`

`sslmode=require` encrypts the connection but doesn't verify *who*
you're talking to — a MITM with any valid cert can intercept. Use
`verify-full` for production (default rustls trust store) and keep
`require` for development against self-signed certs paired with
`--insecure`.

### MySQL: `caching_sha2_password` errors with old clients

MySQL 8 defaults to `caching_sha2_password`. `mysql_async` supports
it, but the first connection after a server restart sometimes fails
with `auth method unknown`. Reconnecting succeeds. If you can't
upgrade, change the user's auth plugin server-side:

```sql
ALTER USER 'app'@'%' IDENTIFIED WITH mysql_native_password BY '...';
```

### MSSQL: self-signed cert in the docker test image

The official `mcr.microsoft.com/mssql/server` image ships a
self-signed TLS cert. Append `?trustServerCertificate=true` to the
URL — that's narrower than `--insecure`, since it accepts the
self-signed cert without disabling hostname verification globally.

### Oracle: `libclntsh.so` not found

The `oracle` feature is opt-in. The crate dynamically links against
Oracle Instant Client at runtime; ferrule's compile step does not
require it.

```text
ferrule::connection
  × Oracle Instant Client (libclntsh.so) not found.
  └─ Install Instant Client and add to LD_LIBRARY_PATH:
     https://www.oracle.com/database/technologies/instant-client.html
```

Steps (Linux x86_64):

1. Download Basic from Oracle's site (license click-through required).
2. `unzip -q instantclient-basic-linux.x64-*.zip -d ~/opt/oracle`
3. `export LD_LIBRARY_PATH="$HOME/opt/oracle/instantclient_23_26:$LD_LIBRARY_PATH"`
4. On Ubuntu 24.04+, symlink `libaio`:
   ```bash
   ln -sf /usr/lib/x86_64-linux-gnu/libaio.so.1t64 \
          ~/opt/oracle/instantclient_23_26/libaio.so.1
   ```
5. Verify: `ldd ~/opt/oracle/instantclient_*/libclntsh.so | grep "not found"`
   should print nothing.

Don't extract `libclntsh.so` from a running database container — it
expects a full `$ORACLE_HOME` layout and segfaults at init.

## Performance

### `--timing` for a quick breakdown

```bash
ferrule query prod "SELECT * FROM big_table" --timing
```

Prints to stderr:

```text
[ferrule] connect: 142ms
[ferrule] query:    3.41s
[ferrule] format:   18ms
[ferrule] total:    3.57s
```

If `connect` dominates and you're running many short queries, the
[connection-pooling daemon](daemon.md) is what you want.

### `--limit` is client-side after the query runs

`--limit` slices results *after* they reach ferrule. For paging on
the server side, write `LIMIT` / `OFFSET` (or `OFFSET ... FETCH
NEXT` for MSSQL) into the SQL itself. The auto-injection that fires
when neither limit nor `--limit 0` is present handles single-statement
queries; for fine-grained control, do it explicitly.

## REPL

### `Interactive REPL requires a TTY`

The REPL refuses to start if stdin isn't a terminal — running it
under `nohup` or piping into it will trip this. For headless
batch use, fall back to `ferrule query --stdin`:

```bash
echo "SELECT 1;" | ferrule query prod --stdin
```

### History keeps growing

REPL history lives at `~/.cache/ferrule/history` and grows unbounded.
If it gets unwieldy, truncate it:

```bash
tail -n 1000 ~/.cache/ferrule/history > ~/.cache/ferrule/history.new
mv ~/.cache/ferrule/history.new ~/.cache/ferrule/history
```

## Still stuck

Open an issue with:

- The output of `ferrule --version`.
- The full command you ran (passwords redacted).
- The full stderr (with `-v --timing`).
- The host OS and the backend version.
- Whether the native client (`psql` / `mysql` / `sqlcmd`) reproduces
  the failure with the same URL.

Reports that include all five resolve in roughly half the time of
ones that don't.
