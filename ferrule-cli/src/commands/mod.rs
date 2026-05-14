pub mod bookmark;
pub mod conn;
pub mod copy;
pub mod describe;
pub mod diff;
pub mod dump;
pub mod explain;
pub mod export;
pub mod load;
pub mod migrate;
pub mod query;
pub mod repl;
pub mod resolver;
pub mod tables;
pub mod watch;

pub use bookmark::BookmarkArgs;
pub use dump::DumpArgs;
pub use explain::ExplainArgs;
pub use export::ExportArgs;
pub use load::LoadArgs;
pub use migrate::MigrateArgs;
pub use repl::ReplArgs;
pub use resolver::{check_daemon_ssh_compat, connect_resolved, resolve_connection};
pub use watch::WatchArgs;

use clap::{Args, Subcommand, ValueEnum};
use ferrule_config::profile::GlobalConfig;
use ferrule_core::copy::{BulkMode, CopyFormat};

/// CLI-side representation of [`BulkMode`]. Derived from
/// `clap::ValueEnum` so `--help` enumerates the valid values and
/// `--bulk-native=invalid` is rejected by clap with a usage error
/// (exit code 2), rather than the runtime `BulkMode::parse` path
/// returning `None` and surfacing as `CliError::usage` later.
#[derive(Clone, Copy, Debug, ValueEnum)]
pub enum BulkNativeMode {
    /// Always use the generic multi-row INSERT path. v1 default.
    Off,
    /// Try the native bulk path; on `BulkUnavailable` fall back to
    /// generic INSERT for that batch (one stderr warning per
    /// fallback).
    Auto,
    /// Require the native bulk path. `BulkUnavailable` becomes a
    /// hard error referencing this flag.
    On,
}

impl From<BulkNativeMode> for BulkMode {
    fn from(arg: BulkNativeMode) -> Self {
        match arg {
            BulkNativeMode::Off => BulkMode::Off,
            BulkNativeMode::Auto => BulkMode::Auto,
            BulkNativeMode::On => BulkMode::On,
        }
    }
}

/// CLI-side representation of [`CopyFormat`]. Postgres-only; other
/// destination backends silently ignore the flag.
#[derive(Clone, Copy, Debug, ValueEnum)]
pub enum CopyFormatArg {
    /// `COPY … WITH (FORMAT TEXT)` — the v1 default. Tab-separated
    /// wire format; encoded by ferrule's tiny in-crate encoder.
    Text,
    /// `COPY … WITH (FORMAT BINARY)` — opt-in. Streamed via
    /// `tokio_postgres::binary_copy`. Faster on numeric / timestamp /
    /// UUID-heavy schemas; at-best break-even on TEXT / JSONB / BYTEA-
    /// heavy ones because typed length prefixes inflate small payloads.
    Binary,
}

impl From<CopyFormatArg> for CopyFormat {
    fn from(arg: CopyFormatArg) -> Self {
        match arg {
            CopyFormatArg::Text => CopyFormat::Text,
            CopyFormatArg::Binary => CopyFormat::Binary,
        }
    }
}

/// Common flags shared by query-like commands.
#[derive(Args, Clone, Debug)]
pub struct OutputFlags {
    /// Output format
    #[arg(short, long, value_name = "FORMAT")]
    pub format: Option<String>,

    /// Write results to file
    #[arg(short, long, value_name = "FILE")]
    pub output: Option<String>,

    /// Limit number of rows returned
    #[arg(short = 'n', long, value_name = "N")]
    pub limit: Option<usize>,

    /// Skip first N rows
    #[arg(long, value_name = "N")]
    pub offset: Option<usize>,

    /// Show execution timing
    #[arg(long)]
    pub timing: bool,

    /// Verbose output
    #[arg(short, long)]
    pub verbose: bool,
}

