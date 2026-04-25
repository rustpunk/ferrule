# `ferrule` — A Rust-native Database Query CLI

> The collar that joins you to your data.

`ferrule` is a single, fast, statically linked CLI for querying relational databases. Register named connections, run SQL from the command line or interactively, and get beautifully formatted results — with zero runtime dependencies for Postgres, MySQL, MSSQL, and SQLite. Oracle support is available as an opt-in feature for users who can install the Oracle Instant Client.

Think **psql + sqlcmd + mysql +Oracle SQL*Plus, but one binary, one interface, and Rust all the way down.**

---

## Why This Exists

The current landscape is fragmented, heavy, or missing:

| Tool | Language | Databases | Limitation |
|------|----------|-----------|------------|
| **psql** | C | Postgres only | Requires libpq at runtime |
| **mysql** | C | MySQL only | Requires client libraries |
| **sqlcmd** | .NET / C | MSSQL only | Windows-centric, bloated |
| **SQL*Plus** | C | Oracle only | Ancient UX, painful output |
| **rainfrog** | Rust | Postgres, MySQL, Oracle | No MSSQL; TUI-only, no script mode |
| **usql** | Go | Universal | Excellent coverage, but Go; slower startup, large binary |
| **sqlx-cli** | Rust | Postgres, MySQL, SQLite | Migration tool, not a query CLI |
| **bacon** | Rust | SQLite only | Single-database |
| **ferrule** | **Rust** | **Postgres, MySQL, MSSQL, SQLite + opt-in Oracle** | **Single static binary, unified UX** |

No Rust tool gives you a single static binary that speaks Postgres, MySQL, MSSQL, and SQLite out of the box, with a consistent CLI interface across all of them.

---

## Core Architecture

```
┌─────────────────────────────────────────────────────────────┐
│                     ferrule CLI                             │
│                                                             │
│  ┌──────────┐   ┌──────────┐   ┌────────────────────────┐  │
│  │ Config   │   │ Registry │   │ Shell / Completion     │  │
│  │ Manager  │   │ (conns)  │   │ Engine                 │  │
│  └────┬─────┘   └────┬─────┘   └────────────────────────┘  │
│       │              │                                      │
│  ┌────▼──────────────▼──────────────────────────────────┐    │
│  │           Connection Engine                         │    │
│  │  ┌────────────┐  ┌────────────┐  ┌─────────────┐ │    │
│  │  │ Resolver   │  │ Pool       │  │ Backend     │ │    │
│  │  │ (config)   │→ │ (per conn) │→ │ Router      │ │    │
│  │  └────────────┘  └────────────┘  └──────┬──────┘ │    │
│  └─────────────────────────────────────────┼──────────┘    │
│                                            │               │
│  ┌─────────────────────────────────────────▼──────────┐    │
│  │           Backend Drivers (feature-gated)          │    │
│  │  ┌─────────┐ ┌─────────┐ ┌─────────┐ ┌─────────┐│    │
│  │  │ Postgres│ │ MySQL   │ │ MSSQL   │ │ SQLite  ││    │
│  │  │ (native)│ │ (native)│ │ (native)│ │ (native)││    │
│  │  └─────────┘ └─────────┘ └─────────┘ └─────────┘│    │
│  │  ┌─────────┐                                          │
│  │  │ Oracle  │  ← opt-in, requires Instant Client       │
│  │  │ (ODPI-C)│                                          │
│  │  └─────────┘                                          │
│  └──────────────────────────────────────────────────────┘    │
│                                                             │
│  ┌──────────────────────────────────────────────────────┐   │
│  │           Query Engine                               │   │
│  │  ┌──────────┐ ┌──────────┐ ┌──────────────────────┐ │   │
│  │  │ Parser   │ │ Executor │ │ Result Formatter     │ │   │
│  │  │ (SQL)    │ │ (async)  │ │ (table/JSON/CSV/YAML)│ │   │
│  │  └──────────┘ └──────────┘ └──────────────────────┘ │   │
│  └──────────────────────────────────────────────────────┘   │
└─────────────────────────────────────────────────────────────┘
```

---

## Crate Dependencies

