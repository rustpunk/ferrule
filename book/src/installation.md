# Installation

## From Source

Prerequisites:
- [Rust](https://rustup.rs/) (stable toolchain)
- For Oracle support: Oracle Instant Client libraries

```bash
git clone https://github.com/rustpunk/ferrule.git
cd ferrule

# Default build (Postgres, MySQL, MSSQL, SQLite)
cargo build --release --bin ferrule

# With Oracle support
cargo build --release --bin ferrule --features oracle

# Install to ~/.cargo/bin
cargo install --path ferrule-cli
```

The resulting binary is at `./target/release/ferrule`.

## Pre-built Binaries

Pre-built binaries are published on the [releases page](https://github.com/rustpunk/ferrule/releases).

```bash
curl -L https://github.com/rustpunk/ferrule/releases/latest/download/ferrule-linux-x64.tar.gz | tar xz
sudo mv ferrule /usr/local/bin/
```

## Feature Flags

| Feature | Default | Description |
|---------|---------|-------------|
| `postgres` | ✅ | PostgreSQL backend (via `tokio-postgres` + `rustls`) |
| `mysql` | ✅ | MySQL backend (via `mysql_async`) |
| `mssql` | ✅ | MSSQL backend (via `tiberius`) |
| `sqlite` | ✅ | SQLite backend (via `rusqlite`, bundled) |
| `oracle` | ❌ | Oracle backend (requires Instant Client) |

To build a minimal binary with only SQLite:

```bash
cargo build --release --bin ferrule --no-default-features --features sqlite
```
