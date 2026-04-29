# Internal Idea Backlog

Unsorted ideas for ferrule features, refactors, and experiments.
Not a roadmap — more like a scratchpad.

## Migration Runner

A lightweight migration runner built on top of ferrule's existing backend abstraction.

### Pitch

`ferrule migrate up` / `ferrule migrate down` against any supported database (Postgres, MySQL, MSSQL, SQLite, Oracle). Each migration is a pair of `.sql` files (up/down) stamped with a timestamp prefix. A `ferrule_migrations` table tracks applied versions. No ORM, no DSL — just raw SQL that you own.

### Why it's interesting

Most migration tools are tied to a specific language or framework (Django, Rails, SQLx, Flyway). Ferrule already speaks five backends. A CLI-native migration tool would be framework-agnostic and backend-agnostic — write SQL once, run it anywhere.

### Rough design

- `ferrule migrate init` — create `ferrule_migrations` tracking table
- `ferrule migrate create <name>` — create `migrations/<timestamp>_<name>.{up,down}.sql`
- `ferrule migrate up [--target <version>]` — run pending up scripts
- `ferrule migrate down [--target <version>]` — run down scripts in reverse order
- `ferrule migrate status` — show applied / pending / broken

Transaction wrapping per backend (commit on success, rollback on error). Oracle and MSSQL require explicit `BEGIN TRAN` / `COMMIT` syntax variation; Postgres, MySQL, SQLite use standard `BEGIN` / `COMMIT`.

### Stack

- `ferrule-core` query executor (already handles multi-statement strings)
- `walkdir` or `glob` for collecting migration files
- `sha256` hash of file contents stored in tracking table for drift detection

### Timeline

Week project. Core runner in 2-3 days, the rest is edge cases and backend-specific transaction semantics.

---

*Add new ideas at the top so the newest stuff is visible first.*