```toml
[dependencies]
# CLI
clap = { version = "4", features = ["string", "wrap_help"] }

# Async runtime
tokio = { version = "1", features = ["rt", "macros", "net", "time", "io-util"] }

# Serialization
serde = { version = "1", features = ["derive"] }
serde_json = "1"

# Output
tabled = "0.20"
crossterm = "0.29"
is-terminal = "0.4"

# Config / Connection registry
dirs = "5"
toml = "0.8"
url = "2"

# Diagnostics
thiserror = "2"
miette = { version = "7", features = ["fancy"] }

# Postgres — pure Rust
tokio-postgres = { version = "0.7", optional = true }
rustls = { version = "0.23", optional = true }
tokio-rustls = { version = "0.26", optional = true }
webpki-roots = { version = "0.26", optional = true }

# MySQL — pure Rust
mysql_async = { version = "0.34", optional = true }

# MSSQL — pure Rust
tiberius = { version = "0.12", optional = true }

# SQLite — pure Rust (via sqlx or rusqlite)
sqlx = { version = "0.8", features = ["runtime-tokio", "sqlite"], optional = true }

# Oracle — wraps ODPI-C (opt-in only)
oracle = { version = "0.6", optional = true }

# Utilities
indexmap = { version = "2", features = ["serde"] }
secrecy = "0.10"
chrono = { version = "0.4", features = ["serde"] }
decimal-rs = { version = "0.1", optional = true }
rust_decimal = { version = "1", optional = true }

[features]
default = ["postgres", "mysql", "mssql", "sqlite"]
postgres = ["dep:tokio-postgres", "dep:rustls", "dep:tokio-rustls", "dep:webpki-roots"]
mysql = ["dep:mysql_async"]
mssql = ["dep:tiberius"]
sqlite = ["dep:sqlx"]
oracle = ["dep:oracle"]
all = ["postgres", "mysql", "mssql", "sqlite", "oracle"]
```

### Why these choices

- **`tokio-postgres` + `rustls`**: Pure Rust TLS via `rustls` with `webpki-roots` — zero system OpenSSL dependency. Statically linked, cross-compiles cleanly.
- **`mysql_async`**: Pure Rust MySQL driver. Async-native, no libmysqlclient runtime requirement.
- **`tiberius`**: Pure Rust TDS (MSSQL wire protocol) implementation. The gap rainfrog leaves open. Zero Microsoft client libraries required.
- **`sqlx` (SQLite only)**: SQLite is the one backend where we deliberately reuse `sqlx` because `libsqlite3` is universally available and the `sqlx` SQLite driver is mature. Keeps our driver surface small.
- **`oracle` (kubo/rust-oracle)**: Wraps ODPI-C, which is vendored and compiled into the binary. ODPI-C dynamically loads `libclntsh` from Oracle Instant Client at runtime — only when an Oracle connection is first attempted. Users must install Instant Client separately. This is legally required; we cannot redistribute Oracle's client.
- **`secrecy`**: All passwords stored in `SecretString` — zeroizes on drop, redacts in Debug.
- **`tabled`**: Beautiful table formatting for query results. Handles wide results gracefully.
- **`chrono`**: Common datetime type across all backends. Each driver maps its own datetime type to/from `chrono::NaiveDateTime` / `DateTime<Utc>` in the unified result set.
- **`IndexMap`**: Preserves column order from queries — critical for predictable output.
- **`miette` + `thiserror`**: `thiserror` for typed library errors, `miette` for rich diagnostics in the binary crate.
- **`tokio` current_thread**: CLI tools do sequential work. Lower latency, smaller memory, no `Send` bounds required.

---

## Feature Map

### Wave 1 — Core Query Flow (shippable MVP)

