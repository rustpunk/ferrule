# Configuration

## Discovery Order

Ferrule loads configuration from (first found wins):

1. `--config <path>` CLI flag
2. `./.ferrule.toml` (project-local)
3. `~/.config/ferrule/ferrule.toml` (platform-appropriate)

## Example `.ferrule.toml`

```toml
[default]
format = "json"
limit = 1000

[connection.production]
url = "postgres://user@db.example.com/app"

[connection.staging]
url = "mysql://user@staging.internal/app"
```

## Environment Interpolation

Profile URLs can reference environment variables with `${VAR}`:

```toml
[connection.production]
url = "postgres://user@${DB_HOST}/app"
```

## Per-Profile Defaults

```toml
[connection.readonly]
url = "postgres://readonly@replica.example.com/app"
format = "table"   # override global default for this profile
limit = 50         # narrower default paging
```
