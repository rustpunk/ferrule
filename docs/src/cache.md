# Result Cache

Ferrule can transparently cache `SELECT` results so repeat invocations
return in microseconds instead of round-tripping to the database. The
cache is keyed off `(redacted_connection, normalized_sql,
named_params)`, lives in a **separate** SQLite store from the history
log, and never blocks the user's query on failure — every cache error
falls through to the real database transparently.

This chapter documents the v1 *spike*. It is intentionally narrow:
single-statement `SELECT` only, no DDL invalidation, no cross-machine
sharing, no streaming. See [#5b](https://github.com/rustpunk/ferrule/issues)
for the deferred questions.

## What's cached

Exactly one shape: a successful single-statement `SELECT`. Everything
else bypasses the cache.

| Path                                     | Cached?     |
|------------------------------------------|-------------|
| `ferrule query db "SELECT ..."`          | yes         |
| `ferrule query db "INSERT/UPDATE/DELETE/MERGE/CREATE/DROP/ALTER/TRUNCATE..."` | no — `is_modifying(sql) == true` bypasses both lookup and insert |
| `ferrule query db "SELECT ...; SELECT ..."` (multi-statement) | no |
| `ferrule query db ... --bench N`         | no — `--bench` implicitly disables cache |
| `ferrule query db ... --explain`         | no |
| `ferrule query db ... --watch`           | no |
| `ferrule query db ... --dry-run`         | no |
| `ferrule query db ... --daemon`          | no |
| `ferrule query db ... --no-cache`        | no — explicit bypass for one invocation |
| `FERRULE_NO_CACHE=1 ferrule query ...`   | no — env kill switch |
| `[cache] default_ttl = "0"`              | no — config disable without disabling lookup |
| `[cache] enabled = false`                | no — open-time disable |

## Configuration

```toml
[cache]
enabled = true                                # default-on
default_ttl = "5m"                            # applied to inserts when --cache not passed
max_age_days = 7                              # 0 disables age-based pruning
max_rows = 10_000                             # 0 disables count-based pruning
path = "~/.local/share/ferrule/results.db"    # default uses dirs::data_local_dir()
```

The store is a *separate* `results.db` from the history log
(`history.db`) — cache eviction churns faster than telemetry retention,
and the two stores are tuned for different access patterns.

Disable for a single invocation:

```bash
FERRULE_NO_CACHE=1 ferrule query db "SELECT now()"
# — or —
ferrule query --no-cache db "SELECT now()"
```

## Reading the cache (per-invocation overrides)

```bash
# Opt in with an explicit TTL for this query
ferrule query db "SELECT * FROM v_orders" --cache 1h

# Bypass cache for this query but leave the config setting alone
ferrule query db "SELECT * FROM v_orders" --no-cache

# Bypass cache once via env (useful in pipelines / Docker entrypoints)
FERRULE_NO_CACHE=1 ferrule query db "SELECT * FROM v_orders"
```

`--cache <DURATION>` overrides `[cache] default_ttl` for one
invocation. Grammar matches `--since`: `30s`, `5m`, `2h`, `7d`. Pass
`--cache 0` to bypass without disabling cache globally.

Under `--verbose`, ferrule prints a one-line stderr trace on every
cache event:

```text
[ferrule] cache hit (key=ab12cd34…, age=42s)
[ferrule] cache miss; inserted (ttl=300s)
```

Without `--verbose`, cache events are silent — the hit looks
identical to a normal query, just faster.

## Bypass rules

Cache lookup AND insert are bypassed when ANY of:

- `--no-cache` set
- `--bench N` set (implicit; `--bench` measures the database, not the cache)
- `--explain` set
- `--watch` or `--watch-file` set (each tick reopens the loop)
- `--dry-run` set
- `--daemon` set (the daemon path returns a pre-rendered payload)
- The SQL is modifying (INSERT/UPDATE/DELETE/MERGE/DDL — anything
  `ferrule_core::explain::is_modifying` flags)
- The SQL is a multi-statement batch (only single-statement SELECT
  results are cached; insert is gated on `results.len() == 1`)
- No `--cache` flag AND `[cache] default_ttl` is `"0"` or empty

Bypass is silent unless `--verbose`.

## Failure semantics

The cache is best-effort, never load-bearing. Every failure path
falls through to the real query:

| Failure                          | Behaviour                                                  |
|----------------------------------|------------------------------------------------------------|
| Lookup error (DB locked, etc.)   | Run the real query. `--verbose` prints a stderr trace.     |
| Insert error                     | Print result as normal. `--verbose` prints a stderr trace. |
| Corrupted `results.db`           | Open returns Err; treated as `None`. Real query runs.      |
| Schema downgrade (`user_version > 1`) | `CliError::Usage` with a hint to delete `results.db`. |

The cache file is **never** auto-recovered. Manual reset:

```bash
rm ~/.local/share/ferrule/results.db
```

The next invocation will re-create it with the current schema.

## Security

The cache key is derived from `DatabaseUrl::redacted()` — the
**password is scrubbed before it enters `Sha256::update`**. This is
test-asserted (`cache_key_never_contains_password_bytes` in
`ferrule-cli/src/cache.rs`).

However, the cached payload itself is user data. `rows_json` contains
the bytes of every row returned by every SELECT. Treat
`results.db` like `bash_history`:

- Inherits home directory permissions (typically `0700`)
- Don't sync it across machines unless you trust the destination
- Don't include it in screen recordings or `tar` bundles you share

If you have queries whose results should never be persisted, prefer:

```bash
ferrule query db --no-cache "SELECT credit_card_number FROM ..."
```

## What this leaves for follow-up

Filed as [R5b]:

1. **DDL-elsewhere invalidation.** A third client `ALTER`s a cached
   table; ferrule silently returns stale rows until TTL expires.
2. **Eviction on local writes.** When ferrule itself runs modifying
   SQL against a cached connection, should it invalidate matching
   entries? Today, no.
3. **Cross-machine shared cache.** Redis / S3 backend behind the same
   `CacheDb` trait.
4. **Multi-statement caching.** Round-tripping `Vec<StatementResult>`
   is harder than `QueryResult`.
5. **Prepared-statement cache (daemon mode).** Shared prepared-stmt +
   result cache once full daemon mode lands.
6. **Size budget.** Today unbounded until `prune()` fires against
   `max_rows`. Add `max_size_mb` analogous to slow-log `max_size`.
7. **Streaming cache.** Today the full result set materializes
   before insert. Add `max_rows_per_cache_entry` (or skip cache above
   threshold).

## Quick smoke

```bash
# Isolated config so the smoke doesn't touch ~/.local/share.
cat > /tmp/ferrule-cache.toml <<'EOF'
[cache]
enabled = true
default_ttl = "5m"
path = "/tmp/ferrule-cache.db"
EOF

# 1. Miss + insert.
ferrule -c /tmp/ferrule-cache.toml query \
  "sqlite:///tmp/ferrule-cache-data.db" "SELECT 1" --verbose
# stderr: [ferrule] cache miss; inserted (ttl=300s)

# 2. Hit.
ferrule -c /tmp/ferrule-cache.toml query \
  "sqlite:///tmp/ferrule-cache-data.db" "SELECT 1" --verbose
# stderr: [ferrule] cache hit (key=..., age=Xs)

# 3. --no-cache bypass.
ferrule -c /tmp/ferrule-cache.toml query \
  "sqlite:///tmp/ferrule-cache-data.db" "SELECT 1" --no-cache --verbose
# (no cache-hit/miss line — went straight to the DB)

# 4. is_modifying bypass.
ferrule -c /tmp/ferrule-cache.toml query \
  "sqlite:///tmp/ferrule-cache-data.db" \
  "CREATE TABLE t(x); INSERT INTO t VALUES (1)" --verbose
# (no cache-hit/miss line — INSERT is modifying SQL)

# 5. Env kill switch.
FERRULE_NO_CACHE=1 ferrule -c /tmp/ferrule-cache.toml query \
  "sqlite:///tmp/ferrule-cache-data.db" "SELECT 1" --verbose
# (no cache-hit/miss line — env disabled the cache wholesale)

# Inspect the store directly:
sqlite3 /tmp/ferrule-cache.db \
  "SELECT key, ts, ttl_secs, sql_preview FROM cache;"

# Cleanup:
rm /tmp/ferrule-cache.toml /tmp/ferrule-cache.db /tmp/ferrule-cache-data.db
```
