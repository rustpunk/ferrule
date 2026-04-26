# Schema Introspection

## List Tables

```bash
ferrule tables production
ferrule tables production --format json
```

Output:
```json
[
  {"table_name": "users"},
  {"table_name": "events"},
  {"table_name": "schema_migrations"}
]
```

## Describe Table

```bash
ferrule describe production users
```

Output:
```json
[
  {
    "column_name": "id",
    "data_type": "integer",
    "is_nullable": "NO",
    "column_default": "nextval('users_id_seq'::regclass)"
  },
  {
    "column_name": "email",
    "data_type": "character varying(255)",
    "is_nullable": "NO"
  }
]
```

> Note: Column default and nullable information varies by backend capability.