impl OutputFlags {
    /// Merge CLI flags with global config defaults.
    pub fn resolve_format(&self, global_config: &GlobalConfig) -> ferrule_core::OutputFormat {
        self.format
            .as_deref()
            .and_then(ferrule_core::OutputFormat::parse)
            .unwrap_or_else(|| {
                ferrule_core::OutputFormat::parse(&global_config.resolve_format(None))
                    .unwrap_or_else(crate::output::default_format)
            })
    }

    pub fn resolve_limit(&self, global_config: &GlobalConfig) -> Option<usize> {
        self.limit.or(global_config.resolve_limit(None))
    }
}

/// Connection flags shared by query-like commands.
#[derive(Args, Clone, Debug)]
pub struct ConnectionFlags {
    /// Disable TLS certificate verification (warns on stderr).
    #[arg(long)]
    pub insecure: bool,

    /// Route through the connection pooling daemon.
    #[arg(long)]
    pub daemon: bool,

    /// Open the connection through an SSH tunnel.
    ///
    /// Accepts `[user@]host[:port]` (matches pgcli). User defaults to
    /// `$USER`; port defaults to 22. Overrides the corresponding
    /// `ssh_host` / `ssh_user` / `ssh_port` profile keys atomically.
    #[arg(long, value_name = "USER@HOST[:PORT]")]
    pub ssh_tunnel: Option<String>,

    /// Path to the SSH private key for the tunnel.
    ///
    /// Overrides the profile's `ssh_key`. When neither this flag nor
    /// the profile sets a key, the tunnel layer falls back to
    /// `~/.ssh/id_ed25519`, `~/.ssh/id_rsa`, then `SSH_AUTH_SOCK`.
    #[arg(long, value_name = "PATH")]
    pub ssh_key: Option<String>,

    /// HTTP CONNECT proxy URL (e.g. `http://proxy:8080`).
    ///
    /// May include Basic-auth credentials: `http://user:pass@proxy:8080`.
    /// Overrides the corresponding `proxy_url` profile key and the
    /// `HTTP_PROXY` / `HTTPS_PROXY` / `ALL_PROXY` / `NO_PROXY`
    /// environment variables.
    #[arg(long, value_name = "URL")]
    pub proxy_url: Option<String>,
}

/// Connection management subcommands.
#[derive(Args, Clone, Debug)]
pub struct ConnArgs {
    #[command(subcommand)]
    pub command: ConnCommand,
}

#[derive(Subcommand, Clone, Debug)]
pub enum ConnCommand {
    /// Add a named connection
    Add { name: String, url: String },
    /// List saved connections
    List,
    /// Remove a connection
    Remove { name: String },
    /// Test a connection
    Test {
        name: String,
        #[command(flatten)]
        conn_flags: ConnectionFlags,
    },
    /// Store a password in the OS keyring
    SetPassword { name: String },
    /// Remove a password from the OS keyring
    DeletePassword { name: String },
    /// Start the connection pooling daemon
    Start {
        /// Run in background
        #[arg(long)]
        background: bool,
    },
    /// Stop the connection pooling daemon
    Stop,
    /// Show daemon status
    Status,
    /// Restart the connection pooling daemon
    Restart,
}

/// Query command arguments.
#[derive(Args, Clone, Debug)]
pub struct QueryArgs {
    /// Connection name or raw URL
    pub connection: String,

    /// SQL statement (or use --file / --stdin)
    pub sql: Option<String>,

    /// Read SQL from file
    #[arg(long)]
    pub file: Option<String>,

    /// Read SQL from stdin
    #[arg(long)]
    pub stdin: bool,

    /// Named parameter (repeatable)
    #[arg(long = "param", value_name = "NAME=VALUE")]
    pub params: Vec<String>,

    /// Read parameters from a JSON file
    #[arg(long, value_name = "PATH")]
    pub param_file: Option<String>,

    /// Explain the query instead of executing it
    #[arg(long)]
    pub explain: bool,

    #[command(flatten)]
    pub output: OutputFlags,

    #[command(flatten)]
    pub conn_flags: ConnectionFlags,

    /// Connection password
    #[arg(short = 'p', long)]
    pub password: Option<String>,

