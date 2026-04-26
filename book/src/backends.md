# Backends

## PostgreSQL

- Pure Rust via `tokio-postgres` + `rustls` (no OpenSSL)
- Supports SSL modes: `prefer`, `require`, `disable`
- Multi-statement batches supported
- UUID, JSONB, arrays mapped to Ferrule `Value` types

```bash
ferrule query "postgres://user:pass@host/db?sslmode=require" "SELECT 1;"
```

## MySQL

- Pure Rust via `mysql_async`
- Works with MySQL 5.7+ / MariaDB 10.3+
- JSON column type mapped to `Value::Json`

```bash
ferrule query "mysql://root:pass@127.0.0.1:3306/mydb" "SELECT * FROM users;"
```

## MSSQL

- Pure Rust via `tiberius`
- Supports Windows Authentication (Kerberos) and SQL Authentication
- `DATETIMEOFFSET` mapped to `Value::DateTimeTz`
- Bit columns mapped to `Value::Bool`

```bash
ferrule query "mssql://sa:pass@host/db?trustServerCertificate=true" "SELECT 1;"
```

## SQLite

- Statically linked via `rusqlite` with `bundled` feature
- No runtime library required
- In-memory databases via `sqlite::memory:`

```bash
ferrule query "sqlite::memory:" "SELECT 1;"
ferrule query "sqlite:///tmp/mydb.sqlite3" "SELECT * FROM users;"
```

## Oracle

Optional feature requiring Oracle Instant Client on the host at runtime.

```bash
cargo build --release --features oracle

ferrule query "oracle://user:pass@host:1521/service" "SELECT * FROM dual;"
```

If Instant Client is missing, Ferrule emits a diagnostic with download links.

## Type Mapping Summary

| Ferrule `Value` | Postgres | MySQL | MSSQL | SQLite | Oracle |
|----------------|----------|-------|-------|--------|--------|
| `Bool` | `BOOLEAN` | `BOOLEAN` | `BIT` | `INTEGER` | `NUMBER(1)` |
| `Int64` | `BIGINT` | `BIGINT` | `BIGINT` | `INTEGER` | `NUMBER` |
| `Float64` | `DOUBLE` | `DOUBLE` | `FLOAT` | `REAL` | `BINARY_FLOAT` |
| `Decimal` | `NUMERIC` | `DECIMAL` | `DECIMAL` | `NUMERIC` | `NUMBER` |
| `String` | `TEXT` | `VARCHAR` | `NVARCHAR` | `TEXT` | `VARCHAR2` |
| `Bytes` | `BYTEA` | `BLOB` | `VARBINARY` | `BLOB` | `RAW` |
| `Date` | `DATE` | `DATE` | `DATE` | `TEXT` | `DATE` |
| `DateTime` | `TIMESTAMP` | `DATETIME` | `DATETIME2` | `TEXT` | `TIMESTAMP` |
| `DateTimeTz` | `TIMESTAMPTZ` | `TIMESTAMP` | `DATETIMEOFFSET` | `TEXT` | `TIMESTAMP WITH TIME ZONE` |
| `Json` | `JSONB` | `JSON` | `NVARCHAR(MAX)` | `TEXT` | `CLOB` |
| `Uuid` | `UUID` | `CHAR(36)` | `UNIQUEIDENTIFIER` | `TEXT` | `RAW(16)` |
