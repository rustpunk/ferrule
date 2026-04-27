# SSH Tunnels

Many production databases sit behind a bastion: the database refuses
direct connections from the open internet, but you can `ssh` into a
jumphost on the inside that *can* reach it. Most desktop database
tools (DBeaver, DataGrip, Beekeeper, TablePlus) handle this with a
"use SSH tunnel" checkbox; ferrule does the same with a CLI flag.

The tunnel is set up with [russh][russh] (a pure-Rust SSH 2 client),
opens a `direct-tcpip` channel to the database, and passes the
underlying stream into the database driver. There is no `ssh` binary
shelled out to and no `~/.ssh/config` honored.

[russh]: https://crates.io/crates/russh

## Quick start

```bash
ferrule query \
  --ssh-tunnel ec2-user@bastion.example.com \
  --ssh-key ~/.ssh/id_ed25519 \
  "postgres://app:pwd@db.internal:5432/myapp" \
  "SELECT * FROM users LIMIT 10;"
```

This:

1. Opens an SSH session to `ec2-user@bastion.example.com:22` using
   the key at `~/.ssh/id_ed25519`.
2. Asks the bastion to open a `direct-tcpip` channel to
   `db.internal:5432`.
3. Hands that channel directly to `tokio_postgres` via
   `connect_raw`. The Postgres protocol — and TLS, if your URL has
   `sslmode=require` — runs end-to-end through the SSH stream.

The database URL stays clean: it's the same string you'd copy out of
the AWS RDS, GCP Cloud SQL, or Heroku console. SSH config goes in
its own flags or its own profile keys.

## Where SSH config lives

Three layers, primary to ad-hoc:

### 1. Profile keys (primary — `.ferrule.toml`)

For a connection you use repeatedly, put the SSH bits in the profile:

```toml
[connection.prod-pg]
url = "postgres://app:pwd@db.internal:5432/myapp"
ssh_host = "bastion.example.com"
ssh_user = "ec2-user"
ssh_port = 22
ssh_key  = "~/.ssh/prod-bastion.pem"
```

Then `ferrule query prod-pg "SELECT 1"` automatically tunnels
through `ec2-user@bastion.example.com:22` using the named key.

### 2. CLI flags (ad-hoc)

For one-shot use against a connection you haven't profiled:

