# Ferrule — Rust-native Database Query CLI

> The collar that joins you to your data.

## Project Identity

`ferrule` is a CLI tool for querying relational databases from the terminal. One static binary speaks Postgres, MySQL, MSSQL, and SQLite. Oracle is opt-in. No runtime client libraries required for the default backends.

## Architecture

Three-crate workspace:

- **`ferrule-core`** — Backend drivers (feature-gated), unified `Value`/`Row` types, `DatabaseUrl` parser, result formatters.
- **`ferrule-config`** — Connection registry, credential resolution stack, profiles.
- **`ferrule-cli`** — Binary crate. clap derive-based command tree, dispatch, output routing, exit codes.

## Tech Stack

- **Postgres**: `tokio-postgres` + `rustls` (pure Rust, zero runtime deps)
- **MySQL**: `mysql_async` (pure Rust)
- **MSSQL**: `tiberius` (pure Rust TDS)
- **SQLite**: `rusqlite` with `bundled` (statically linked SQLite)
- **Oracle**: `oracle` crate (ODPI-C, opt-in only; user must install Instant Client)
- **CLI**: `clap` derive API
- **Runtime**: `tokio` `current_thread`
- **Errors**: `thiserror` in libraries, `miette` in CLI
- **Secrets**: `secrecy::SecretString` everywhere. Never raw strings.
- **Formatting**: `tabled` + `serde_json` + `crossterm`

## Critical Conventions

- Zero runtime deps for default features. Oracle alone requires external Instant Client.
- All passwords wrapped in `SecretString` — zeroize on drop, redact in Debug.
- Connection URLs: password component redacted in every log/diagnostic.
- Exit codes: 0=success, 1=notable result (diff differences, future `--fail-on-empty`, future check/validate findings — GNU diff convention), 2=usage, 3=connection, 4=query error.
- `--format table` is default when TTY; `--format json` when piped.
- `--insecure` flag required to disable TLS verification. Warns on stderr.
- `#[tokio::main(flavor = "current_thread")]` in `main.rs`.
- `IndexMap` over `HashMap` where column/row order matters.
- No `unwrap()` in library crates. Return `Result`.
- `#[must_use]` on `Result`-returning functions.

## Build / Test / Lint

```bash
cargo build --workspace
cargo test --workspace
cargo clippy --workspace
cargo doc --workspace --no-deps
```

### Prerequisite: clone the sibling `hasp` repo

`ferrule-config` depends on `hasp` via a path dependency
(`hasp = { path = "../../hasp/crates/hasp", ... }`). Before the
workspace will resolve, clone `rustpunk/hasp` as a sibling of
`rustpunk/ferrule`:

```bash
git clone https://github.com/rustpunk/hasp.git ../hasp
sudo apt-get install -y libdbus-1-dev pkg-config   # for hasp's keyring backend on Linux
```

The crates.io `hasp` (`0.1.0-alpha`) is a name-reservation placeholder
and does not satisfy ferrule's feature requirements. Use the GitHub
source. If you build without `hasp/` present, cargo fails with
`failed to read /home/.../hasp/crates/hasp/Cargo.toml`.

## How to Test

### SQLite — no setup required

SQLite is the default backend and requires no external services. The integration
tests create `:memory:` or temporary file databases automatically. All `ferrule`
commands work out of the box against `sqlite:///path/to/db`.

### MySQL — start container first

The MySQL backend requires a running MySQL server. The inline tests at
`ferrule-core/src/backends/mysql.rs` connect to
`mysql://root:ferrule@127.0.0.1:13306/ferrule` and skip gracefully when the
container is absent.

```bash
docker run -d --name ferrule-mysql-test \
  -e MYSQL_ROOT_PASSWORD=ferrule \
  -e MYSQL_DATABASE=ferrule \
  -p 127.0.0.1:13306:3306 \
  mysql:8
```

Wait until ready, then seed:

