# Plan: FE-005 — Connection Pooling Daemon

**Target:** `ferrule-cli/src/commands/conn.rs`, new `ferrule-cli/src/daemon.rs`  
**Crate:** `ferrule-cli`  
**Feature:** default  
**Estimate:** Large  
**Reference Implementation:** n/a — new subcommand

---

## Why This Matters

Spinning up a new TCP + TLS handshake for every query is slow for interactive use. A background daemon keeps hot connections alive and amortizes authentication cost across many queries. This is especially valuable for Oracle (slow ODPI-C init) and MSSQL (heavy TDS setup).

---

## Architecture

```
ferrule-cli/src/daemon.rs
├─ Daemon { listener: UnixListener / TcpListener, pool: DashMap<url, PooledConn> }
├─ PooledConn { conn: Box<dyn Connection>, last_used: Instant }
├─ DaemonClient { stream: UnixStream / TcpStream }
├─ Request { Ping, Query(sql), Execute(sql), Tables(schema), Describe(s,t) }
└─ Response { Ok(String), Err(String) }

ferrule-cli/src/commands/conn.rs
├─ ConnCommand::Start { background }
├─ ConnCommand::Stop
├─ ConnCommand::Status
└─ ConnCommand::Restart
```

---

## Implementation Checklist

1. **IPC Transport**
   - Unix: `~/.cache/ferrule/daemon.sock` (Linux/macOS)
   - Windows: `TcpListener::bind("127.0.0.1:0")` with port written to `~/.cache/ferrule/daemon.port`
   - Serialize requests/responses with `serde_json` + length-delimited framing

2. **Daemon Core**
   - Spawn a `tokio::task` that accepts connections on the socket
   - Maintain `DashMap<String, (Box<dyn Connection>, Instant)>` keyed by `url.redacted()`
   - TTL eviction: drop connections idle > 5 minutes
   - Graceful shutdown on `SIGTERM` / `Stop` command

3. **Client Side**
   - `ferrule conn start` — forks daemon if not running, prints PID
   - `ferrule conn stop` — sends `Stop` request, cleans up socket file
   - `ferrule conn status` — ping daemon, print active connections & uptime
   - `ferrule conn restart` — stop then start

4. **CLI Integration**
   - Add `--daemon` flag to `query`, `tables`, `describe` commands
   - When `--daemon` is set:
     1. Try to connect to socket
     2. If daemon absent, fall back to direct connect with a warning
     3. Send `Request`, print `Response`

5. **Security**
   - Socket file mode `0o600` (user-only) on Unix
   - Never write raw passwords to the socket payload — pass `DatabaseUrl` with password already embedded

6. **Verification**
   - [ ] `cargo build --workspace` ✅
   - [ ] `cargo clippy --workspace` ✅
   - [ ] `cargo test --workspace` ✅ (mock DaemonClient tests)
   - [ ] No `todo!()` remaining

---

## Integration Tests

```bash
# Start daemon
ferrule conn start

# Query via daemon
ferrule query "postgres://..." "SELECT 1" --daemon

# Status
ferrule conn status

# Stop
ferrule conn stop
```

---

## Cargo.toml Additions

```toml
[dependencies]
dashmap = "6"
serde_json = "1"
```

`tokio` is already present.

---

## Risks & Gotchas

1. **Forking on Unix** — `fork()` in a Tokio process is dangerous (file descriptors, threads). Use `std::process::Command` to spawn a separate `ferrule daemon` binary or a hidden subcommand.
2. **`Box<dyn Connection>` is not `Serialize`** — The daemon must hold the actual connection object in memory, not serialize it. Only the *request/response* crosses the wire.
3. **Windows vs Unix** — The implementation bifurcates at the transport layer. Use `#[cfg(unix)]` / `#[cfg(windows)]` liberally.
4. **Connection leaks** — A crashed daemon leaves zombie Postgres/MySQL sessions. Implement `Drop` for `PooledConn` to call `conn.close()` or rely on backend TCP timeout.
5. **Signal handling** — `ctrlc` crate or `tokio::signal` for SIGTERM on Unix.

---

## Related Files

- `ferrule-cli/src/commands/conn.rs` — New subcommands
- `ferrule-cli/src/main.rs` — Daemon entry point (`cli.command` dispatch)
- `ferrule-core/src/connection.rs` — `Connection` trait (unchanged)
- `ferrule-core/src/backend.rs` — `connect()` used by daemon to warm pools

---

*Plan generated after Wave 1 completion.*

---

## Status

**Completed** ✅
