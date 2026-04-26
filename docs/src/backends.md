# Backends

Ferrule speaks five database protocols. Four are on by default; Oracle
is opt-in. Each is implemented in a feature-gated module under
`ferrule-core/src/backends/`, and they all expose results through the
[unified `Value` type](concepts.md#the-unified-value-type).

## Backend matrix

| Backend | URL schemes | Feature flag | Driver |
|---|---|---|---|
| PostgreSQL | `postgres`, `postgresql` | `postgres` (default) | `tokio-postgres` + `rustls` |
| MySQL | `mysql`, `mariadb` | `mysql` (default) | `mysql_async` |
| MSSQL | `mssql`, `sqlserver`, `tds` | `mssql` (default) | `tiberius` |
| SQLite | `sqlite` | `sqlite` (default) | `rusqlite` (bundled) |
| Oracle | `oracle` | `oracle` (opt-in) | `oracle` crate (ODPI-C) |

To opt into Oracle, build with `cargo install ferrule --features oracle`
and arrange Instant Client at runtime — see
[Troubleshooting](troubleshooting.md#oracle-libclntshso-not-found).

## PostgreSQL

- Pure Rust; no `libpq` required.
- TLS via `rustls` (no OpenSSL dependency).
- SSL modes: `prefer`, `require`, `disable`, `verify-ca`, `verify-full`.
- **Multi-statement batches supported** — `;`-separated statements
  in one call.
- UUID, JSONB, arrays mapped to ferrule `Value` types natively.
- Numeric / decimal preserved as `Value::Decimal` (string-backed) to
  avoid precision loss.

```bash
ferrule query "postgres://user:pass@host/db?sslmode=require" "SELECT 1;"
```

For TLS posture, `verify-full` checks the full chain *and* the
hostname; `require` encrypts but doesn't verify identity. See
[Security](security.md#tls-posture-per-backend) for guidance on
which to use when.

## MySQL

- Pure Rust via `mysql_async`.
- Works with MySQL 5.7+ and MariaDB 10.3+.
- TLS handled by the driver; opt in via the URL or server config.
- JSON column type maps to `Value::Json`.
- ENUM columns map to `Value::String` (the variant name).
- **Multi-statement batches not supported** at the ferrule layer —
  the driver allows it, but we don't expose it to keep behavior
  consistent across backends. Issue separate `ferrule query` calls.

```bash
ferrule query "mysql://root:pass@127.0.0.1:3306/mydb" "SELECT * FROM users;"
```

If you hit `caching_sha2_password` errors, see
[Troubleshooting](troubleshooting.md#mysql-caching_sha2_password-errors-with-old-clients).

## MSSQL

- Pure Rust via `tiberius` (TDS protocol — Microsoft's wire format).
- Supports SQL Authentication out of the box; Windows Authentication
  / Kerberos depends on platform support in `tiberius`.
- **Multi-statement batches supported.**
- `DATETIMEOFFSET` → `Value::DateTimeTz`; `BIT` → `Value::Bool`;
  `UNIQUEIDENTIFIER` → `Value::Uuid`.
- No native JSON type — store JSON in `NVARCHAR(MAX)` and ferrule
  returns it as either `Value::Json` (if it parses) or
  `Value::String`.

```bash
ferrule query "mssql://sa:pass@host/db?trustServerCertificate=true" "SELECT 1;"
```

The `trustServerCertificate=true` query parameter accepts a
self-signed cert *only* — that's narrower than the global
`--insecure` flag and the right choice for Docker test images that
ship a self-signed cert.

## SQLite

- Statically linked via `rusqlite` with the `bundled` feature; no
  runtime library required.
- File-based, in-memory (`sqlite::memory:`), or shared-cache
  (`sqlite::memory:?cache=shared`) variants supported.
- Single-statement only at the ferrule layer; multi-statement
  scripts run via SQLite's own `exec` from the driver, but ferrule
  doesn't surface multi-statement batches.
- Schema introspection uses `pragma_table_info` and `sqlite_schema`
  — see [Schema Introspection](schema.md).

```bash
ferrule query "sqlite::memory:" "SELECT 1;"
ferrule query "sqlite:///tmp/mydb.sqlite3" "SELECT * FROM users;"
```

The bundled SQLite version is whatever `rusqlite` ships with at
build time — usually a few minor versions behind the latest.

## Oracle

Opt-in only. Compiled in with `cargo build --features oracle`. At
runtime, the `oracle` crate dynamically loads `libclntsh.so` (Linux
/ macOS) / `oci.dll` (Windows) from Oracle Instant Client. Without
Instant Client present, the first connection fails with a ferrule
diagnostic:

```text
ferrule::connection
  × Oracle Instant Client (libclntsh.so) not found.
```

```bash
cargo install ferrule --features oracle
export LD_LIBRARY_PATH="$HOME/opt/oracle/instantclient_23_26:$LD_LIBRARY_PATH"

ferrule query "oracle://user:pass@host:1521/service" "SELECT * FROM dual;"
```

Schema notes:

- No native `BOOLEAN` until 23c — most schemas use `NUMBER(1)`.
- No native `UUID` — use `RAW(16) DEFAULT SYS_GUID()`.
- JSON is `CLOB` with an `IS JSON` check constraint (12c+).

Setup details, including the `libaio` symlink workaround for Ubuntu
24.04+, live in [Troubleshooting](troubleshooting.md#oracle-libclntshso-not-found).

## TLS posture summary

| Backend | TLS by default? | Requires it? | Self-signed dev cert |
|---|---|---|---|
| PostgreSQL | Negotiated; client decides | `?sslmode=require` (encrypt) / `verify-full` (full verify) | `?sslmode=require` + `--insecure` |
| MySQL | Off unless server demands; rustls | server-side `require_secure_transport` | `--insecure` |
| MSSQL | Always negotiated; cert often self-signed | (default) | `?trustServerCertificate=true` *or* `--insecure` |
| SQLite | N/A — local file or memory | — | — |
| Oracle | Server-configured (TNS) | TNS listener config | TNS-side, not URL |

`--insecure` disables both certificate-chain verification *and*
hostname verification globally for the call. The narrower MSSQL
query parameter is preferred where it applies.

## Multi-statement support

| Backend | Batch via `;` | Notes |
|---|---|---|
| PostgreSQL | ✅ | First-class; result sets and DML row counts both reported |
| MSSQL | ✅ | Via TDS row-set framing |
| MySQL | ❌ | Driver supports it; ferrule does not surface it. Issue separate calls |
| SQLite | ❌ | Use a script via `--file` if needed; one statement per `query` call |
| Oracle | ❌ | Use anonymous PL/SQL blocks for multi-statement work |

When using batches, remember `--limit` and `--offset` are not
allowed — see [Querying](querying.md#paging) for the workaround.

## Type mapping

Ferrule maps every backend's native types to a single `Value` enum.
This table is the canonical home — it's referenced from
[Concepts](concepts.md), [Querying](querying.md), and the rest of
the docs.

| Ferrule `Value` | Postgres | MySQL | MSSQL | SQLite | Oracle |
|---|---|---|---|---|---|
| `Bool` | `BOOLEAN` | `BOOLEAN` (TINYINT(1)) | `BIT` | `INTEGER` | `NUMBER(1)` |
| `Int64` | `BIGINT` | `BIGINT` | `BIGINT` | `INTEGER` | `NUMBER` |
| `Float64` | `DOUBLE PRECISION` | `DOUBLE` | `FLOAT` | `REAL` | `BINARY_FLOAT` |
| `Decimal` | `NUMERIC` | `DECIMAL` | `DECIMAL` | `NUMERIC` | `NUMBER` |
| `String` | `TEXT` / `VARCHAR` | `VARCHAR` | `NVARCHAR` | `TEXT` | `VARCHAR2` |
| `Bytes` | `BYTEA` | `BLOB` | `VARBINARY` | `BLOB` | `RAW` |
| `Date` | `DATE` | `DATE` | `DATE` | `TEXT` (ISO 8601) | `DATE` |
| `DateTime` | `TIMESTAMP` | `DATETIME` | `DATETIME2` | `TEXT` (ISO 8601) | `TIMESTAMP` |
| `DateTimeTz` | `TIMESTAMPTZ` | `TIMESTAMP` | `DATETIMEOFFSET` | `TEXT` (ISO 8601) | `TIMESTAMP WITH TIME ZONE` |
| `Json` | `JSONB` / `JSON` | `JSON` | `NVARCHAR(MAX)` (JSON-shaped) | `TEXT` | `CLOB` (with `IS JSON`) |
| `Uuid` | `UUID` | `CHAR(36)` | `UNIQUEIDENTIFIER` | `TEXT` | `RAW(16)` |
| `Array` | `T[]` (any) | (n/a — pass JSON) | (n/a — pass JSON) | (n/a) | (n/a — pass JSON) |

Types that don't have a clean `Value` slot (Postgres ranges, custom
composite types, MySQL `SET`, etc.) fall back to `Value::String`
with the driver's native rendering.