```bash
until docker exec ferrule-mysql-test mysqladmin ping -h 127.0.0.1 -uroot -pferrule --silent >/dev/null 2>&1; do
  sleep 1
done

docker exec -i ferrule-mysql-test mysql -uroot -pferrule ferrule <<'SQL'
CREATE TABLE test_users (
  id INT AUTO_INCREMENT PRIMARY KEY,
  name VARCHAR(255),
  age INT,
  score DECIMAL(10,2),
  created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
  active BOOLEAN,
  meta JSON,
  uid CHAR(36) DEFAULT (UUID())
);
INSERT INTO test_users (name, age, score, active, meta) VALUES
  ('Alice', 30, 99.5,  TRUE,  '{"role": "admin"}'),
  ('Bob',   25, 88.25, FALSE, '{"role": "user"}');
CREATE TABLE test_orders (
  id INT AUTO_INCREMENT PRIMARY KEY,
  user_id INT,
  total DECIMAL(10,2),
  FOREIGN KEY (user_id) REFERENCES test_users(id) ON DELETE CASCADE
);
INSERT INTO test_orders (user_id, total) VALUES
  (1, 19.99), (1, 4.50), (2, 12.00);
SQL
```

`test_orders` adds a child table with an ON DELETE CASCADE FK back to
`test_users`. The Phase-1 introspection tests
(`Connection::primary_key` / `list_foreign_keys`) and the Phase-3
multi-table copy fixtures (`ferrule copy --all-tables`) both rely on
this edge to exercise FK ordering.

Smoke commands (mirrors the Postgres section):

```bash
ferrule query    "mysql://root:ferrule@127.0.0.1:13306/ferrule" "SELECT * FROM test_users;" --format json
ferrule query    "mysql://root:ferrule@127.0.0.1:13306/ferrule" "INSERT INTO test_users (name, age) VALUES ('Charlie', 35);"
ferrule query    "mysql://root:ferrule@127.0.0.1:13306/ferrule" \
  "INSERT INTO test_users (name, age) VALUES ('Dave', 40); SELECT COUNT(*) FROM test_users;" --format table
ferrule tables   "mysql://root:ferrule@127.0.0.1:13306/ferrule" --format table
ferrule describe "mysql://root:ferrule@127.0.0.1:13306/ferrule" test_users
ferrule conn test "mysql://root:ferrule@127.0.0.1:13306/ferrule"
```

Clean up when done:

```bash
docker stop ferrule-mysql-test && docker rm ferrule-mysql-test
```

### Postgres — start container first

The Postgres backend requires a running Postgres server for runtime validation.
Use the following Docker one-liner:

```bash
docker run -d --name ferrule-pg-test \
  -e POSTGRES_PASSWORD=ferrule \
  -e POSTGRES_USER=ferrule \
  -e POSTGRES_DB=ferrule \
  -p 127.0.0.1:15432:5432 \
  postgres:17-alpine
```

Wait for the container to be ready, then seed it:

```bash
PGPASSWORD=ferrule psql -h 127.0.0.1 -p 15432 -U ferrule -d ferrule -c "
CREATE TABLE test_users (
  id SERIAL PRIMARY KEY,
  name TEXT,
  age INT,
  score NUMERIC(10,2),
  created_at TIMESTAMP WITH TIME ZONE DEFAULT NOW(),
  active BOOLEAN,
  meta JSONB,
  uid UUID DEFAULT gen_random_uuid()
);
INSERT INTO test_users (name, age, score, active, meta)
  VALUES ('Alice', 30, 99.5, true, '{\"role\": \"admin\"}'),
         ('Bob', 25, 88.25, false, '{\"role\": \"user\"}');
CREATE TABLE test_orders (
  id SERIAL PRIMARY KEY,
  user_id INT REFERENCES test_users(id) ON DELETE CASCADE,
  total NUMERIC(10,2)
);
INSERT INTO test_orders (user_id, total) VALUES
  (1, 19.99), (1, 4.50), (2, 12.00);
"
```

