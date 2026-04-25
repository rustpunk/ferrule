# Plan: FE-003 — Oracle Backend Driver

**Target:** `ferrule-core/src/backends/oracle.rs`
**Crate:** `ferrule-core`
**Feature:** `oracle` (**OPT-IN ONLY** — not in default features)
**Estimate:** Medium
**Reference Implementation:** `ferrule-core/src/backends/mysql.rs` (FE-001)

---

## Why This Matters

Oracle is opt-in because it requires users to install Oracle Instant Client separately. We **cannot redistribute** Oracle's client libraries. However, the driver itself compiles fine — ODPI-C dynamically loads `libclntsh` on first connection attempt. Non-Oracle connections work normally.

---

## Architecture

```
ferrule-core/src/backends/oracle.rs
├─ OracleConnection { conn: oracle::Connection }
├─ #[async_trait] impl Connection for OracleConnection
│   ├─ execute(sql) → ExecutionSummary
│   ├─ query(sql) → QueryResult
│   ├─ execute_multi(sql) → Vec<StatementResult>
│   ├─ ping() → Result<(), CoreError>
│   ├─ list_tables(schema?) → Vec<String>
│   └─ describe_table(schema?, table) → QueryResult
├─ connect(url, opts) → OracleConnection  (uses spawn_blocking)
└─ oracle_to_value() + type tests
```

**Important:** The `oracle` crate is **synchronous** (blocking C calls). Every interaction must be wrapped in `tokio::task::spawn_blocking`.

---

## Implementation Checklist

### 1. Connection Setup

```rust
pub async fn connect(url: &DatabaseUrl, _opts: &ConnectOptions) -> Result<OracleConnection, CoreError>
```

- Parse URL components from `DatabaseUrl`:
  - `host` (default localhost)
  - `port` (default 1521)
  - `username`
  - `password` (SecretString → `ExposeSecret`)
  - `database` → Oracle service name
- Build connection string: `//host:port/SERVICE_NAME`
- Connect inside `tokio::task::spawn_blocking`:
  ```rust
  oracle::Connection::connect(username, password, connect_string)
  ```
- **ODPI-C lazy loading:** If `libclntsh` is missing, `oracle::Error` is returned. Check for "DPI-1047" or "libclntsh" and emit a **helpful diagnostic**:
  > "Oracle Instant Client not found. Install it from https://www.oracle.com/database/technologies/instant-client/downloads.html and ensure it is on your `LD_LIBRARY_PATH` (Linux), `DYLD_LIBRARY_PATH` (macOS), or `PATH` (Windows)."
- Map connection errors to `CoreError::ConnectionFailed`

### 2. Connection Trait Methods

| Method | oracle crate API | Notes |
|--------|-----------------|-------|
| `execute` | `conn.execute(sql, &[]).row_count()` | Inside `spawn_blocking` |
| `query` | Prepare → execute → fetch rows | Map column types dynamically |
| `execute_multi` | Default trait fallback | Acceptable for Wave 1 |
| `ping` | `SELECT 1 FROM DUAL` | |
| `list_tables` | `SELECT table_name FROM user_tables` | `schema` → Oracle `owner` |
| `describe_table` | `all_tab_columns` | Match Postgres output format |

### 3. Type Mapping (CRITICAL)

| Oracle Type | ferrule `Value` |
|---|---|
| `NUMBER` (scale=0) | `Int64` |
| `NUMBER` (scale>0) | `Decimal` (string) |
| `VARCHAR2`, `NVARCHAR2`, `CHAR`, `NCHAR`, `CLOB`, `NCLOB` | `String` |
| `BLOB`, `RAW`, `LONG RAW` | `Bytes` |
| `DATE` | `DateTime` (Oracle DATE has time) |
| `TIMESTAMP` | `DateTime` |
| `TIMESTAMP WITH TIME ZONE` | `DateTimeTz` |
| `BOOLEAN` | `Bool` (rare in Oracle) |
| `NULL` | `Null` |

**Oracle NULL Behavior:** Oracle treats empty strings (`''`) as NULL. Always use `Option<T>` for `row.get()`.

**Type Detection:** The `oracle` crate exposes `Statement::column_info()` (or similar). Check `ColumnInfo::oracle_type()` to branch on the Rust type to fetch. If type detection is unreliable, fetch as string and parse.

### 4. Integration Tests

Oracle is difficult to containerize. Use environment-gated skip pattern:

```rust
#[tokio::test]
async fn test_oracle_connect() {
    let url = match std::env::var("ORACLE_TEST_URL") {
        Ok(u) => u,
        Err(_) => {
            eprintln!("Skipping Oracle test: ORACLE_TEST_URL not set");
            return; // skip
        }
    };
    // ... test
}
```

Minimum tests:
- `test_oracle_connect`
- `test_oracle_ping`
- `test_oracle_query`
- `test_oracle_execute`
- `test_oracle_list_tables`
- `test_oracle_describe_table`
- `test_oracle_type_mapping` (if ORACLE_TEST_URL present)
- `test_oracle_missing_client_error` — verify graceful error if Instant Client is absent

### 5. Verification

- [ ] `cargo build --workspace --features oracle` ✅ (compiles even without Instant Client)
- [ ] `cargo clippy --workspace --features oracle` ✅
- [ ] `cargo test --workspace --features oracle` ✅ (tests skip gracefully)
- [ ] No `todo!()` remaining

---

## Cargo.toml Already Has

```toml
oracle = ["dep:oracle"]

[dependencies]
oracle = { version = "0.6", optional = true }
```

---

## Key Differences from MySQL Backend

| Aspect | MySQL | Oracle |
|--------|-------|--------|
| Connection | `mysql_async::Conn::new(opts)` | `oracle::Connection::connect()` in `spawn_blocking` |
| Async/sync | Pure async | Synchronous wrapper |
| Default schema | Current db | `user_tables` |
| Dates | Native chrono | Native chrono (verify oracle crate support) |
| Multi-resultset | Default fallback is OK | Default fallback is OK |
| NULL handling | Standard | Empty string = NULL |
| Error on missing deps | Connection-time error | DPI-1047 / libclntsh not found |
| Feature status | Default | Opt-in only |

---

## Risks & Gotchas

1. **ODPI-C lazy loading** — The crate compiles fine. Errors only appear at runtime on first connection. The error message MUST be helpful.
2. **`spawn_blocking` everywhere** — Forgetting to wrap in `spawn_blocking` will block the tokio runtime (bad for CLI performance).
3. **Oracle Instant Client path** — On Linux/macOS, `LD_LIBRARY_PATH` / `DYLD_LIBRARY_PATH` must be set. On Windows, `PATH`. The diagnostic should mention all three.
4. **Number precision** — Oracle `NUMBER` can be huge. `rust_decimal::Decimal` handles it, but verify the `oracle` crate supports it.

---

## Related Files

- `ferrule-core/src/backends/mod.rs` — Already exports `#[cfg(feature = "oracle")] pub mod oracle;`
- `ferrule-core/src/connection.rs` — Connection trait (unchanged)
- `ferrule-core/src/value.rs` — Value enum (unchanged)
- `ferrule-core/src/url.rs` — DatabaseUrl API (unchanged)
- `ferrule-core/src/error.rs` — CoreError variants (unchanged)

---

*Plan generated after FE-001 (MySQL) completion. MySQL is the primary reference implementation.*
