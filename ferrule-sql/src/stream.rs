//! Bounded-memory streaming reads — the native-cursor counterpart to
//! the eager [`Connection::query`](crate::Connection::query).
//!
//! `query` materializes the whole result into a `Vec<Row>`, which is
//! fine for the CLI table view but unusable for an embedder ingesting a
//! multi-million-row table under a fixed memory budget. [`RowCursor`]
//! pulls rows from a **native driver cursor** one bounded batch at a
//! time, so peak resident memory is `O(batch + cap)` rather than
//! `O(total rows)`.
//!
//! **Bounded-memory model.** Every backend drives a row producer that is
//! back-pressured: the async network drivers (`tokio-postgres`
//! `query_raw`, `mysql_async` `query_iter`, `tiberius` `QueryStream`)
//! expose a native row stream that only fetches more from the server as
//! the consumer pulls; the synchronous drivers (`rusqlite`, ODPI-C
//! `oracle`) run their row-stepping loop on a `spawn_blocking` thread
//! that feeds a **bounded `tokio::sync::mpsc` channel** — the producer
//! blocks on `blocking_send` once `cap` rows are in flight, so at most
//! `cap` rows are ever buffered ahead of the consumer. Either way no
//! code path collects the full result.
//!
//! **Blocking model.** [`RowCursor`] borrows its connection's private
//! current-thread runtime; each [`next_batch`](RowCursor::next_batch) /
//! [`Iterator::next`] call drives the producer with `block_on`, blocking
//! the calling thread until the next row(s) arrive. The same
//! current-thread reentrancy rule as [`Connection`](crate::Connection)
//! applies — never pull from a cursor inside another `block_on` on the
//! same thread.

use crate::error::SqlError;
use crate::value::{ColumnInfo, Row};
use futures_util::stream::{Stream, StreamExt};
use std::pin::Pin;

/// Default number of rows a streaming cursor keeps in flight between the
/// producer and the consumer. Caps the bounded channel for the
/// synchronous (`spawn_blocking`) backends and the default
/// [`next_batch`](RowCursor::next_batch) chunk; the async backends are
/// additionally bounded by the driver's own server-side fetch window.
pub const DEFAULT_CURSOR_CAPACITY: usize = 1024;

/// A boxed, backend-erased stream of decoded rows.
///
/// Each backend's `query_stream` builds one of these from its native
/// cursor. The `'a` lifetime ties an async backend's stream to the
/// `&mut` borrow of its driver handle (so no self-referential storage /
/// `unsafe` is needed); the synchronous backends produce a `'static`
/// stream backed by their bounded channel.
pub type BoxRowStream<'a> = Pin<Box<dyn Stream<Item = Result<Row, SqlError>> + Send + 'a>>;

/// A streaming read handle over a native database cursor.
///
/// Hand back by [`Connection::query_cursor`](crate::Connection::query_cursor).
/// Pull rows with [`next_batch`](Self::next_batch) (bounded chunk) or by
/// iterating ([`Iterator`] yields one `Result<Row, SqlError>` at a
/// time). See the [module docs](self) for the bounded-memory and
/// blocking contracts.
pub struct RowCursor<'a> {
    columns: Vec<ColumnInfo>,
    /// The private runtime that drives the producer. Borrowed from the
    /// owning connection; `block_on` on it advances the stream.
    rt: &'a tokio::runtime::Runtime,
    stream: BoxRowStream<'a>,
    /// Set once the underlying stream has yielded `None`, so subsequent
    /// pulls short-circuit without re-polling an exhausted stream.
    exhausted: bool,
}

impl<'a> RowCursor<'a> {
    /// Construct a cursor from a backend stream plus the runtime that
    /// drives it. Internal — backends return their stream and the public
    /// `query_cursor` wires in the runtime.
    pub(crate) fn new(
        columns: Vec<ColumnInfo>,
        rt: &'a tokio::runtime::Runtime,
        stream: BoxRowStream<'a>,
    ) -> Self {
        Self {
            columns,
            rt,
            stream,
            exhausted: false,
        }
    }

    /// The column metadata for the streamed result, available before any
    /// row is pulled.
    #[must_use]
    pub fn columns(&self) -> &[ColumnInfo] {
        &self.columns
    }

    /// Pull up to `n` rows, blocking until they arrive or the result is
    /// exhausted. Returns fewer than `n` (possibly empty) at end of
    /// stream. This call's transient allocation is bounded by `n` rows
    /// plus the producer's in-flight window — never the full result.
    ///
    /// `n` of `0` returns an empty `Vec` without touching the producer.
    /// The first driver error is returned immediately; callers should
    /// stop on `Err`.
    pub fn next_batch(&mut self, n: usize) -> Result<Vec<Row>, SqlError> {
        if n == 0 || self.exhausted {
            return Ok(Vec::new());
        }
        // Drive the producer cooperatively on the owned runtime. The
        // closure pulls at most `n` rows and returns the owned batch, so
        // `block_on` returns as soon as that batch is assembled — the
        // producer is never run ahead of the consumer beyond its own
        // bounded window.
        let stream = &mut self.stream;
        let cap = n.min(DEFAULT_CURSOR_CAPACITY);
        let result: Result<(Vec<Row>, bool), SqlError> = self.rt.block_on(async move {
            let mut out = Vec::with_capacity(cap);
            for _ in 0..n {
                match stream.next().await {
                    Some(Ok(row)) => out.push(row),
                    Some(Err(e)) => return Err(e),
                    None => return Ok((out, true)),
                }
            }
            Ok((out, false))
        });
        match result {
            Ok((out, reached_end)) => {
                self.exhausted = reached_end;
                Ok(out)
            }
            Err(e) => {
                // A driver error abandons the batch; mark exhausted so a
                // careless re-pull does not double-drive a half-consumed
                // async stream.
                self.exhausted = true;
                Err(e)
            }
        }
    }
}

impl Iterator for RowCursor<'_> {
    type Item = Result<Row, SqlError>;

    /// Yield the next row, blocking until it arrives. `None` marks
    /// end-of-stream; an `Err` item marks a driver failure (after which
    /// iteration stops).
    fn next(&mut self) -> Option<Self::Item> {
        if self.exhausted {
            return None;
        }
        match self.next_batch(1) {
            Ok(mut rows) => rows.pop().map(Ok),
            Err(e) => Some(Err(e)),
        }
    }
}

/// Build a `'static` [`BoxRowStream`] from a bounded `tokio::sync::mpsc`
/// receiver. Used by the synchronous backends (`rusqlite`, `oracle`)
/// whose `spawn_blocking` producer sends decoded rows through the
/// channel; the channel's bound is the in-flight cap.
pub(crate) fn channel_stream(
    rx: tokio::sync::mpsc::Receiver<Result<Row, SqlError>>,
) -> BoxRowStream<'static> {
    Box::pin(tokio_stream_from_channel(rx))
}

/// Adapt a bounded mpsc receiver into a [`Stream`]. Kept as a free
/// function (rather than pulling in `tokio-stream`) so the dependency
/// surface stays minimal — `futures_util` is already a backend dep.
fn tokio_stream_from_channel(
    mut rx: tokio::sync::mpsc::Receiver<Result<Row, SqlError>>,
) -> impl Stream<Item = Result<Row, SqlError>> + Send + 'static {
    futures_util::stream::poll_fn(move |cx| rx.poll_recv(cx))
}