`test_orders` adds a child table with an `ON DELETE CASCADE` FK back
to `test_users`. The Phase-1 introspection tests
(`Connection::primary_key` / `list_foreign_keys`) and the Phase-3
multi-table copy fixtures (`ferrule copy --all-tables`) both rely on
this edge to exercise FK ordering.

Verify the three backend paths (extended protocol, single DML, multi-statement):

```bash
# 1. Single SELECT — typed values via extended protocol
ferrule query "postgres://ferrule:ferrule@127.0.0.1:15432/ferrule?sslmode=disable" \
  "SELECT * FROM test_users;" --format json

# 2. Single DML — execute path, "N rows affected" on stderr
ferrule query "postgres://ferrule:ferrule@127.0.0.1:15432/ferrule?sslmode=disable" \
  "INSERT INTO test_users (name, age) VALUES ('Charlie', 35);"

# 3. Multi-statement — mixed DML and SELECT
ferrule query "postgres://ferrule:ferrule@127.0.0.1:15432/ferrule?sslmode=disable" \
  "INSERT INTO test_users (name, age) VALUES ('Dave', 40); SELECT COUNT(*) FROM test_users;" \
  --format table

# 4. Auxiliary commands
ferrule tables   "postgres://ferrule:ferrule@127.0.0.1:15432/ferrule?sslmode=disable" --format table
ferrule describe "postgres://ferrule:ferrule@127.0.0.1:15432/ferrule?sslmode=disable" test_users
ferrule conn test "postgres://ferrule:ferrule@127.0.0.1:15432/ferrule?sslmode=disable"
```

Clean up when done:

```bash
docker stop ferrule-pg-test && docker rm ferrule-pg-test
```

### MSSQL — start container first

The MSSQL backend requires a running SQL Server instance. The inline tests at
`ferrule-core/src/backends/mssql.rs` connect to
`mssql://sa:Ferrule123!@127.0.0.1:11433/ferrule` and skip gracefully when the
container is absent. The credentials and port are pinned to that constant —
do not change them without also updating `TEST_MSSQL_URL`.

```bash
docker run -d --name ferrule-mssql-test \
  -e ACCEPT_EULA=Y \
  -e MSSQL_SA_PASSWORD='Ferrule123!' \
  -e MSSQL_PID=Developer \
  -p 127.0.0.1:11433:1433 \
  mcr.microsoft.com/mssql/server:2022-latest
```

Notes:
- The `sa` password must satisfy SQL Server's complexity policy (≥8 chars,
  three of: upper/lower/digit/special). `Ferrule123!` does.
- `MSSQL_PID=Developer` selects the free Developer edition — full feature
  set, no production license.
- `2022-latest` ships `sqlcmd` at `/opt/mssql-tools18/bin/sqlcmd` and
  defaults to a self-signed TLS cert (hence the `-C` / `trustServerCertificate`
  flags below).

Wait until ready (the image takes ~15–25s on first run; longer on slow disks),
then create the database and seed it:

