# HTTP CONNECT Proxy

Corporate networks often force outbound traffic through an HTTP CONNECT
proxy (Squid, Blue Coat, Zscaler, etc.). Ferrule supports this for both
direct database connections and SSH tunnels.

## Quick start

```bash
ferrule query \
  --proxy-url http://proxy.corp.example.com:8080 \
  "postgres://app:pwd@db.internal:5432/myapp" \
  "SELECT * FROM users LIMIT 10;"
```

This:

1. Opens a TCP connection to `proxy.corp.example.com:8080`.
2. Sends `CONNECT db.internal:5432 HTTP/1.1`.
3. After the proxy returns `200 Connection established`, negotiates the
   Postgres protocol through the tunnelled stream.

## Proxy configuration layers

Five layers, first hit wins:

1. **`--proxy-url <URL>`** CLI flag.
2. **`proxy_url = "..."`** in the profile (`.ferrule.toml`).
3. **`FERRULE_<NAME>_PROXY_URL=<URL>`** env var (where `<NAME>` is the
   uppercased connection name with `-` → `_`).
4. **`ALL_PROXY`**, **`HTTPS_PROXY`**, or **`HTTP_PROXY`** env vars.
5. No proxy.

`NO_PROXY` is honored at layer 4. If the target host matches a
`NO_PROXY` entry, the env-var proxy is skipped even when set.

### Example profile

```toml
[connection.prod-pg]
url = "postgres://app:pwd@db.internal:5432/myapp"
proxy_url = "http://proxy.corp.example.com:8080"
```

### Authenticated proxies

Include credentials in the URL:

```bash
ferrule query \
  --proxy-url http://user:pass@proxy.corp.example.com:8080 \
  ...
```

The credentials are sent via `Proxy-Authorization: Basic <base64>` during
the CONNECT handshake. Use `secrecy::SecretString` internally; ferrule
never prints the password in diagnostics.

## How each backend connects through the proxy

| Backend | Proxy path |
|---|---|
| **Postgres** | `http_connect` → `tokio_postgres::Config::connect_raw` (direct stream) |
| **MySQL** | `http_connect` per accepted connection → local TCP listener → driver |
| **MSSQL** | `http_connect` per accepted connection → local TCP listener → driver |
| **Oracle** | `http_connect` per accepted connection → local TCP listener → driver |
| **SQLite** | No-op — SQLite is local-file only |

For MySQL, MSSQL, and Oracle, ferrule binds `127.0.0.1:<random>` and
spawns a tiny forwarder: each inbound TCP connection triggers a fresh
`http_connect` to the database, then `tokio::io::copy_bidirectional`
pumps bytes. The driver sees only the local port.

## Proxy + SSH tunnel (bastion behind a corporate proxy)

Both flags compose: the proxy opens a tunnel to the SSH bastion, then
the SSH tunnel opens a `direct-tcpip` channel to the database.

```bash
ferrule query \
  --proxy-url http://proxy.corp.example.com:8080 \
  --ssh-tunnel ec2-user@bastion.example.com \
  --ssh-key ~/.ssh/id_ed25519 \
  "postgres://app:pwd@db.internal:5432/myapp" \
  "SELECT * FROM users;"
```

Byte flow:

```
ferrule → proxy HTTP CONNECT → bastion:22
       → SSH session
       → direct-tcpip channel → db.internal:5432
       → Postgres protocol
```

## `NO_PROXY` rules

`NO_PROXY` supports the same syntax as curl:

- `*` — disables proxy for every host.
- `localhost,127.0.0.1` — exact matches, comma-separated.
- `.example.com` — suffix match (`db.example.com` matches,
  `example.com` does not).
- Port numbers in patterns are ignored.

Example:

```bash
export HTTP_PROXY=http://proxy.corp.example.com:8080
export NO_PROXY="localhost,127.0.0.1,.internal.example.com"

ferrule query "postgres://app:pwd@db.internal.example.com:5432/myapp" "SELECT 1;"
# → NOT proxied (matches .internal.example.com)

ferrule query "postgres://app:pwd@db.external.example.com:5432/myapp" "SELECT 1;"
# → proxied through proxy.corp.example.com:8080
```

## SOCKS5

SOCKS5 is not implemented in this release. If your network requires it,
use a local SOCKS5-to-HTTP-CONNECT adapter (e.g. `proxychains-ng` or a
small `nc` wrapper) and point ferrule at that.