- `--ssh-tunnel [user@]host[:port]` — atomic-replacement for the
  three SSH connection parameters. Matches [pgcli's][pgcli] flag
  syntax verbatim. If you pass `--ssh-tunnel host` (no user, no
  port), the user falls back to `$USER` and the port falls back to
  22 — *not* to whatever the profile said. "One flag, one tunnel
  target."
- `--ssh-key <path>` — overrides `ssh_key` independently. Useful
  for testing different keys against the same bastion.

[pgcli]: https://www.pgcli.com/

### 3. The URL stays plain

There is no `ssh+postgres://` scheme. That style was tried, then
backed out — every other tool in the ecosystem (DBeaver, DataGrip,
Beekeeper, TablePlus, Sequel Ace, pgAdmin, MySQL Workbench, Navicat,
SQLAlchemy, Prisma, TypeORM, libpq pg_service.conf, pgcli) keeps
the SSH section separate from the database URL, and ferrule matches
that consensus. The same `postgres://...` string works inside or
outside the tunnel.

## Key resolution

When you pass `--ssh-tunnel ...` (or set `ssh_host` in a profile),
ferrule resolves the SSH key in this order. First hit wins:

1. `--ssh-key <path>` (CLI) or `ssh_key = "..."` (profile).
2. `FERRULE_<NAME>_SSH_KEY=<path>` env var (where `<NAME>` is the
   uppercased connection name with `-` → `_`).
3. `~/.ssh/id_ed25519`.
4. `~/.ssh/id_rsa`.
5. The SSH agent at `$SSH_AUTH_SOCK`.

If none of those resolve, ferrule errors out with a diagnostic
listing every option it tried — the same shape as the password
resolution stack.

```text
no SSH key resolved for connection 'prod-pg'. Provide one of:
  --ssh-key <path>
  ssh_key in the profile
  FERRULE_PROD_PG_SSH_KEY=<path> env var
  ~/.ssh/id_ed25519 or ~/.ssh/id_rsa identity file
  a running SSH agent (SSH_AUTH_SOCK)
```

### Encrypted keys

If `load_secret_key` reports the key needs a passphrase, ferrule
prompts for it interactively:

```text
Enter passphrase for SSH key /home/user/.ssh/id_ed25519:
```

In non-interactive contexts (CI, scripts, pipes) the prompt is
skipped and ferrule returns an error:

```text
SSH key /path/to/key is encrypted. Passphrase prompting requires an interactive terminal.
Use an SSH agent or decrypt the key on disk.
```

Workarounds when a terminal is not available:

- Use the SSH agent. `ssh-add ~/.ssh/encrypted-key` once per shell
  session, then ferrule will route signing requests through the
  agent.
- Decrypt the key on disk: `ssh-keygen -p -f ~/.ssh/encrypted-key`
  removes the passphrase (don't do this for keys you don't
  exclusively control).

## Transport: how the bytes flow

Two transports, picked by backend automatically:

### (b) Direct stream — Postgres

The russh `ChannelStream` is fed straight into
`tokio_postgres::Config::connect_raw(stream, tls_connector)`. There
is no extra TCP hop on the local side. TLS, if requested via
`?sslmode=require`/`verify-full`, is negotiated end-to-end inside
the SSH channel — so a URL like
`postgres://app:pwd@db/myapp?sslmode=require` paired with
`--ssh-tunnel bastion` gets BOTH SSH transport AND TLS to the
database. The two layers compose.

### (a) Local listener — MySQL, MSSQL, Oracle

Those drivers don't expose a custom-stream injection API, so ferrule
binds a `127.0.0.1:<random>` TCP listener, spawns a forwarder task
that pumps bytes between the listener and the SSH channel via
`tokio::io::copy_bidirectional`, and rewrites the URL to point at
the local port before handing it to the driver.

The listener accepts a single connection, then the forwarder runs
for the connection's lifetime. ferrule's CLI is one-shot per
invocation, so this matches how the drivers behave anyway.

### Sqlite is rejected

SQLite is local-file only — there's no host:port for SSH to forward
to. Combining `--ssh-tunnel ... sqlite:///path/to/db` produces:

```text
SSH tunneling is not applicable to SQLite (local-file backend)
```

## Host-key verification: not yet

Ferrule's SSH client currently accepts every server key
unconditionally. Each tunnel setup prints:

```text
[ferrule] Warning: SSH host key verification is disabled
(known_hosts comparison not yet implemented). Connecting to
bastion.example.com:22 on faith.
```

This is dev-quality only. In production this means a
man-in-the-middle on the bastion's network can intercept the SSH
session. Mitigations:

- Run ferrule from a network you trust to not host an MITM.
- Use a VPN to reach the bastion, not the open internet.
- Wait — TOFU/known_hosts comparison is staged as its own design
  discussion and will land in a future release.

The warning is intentionally noisy so users notice. If you find it
distracting, either accept the silence-implies-blanket-trust
trade-off and pipe `2>/dev/null`, or wait for the known_hosts work.

## SSH and the daemon don't mix

If you pass `--daemon` *and* an SSH tunnel, ferrule rejects the
combination:

```text
SSH tunnels bypass the connection pooling daemon. The tunnel
session lifecycle is tied to the request, so pooling tunneled
connections would introduce a class of failure modes around session
timeout that ferrule does not currently handle. Either drop --daemon
or open without a tunnel.
```

Why: pooling tunneled connections has a real failure mode that is
hard to handle correctly. When the SSH session times out (most
bastions kill idle sessions in 5-15 minutes), the pooled DB
connection above it goes dead. The DB driver then tries to talk to
a now-dead local port and returns confusingly long timeouts. DBeaver
has fought this for years; ferrule sidesteps it by not pooling
tunnels at all.

If you need a long-lived tunnel for many queries, use the REPL —
`ferrule repl --ssh-tunnel ... <conn>` keeps a single session open
for the whole REPL session, and `\conn <name>` switches the inner DB
without reopening the bastion.

## Build feature

The SSH stack is opt-in via the `ssh` Cargo feature:

```bash
# Default build — no SSH support, --ssh-tunnel errors out with a
# diagnostic.
cargo build --release

# With SSH support
cargo build --release --features ferrule-cli/ssh

# All features (Oracle + SSH)
cargo build --release --features ferrule-cli/all
```

The default build excludes russh because the SSH dependency stack
adds ~4 MB to the release binary (20 MB → 24 MB on Linux x86_64,
measured 2026-04-27). Most users who never tunnel don't need to pay
that.

If you try to use `--ssh-tunnel` against a default-features
binary:

```text
This ferrule binary was built without the `ssh` feature. Rebuild
with `cargo build --features ferrule-cli/ssh` (or `--features all`).
```

## Troubleshooting

| Symptom | Likely cause | Fix |
|---|---|---|
| `connect to <host>:<port>: Connection refused` | Bastion isn't listening on that port, or you're not on its allow-list | Confirm with plain `ssh -p <port> user@host` |
| `publickey auth failed for user 'X' (server rejected key)` | Key not in `~user/.ssh/authorized_keys` on the bastion | Confirm with plain `ssh -i <key> user@host` |
| `SSH agent at <sock> has no identities loaded` | Agent is running but empty | `ssh-add ~/.ssh/id_ed25519` |
| `load SSH key from <path>: ...` | Wrong passphrase or corrupted key | Check the passphrase or regenerate the key |
| `SSH key <path> is encrypted. Passphrase prompting requires an interactive terminal.` | Encrypted key in a non-interactive context | Use the agent or decrypt on disk (see above) |
| `SSH tunneling is not applicable to SQLite` | The URL is a sqlite:// scheme | Drop `--ssh-tunnel` for sqlite |
| Long hang then "connection failed" | DB host unreachable from the bastion | Confirm with `ssh user@host -- nc -zv <db-host> <db-port>` |

When in doubt, drop ferrule and try the plain `ssh` and `psql`/`mysql`
binaries against the same hosts. If those work, ferrule should too.