```bash
until docker exec ferrule-mssql-test /opt/mssql-tools18/bin/sqlcmd \
  -S localhost -U sa -P 'Ferrule123!' -C -Q "SELECT 1" >/dev/null 2>&1; do
  sleep 1
done

docker exec -i ferrule-mssql-test /opt/mssql-tools18/bin/sqlcmd \
  -S localhost -U sa -P 'Ferrule123!' -C <<'SQL'
IF DB_ID('ferrule') IS NULL CREATE DATABASE ferrule;
GO
USE ferrule;
GO
CREATE TABLE test_users (
  id INT IDENTITY(1,1) PRIMARY KEY,
  name NVARCHAR(255),
  age INT,
  score DECIMAL(10,2),
  created_at DATETIMEOFFSET DEFAULT SYSDATETIMEOFFSET(),
  active BIT,
  meta NVARCHAR(MAX),
  uid UNIQUEIDENTIFIER DEFAULT NEWID()
);
INSERT INTO test_users (name, age, score, active, meta) VALUES
  ('Alice', 30, 99.5,  1, '{"role": "admin"}'),
  ('Bob',   25, 88.25, 0, '{"role": "user"}');
GO
CREATE TABLE test_orders (
  id INT IDENTITY(1,1) PRIMARY KEY,
  user_id INT FOREIGN KEY REFERENCES test_users(id) ON DELETE CASCADE,
  total DECIMAL(10,2)
);
INSERT INTO test_orders (user_id, total) VALUES
  (1, 19.99), (1, 4.50), (2, 12.00);
GO
SQL
```

`test_orders` is the child table used by Phase-1 introspection tests
(`Connection::primary_key` / `list_foreign_keys`) and the Phase-3
multi-table copy fixtures.

Schema deviations from Postgres: MSSQL has no native `BOOLEAN` (use `BIT`),
no native JSON type (store JSON in `NVARCHAR(MAX)` — the type-mapping test
at `mssql.rs` already accepts `Json | String`), and uses `UNIQUEIDENTIFIER`
in place of `UUID`. `DATETIMEOFFSET` is the closest analog to Postgres
`TIMESTAMP WITH TIME ZONE`.

Smoke commands (use `?trustServerCertificate=true` because the image's TLS
cert is self-signed):

```bash
ferrule query    "mssql://sa:Ferrule123!@127.0.0.1:11433/ferrule?trustServerCertificate=true" "SELECT * FROM test_users;" --format json
ferrule query    "mssql://sa:Ferrule123!@127.0.0.1:11433/ferrule?trustServerCertificate=true" "INSERT INTO test_users (name, age) VALUES ('Charlie', 35);"
ferrule tables   "mssql://sa:Ferrule123!@127.0.0.1:11433/ferrule?trustServerCertificate=true" --format table
ferrule describe "mssql://sa:Ferrule123!@127.0.0.1:11433/ferrule?trustServerCertificate=true" test_users
ferrule conn test "mssql://sa:Ferrule123!@127.0.0.1:11433/ferrule?trustServerCertificate=true"
```

Clean up when done:

```bash
docker stop ferrule-mssql-test && docker rm ferrule-mssql-test
```

### Oracle — start container AND install Instant Client

Oracle has *two* setup steps the other backends don't: the container **and**
the host-side Oracle Instant Client. The `oracle` crate is a thin wrapper
around ODPI-C, which `dlopen`s `libclntsh.so` on the *first* `oracle://`
connection — `cargo build --features oracle` does not require Instant Client,
but the first runtime connection does.

The Oracle backend (`ferrule-core/src/backends/oracle.rs`) is fully
implemented, including an explicit Instant-Client-missing diagnostic in
`map_oracle_error` (oracle.rs). Inline tests at oracle.rs:296-445
use the `ORACLE_TEST_URL` environment variable (defaults to
`oracle://ferrule:ferrule@127.0.0.1:11521/FREEPDB1`) and use the same
`try_connect()` graceful-skip pattern as `mysql.rs` / `mssql.rs`. They
include a dedicated `test_oracle_missing_client_error` that asserts the
diagnostic fires when no client / no DB is reachable.

#### Container (gvenzl/oracle-free)

```bash
docker run -d --name ferrule-oracle-test \
  -e ORACLE_PASSWORD=ferrule \
  -e APP_USER=ferrule \
  -e APP_USER_PASSWORD=ferrule \
  -p 127.0.0.1:11521:1521 \
  gvenzl/oracle-free:latest
```

Behaviour:
- `ORACLE_PASSWORD` sets the `SYS` / `SYSTEM` password (admin only — ferrule
  tests do not use these).
