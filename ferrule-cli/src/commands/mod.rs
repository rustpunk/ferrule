pub mod bookmark;
pub mod conn;
pub mod describe;
pub mod explain;
pub mod query;
pub mod repl;
pub mod tables;

pub use bookmark::BookmarkArgs;
pub use explain::ExplainArgs;
pub use repl::ReplArgs;

use crate::error::CliError;
use clap::{Args, Subcommand};
use ferrule_config::profile::GlobalConfig;
use ferrule_config::registry::ConnectionRegistry;
use ferrule_core::url::DatabaseUrl;
use secrecy::ExposeSecret;

/// Resolve a connection string — either a raw URL or a registry/profile entry.
pub async fn resolve_connection(
    connection: &str,
    password: Option<String>,
    global_config: &GlobalConfig,
) -> Result<DatabaseUrl, CliError> {
    match DatabaseUrl::parse(connection) {
        Ok(mut url) => {
            if let Some(pwd) = password {
                url.set_password(Some(&pwd));
            }
            Ok(url)
        }
        Err(_) => {
            // 1. Try profile (from .ferrule.toml)
            if let Some(profile) = global_config.connection.get(connection) {
                let mut url = DatabaseUrl::parse(&profile.url).map_err(|e| {
                    CliError::usage(format!(
                        "Invalid URL in profile for '{}': {}",
                        connection, e
                    ))
                })?;
                let resolved = resolve_password_stack(
                    connection,
                    password.map(|p| secrecy::SecretString::new(p.into())),
                )
                .await?;
                if let Some(pwd) = resolved {
                    url.set_password(Some(pwd.expose_secret()));
                }
                return Ok(url);
            }

            // 2. Fall back to registry (connections.toml)
            let registry = ConnectionRegistry::load_default().map_err(CliError::registry)?;
            let entry = registry.get(connection).ok_or_else(|| {
                CliError::usage(format!(
                    "Connection '{}' is not a valid URL and not found in registry or profile.",
                    connection
                ))
            })?;
            let mut url = DatabaseUrl::parse(&entry.url).map_err(|e| {
                CliError::usage(format!(
                    "Invalid URL in registry for '{}': {}",
                    connection, e
                ))
            })?;

            let resolved = resolve_password_stack(
                connection,
                password.map(|p| secrecy::SecretString::new(p.into())),
            )
            .await?;

            if let Some(pwd) = resolved {
                url.set_password(Some(pwd.expose_secret()));
            }
            Ok(url)
        }
    }
}

/// Credential resolution stack:
/// 1. Explicit override
/// 2. `FERRULE_{NAME}_PASSWORD` env var
/// 3. OS keyring
/// 4. Interactive prompt (TTY only)
pub async fn resolve_password_stack(
    name: &str,
    explicit: Option<secrecy::SecretString>,
) -> Result<Option<secrecy::SecretString>, CliError> {
    if let Some(pwd) = explicit {
        return Ok(Some(pwd));
    }

    if let Some(pwd) = ferrule_config::credentials::resolve_env_password(name) {
        return Ok(Some(pwd));
    }

    if let Some(pwd) = ferrule_config::credentials::resolve_keyring_password(name) {
        return Ok(Some(pwd));
    }

    let tty = is_terminal::IsTerminal::is_terminal(&std::io::stdin());
    if tty {
        let prompt = format!("Password for '{}': ", name);
        let pwd = tokio::task::spawn_blocking(move || rpassword::prompt_password(prompt))
            .await
            .map_err(|e| CliError::usage(format!("Password prompt failed: {e}")))?
            .map_err(CliError::Io)?;
        if !pwd.is_empty() {
            return Ok(Some(secrecy::SecretString::new(pwd.into())));
        }
    }

    Ok(None)
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

    /// Dry run — print without executing
    #[arg(long)]
    pub dry_run: bool,
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
