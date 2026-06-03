//! End-to-end embedding example: connect → streaming read → batched write.
//!
//! Run it with the `sqlite` backend (no server, no Docker required):
//!
//! ```text
//! cargo run -p ferrule-sql --example embed --features sqlite
//! ```
//!
//! This mirrors the three-step flow documented on the crate root:
//!
//! 1. [`connect`] with a caller-resolved [`SecretString`] in
//!    [`ConnectOptions`] (the credential never has to live in the URL).
//! 2. A bounded-memory streaming read via
//!    [`Connection::query_cursor`] — rows are pulled one batch at a time,
//!    never the whole result.
//! 3. A back-pressured batched write via [`write_rows`] — an unbounded
//!    row iterator lands in fixed-size batches at `O(batch_size)` memory,
//!    returning a structured [`WriteReport`].
//!
//! SQLite is a local-file backend with no authentication, so the
//! [`SecretString`] passed below is accepted and simply unused; for a
//! networked backend (`postgres`, `mysql`, …) the same field carries the
//! real password the host resolved from its own keyring / prompt / env.

use ferrule_sql::{
    ColumnInfo, ConnectOptions, Connection, DatabaseUrl, Row, TypeHint, Value, WriteMode,
    WriteOptions, write_rows,
};
use secrecy::SecretString;
use std::error::Error;

fn main() -> Result<(), Box<dyn Error>> {
    // A throwaway on-disk fixture in the temp dir. Using a file (rather
    // than `:memory:`) keeps a single shared database across the two
    // statements we run; the example removes it on the way out.
    let path =
        std::env::temp_dir().join(format!("ferrule-embed-example-{}.db", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let url = DatabaseUrl::parse(&format!("sqlite://{}", path.display()))?;

    // Step 1 — connect with a caller-resolved credential.
    //
    // `ConnectOptions::password` carries a `SecretString` the *host*
    // resolved (env var, OS keyring, interactive prompt, …); `ferrule-sql`
    // does no credential resolution of its own. The secret is redacted in
    // `Debug` and zeroized on drop. SQLite ignores it (no auth), but a
    // networked backend would consume it here.
    let opts = ConnectOptions {
        insecure: false,
        password: Some(SecretString::from("unused-for-sqlite")),
    };
    let mut conn = ferrule_sql::connect(&url, &opts, None)?;

    // Seed a fixture table to read back. `execute` blocks until the
    // statement completes.
    conn.execute("CREATE TABLE widget (id INTEGER PRIMARY KEY, name TEXT)")?;
    conn.execute(
        "INSERT INTO widget (id, name) VALUES \
         (1, 'alpha'), (2, 'beta'), (3, 'gamma'), (4, 'delta'), (5, 'epsilon')",
    )?;

    // Step 2 — streaming read at bounded memory.
    //
    // `query_cursor` opens a native cursor; `next_batch(n)` pulls at most
    // `n` rows at a time, so peak memory is `O(batch)` no matter how large
    // the table is. The cursor borrows the connection for its lifetime, so
    // it is scoped here and dropped before we write.
    println!("streaming read (batched, bounded memory):");
    let mut streamed = 0u64;
    {
        let mut cursor = conn.query_cursor("SELECT id, name FROM widget ORDER BY id")?;
        loop {
            let batch = cursor.next_batch(2)?; // 2 rows per pull
            if batch.is_empty() {
                break; // end of stream
            }
            for row in &batch {
                println!("  row: {} = {}", render(&row[0]), render(&row[1]));
                streamed += 1;
            }
        }
    }
    println!("  streamed {streamed} rows total\n");

    // Step 3 — batched write with a structured report.
    //
    // `write_rows` consumes any `IntoIterator<Item = Row>` and flushes it
    // in fixed-size batches (`WriteOptions::batch_size`), buffering only
    // one batch at a time. The iterator below is materialized for brevity,
    // but it could be a lazy generator over millions of rows — memory
    // stays `O(batch_size)`. Pair it with the `query_cursor` above for an
    // end-to-end bounded-memory pipe.
    conn.execute("CREATE TABLE sink (id INTEGER PRIMARY KEY, label TEXT)")?;
    let columns = [
        ColumnInfo {
            name: "id".into(),
            type_hint: TypeHint::Int64,
            nullable: false,
        },
        ColumnInfo {
            name: "label".into(),
            type_hint: TypeHint::String,
            nullable: true,
        },
    ];
    let rows: Vec<Row> = (1..=2500)
        .map(|i| vec![Value::Int64(i), Value::String(format!("item-{i}"))])
        .collect();
    let write_opts = WriteOptions {
        mode: WriteMode::Insert,
        batch_size: 500, // 5 batches of 500; only one buffered at a time
        ..Default::default()
    };
    let report = write_rows(
        &mut *conn,
        ferrule_sql::Backend::Sqlite,
        "sink",
        &columns,
        rows,
        &write_opts,
    )?;
    println!("batched write report:");
    println!("  rows attempted : {}", report.rows_attempted);
    println!("  rows written   : {}", report.rows_written);
    println!("  batches        : {}", report.batches_committed);
    println!("  complete       : {}", report.is_complete());

    // Confirm the write landed.
    let count = conn.query("SELECT COUNT(*) FROM sink")?;
    if let Some(Value::Int64(n)) = count.rows.first().and_then(|r| r.first()) {
        println!("  verified count : {n}");
    }

    let _ = std::fs::remove_file(&path);
    Ok(())
}

/// Render a [`Value`] for the demo's stdout. The crate's own
/// `Value: Display` impl could be used directly; this keeps the example
/// self-contained and shows the enum the host actually receives.
fn render(v: &Value) -> String {
    match v {
        Value::Null => "NULL".to_string(),
        Value::Int64(n) => n.to_string(),
        Value::String(s) => s.clone(),
        other => format!("{other}"),
    }
}