- `APP_USER` + `APP_USER_PASSWORD` create a regular user inside pluggable
  database `FREEPDB1`. This is the user ferrule connects as.
- Service name is `FREEPDB1` (the default PDB on Oracle Database Free 23ai).
- First boot is slow (~60–120s) while Oracle initialises the PDB.

Wait until ready, then seed:

```bash
until docker exec ferrule-oracle-test ./healthcheck.sh >/dev/null 2>&1; do
  sleep 2
done

docker exec -i ferrule-oracle-test \
  sqlplus -S ferrule/ferrule@//localhost:1521/FREEPDB1 <<'SQL'
CREATE TABLE test_users (
  id NUMBER GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
  name VARCHAR2(255),
  age NUMBER(10),
  score NUMBER(10,2),
  created_at TIMESTAMP WITH TIME ZONE DEFAULT SYSTIMESTAMP,
  active NUMBER(1),
  meta CLOB CONSTRAINT meta_is_json CHECK (meta IS JSON),
  guid RAW(16) DEFAULT SYS_GUID()
);
INSERT INTO test_users (name, age, score, active, meta) VALUES ('Alice', 30, 99.5,  1, '{"role": "admin"}');
INSERT INTO test_users (name, age, score, active, meta) VALUES ('Bob',   25, 88.25, 0, '{"role": "user"}');
CREATE TABLE test_orders (
  id NUMBER GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
  user_id NUMBER,
  total NUMBER(10,2),
  CONSTRAINT test_orders_user_fk FOREIGN KEY (user_id)
    REFERENCES test_users(id) ON DELETE CASCADE
);
INSERT INTO test_orders (user_id, total) VALUES (1, 19.99);
INSERT INTO test_orders (user_id, total) VALUES (1,  4.50);
INSERT INTO test_orders (user_id, total) VALUES (2, 12.00);
COMMIT;
EXIT
SQL
```

`test_orders` mirrors the other backends' child-table fixture for the
Phase-1 introspection tests and the Phase-3 `--all-tables` copy
smokes. Note Oracle requires the constraint to be named (no anonymous
inline `FOREIGN KEY ... REFERENCES`).

Schema deviations from Postgres: Oracle has no native `BOOLEAN` until 23c
(`NUMBER(1)`), no `UUID` type (use `RAW(16)` + `SYS_GUID()` -- note `guid` not
`uid` since `uid` is an Oracle reserved keyword), and JSON is a `CLOB` with
an `IS JSON` check constraint (Oracle 12c+).

#### Host-side Oracle client libraries (Linux x64)

