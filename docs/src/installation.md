# Installation

Ferrule ships as a single static binary. No runtime libraries are
required for the four default backends (PostgreSQL, MySQL, MSSQL,
SQLite). Oracle alone needs Oracle Instant Client — see
[Backends](backends.md) and [Troubleshooting](troubleshooting.md#oracle-libclntshso-not-found).

## Pre-built binaries

The fastest path on any supported OS:

```bash
# Linux x86_64
curl -L https://github.com/rustpunk/ferrule/releases/latest/download/ferrule-linux-x64.tar.gz \
  | tar xz
sudo mv ferrule /usr/local/bin/
```

Replace the asset name for other targets (`ferrule-macos-arm64.tar.gz`,
`ferrule-macos-x64.tar.gz`, `ferrule-windows-x64.zip`). The full list is
on the [releases page](https://github.com/rustpunk/ferrule/releases).

On macOS, if Gatekeeper blocks the binary the first time, allow it
under **System Settings → Privacy & Security**, or strip the
quarantine attribute:

```bash
xattr -d com.apple.quarantine /usr/local/bin/ferrule
```

On Windows, drop `ferrule.exe` somewhere on `%PATH%` (or run it from
a folder you've added to `PATH`).

## From source

Requires a stable Rust toolchain (install via [rustup](https://rustup.rs/)).

```bash
git clone https://github.com/rustpunk/ferrule.git
cd ferrule

# Default build — Postgres, MySQL, MSSQL, SQLite
cargo build --release --bin ferrule

# With Oracle (requires Instant Client at runtime, not at compile time)
cargo build --release --bin ferrule --features oracle

# Or install straight to ~/.cargo/bin
cargo install --path ferrule-cli
```

The release binary lands at `./target/release/ferrule`.

## Feature flags

| Feature | Default | Notes |
|---|---|---|
| `postgres` | ✅ | `tokio-postgres` + `rustls` (no OpenSSL) |
| `mysql` | ✅ | `mysql_async` |
| `mssql` | ✅ | `tiberius` |
| `sqlite` | ✅ | `rusqlite` with bundled SQLite |
| `oracle` | ❌ | `oracle` crate; needs Instant Client at first connection |

Build a minimal binary with only the backends you need:

```bash
# SQLite-only — useful for embedding in fixture scripts
cargo build --release --bin ferrule --no-default-features --features sqlite

# Postgres + SQLite for a Postgres-focused workflow
cargo build --release --bin ferrule --no-default-features --features postgres,sqlite
```

Cutting unused backends shrinks the binary and trims the dependency
graph; functionally there's no difference for the backends you keep.

## Verify the install

```bash
ferrule --version
# ferrule 0.x.y

# First config-touching command — creates ~/.config/ferrule if absent
ferrule conn list
# (no output if the registry is empty)

# Smoke-test SQLite (no server required)
ferrule query "sqlite::memory:" "SELECT 'ok' AS status;" --format table
```

If any of these fail, see [Troubleshooting](troubleshooting.md).

## Next steps

- [Quick Start](quickstart.md) — run your first real query.
- [How Ferrule Thinks](concepts.md) — the mental model.