| Feature | Description |
|---------|-------------|
| `ferrule conn add <name> <url>` | Register a named connection from a URL |
| `ferrule conn list` | List registered connections |
| `ferrule conn remove <name>` | Remove a connection |
| `ferrule conn test <name>` | Test connectivity |
| `ferrule query <conn> "<sql>"` | Execute a single SQL statement |
| `ferrule query <conn> -f file.sql` | Execute SQL from file |
| `ferrule query <conn> -` | Execute SQL from stdin |
| `--format table` | Default TTY: formatted table output |
| `--format json` | JSON array of objects |
| `--format csv` | Comma-separated values |
| `--format yaml` | YAML output |
| `--format raw` | Tab-separated, unquoted |
| `--output <file>` / `-o` | Redirect result to file |
| `--limit <n>` / `-n` | Limit rows returned |
| `--timeout <secs>` | Connection/query timeout (default: 30s) |
| `--dry-run` | Print parsed SQL and connection info without executing |
| `--verbose` / `-v` | Show connection setup, query plan if available, timing |
| `--timing` | Print execution time to stderr |
| `ferrule tables <conn>` | List all tables (schema-aware) |
| `ferrule describe <conn> <table>` | Show column definitions for a table |
| `ferrule schema <conn>` | List schemas/databases |
| Transaction support | `--begin`, `--commit`, `--rollback` flags per-session (script mode) |
| Multi-statement execution | Split `;`-delimited statements, execute sequentially |
| Exit code convention | 0=success, 1=usage, 2=connection, 3=query error, 4=no rows (optional) |

### Wave 1.5 — Performance & QoL

| Feature | Description |
|---------|-------------|
| Connection pooling | Reuse TCP/TLS handshakes across `ferrule` invocations via a Unix socket daemon |
| Paging | Auto-paginate large results (configurable page size) |
| Result caching | Cache result metadata for repeated identical queries |
| Config profiles | `[profile.staging]`, `[profile.production]` in config |
| Environment variable interpolation | `${DB_PASSWORD}` in connection URLs |
| `.ferrule.toml` local config | Per-directory connection defaults |
| History | SQLite log of recent queries with timing |

### Wave 2 — Interactive & Power

| Feature | Description |
|---------|-------------|
| `ferrule repl <conn>` | Interactive REPL with readline, syntax highlighting, autocomplete |
| `ferrule shell <conn>` | Alias for repl |
| Query bookmarks | Save named queries: `ferrule bookmark add <name> "<sql>"` |
| Parameterized queries | `--param key=value` for safe parameterized execution |
| Explain plan | `ferrule explain <conn> "<sql>"` — formatted query plan |
| Export/import | `ferrule dump <conn> <table>` / `ferrule load <conn> <table>` for CSV/JSON |
| Watch mode | `ferrule query <conn> "<sql>" --watch <secs>` — re-run periodically |
| Parallel multi-DB | `ferrule query --on db1 --on db2 "<sql>"` — fan out |

### Wave 3 — Advanced

| Feature | Description |
|---------|-------------|
| Schema diff | Compare table structures between two connections |
| Migration runner | Basic migration tracking (lightweight alternative to sqlx-cli) |
| Result filtering | `--filter '.name'` (JMESPath-like) for JSON output |
| Row-level security awareness | Show RLS policies in `describe` |
| SSH tunnel support | `ssh://user@host/db` URL scheme |
| Kubernetes port-forward | `k8s://namespace/pod/db` URL scheme |
| Plugin system | WASM plugins for custom formatters or auth |
| Daemon mode | Background connection pool server; CLI becomes thin client |

---

## Connection URLs

Ferrule uses standard URL schemes with backend-specific query parameters:

```
postgres://user:pass@host:5432/dbname?sslmode=require
mysql://user:pass@host:3306/dbname
mssql://user:pass@host:1433/dbname?encrypt=true
sqlite:///absolute/path/to/db.sqlite
sqlite:file:relative.sqlite
oracle://user:pass@host:1521/SERVICE_NAME
```

URL parsing is unified: every backend receives a `DatabaseUrl` struct with `scheme`, `username`, `password` (`SecretString`), `host`, `port`, `database`, and `params: IndexMap<String, String>`.

### Backend-specific parameters

| Backend | Parameter | Meaning |
|---------|-----------|---------|
| Postgres | `sslmode` | disable, prefer, require, verify-ca, verify-full |
| Postgres | `sslrootcert` | Path to CA certificate |
| MySQL | `ssl-mode` | DISABLED, PREFERRED, REQUIRED, VERIFY_CA, VERIFY_IDENTITY |
| MSSQL | `encrypt` | true/false (default true) |
| MSSQL | `trust_server_certificate` | true/false |
| SQLite | `mode` | rwc, ro, memory |
| Oracle | `sid` | Alternative to SERVICE_NAME |

---

## Key Design Decisions

### 1. Unified Result Set

Every backend returns rows through a common abstraction:

