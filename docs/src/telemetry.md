# Query Telemetry

Ferrule remembers and measures what you do. Every invocation is timed,
the redacted connection URL and SQL body are recorded to a local
SQLite store, and four flags let you query, benchmark, or gate on
that history from the same CLI.

The store is the foundation; the four features layered on top all
hang off the same `record_dispatch` hook in `main.rs`.

## What gets recorded

Every successful or failing ferrule invocation records exactly one row
into `~/.local/share/ferrule/history.db` with these fields:

| Field         | Description |
|---------------|-------------|
| `ts`          | RFC 3339 timestamp (UTC) |
| `conn`        | Connection target. For raw URLs this is `DatabaseUrl::redacted()` — passwords are scrubbed before recording. Registry names and SQLite paths pass through unchanged. |
| `command`     | Subcommand name (`query`, `copy`, `tables`, …) |
| `sql`         | The SQL body for query-shaped commands. `--bench` invocations record one rolled-up row with `sql = "bench(N): <original SQL>"` instead of N per-iteration rows. |
| `duration_ms` | End-to-end wall-clock in milliseconds |
| `rows`        | Sample count for `--bench`, otherwise `NULL` (the dispatch hook can't see the per-command row count) |
| `exit_code`   | `0` on success, `1` for `--fail-on-empty` firing or other notable-result gates, `2` for usage errors, `3` for connection failures, `4` for query failures |
| `error`       | `miette` error class on failure: `"connection"`, `"query"`, `"usage"`, `"registry"`, `"io"`, or `"result_notable"`. `NULL` on success. |

The `ferrule history` subcommand itself is the only command that does
**not** record itself — recording every `ferrule history --last 5`
read would clutter the table without adding signal.

## Configuration

```toml
[history]
enabled = true                                # default-on
max_age_days = 30                             # 0 disables age-based pruning
max_rows = 100_000                            # 0 disables count-based pruning
path = "~/.local/share/ferrule/history.db"    # default uses dirs::data_local_dir()
```

Disable for a single invocation:

```bash
FERRULE_NO_HISTORY=1 ferrule query db "SELECT secrets()"
```

Pruning is open-loop: every recorded run drops rows older than
`max_age_days` (unless `0`) and trims to `max_rows` (unless `0`),
deleting oldest rows first.

## Reading history

```bash
ferrule history --last 20
ferrule history --conn '*prod*' --slowest
ferrule history --grep "DELETE FROM orders" --since 24h
ferrule history --min-duration-ms 500
```

Filters AND-combine. `--conn` is a shell-style glob
(`*` / `?`, case-insensitive). `--since` accepts `30s`, `5m`, `2h`,
`7d`. `--grep` is a case-insensitive substring match on the SQL body.
`--slowest` sorts by `duration_ms` descending instead of `ts` desc.

Output flows through ferrule's standard `--format table|json|csv|yaml|raw`
selection.

## Slow-query log

Opt-in side channel that tees every run crossing the configured
threshold to an append-only file. Useful for "the query that took
30 seconds got lost in scrollback" forensics.

```toml
[slow_log]
enabled = true
threshold = "250ms"   # humantime (1s, 250ms, 5m, 1h, 2d) or bare integer ms
path = "~/.local/share/ferrule/slow.log"
```

The log format is tab-separated:

```
<rfc3339 ts>\t<conn>\t<duration_ms>\t<sql one-line>\t<rows or ->
```

Read paths:

```bash
ferrule history --slow              # use the configured threshold
ferrule history --min-duration-ms 1000   # explicit override (overrides --slow)
ferrule slow                        # alias; --slowest implied, drops --slow flag
```

Slow-log open failure is **fatal** at the dispatch boundary because
the user explicitly opted in. The rest of history-store I/O is
swallowed (`Ok(_) | Err(_) => return`) so the user's command never
blocks on a busted store.

## Benchmark mode

```bash
ferrule query db "SELECT count(*) FROM events" --bench 100 --bench-warmup 5
bench: n=100 warmup=5 min=48.8µs mean=69.0µs max=265.8µs
       p50=59.0µs p95=103.8µs p99=191.4µs
  48.8µs..59.6µs     │██████████████████████████████████████████████ 30
  59.6µs..70.5µs     │█████████████████████                          12
  ...
```

The connect cost is taken once outside the loop, so samples represent
query time, not handshake + query. Warmup iterations are dropped from
the histogram and percentile computation.

`--bench-output PATH` additionally emits per-iteration timings as
`iteration,duration_ns` CSV, useful for piping into other tools:

```bash
ferrule query db "..." --bench 1000 --bench-output samples.csv
# Compare two queries:
paste -d, q1.csv q2.csv | awk -F, '{print $2-$4}' | sort -n
```

History interaction: each bench run records exactly one row with
`sql="bench(N): <SQL>"` and `rows=N`, not N per-iteration rows.

## Gating on empty results

GNU diff convention: exit `0` for "ran cleanly", `1` for "ran cleanly
but the result is something the caller likely wants to gate on", `2+`
for real errors. `ferrule diff` already uses code `1`. `--fail-on-empty`
extends that to `query` and `export`.

```bash
ferrule query prod "SELECT * FROM jobs WHERE failed" --fail-on-empty \
  || ./alert.sh "no failed jobs to retry"

ferrule export prod "SELECT * FROM dlq" --file dlq.csv --fail-on-empty \
  || ./alert.sh "dlq is empty - good news, maybe"
```

Contract:

- Single SELECT returning 0 rows → exit 1.
- Single SELECT returning ≥1 row → exit 0.
- DML-only batch (`CREATE`, `INSERT`, …) → exit 2 (usage error: the
  gate is about row count, which DML doesn't carry).
- Multi-statement batch → the **first SELECT** decides.

The exit-1 case prints a plain stderr line (`ferrule: query returned
no rows (--fail-on-empty)`), not a `miette` error box. Recorded in
history with `exit_code=1, error="result_notable"`.

## What this unblocks

The history store is the foundation for several follow-ups already
filed:

- **[#5] Result caching by query hash** — keys off `(conn_redacted,
  normalized_sql, params)` against this same SQLite store.
- **[#48] `ferrule history prune`** — explicit pruning beyond the
  open-loop retention knobs.
- **[#49] SQL-body redaction** — pluggable regex-based scrubbing for
  the rare case where SQL bodies carry secrets directly instead of
  via parameter binding.

## Quick smoke

```bash
# 1. History records a run and we can read it back.
ferrule query "sqlite::memory:" "SELECT 1, 2, 3"
ferrule history --last 1

# 2. Slow log captures the slow run; fast one is skipped.
cat > /tmp/slow.toml <<'EOF'
[slow_log]
enabled = true
threshold = "10ms"
path = "/tmp/ferrule-slow.log"
EOF
ferrule -c /tmp/slow.toml query "sqlite::memory:" \
  "WITH RECURSIVE r(i) AS (SELECT 1 UNION ALL SELECT i+1 FROM r WHERE i<200000) SELECT count(*) FROM r"
cat /tmp/ferrule-slow.log    # one tab-separated line

# 3. Bench mode produces a histogram and a single history row.
ferrule query "sqlite::memory:" "SELECT 1" --bench 50 --bench-warmup 5
ferrule history --last 1 --format json | grep -q '"bench('

# 4. --fail-on-empty gates exit code.
ferrule query "sqlite::memory:" "SELECT 1 WHERE 0=1" --fail-on-empty; test $? -eq 1
ferrule query "sqlite::memory:" "SELECT 1"            --fail-on-empty; test $? -eq 0
```