    /// JMESPath expression applied to JSON output before printing.
    ///
    /// Implies --format json. Errors fail with exit code 4 (query class).
    #[arg(long, value_name = "EXPR")]
    pub filter: Option<String>,

    /// Dry run — print without executing
    #[arg(long)]
    pub dry_run: bool,

    /// Watch a file for changes instead of polling on interval
    #[arg(long, value_name = "PATH")]
    pub watch_file: Option<std::path::PathBuf>,

    /// Watch mode — re-run the query periodically
    #[arg(long)]
    pub watch: bool,

    /// Watch interval in seconds (default: 5)
    #[arg(long, value_name = "SECS", default_value_t = 5)]
    pub watch_interval: u64,
}

/// Tables command arguments.
#[derive(Args, Clone, Debug)]
pub struct TablesArgs {
    /// Connection name
    pub connection: String,

    #[command(flatten)]
    pub output: OutputFlags,

    #[command(flatten)]
    pub conn_flags: ConnectionFlags,
}

/// Describe command arguments.
#[derive(Args, Clone, Debug)]
pub struct DescribeArgs {
    /// Connection name
    pub connection: String,

    /// Table name
    pub table: String,

    #[command(flatten)]
    pub output: OutputFlags,

    #[command(flatten)]
    pub conn_flags: ConnectionFlags,
}

/// Copy command arguments — stream rows from a source DB into a target DB.
#[derive(Args, Clone, Debug)]
pub struct CopyArgs {
    /// Source connection: registry name or raw URL.
    pub source: String,

    /// Destination connection: registry name or raw URL.
    pub dest: String,

    /// Whole-table mode: copy `<table>` from source to dest. Mutually
    /// exclusive with `--query` and `--all-tables`.
    #[arg(long, value_name = "NAME", conflicts_with_all = ["query", "all_tables"])]
    pub table: Option<String>,

    /// Query mode: run this SELECT against the source. Requires
    /// `--into NAME` for the target table.
    #[arg(long, value_name = "SQL", requires = "into", conflicts_with = "all_tables")]
    pub query: Option<String>,

    /// Target table name when using `--query`.
    #[arg(long, value_name = "NAME")]
    pub into: Option<String>,

    /// Schema-level mode: discover every table on the source and copy
    /// each one in foreign-key-respecting order (parents before
    /// children). Mutually exclusive with `--table` and `--query`.
    /// Pair with `--if-exists truncate` for the "refresh dev from
    /// prod" workflow; `--yes` is required at most once per copy, not
    /// per table.
    #[arg(long)]
    pub all_tables: bool,

    /// Repeatable glob (shell-style `*` / `?`) restricting which
    /// tables `--all-tables` includes. Default: include every table.
    /// Case-sensitive against the identifier shape the source returns.
    #[arg(long, value_name = "PATTERN")]
    pub include: Vec<String>,

    /// Repeatable glob (shell-style `*` / `?`) excluding tables from
    /// `--all-tables`. Applied after `--include`.
    #[arg(long, value_name = "PATTERN")]
    pub exclude: Vec<String>,

    /// Tolerate foreign-key cycles in `--all-tables` mode by copying
    /// in discovery order. FK violations may surface as driver
    /// errors; useful when targets have deferrable FKs or you plan
    /// to rebuild constraints after the copy.
    #[arg(long)]
    pub no_fk_check: bool,

    /// Translate source column metadata into a CREATE TABLE on the
    /// target if it does not yet exist.
    #[arg(long)]
    pub create_table: bool,

    /// When set with `--create-table`, also lift the source table's
    /// declared primary key into the emitted DDL. Default off keeps
    /// the v1 contract that `--create-table` is data-movement, not
    /// schema migration. Best-effort: source tables with no PK still
    /// get the column-only DDL. Ignored in `--query` mode.
    #[arg(long, requires = "create_table")]
    pub preserve_pk: bool,

