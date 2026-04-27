pub mod bookmark;
pub mod conn;
pub mod describe;
pub mod diff;
pub mod dump;
pub mod explain;
pub mod load;
pub mod query;
pub mod repl;
pub mod tables;
pub mod watch;

pub use bookmark::BookmarkArgs;
pub use dump::DumpArgs;
pub use explain::ExplainArgs;
pub use load::LoadArgs;
pub use repl::ReplArgs;
pub use watch::WatchArgs;

use crate::error::CliError;
use crate::ssh_keys::KeySource;
use clap::{Args, Subcommand};
use ferrule_config::profile::GlobalConfig;
use ferrule_config::registry::ConnectionRegistry;
use ferrule_core::tunnel::SshConfig;
use ferrule_core::url::DatabaseUrl;
use secrecy::ExposeSecret;

/// SSH inputs collected by [`resolve_connection`]: the merged SSH
/// configuration plus the resolved [`KeySource`]. The caller hands
/// these to the dispatch layer (which converts to the core-side
/// types and calls `connect_with_tunnel`).
#[derive(Debug, Clone)]
pub struct SshTunnelInputs {
    pub config: SshConfig,
    pub key_source: KeySource,
}

/// Bundled output of [`resolve_connection`]: the URL (possibly with
/// password injected from the credential stack) plus optional SSH
/// tunnel inputs. When `ssh` is `Some`, the dispatch layer will set
/// up the tunnel before connecting; when `None`, it connects
/// directly.
#[derive(Debug, Clone)]
pub struct ResolvedConnection {
    pub url: DatabaseUrl,
    pub ssh: Option<SshTunnelInputs>,
}

/// Resolve a connection string — either a raw URL or a registry/profile
/// entry — into a [`ResolvedConnection`].
///
/// `ssh_tunnel` and `ssh_key` are the optional `--ssh-tunnel` and
/// `--ssh-key` CLI flag values; pass `None`/`None` from call sites
/// that don't expose those flags (e.g. `conn list`).
pub async fn resolve_connection(
    connection: &str,
    password: Option<String>,
    ssh_tunnel: Option<&str>,
    ssh_key: Option<&str>,
    global_config: &GlobalConfig,
) -> Result<ResolvedConnection, CliError> {
    let url = resolve_url(connection, password, global_config).await?;

    let ssh_config = crate::ssh_flags::resolve_ssh_config(
        connection,
        ssh_tunnel,
        ssh_key,
        global_config,
    )?;

    let ssh = match ssh_config {
        Some(cfg) => {
            let key_source = crate::ssh_keys::resolve_key_source_default(
                connection,
                cfg.key_path.as_deref(),
            )?;
            Some(SshTunnelInputs {
                config: cfg,
                key_source,
            })
        }
        None => None,
    };

    Ok(ResolvedConnection { url, ssh })
}

/// Per Wave 3 B3 §2d: reject `--daemon` together with any SSH
/// tunnel configuration. The connection pooling daemon does not
/// pool tunneled connections, so the combination would silently
/// either ignore the tunnel or queue connections behind a single
/// shared session that is fragile under SSH idle timeouts. Either
/// is a bad UX; we surface the conflict clearly instead.
pub fn check_daemon_ssh_compat(
    daemon: bool,
    resolved: &ResolvedConnection,
) -> Result<(), CliError> {
    if daemon && resolved.ssh.is_some() {
        return Err(CliError::usage(
            "SSH tunnels bypass the connection pooling daemon. The tunnel \
             session lifecycle is tied to the request, so pooling tunneled \
             connections would introduce a class of failure modes around \
             session timeout that ferrule does not currently handle. Either \
             drop --daemon or open without a tunnel."
                .to_string(),
        ));
    }
    Ok(())
}

/// Establish a [`Connection`](ferrule_core::Connection) from a
/// [`ResolvedConnection`]. Routes through the SSH tunnel when
/// `resolved.ssh` is `Some` (gated behind the `ssh` feature; without
/// it, this returns a "compiled without SSH support" diagnostic).
pub async fn connect_resolved(
    resolved: ResolvedConnection,
    opts: &ferrule_core::ConnectOptions,
) -> Result<Box<dyn ferrule_core::Connection>, CliError> {
    if let Some(ssh) = resolved.ssh {
        return connect_via_ssh_tunnel(resolved.url, ssh, opts).await;
    }
    ferrule_core::connect(&resolved.url, opts)
        .await
        .map_err(CliError::connection)
}

#[cfg(feature = "ssh")]
async fn connect_via_ssh_tunnel(
    url: DatabaseUrl,
    ssh: SshTunnelInputs,
    opts: &ferrule_core::ConnectOptions,
) -> Result<Box<dyn ferrule_core::Connection>, CliError> {
    let core_key_source: ferrule_core::KeySource = ssh.key_source.into();
    ferrule_core::connect_with_tunnel(&url, opts, &ssh.config, &core_key_source)
        .await
        .map_err(CliError::connection)
}

#[cfg(not(feature = "ssh"))]
async fn connect_via_ssh_tunnel(
    _url: DatabaseUrl,
    ssh: SshTunnelInputs,
    _opts: &ferrule_core::ConnectOptions,
) -> Result<Box<dyn ferrule_core::Connection>, CliError> {
    // Read the fields so they aren't flagged as dead code in
    // ssh-feature-off builds; the struct exists in both feature
    // modes because `ResolvedConnection.ssh` is non-gated.
    let _ = (&ssh.config, &ssh.key_source);
    Err(CliError::usage(
        "This ferrule binary was built without the `ssh` feature. \
         Rebuild with `cargo build --features ferrule-cli/ssh` (or `--features all`)."
            .to_string(),
    ))
}

/// Resolve just the URL (and credential stack) without touching SSH.
/// Internal to `resolve_connection`; split out so the SSH branch
/// reuses it cleanly.
async fn resolve_url(
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
                let resolved = ferrule_config::credentials::resolve_password_stack(
                    connection,
                    password.map(|p| secrecy::SecretString::new(p.into())),
                    profile.password_url.as_deref(),
                )
                .map_err(CliError::registry)?;
                let final_pwd = if let Some(pwd) = resolved {
                    Some(pwd)
                } else {
                    prompt_password_interactive(connection).await?
                };
                if let Some(pwd) = final_pwd {
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

            let resolved = ferrule_config::credentials::resolve_password_stack(
                connection,
                password.map(|p| secrecy::SecretString::new(p.into())),
                None,
            )
            .map_err(CliError::registry)?;
            let final_pwd = if let Some(pwd) = resolved {
                Some(pwd)
            } else {
                prompt_password_interactive(connection).await?
            };
            if let Some(pwd) = final_pwd {
                url.set_password(Some(pwd.expose_secret()));
            }
            Ok(url)
        }
    }
}

/// Prompt for a password interactively (TTY only).
async fn prompt_password_interactive(
    name: &str,
) -> Result<Option<secrecy::SecretString>, CliError> {
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
