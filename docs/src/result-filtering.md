# Filtering Results with JMESPath

`ferrule query --filter <expr>` runs a [JMESPath] expression over the
JSON output of a query before printing it. It's the same query
language `aws --query` uses, so anyone who's poked at AWS CLI output
will recognize the shape.

The filter operates on the *rendered JSON* — the same structure you
get from `--format json` — not on the SQL result set. That means you
can use it to reshape, reduce, or summarize what the database
returned, without rewriting the SQL.

[JMESPath]: https://jmespath.org/

## When to use it

- You want a count, sum, or max from a query that returned rows, but
  you don't want to write the aggregate in SQL — maybe because the
  rows are already cached, or because you're piping through `jq`-like
  follow-up filters.
- You want to extract a single column from a SELECT and feed it into
  the next shell command, without `awk`-ing CSV.
- You want to reshape the output as objects-of-objects or
  arrays-of-strings without touching the SQL.

`--filter` is *not* a substitute for a `WHERE` clause. The query
still pulls every row across the wire; the filter just trims the
output. Push the predicate into SQL when the row count is large.

## Implies `--format json`

`--filter` and JSON go together. Passing `--format table` with a
filter is rejected up front:

```text
$ ferrule query demo "SELECT * FROM users" --filter "[*].name" --format table
ferrule: error: --filter requires --format json (got --format table)
```

If you don't pass `--format` at all, ferrule treats the filter as
implicitly setting JSON. Useful when piping output:

```bash
ferrule query demo "SELECT id, name FROM users" --filter "[*].name" \
  | jq -r '.[]' \
  | head -5
```

## Examples

Pull a single column as an array of strings:

```bash
ferrule query demo "SELECT name, email FROM users" \
  --filter "[*].name"
# → ["Alice", "Bob", "Charlie"]
```

Filter rows where a field matches:

```bash
ferrule query demo "SELECT id, name, role FROM users" \
  --filter "[?role=='admin'].name"
# → ["Alice"]
```

Reshape into a map keyed by name:

```bash
ferrule query demo "SELECT name, age FROM users" \
  --filter "{by_name: [*].{name: name, age: age}}"
```

Count rows that match a predicate:

```bash
ferrule query demo "SELECT id, active FROM users" \
  --filter "length([?active])"
# → 7
```

Take the first row:

```bash
ferrule query demo "SELECT * FROM users ORDER BY created_at DESC LIMIT 1" \
  --filter "[0]"
```

## Scope and limits

| Combination | Result |
|---|---|
| `--filter` + single SELECT result | Filter runs on the result rows array |
| `--filter` + `INSERT/UPDATE/DELETE` (summary) | Rejected — "requires a SELECT-style query that returns rows" |
| `--filter` + multi-statement | Rejected — "cannot be applied to multi-statement queries" |
| `--filter` + `--explain` | Rejected — explain payloads are XML/text/JSON-of-plan, not row data |
| Invalid JMESPath syntax | Exits with code 3 (query error class) and a clear diagnostic |

The output is always re-serialized as pretty-printed JSON (two-space
indentation), so the result is human-readable AND pipe-friendly for
`jq`/`yq`/`fx`.

## Filter language quick reference

A few JMESPath patterns ferrule users hit most often. The full
language is much larger — see [the JMESPath tutorial][tutorial].

[tutorial]: https://jmespath.org/tutorial.html

| Pattern | Meaning |
|---|---|
| `[*].field` | Project `field` from every element of the array |
| `[?field=='X']` | Filter elements where `field == 'X'` |
| `[?age > \`30\`]` | Filter where `age > 30` (note backticks for literals) |
| `length(@)` | Length of the current node |
| `sort_by(@, &field)` | Sort array by a field |
| `[0:5]` | First five elements |
| `{a: x, b: y}` | Build a new object literal |
| `@` | The current node (whole result) |

## Exit codes

- `0` — filter ran successfully and produced output (even if the
  filtered output is an empty array `[]`).
- `3` — JMESPath parse error, JMESPath evaluation error, or output
  could not be re-serialized as JSON. Same class as a SQL query
  failure.
