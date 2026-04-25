# Plan: FE-002 — MSSQL Backend Driver

**Target:** `ferrule-core/src/backends/mssql.rs`
**Crate:** `ferrule-core`
**Feature:** `mssql` (default)
**Estimate:** Medium
**Reference Implementation:** `ferrule-core/src/backends/mysql.rs` (FE-001)

---

## Why This Matters

MSSQL is a key differentiator for ferrule vs. rainfrog (which lacks MSSQL). It uses `tiberius` — a pure Rust TDS protocol implementation — so no Microsoft client libraries are required.

---

## Architecture

The MSSQL backend is a `ferrule-core` crate module gated by `#[cfg(feature = "mssql")]`.

```
ferrule-core/src/backends/mssql.rs
├─ MssqlConnection { client: tiberius::Client }
├─ #[async_trait] impl Connection for MssqlConnection
│   ├─ execute(sql) → ExecutionSummary
│   ├─ query(sql) → QueryResult
│   ├─ execute_multi(sql) → Vec<StatementResult>
│   ├─ ping() → Result<(), CoreError>
│   ├─ list_tables(schema?) → Vec<String>
│   └─ describe_table(schema?, table) → QueryResult
├─ connect(url, opts) → MssqlConnection
└─ mssql_to_value() + type tests
```

---

## Implementation Checklist

### 1. Connection Setup

```rust
pub async fn connect(url: &DatabaseUrl, opts: &ConnectOptions) -> Result<MssqlConnection, CoreError>
```

- Build `tiberius::Config` from `DatabaseUrl`:
  - `host` (default localhost)
  - `port` (default 1433)
  - `database`
  - `authentication` → `AuthMethod::sql_server(user, password)`
- Handle URL params:
  - `encrypt=true/false` (default true)
  - `trust_server_certificate=true/false`
  - Map `--insecure` (`opts.insecure`) to TLS config
- Connect via `tokio::net::TcpStream` + `tokio_util::compat::TokioAsyncWriteCompatExt::compat()`
- Spawn connection background task

### 2. Connection Trait Methods

| Method | tiberius API | Notes |
|--------|-------------|-------|
| `execute` | `client.execute(sql, &[]).await` | `rows_affected()` from result |
| `query` | `client.query(sql, &[]).await` | Iterate rows, map columns |
| `execute_multi` | Default trait fallback | OR iterate `next_resultset()` |
| `ping` | `SELECT 1` | |
| `list_tables` | `information_schema.tables` | Default schema = `dbo` |
| `describe_table` | `information_schema.columns` | Match Postgres output format |

### 3. Type Mapping (CRITICAL)

| SQL Server Type | ferrule `Value` |
|---|---|
| `BIT` | `Bool` |
| `INT`, `BIGINT`, `SMALLINT`, `TINYINT` | `Int64` |
| `FLOAT`, `REAL` | `Float64` |
| `DECIMAL`, `NUMERIC`, `MONEY`, `SMALLMONEY` | `Decimal` (string) |
| `NVARCHAR`, `VARCHAR`, `NCHAR`, `CHAR`, `TEXT`, `NTEXT` | `String` |
| `VARBINARY`, `BINARY`, `IMAGE` | `Bytes` |
| `DATE` | `Date` |
| `TIME` | `Time` |
| `DATETIME`, `DATETIME2`, `SMALLDATETIME` | `DateTime` |
| `DATETIMEOFFSET` | `DateTimeTz` |
| `UNIQUEIDENTIFIER` | `Uuid` (string) |
| Others | `String` (fallback) |

**Warning:** tiberius `chrono` support is limited. Dates may need manual conversion through string → parse.

### 4. Integration Tests (Docker)

```bash
docker run -d --name ferrule-mssql-test \
  -e "ACCEPT_EULA=Y" \
  -e "SA_PASSWORD=Ferrule123!" \
  -e "MSSQL_PID=Express" \
  -p 127.0.0.1:11433:1433 \
  mcr.microsoft.com/mssql/server:2022-latest
```

Wait 30s for initialization, then seed a `test_users` table matching the MySQL test schema.

### 5. Verification

- [ ] `cargo build --workspace --features mssql` ✅
- [ ] `cargo clippy --workspace --features mssql` ✅
- [ ] `cargo test --workspace --features mssql` ✅ (6 tests)
- [ ] No `todo!()` remaining

---

## Cargo.toml Already Has

```toml
mssql = ["dep:tiberius", "dep:tokio-util"]

[dependencies]
tiberius = { version = "0.12", optional = true }
tokio-util = { version = "0.7", features = ["compat"], optional = true }
```

---

## Key Differences from MySQL Backend

| Aspect | MySQL | MSSQL |
|--------|-------|-------|
| Connection | `mysql_async::Conn::new(opts)` | `Client::connect(config, tcp.compat())` |
| TLS | `SslOpts` | Native via `encrypt=` + `trust_server_certificate=` |
| Default schema | Current database | `dbo` |
| Dates | Native `chrono` types | May need manual string → parse |
| Multi-resultset | Simple query | `next_resultset()` |
| Sync/Async | Pure async | Pure async |

---

## Risks & Gotchas

1. **Tiberius `chrono` support** — Verify tiberius 0.12 maps `DATETIME` → `chrono::NaiveDateTime`. If not, parse via `str::parse`.
2. **TLS errors on first connection** — MSSQL Azure/encrypted instances may fail with cryptic TLS errors. The `encrypt=false` param should be honored.
3. **`affected_rows()` accuracy** — Some tiberius versions return `u64(0)` for DDL. Handle gracefully.

---

## Related Files

- `ferrule-core/src/backends/mod.rs` — Already exports `#[cfg(feature = "mssql")] pub mod mssql;`
- `ferrule-core/src/connection.rs` — Connection trait (unchanged)
- `ferrule-core/src/value.rs` — Value enum (unchanged)
- `ferrule-core/src/url.rs` — DatabaseUrl API (unchanged)
- `ferrule-core/src/error.rs` — CoreError variants (unchanged)

---

*Plan generated after FE-001 (MySQL) completion. MySQL is the primary reference implementation.*
