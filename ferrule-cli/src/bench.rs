//! `--bench N` mode for `ferrule query` (P4 / #15).
//!
//! Runs a query N+K times (K warmup, discarded), collects per-iteration
//! `Duration` samples, and renders an ASCII histogram + p50/p95/p99
//! summary. Pairs naturally with the connection-pooling daemon (no
//! per-iteration handshake cost) but works against any backend.
//!
//! Per the Phase-1 dispatch hook contract, a `--bench` invocation
//! records *one* `RunRecord` summarising the run rather than N
//! per-iteration rows — see the `main.rs::record_dispatch` call site
//! and the `BenchSummary::history_sql` rollup string.

use std::cell::RefCell;
use std::time::Duration;

thread_local! {
    /// Set by `run_bench` to communicate a one-row rollup to the
    /// dispatch hook in `main.rs::record_dispatch`. The dispatch hook
    /// reads (and clears) this after the per-command run returns,
    /// folds it into the `RunRecord`'s `sql` + `rows` fields so the
    /// history table shows one row per `--bench` invocation, not N.
    static LAST_BENCH: RefCell<Option<(String, i64)>> = const { RefCell::new(None) };
}

/// Stash the bench rollup so the dispatch hook can read it after the
/// run returns. `sql` is the user-facing summary (e.g.
/// `bench(50): SELECT 1`); `rows` is the sample count.
pub fn record_last(sql: String, rows: i64) {
    LAST_BENCH.with(|cell| *cell.borrow_mut() = Some((sql, rows)));
}

/// Take the stashed rollup (if any). Called once per dispatch.
pub fn take_last() -> Option<(String, i64)> {
    LAST_BENCH.with(|cell| cell.borrow_mut().take())
}

/// Aggregate of a `--bench` run. Built incrementally in
/// `commands::query::run` and rendered once at the end.
#[derive(Debug, Clone)]
pub struct BenchSummary {
    /// All non-warmup samples, in execution order.
    pub samples: Vec<Duration>,
    /// Warmup samples that were collected then discarded. Kept for the
    /// summary line so the user can see we didn't cheat.
    pub warmup_dropped: usize,
}

impl BenchSummary {
    pub fn new(warmup_dropped: usize) -> Self {
        Self {
            samples: Vec::new(),
            warmup_dropped,
        }
    }

    pub fn push(&mut self, d: Duration) {
        self.samples.push(d);
    }

    pub fn n(&self) -> usize {
        self.samples.len()
    }

    pub fn min(&self) -> Duration {
        self.samples.iter().copied().min().unwrap_or_default()
    }

    pub fn max(&self) -> Duration {
        self.samples.iter().copied().max().unwrap_or_default()
    }

    pub fn mean(&self) -> Duration {
        if self.samples.is_empty() {
            return Duration::ZERO;
        }
        let sum: u128 = self.samples.iter().map(|d| d.as_nanos()).sum();
        Duration::from_nanos((sum / self.samples.len() as u128) as u64)
    }

    /// Linear-interpolation percentile against the sorted sample list.
    /// `p` is in `[0.0, 100.0]`; values outside saturate to ends.
    pub fn percentile(&self, p: f64) -> Duration {
        if self.samples.is_empty() {
            return Duration::ZERO;
        }
        let mut sorted: Vec<Duration> = self.samples.clone();
        sorted.sort();
        let p = p.clamp(0.0, 100.0);
        if self.samples.len() == 1 || p <= 0.0 {
            return *sorted.first().unwrap();
        }
        if p >= 100.0 {
            return *sorted.last().unwrap();
        }
        let pos = p / 100.0 * (sorted.len() - 1) as f64;
        let lo = pos.floor() as usize;
        let hi = pos.ceil() as usize;
        if lo == hi {
            return sorted[lo];
        }
        let frac = pos - lo as f64;
        let a = sorted[lo].as_nanos() as f64;
        let b = sorted[hi].as_nanos() as f64;
        Duration::from_nanos((a + (b - a) * frac) as u64)
    }

    /// Render the summary plus a 20-bucket ASCII histogram. `width` is
    /// the bar pixel budget (terminal cols minus label padding).
    pub fn render(&self, width: usize) -> String {
        let mut out = String::new();
        out.push_str(&format!(
            "bench: n={} warmup={} min={} mean={} max={}\n",
            self.n(),
            self.warmup_dropped,
            fmt_dur(self.min()),
            fmt_dur(self.mean()),
            fmt_dur(self.max()),
        ));
        out.push_str(&format!(
            "       p50={} p95={} p99={}\n",
            fmt_dur(self.percentile(50.0)),
            fmt_dur(self.percentile(95.0)),
            fmt_dur(self.percentile(99.0)),
        ));
        out.push_str(&self.histogram(width));
        out
    }

