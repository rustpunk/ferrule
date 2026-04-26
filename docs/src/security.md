# Security

Ferrule talks to databases, which means it handles credentials, opens
TLS sessions, and emits diagnostics that could leak secrets if it
weren't careful. This chapter is the canonical home for ferrule's
security guarantees and how to use them.

## Threat model in one paragraph

The defaults assume a single-user workstation or a CI runner where the
attacker is another *process* running as the same OS user (or someone
who later reads your shell history). Ferrule cannot defend against an
attacker who already has root, who can read raw memory, or who controls
the database server itself. Within that scope, the goal is: **secrets
should not be written to the filesystem in plaintext, should not be
visible to other processes, and should not be logged.**

## Password best practices, ranked

The same options ranked top-to-bottom by safety. Match the option to
your environment.

### 1. `keyring://` — best for workstations

OS-native credential store. macOS Keychain, Windows Credential Manager,
or Linux Secret Service (libsecret / GNOME Keyring / KWallet). Secrets
are encrypted at rest and unlocked by the desktop session.

```toml
[connection.production]
url = "postgres://app@db.example.com/myapp"
password_url = "keyring://ferrule/production"
```

Store a password without ever putting it on disk:

```bash
ferrule conn set-password production
# Password: ••••••••
```

Limitations: requires an unlocked keyring session. If you SSH in
without `-Y` or run from cron, the keyring is often locked — see
[Troubleshooting](troubleshooting.md#keyring-permission-denied).

### 2. `file://` — best for containers

A file mounted at a known path. Docker swarm secrets, Kubernetes
secrets, and `systemd` `LoadCredential=` all work this way. The file
should be `chmod 0600` and owned by the runtime user.

```toml
[connection.production]
url = "postgres://app@db.example.com/myapp"
password_url = "file:///run/secrets/db_password"
```

Pros over `env://`: not visible in `/proc/<pid>/environ`, not inherited
by child processes, easy to rotate (overwrite the file).

Append `?raw=true` if the file must be read verbatim — by default hasp
trims a single trailing newline, which matters for tools like
`docker secret create` that don't add one.

### 3. `env://` — convenient, fine for development

An environment variable. Visible to any process running as the same
user via `/proc/<pid>/environ`, and inherited by every child process.

```toml
[connection.staging]
url = "mysql://user@staging.internal/app"
password_url = "env://STAGING_DB_PASSWORD"
```

A legacy convention `FERRULE_<NAME>_PASSWORD` is also recognized
without an explicit `password_url`:

```bash
export FERRULE_STAGING_PASSWORD="...";
ferrule query staging "SELECT 1"
```

### 4. Interactive prompt

If everything else falls through and stdin is a TTY, ferrule prompts.
The secret never touches disk, env, or shell history. Slow if you run
many queries; fine for occasional use.

### 5. `--password` flag — last resort

Leaks the secret into shell history (`~/.bash_history`,
`~/.zsh_history`) and process listings (`ps aux`). Useful only for
ephemeral CI snippets where the secret comes from an injected
variable, or for one-off debugging where you'll rotate the password
afterward.

```bash
ferrule query prod "SELECT 1" --password "$INJECTED_FROM_CI"
```

### 0. Passwords in URLs — don't

```bash
# Bad: ends up in plain-text TOML, shell history, and ps output
ferrule conn add prod "postgres://user:secret@host/db"
```

Save the URL without the password, and let ferrule resolve credentials
through the stack:

```bash
ferrule conn add prod "postgres://user@host/db"
ferrule conn set-password prod
```

## TLS posture per backend

By default, ferrule verifies TLS certificates for every backend that
supports TLS. The `--insecure` flag disables both certificate-chain
verification *and* hostname verification — it does not just skip one or
the other.

| Backend | TLS by default | How to require it | Self-signed dev cert |
|---|---|---|---|
| PostgreSQL | Negotiated; client decides | `?sslmode=require` (encryption only) or `?sslmode=verify-full` (chain + hostname) | `?sslmode=require` + `--insecure` |
| MySQL | Off unless server requires; rustls-based | Server-side `require_secure_transport` | `--insecure` |
| MSSQL | Always negotiated; cert often self-signed | (default) | `?trustServerCertificate=true` *or* `--insecure` |
| SQLite | N/A — local file or memory | — | — |
| Oracle | Server-configured | TNS listener config | TNS-side, not URL |

When ferrule runs with `--insecure`, it prints a warning to stderr:

```text
Warning: --insecure disables TLS certificate verification.
```

That line is intentional — make sure your scripts don't grep stderr
for emptiness.

### When `--insecure` is defensible

- A local Docker container with a self-signed cert that you control.
  (The MSSQL test setup in `CLAUDE.md` is the canonical example.)
- Reaching a private replica over an already-encrypted tunnel
  (Wireguard, SSH `-L`).
- Bootstrapping when the production cert hasn't been provisioned yet.

### When it isn't

- Anything across the public internet.
- Production DBs accessed over corporate VPNs (the VPN protects
  routing, not the DB session).
- Anywhere the diagnostic warning will be silently captured into a
  log and never read.

## Hasp URL schemes side-by-side

`password_url` and ferrule's internal credential resolver both use
[hasp](https://crates.io/crates/hasp) URLs. The three relevant schemes
expose a secret to a different attacker class:

| Scheme | What it is | Visible to | Survives reboot |
|---|---|---|---|
| `env://NAME` | Environment variable in this process | Anything reading `/proc/<pid>/environ` for this PID + every child | No (set per-shell) |
| `keyring://service/account` | OS keyring entry | Code running as the same desktop session | Yes |
| `file:///path` | File on disk | Anyone who can `read(2)` the file | Yes |

Pick by what attacker you're defending against, not by what's most
ergonomic. A leaked `env://` secret is a leaked secret; a leaked
`keyring://` lookup that fails is just a re-prompt.

## OS keyring details

Per-platform backends used by `ferrule conn set-password` and any
`keyring://` URL:

- **macOS** — Keychain (login keychain by default).
- **Windows** — Credential Manager.
- **Linux** — Secret Service over D-Bus (libsecret-compatible:
  GNOME Keyring, KeePassXC's secret-service, KWallet via secret
  service).

Stored under `service=ferrule`, `account=<connection-name>`. You can
inspect with the platform's native tool (`security find-generic-password
-s ferrule -a production` on macOS, Seahorse on GNOME, etc.).

## Redaction policy

A few invariants ferrule guarantees:

- **Passwords are wrapped in `secrecy::SecretString`** in memory.
  `Debug` formatting prints `[REDACTED]`; the underlying buffer is
  zeroed on drop.
- **URLs are redacted before logging.** The `--verbose` flag prints
  the resolved URL with the password component replaced by `***`:
  ```text
  [ferrule] Resolved URL: postgres://user:***@host/db
  ```
  Search the codebase for `display_redacted` if you want to see
  exactly what is and isn't elided.
- **Error messages from drivers may not redact.** A driver-level
  error from `tokio-postgres` or `tiberius` is wrapped, not parsed.
  In rare cases it could echo a portion of the URL. If you see a
  redaction failure, that's a bug worth filing.

## Caveats

- **Inline passwords in a URL bypass the credential stack entirely.**
  This is by design — sometimes you really do want the URL to be
  self-contained — but means ferrule cannot help you redact it.
  Don't save such URLs to the registry.
- **`ferrule conn add` writes to plain-text TOML.** Use it only for
  URLs without passwords. Add passwords with `set-password`
  afterward.
- **The daemon (`ferrule conn start`) holds connections open.** That
  means it also holds the resolved password in memory for the
  daemon's lifetime. If you don't trust other processes on the box,
  prefer one-shot connections without `--daemon`.