```rust
pub struct Row {
    pub columns: Vec<ColumnInfo>,
    pub values: Vec<Value>,
}

pub struct ColumnInfo {
    pub name: String,
    pub type_hint: TypeHint,
    pub nullable: bool,
}

pub enum Value {
    Null,
    Bool(bool),
    Int64(i64),
    Float64(f64),
    Decimal(Decimal),      // rust_decimal for precision
    String(String),
    Bytes(Vec<u8>),
    Date(chrono::NaiveDate),
    Time(chrono::NaiveTime),
    DateTime(chrono::NaiveDateTime),
    DateTimeTz(chrono::DateTime<chrono::Utc>),
    Json(serde_json::Value),
    Uuid(uuid::Uuid),
    Array(Vec<Value>),     // Array support (Postgres native; others emulated)
}
```

Each backend driver maps its native types to this enum. The formatter layer never sees driver-specific types.

**Type mapping strategy:**

| Backend | Maps to |
|---------|---------|
| Postgres | `TEXT`/`VARCHAR` → `String`; `INT4`/`INT8` → `Int64`; `NUMERIC` → `Decimal`; `TIMESTAMP` → `DateTime`; `TIMESTAMPTZ` → `DateTimeTz`; `JSON`/`JSONB` → `Json`; arrays → `Array`; `BYTEA` → `Bytes` |
| MySQL | `VARCHAR` → `String`; `INT`/`BIGINT` → `Int64`; `DECIMAL` → `Decimal`; `DATETIME` → `DateTime`; `JSON` → `Json`; `BLOB` → `Bytes` |
| MSSQL | `NVARCHAR`/`VARCHAR` → `String`; `INT`/`BIGINT` → `Int64`; `DECIMAL`/`NUMERIC`/`MONEY` → `Decimal`; `DATETIME2` → `DateTime`; `DATETIMEOFFSET` → `DateTimeTz`; `VARBINARY` → `Bytes` |
| SQLite | `TEXT` → `String`; `INTEGER` → `Int64`; `REAL` → `Float64`; `BLOB` → `Bytes` |
| Oracle | `VARCHAR2` → `String`; `NUMBER` → `Decimal` (or `Int64` if scale=0); `DATE` → `DateTime`; `TIMESTAMP WITH TIME ZONE` → `DateTimeTz`; `CLOB`/`BLOB` → `String`/`Bytes`; `RAW` → `Bytes` |

### 2. Feature-Gated Backend Compilation

```toml
[features]
default = ["postgres", "mysql", "mssql", "sqlite"]
all = ["postgres", "mysql", "mssql", "sqlite", "oracle"]
oracle = ["dep:oracle"]
```

The binary crate uses a backend router:

```rust
pub enum Backend {
    #[cfg(feature = "postgres")]
    Postgres,
    #[cfg(feature = "mysql")]
    MySql,
    #[cfg(feature = "mssql")]
    MsSql,
    #[cfg(feature = "sqlite")]
    Sqlite,
    #[cfg(feature = "oracle")]
    Oracle,
}

impl Backend {
    pub fn from_scheme(scheme: &str) -> Option<Self> { ... }
    pub async fn connect(&self, url: &DatabaseUrl) -> Result<Box<dyn Connection>, Error> { ... }
}
```

This keeps the default binary small (~15–25 MB) and Oracle-free. Oracle users compile with `--features oracle`.

### 3. Dynamic Result Formatting

Output mode is selected at runtime via `--format`:

```rust
pub enum OutputFormat {
    Table,   // Default when TTY. Uses `tabled`.
    Json,    // JSON array of objects. One object per row.
    Csv,     // RFC 4180 CSV.
    Yaml,    // YAML array of objects.
    Raw,     // Tab-separated, unquoted. For piping to awk/cut.
}
```

**TTY detection** via `is-terminal`:
- Terminal + no `--format`: default to `Table`
- Pipe + no `--format`: default to `Json`
- Explicit `--format` always wins

**Table formatting:**
- Truncate very wide columns (>120 chars) with `…` unless `--no-truncate`
- Right-align numeric columns
- Handle `NULL` as italic grey `NULL`
- Wrap wide tables; don't clip columns unless `--fit-width`

**JSON formatting:**
- Pretty-print when TTY, compact when piped
- `NULL` values serialize as `null`
- Dates serialize as ISO 8601 strings
- Decimals serialize as strings to preserve precision

### 4. Named Connection Registry

