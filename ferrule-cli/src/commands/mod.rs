#![allow(dead_code, unused_variables, unused_imports)]

pub mod conn;
pub mod describe;
pub mod query;
pub mod tables;

use crate::error::CliError;
use clap::{Args, Subcommand};
use ferrule_core::connection::ConnectOptions;
use ferrule_core::url::DatabaseUrl;
use ferrule_config::registry::ConnectionRegistry;
use secrecy::ExposeSecret;

/// Resolve a connection string — either a raw URL or a registry entry.
pub async fn resolve_connection(
    connection: &str,
    password: Option<String>,
) -> Result<DatabaseUrl, CliError> {
    match DatabaseUrl::parse(connection) {
        Ok(mut url) => {
            if let Some(pwd) = password {
                url.set_password(Some(&pwd));
            }
            Ok(url)
        }
        Err(_) => {
            let registry = ConnectionRegistry::load_default()
                .map_err(CliError::registry)?;
            let entry = registry.get(connection).ok_or_else(|| {
                CliError::usage(format!(
                    "Connection '{}' is not a valid URL and not found in registry.",
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
/// 3. Interactive prompt (TTY only)
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

    /// Show execution timing
    #[arg(long)]
    pub timing: bool,

    /// Verbose output
    #[arg(short, long)]
    pub verbose: bool,
}

/// Connection flags shared by query-like commands.
#[derive(Args, Clone, Debug)]
pub struct ConnectionFlags {
    /// Disable TLS certificate verification (warns on stderr).
    #[arg(long)]
    pub insecure: bool,
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
    Add {
        name: String,
        url: String,
    },
    /// List saved connections
    List,
    /// Remove a connection
    Remove {
        name: String,
    },
    /// Test a connection
    Test {
        name: String,
        #[command(flatten)]
        conn_flags: ConnectionFlags,
    },
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