Without `libclntsh.so` on the host, the `oracle` crate fails at first
connection and `ferrule` will surface a `miette` diagnostic (see "Oracle
Runtime Behavior" below). ODPI-C accepts client libs from any of: Oracle
Instant Client Basic, Instant Client Basic Light, a full Oracle Database
Client install, or a full Oracle Database install.

**Recommended: Oracle Instant Client Basic**

1. Go to <https://www.oracle.com/database/technologies/instant-client/linux-x64-downloads.html>,
   accept the license agreement, and download the **Basic** package for your
   architecture (`instantclient-basic-linux.x64-<version>.zip`).
2. Extract and install:

```bash
sudo apt-get install -y libaio1t64 || sudo apt-get install -y libaio1
mkdir -p ~/opt/oracle
unzip -q instantclient-basic-linux.x64-*.zip -d ~/opt/oracle
# Symlink libaio if Ubuntu 24.04+ renamed it:
ln -sf /usr/lib/x86_64-linux-gnu/libaio.so.1t64 \
       ~/opt/oracle/instantclient_23_26/libaio.so.1
export LD_LIBRARY_PATH="$HOME/opt/oracle/instantclient_23_26:$LD_LIBRARY_PATH"
```

> **Note:** `libaio1` is required by `libclntsh.so`. On Ubuntu 24.04+ the
> package was renamed `libaio1t64`; the symlink above lets the Instant
> Client find it even when the dynamic linker cache does not include it.

Verify:

```bash
ldd ~/opt/oracle/instantclient_23_26/libclntsh.so | grep "not found"   # should print nothing
ls ~/opt/oracle/instantclient_*   # e.g. instantclient_23_26
```

**Not recommended:** Extracting `libclntsh.so` from a running database
container (e.g. `gvenzl/oracle-free`) and using it on the host will
usually **segfault** during ODPI-C init (`SIGSEGV` at `NULL`).
Database-home libraries expect the full Oracle home directory hierarchy
(`$ORACLE_HOME/network/admin`, `$ORACLE_HOME/nls/data`, etc.) and cannot
be used standalone. Use the official Instant Client package instead.

#### Smoke commands

```bash
ferrule query    "oracle://ferrule:ferrule@127.0.0.1:11521/FREEPDB1" "SELECT * FROM test_users" --format json
ferrule tables   "oracle://ferrule:ferrule@127.0.0.1:11521/FREEPDB1" --format table
ferrule describe "oracle://ferrule:ferrule@127.0.0.1:11521/FREEPDB1" test_users
ferrule conn test "oracle://ferrule:ferrule@127.0.0.1:11521/FREEPDB1"
```

Run the inline integration tests (will skip if container or Instant Client
missing):

```bash
cargo test -p ferrule-core --features oracle -- oracle::tests
```

Clean up when done:

```bash
docker stop ferrule-oracle-test && docker rm ferrule-oracle-test
```

### SSH tunnel — start an OpenSSH server in Docker

Wave 3 B3 wires `--ssh-tunnel`, `--ssh-key`, and the profile keys
(`ssh_host`, `ssh_user`, `ssh_port`, `ssh_key`) through a russh client
that opens a `direct-tcpip` channel and either pipes the russh
`ChannelStream` straight into `tokio_postgres::Config::connect_raw`
(Postgres), or binds a local TCP listener and forwards bytes through
the SSH session (every other backend). The `ssh` Cargo feature is
opt-in: build with `--features ferrule-cli/ssh` (or `--features all`).

Manual integration test: pair a `linuxserver/openssh-server` container
with the existing Postgres container and confirm `ferrule conn test`
goes through the tunnel.

```bash
# 1. Start an SSH server with key auth (no password auth — ferrule
#    requires keys per project policy).
docker run -d --name ferrule-ssh-test -p 127.0.0.1:12222:2222 \
  -e USER_NAME=ferrule \
  -e PUBLIC_KEY="$(cat ~/.ssh/id_ed25519.pub)" \
  -e PASSWORD_ACCESS=false \
  linuxserver/openssh-server

# 2. Wait for the server to come up.
until docker logs ferrule-ssh-test 2>&1 | grep -q "Server listening on"; do
  sleep 1
done

# 3. (Re)start the Postgres container from the Postgres section above
#    if it is not already running.

# 4. Connect through the bastion. The Postgres container itself stays
#    on 127.0.0.1:15432 — but with --ssh-tunnel, ferrule first opens
#    an SSH session to 127.0.0.1:12222 and then asks the bastion to
#    open a direct-tcpip channel to 127.0.0.1:15432.
ferrule conn test \
  --ssh-tunnel ferrule@127.0.0.1:12222 \
  --ssh-key ~/.ssh/id_ed25519 \
  "postgres://ferrule:ferrule@127.0.0.1:15432/ferrule?sslmode=disable"

ferrule query \
  --ssh-tunnel ferrule@127.0.0.1:12222 \
  --ssh-key ~/.ssh/id_ed25519 \
  "postgres://ferrule:ferrule@127.0.0.1:15432/ferrule?sslmode=disable" \
  "SELECT name, age FROM test_users" --format json
```

Expected: `[ferrule] Warning: SSH host key verification is disabled`
on stderr (TOFU/known_hosts is staged separately), then the query
result on stdout. Clean up:

```bash
docker stop ferrule-ssh-test && docker rm ferrule-ssh-test
```

Notes:
- The `linuxserver/openssh-server` image listens on `2222` inside the
  container by default (not 22), which is why the host port mapping
  is `12222:2222`.
- `PASSWORD_ACCESS=false` is required because ferrule's russh client
  only attempts publickey auth — password auth is intentionally not
  implemented to keep the credential surface narrow.
- For other backends (MySQL / MSSQL), swap the URL scheme. Ferrule
  picks the `LocalListener` transport automatically; the database
  driver sees `127.0.0.1:<random>` instead of the original host.

### Cross-DB copy — smoke against the seeded containers

`ferrule copy <SRC> <DST>` streams rows between any pair of backends.
Phase 1 uses a generic batched-INSERT path on the destination;
backend-native bulk loaders (PG `COPY FROM STDIN`, MySQL `LOAD DATA`,
MSSQL `BULK INSERT`, Oracle direct-path) are tracked separately.

Default conflict policy is non-destructive: ferrule refuses to copy
into a non-empty existing target unless `--if-exists append` or
`--if-exists truncate` is set. Truncate also requires `--yes` from a
TTY.

Type translation lives in `ferrule_core::copy::translate_type`. See
`docs/src/copy.md` for the full mapping table.

Smoke against the existing Postgres + SQLite setup (no new container
needed — uses the `test_users` table seeded in the Postgres section
above):

```bash
# Snapshot Postgres → SQLite, creating the target table from source DDL.
ferrule copy \
  "postgres://ferrule:ferrule@127.0.0.1:15432/ferrule?sslmode=disable" \
  "sqlite:///tmp/ferrule-copy-smoke.db" \
  --table test_users --create-table

# Verify the round-trip.
ferrule query "sqlite:///tmp/ferrule-copy-smoke.db" \
  "SELECT count(*) FROM test_users" --format json

# Try the default-conflict guardrail (should fail with a hint).
ferrule copy \
  "postgres://ferrule:ferrule@127.0.0.1:15432/ferrule?sslmode=disable" \
  "sqlite:///tmp/ferrule-copy-smoke.db" \
  --table test_users
# → "Target table 'test_users' already contains rows. Pass --if-exists ..."

# Refresh: clear and reload.
ferrule copy \
  "postgres://ferrule:ferrule@127.0.0.1:15432/ferrule?sslmode=disable" \
  "sqlite:///tmp/ferrule-copy-smoke.db" \
  --table test_users --if-exists truncate --yes

# --query form: project a subset and copy into a new target table.
ferrule copy \
  "postgres://ferrule:ferrule@127.0.0.1:15432/ferrule?sslmode=disable" \
  "sqlite:///tmp/ferrule-copy-smoke.db" \
  --query "SELECT id, name FROM test_users WHERE active = true" \
  --into active_users --create-table

rm /tmp/ferrule-copy-smoke.db
```

Bulk-native paths (`--bulk-native auto|on`) require a destination
backend with a real bulk loader — see `docs/src/copy.md` for the
matrix. The flag is destination-only; SQLite stays on the generic
path. Smoke recipe (Postgres → Postgres against the same seeded
container, since PG is the only backend that has a `COPY` source
that ferrule's source-side SELECT already exercises):

```bash
# Bulk-on path: Postgres native COPY ... FROM STDIN.
ferrule copy \
  "postgres://ferrule:ferrule@127.0.0.1:15432/ferrule?sslmode=disable" \
  "postgres://ferrule:ferrule@127.0.0.1:15432/ferrule?sslmode=disable" \
  --query "SELECT * FROM test_users" --into bulk_pg_smoke \
  --create-table --bulk-native on

# Auto path: chooses native if available, falls back if not.
ferrule copy \
  "postgres://ferrule:ferrule@127.0.0.1:15432/ferrule?sslmode=disable" \
  "sqlite:///tmp/ferrule-copy-bulk.db" \
  --table test_users --create-table --bulk-native auto
# → SQLite has no native loader; auto silently falls back to INSERT.

# `on` against a no-bulk backend is a hard error referencing
# --bulk-native, useful in CI to verify the env supports bulk.
ferrule copy \
  "postgres://ferrule:ferrule@127.0.0.1:15432/ferrule?sslmode=disable" \
  "sqlite:///tmp/ferrule-copy-bulk.db" \
  --table test_users --create-table --bulk-native on
# → "--bulk-native=on but Sqlite bulk path unavailable: ..."

rm -f /tmp/ferrule-copy-bulk.db
```

For MySQL `LOAD DATA LOCAL INFILE`, the server-side
`local_infile=ON` must be set (default is OFF in MySQL 8.0+):

```bash
docker exec ferrule-mysql-test mysql -uroot -pferrule -e "SET GLOBAL local_infile = ON"
ferrule copy \
  "postgres://ferrule:ferrule@127.0.0.1:15432/ferrule?sslmode=disable" \
  "mysql://root:ferrule@127.0.0.1:13306/ferrule" \
  --table test_users --create-table --bulk-native on
```

Inline tests at `ferrule-core/src/copy.rs` cover the SQLite → SQLite
round trip, the default-conflict refusal, the truncate-replaces-rows
path, the query+into mode, and the `BulkMode` dispatcher (Off / Auto /
On behaviour against a tracking wrapper around SQLite). Per-backend
bulk encoders and bulk_insert_rows round trips are covered in each
backend module's inline tests. Cross-backend integration tests are
deferred — the existing per-backend test fixtures already cover both
sides of the type translation in isolation.

### Backend test status

- **SQLite, Postgres, MySQL, MSSQL, Oracle** — runtime-tested via the Docker
  setups above. Inline integration tests in each backend module skip
  gracefully when the container is absent.
- **Oracle** additionally requires Oracle Instant Client on the host
  (`libclntsh.so` / `.dylib` / `.dll`) for the runtime tests to actually
  execute — see the Oracle section above. `cargo build --features oracle`
  itself does not need Instant Client.

## Wave Structure

- **Wave 1** — Core query flow: `conn`, `query`, `tables`, `describe`, multi-statement, JSON/CSV/table output.
- **Wave 1.5** — Connection pooling daemon, paging, config profiles, env interpolation, `.ferrule.toml`.
- **Wave 2** — Interactive REPL, query bookmarks, parameterized queries, explain, dump/load, watch mode.
- **Wave 3** — Schema diff, migration runner, SSH tunnels, k8s port-forward, daemon mode.

## Oracle Runtime Behavior

- ODPI-C lazily loads `libclntsh` on first Oracle connection attempt.
- If Instant Client is missing: emit `miette` diagnostic with download link and `LD_LIBRARY_PATH` help.
- Non-Oracle connections continue to work normally.

## Credential Resolution Stack

1. `--password` CLI flag
2. `FERRULE_<NAME>_PASSWORD` env var
3. OS keyring (`service=ferrule`, `user=<name>`)
4. Interactive prompt (TTY only)
5. Fail with diagnostic

## Result Type Unification

Every backend maps native types to `ferrule_core::value::Value` enum:

```rust
pub enum Value {
    Null, Bool, Int64, Float64, Decimal, String, Bytes,
    Date, Time, DateTime, DateTimeTz, Json, Uuid, Array,
}
```

The formatter layer never sees driver-specific types.

## Stubbing Policy

This scaffold contains many `todo!()` bodies. Each crate root has `#![allow(dead_code, unused_variables, unused_imports)]` so that `cargo clippy` passes. Do not remove these pragmas until Wave 1 is complete.
