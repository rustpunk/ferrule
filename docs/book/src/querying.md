# Querying Data

## Basic Queries

```bash
ferrule query "sqlite::memory:" "SELECT 1 + 1 AS answer;"
```

## Output Formats

| Format | Flag | Best For |
|--------|------|----------|
| Table | `--format table` | Human-readable terminal output |
| JSON | `--format json` | Piping to `jq` or APIs |
| CSV | `--format csv` | Spreadsheets, data pipelines |
| YAML | `--format yaml` | Human-readable structured data |
| Raw | `--format raw` | Simple columnar text |

Default: `table` when TTY, `json` when piped.

## Paging

Server-side `LIMIT` / `OFFSET` is injected when you pass `--limit` and `--offset`:

```bash
# Postgres, MySQL, SQLite
ferrule query db "SELECT * FROM users" --limit 25 --offset 50

# MSSQL uses OFFSET/FETCH syntax automatically
ferrule query mssql_db "SELECT * FROM users" --limit 25 --offset 50
```

Pass `--limit 0` to disable paging entirely (useful for multi-statement batches).

## Multi-Statement Batches

Backends that support it (Postgres, MSSQL) allow multiple statements separated by semicolons:

```bash
ferrule query production "
  INSERT INTO logs (msg) VALUES ('startup');
  SELECT COUNT(*) FROM logs;
"
```

> ⚠️ Multi-statement batches do not support `--limit` / `--offset`.

## Parameterized Queries

Use `${name}` placeholders in SQL and pass values via `--param`:

```bash
ferrule query production \
  'SELECT * FROM events WHERE severity = ${sev} AND created_at > ${date}' \
  --param "sev=error" \
  --param "date=2025-01-01"
```

Types are inferred automatically:
- `true` / `false` → boolean
- `-42` / `3.14` → numeric
- Anything else → string (properly quoted)

Load many parameters from JSON:

```bash
ferrule query production 'SELECT * FROM users WHERE id = ${id}' \
  --param-file params.json
```

Where `params.json` is:
```json
{"id": 42, "name": "Alice"}
```

## Dry Run

Preview the substituted SQL without executing:

```bash
ferrule query production 'SELECT * FROM users WHERE id = ${id}' \
  --param "id=42" --dry-run
```