    /// What to do if the target table already contains rows.
    /// `error` (default, non-destructive), `append`, `truncate`,
    /// `skip` (PK-driven `ON CONFLICT DO NOTHING` / `INSERT IGNORE` /
    /// `MERGE … WHEN NOT MATCHED`), or `upsert` (`ON CONFLICT DO UPDATE`
    /// / `ON DUPLICATE KEY UPDATE` / full `MERGE`). `skip` and
    /// `upsert` require conflict columns — declared PK on the
    /// destination, or `--key COL[,COL...]`.
    #[arg(long, value_name = "STRATEGY", default_value = "error")]
    pub if_exists: String,

    /// Override the conflict-key column list for `--if-exists
    /// skip|upsert`. Repeatable or comma-separated. Useful when the
    /// destination has no declared primary key or when conflict
    /// resolution should key on a unique index that isn't the PK.
    /// Ignored (with a one-line stderr notice) for other strategies.
    #[arg(long, value_name = "COL", value_delimiter = ',')]
    pub key: Vec<String>,

    /// Required confirmation for destructive `--if-exists truncate`
    /// when stdin is a TTY.
    #[arg(long)]
    pub yes: bool,

    /// Wrap the entire copy in a single target-side transaction.
    /// Recommended for snapshots; avoid for million-row migrations.
    #[arg(long)]
    pub atomic: bool,

    /// Source page / target INSERT batch size.
    #[arg(long, value_name = "N", default_value_t = 1000)]
    pub batch: usize,

    /// Route INSERT batches through the destination backend's native
    /// bulk loader (Postgres `COPY`, MSSQL bulk, MySQL `LOAD DATA`,
    /// Oracle array DML).
    ///
    /// `off` (default) uses the portable INSERT path.  `auto` uses
    /// the bulk path when available and falls back to INSERT on
    /// `BulkUnavailable`.  `on` requires the bulk path and errors if
    /// the backend is not yet implemented.
    #[arg(
        long,
        value_name = "MODE",
        default_value = "off",
        value_enum,
        ignore_case = true,
    )]
    pub bulk_native: BulkNativeMode,

    /// Wire format for the Postgres `COPY` bulk path. `text` (default)
    /// is the v1 path; `binary` opts into
    /// `tokio_postgres::binary_copy::BinaryCopyInWriter`. PG-only;
    /// other destination backends silently ignore. Only consulted when
    /// `--bulk-native=auto|on` selects the bulk path.
    #[arg(
        long,
        value_name = "FORMAT",
        default_value = "text",
        value_enum,
        ignore_case = true,
    )]
    pub copy_format: CopyFormatArg,

    /// Password for the source connection (overrides credential stack).
    #[arg(long = "password-src")]
    pub password_src: Option<String>,

    /// Password for the destination connection (overrides credential stack).
    #[arg(long = "password-dst")]
    pub password_dst: Option<String>,

    #[command(flatten)]
    pub output: OutputFlags,

    /// Connection flags. Note: in Phase 1 these apply to *both* source
    /// and destination — independent `--src-*` / `--dst-*` SSH/proxy
    /// flags are tracked as a backlog enhancement.
    #[command(flatten)]
    pub conn_flags: ConnectionFlags,
}

/// Diff command arguments — compare schemas between two connections.
#[derive(Args, Clone, Debug)]
pub struct DiffArgs {
    /// Left side ("A") connection name or raw URL
    pub connection_a: String,

    /// Right side ("B") connection name or raw URL
    pub connection_b: String,

    /// Optional single table to diff (default: diff every table)
    #[arg(long, value_name = "NAME")]
    pub table: Option<String>,

    /// Password for connection A (overrides credential stack)
    #[arg(long = "password-a")]
    pub password_a: Option<String>,

    /// Password for connection B (overrides credential stack)
    #[arg(long = "password-b")]
    pub password_b: Option<String>,

    #[command(flatten)]
    pub output: OutputFlags,

    #[command(flatten)]
    pub conn_flags: ConnectionFlags,
}