    /// 20-bucket ASCII histogram. Independently testable.
    pub fn histogram(&self, width: usize) -> String {
        if self.samples.is_empty() {
            return String::new();
        }
        const BUCKETS: usize = 20;
        let lo = self.min().as_nanos() as f64;
        let hi = self.max().as_nanos() as f64;
        // Flat distribution (lo == hi) → one bucket gets everything.
        let span = (hi - lo).max(1.0);
        let mut counts = [0usize; BUCKETS];
        for d in &self.samples {
            let n = d.as_nanos() as f64;
            let idx = (((n - lo) / span) * BUCKETS as f64).floor() as usize;
            let idx = idx.min(BUCKETS - 1);
            counts[idx] += 1;
        }
        let peak = counts.iter().copied().max().unwrap_or(1).max(1);
        let bar_budget = width.max(10);
        let label_w = 18usize;
        let mut out = String::new();
        for (i, &c) in counts.iter().enumerate() {
            let bucket_lo = lo + span * i as f64 / BUCKETS as f64;
            let bucket_hi = lo + span * (i + 1) as f64 / BUCKETS as f64;
            let bar_len = (c as f64 / peak as f64 * bar_budget as f64).round() as usize;
            let bar: String = "█".repeat(bar_len);
            let label = format!(
                "{}..{}",
                fmt_dur(Duration::from_nanos(bucket_lo as u64)),
                fmt_dur(Duration::from_nanos(bucket_hi as u64))
            );
            let pad = " ".repeat(bar_budget.saturating_sub(bar.chars().count()));
            out.push_str(&format!("  {label:<label_w$} │{bar}{pad} {c}\n"));
        }
        out
    }

    /// Rollup string used by the dispatch hook so the history table
    /// shows one row per bench run, not N. Format matches the plan:
    /// `bench(N): <original SQL trimmed>`.
    pub fn history_sql(&self, original_sql: &str) -> String {
        let trimmed: String = original_sql.split_whitespace().collect::<Vec<_>>().join(" ");
        format!("bench({}): {trimmed}", self.n())
    }

    /// CSV emission for `--bench-output csv`. One row per iteration,
    /// columns: `iteration,duration_ns`.
    pub fn to_csv(&self) -> String {
        let mut out = String::from("iteration,duration_ns\n");
        for (i, d) in self.samples.iter().enumerate() {
            out.push_str(&format!("{i},{}\n", d.as_nanos()));
        }
        out
    }
}

fn fmt_dur(d: Duration) -> String {
    let ns = d.as_nanos();
    if ns < 1_000 {
        format!("{ns}ns")
    } else if ns < 1_000_000 {
        format!("{:.1}µs", ns as f64 / 1_000.0)
    } else if ns < 1_000_000_000 {
        format!("{:.2}ms", ns as f64 / 1_000_000.0)
    } else {
        format!("{:.2}s", ns as f64 / 1_000_000_000.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    fn summary(samples: &[u64]) -> BenchSummary {
        let mut s = BenchSummary::new(0);
        for n in samples {
            s.push(ms(*n));
        }
        s
    }

    #[test]
    fn percentile_p50_p95_p99_matches_handcomputed() {
        let s = summary(&[1, 2, 3, 4, 5, 6, 7, 8, 9, 10]);
        // Linear-interp percentile against sorted 1..=10:
        //   p50 = 5.5ms (between 5 and 6)
        assert_eq!(s.percentile(50.0), Duration::from_micros(5_500));
        // p95 = 9.55ms (between 9 and 10, weight 0.55)
        let p95 = s.percentile(95.0).as_nanos();
        assert!((9_500_000..=9_600_000).contains(&(p95 as u64)));
        // p99 == p100 ~= 10ms here
        let p99 = s.percentile(99.0).as_nanos();
        assert!((9_900_000..=10_000_000).contains(&(p99 as u64)));
    }

    #[test]
    fn percentile_clamps_to_ends() {
        let s = summary(&[10, 20, 30]);
        assert_eq!(s.percentile(-50.0), ms(10));
        assert_eq!(s.percentile(150.0), ms(30));
    }

    #[test]
    fn min_max_mean() {
        let s = summary(&[10, 20, 30, 40]);
        assert_eq!(s.min(), ms(10));
        assert_eq!(s.max(), ms(40));
        assert_eq!(s.mean(), ms(25));
    }

    #[test]
    fn histogram_renders_to_string() {
        let s = summary(&[1, 1, 1, 2, 2, 3, 4, 5, 6, 7, 8, 9, 10]);
        let hist = s.histogram(40);
        assert!(!hist.is_empty());
        // 20 buckets means 20 lines
        assert_eq!(hist.lines().count(), 20);
    }

    #[test]
    fn render_includes_summary_and_histogram() {
        let mut s = summary(&[5, 5, 5, 5]);
        s.warmup_dropped = 2;
        let rendered = s.render(20);
        assert!(rendered.contains("bench: n=4 warmup=2"));
        assert!(rendered.contains("p50="));
    }

    #[test]
    fn history_sql_collapses_whitespace_and_caps_n() {
        let s = summary(&[1, 2, 3]);
        let h = s.history_sql("SELECT\n  *\nFROM\n  x");
        assert_eq!(h, "bench(3): SELECT * FROM x");
    }

    #[test]
    fn to_csv_one_row_per_sample() {
        let s = summary(&[5, 6, 7]);
        let csv = s.to_csv();
        let lines: Vec<_> = csv.lines().collect();
        assert_eq!(lines[0], "iteration,duration_ns");
        assert_eq!(lines.len(), 4);
        assert!(lines[1].starts_with("0,"));
    }

    #[test]
    fn empty_summary_is_safe() {
        let s = BenchSummary::new(0);
        assert_eq!(s.n(), 0);
        assert_eq!(s.percentile(50.0), Duration::ZERO);
        assert_eq!(s.mean(), Duration::ZERO);
        assert_eq!(s.histogram(20), "");
    }
}