Connections are stored in `$CONFIG_DIR/ferrule/connections.toml`:

```toml
[postgres-prod]
url = "postgres://user@host/db"
# Password resolved via env var or keyring; never stored plaintext

[mysql-staging]
url = "mysql://user@host/db"

[mssql-warehouse]
url = "mssql://user@host/db?encrypt=true"

[oracle-legacy]
url = "oracle://user@host/SERVICE"
```

Passwords are NEVER stored in `connections.toml`. Resolution stack:
1. `--password` CLI flag (overrides everything)
2. `FERRULE_<NAME>_PASSWORD` environment variable
3. OS keyring (`service=ferrule`, `user=<connection_name>`)
4. Prompt interactively (TTY only)

### 5. Multi-Statement Execution

When SQL contains multiple `;`-delimited statements:

```sql
SELECT 1;
SELECT 2;
```

Each statement is parsed, executed sequentially, and results are emitted with a header comment:

```
-- Result set 1 (1 row)
1
-- Result set 2 (1 row)
2
```

This is critical for MSSQL and Oracle stored procedure calls that return multiple result sets.

**DML/DDL handling**: Statements that don't return rows (`INSERT`, `UPDATE`, `DELETE`, `CREATE`, `ALTER`) print:
```
-- Rows affected: 42
```

### 6. Oracle Runtime Behavior

Built with `--features oracle`:

- At startup: no error. ODPI-C lazily loads `libclntsh` on first Oracle connection attempt.
- On first Oracle connection: ODPI-C calls `dlopen("libclntsh.so")` (or `.dylib` / `.dll`).
- If Instant Client is missing: DPI-1047 error propagated to user with a helpful diagnostic:
  > `Oracle Instant Client not found. Install it from https://www.oracle.com/database/technologies/instant-client/downloads.html and ensure it is on your LD_LIBRARY_PATH.`
- Non-Oracle connections continue to work normally.

---

## Quality Targets

| Metric | Target |
|--------|--------|
| Binary size (default features) | < 25 MB stripped |
| Binary size (all features) | < 35 MB stripped |
| Cold start latency | < 50 ms |
| Connection establishment (local) | < 100 ms |
| Query result streaming | Yes — don't buffer entire result set in memory |
| Max row streaming | Configurable (default: 10,000 before warning) |
| Cross-compilation | Linux, macOS, Windows via `cross` |

---

## Error Handling Strategy

| Error kind | Behavior |
|------------|----------|
| Connection refused | `miette` diagnostic with connection URL (password redacted) |
| Authentication failure | Suggest checking `FERRULE_<NAME>_PASSWORD` and keyring |
| DPI-1047 (Oracle) | Link to Oracle download page, show `LD_LIBRARY_PATH` help |
| Query syntax error | Show server error message, line hint if available |
| Timeout | Distinguish connection vs query timeout |
| SSL/TLS failure | Show certificate diagnostics, suggest `--insecure` (with warning) |

---

## Security Considerations

- **Passwords**: `secrecy::SecretString` everywhere. Command-line `--password` immediately wrapped. Never logged, never in Debug.
- **Connection URLs**: Password component redacted in all log/diagnostic output.
- **Query logging**: Optional (`--verbose` only). Password-containing connection strings stripped before logging.
- `--insecure` flag: Explicitly required to disable TLS verification. Warns on stderr.
- No query interpolation beyond prepared parameters (`--param`). Never concatenate user input into SQL.

---

## Comparison with Related Tools

| Dimension | ferrule | rainfrog | usql | psql |
|-----------|---------|----------|------|------|
| Language | Rust | Rust | Go | C |
| Postgres | ✅ Native | ✅ Native | ✅ | ✅ |
| MySQL | ✅ Native | ✅ Native | ✅ | ❌ |
| MSSQL | ✅ Native | ❌ | ✅ | ❌ |
| SQLite | ✅ Native | ❌ | ✅ | ❌ |
| Oracle | ✅ Opt-in | ✅ Opt-in | ✅ | ✅ |
| Static binary | ✅ Default | ✅ | Partial | ❌ |
| Script mode | ✅ | Partial | ✅ | ✅ |
| REPL / TUI | Wave 2 | ✅ (TUI) | ✅ | ✅ |
| Unified interface | ✅ | ✅ | ✅ | ❌ |

