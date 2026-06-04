# ferrule-sql

> The embeddable, synchronous, bounded-memory SQL core behind the [`ferrule`](https://github.com/rustpunk/ferrule) CLI.

`ferrule-sql` is the database layer of `ferrule` carved out as a standalone,
embeddable crate. It owns the unified neutral `Value`/`Row` types, the
`DatabaseUrl` parser, the `Connection` trait and its per-backend drivers, the
connect dispatcher (direct, HTTP-proxy, and SSH-tunnel transports), the
transaction helpers, and the cross-backend copy / bulk-load write path.

It carries **no rendering** (`tabled`) and **no credential-resolution**
(`ferrule-config`) dependency, so a host application can embed it directly and
supply already-resolved connection details.

## Why these properties

- **Synchronous public API.** No `async fn` / `Future` crosses the crate
  boundary, so a host with no runtime of its own can call straight through.
  The async network drivers are still used — each `Connection` owns a private
  current-thread `tokio` runtime and blocks on it — but that runtime is an
  implementation detail. SQLite and Oracle are natively synchronous and call
  through directly.
- **Bounded memory.** `Connection::query_cursor` streams from a native
  database cursor one back-pressured batch at a time, and `write_rows` flushes
  an arbitrarily large row iterator in fixed-size batches — both stay
  `O(batch)` regardless of total row count. The eager `Connection::query` is
  capped by per-cell / per-row / per-result `SizeGuards` so a pathological
  result fails fast instead of OOMing.
- **Caller-resolved credentials.** `connect` takes the password as a
  `secrecy::SecretString` on `ConnectOptions`; the crate performs no credential
  resolution and depends on no keyring / prompt library. The secret is redacted
  in `Debug` and zeroized on drop.

## Backends (feature flags)

The `default` feature set is **empty** — enable the backends you need.

| Feature    | Driver                         | C-free |
|------------|--------------------------------|:------:|
| `postgres` | `tokio-postgres` + `rustls`/ring | yes  |
| `mysql`    | `mysql_async` (`rustls`/ring)    | yes¹ |
| `mssql`    | `tiberius`                       | —    |
| `sqlite`   | `rusqlite` (`bundled`)           | yes² |
| `oracle`   | `oracle` (ODPI-C, opt-in)        | no   |
| `ssh`      | `russh` (ring) tunnel transport  | yes  |

¹ Pulls a vendored-static `zstd-sys` (a hard, non-feature-gated dependency of
`mysql_common`); no system-library linkage. ² `bundled` statically links a
vendored SQLite. "C-free" here means: no cmake, no system-library linkage —
self-contained vendored-static `cc` builds (`ring`, `zstd`, bundled SQLite)
are accepted.

## Example

`connect → streaming read → batched write`, the same three steps documented on
the crate root and in the runnable
[`examples/embed.rs`](https://github.com/rustpunk/ferrule/blob/main/ferrule-sql/examples/embed.rs)
(`cargo run -p ferrule-sql --example embed --features sqlite`):

```rust,no_run
use ferrule_sql::{
    connect, write_rows, Backend, ColumnInfo, ConnectOptions, DatabaseUrl,
    Row, TypeHint, Value, WriteMode, WriteOptions,
};
use secrecy::SecretString;

# fn demo() -> Result<(), Box<dyn std::error::Error>> {
// 1. Connect with a caller-resolved secret.
let url = DatabaseUrl::parse("sqlite:///tmp/embed-demo.db")?;
let opts = ConnectOptions {
    insecure: false,
    password: Some(SecretString::from("resolved-by-the-host")),
};
let mut conn = connect(&url, &opts, None)?;

// 2. Streaming read — pull rows one bounded batch at a time.
let mut cursor = conn.query_cursor("SELECT id, name FROM widget ORDER BY id")?;
while !cursor.next_batch(500)?.is_empty() {
    // process the batch; peak memory stays O(batch)
}

// 3. Batched write — flush an unbounded iterator in fixed-size batches.
let columns = [ColumnInfo { name: "id".into(), type_hint: TypeHint::Int64, nullable: false }];
let rows = (1..=10_000).map(|i| vec![Value::Int64(i)]);
let report = write_rows(
    &mut *conn,
    Backend::Sqlite,
    "sink",
    &columns,
    rows,
    &WriteOptions { mode: WriteMode::Insert, batch_size: 500, ..Default::default() },
)?;
assert!(report.is_complete());
# Ok(())
# }
```

## Relationship to `ferrule`

`ferrule-sql` is the lower half of the [`ferrule`](https://github.com/rustpunk/ferrule)
workspace. The `ferrule` CLI layers rendering, credential resolution, a
connection registry, and the command tree on top. Embedders who only need to
talk to databases — without the CLI's output or config machinery — depend on
`ferrule-sql` alone.

## License

Licensed under either of MIT or Apache-2.0 (`SPDX: MIT OR Apache-2.0`) at your
option.
