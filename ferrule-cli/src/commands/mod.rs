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
    pub proxy: Option<ferrule_core::ProxyConfig>,
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
    proxy_url: Option<&str>,
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

    let proxy = resolve_proxy_config(connection, proxy_url, global_config, &url)?;

    Ok(ResolvedConnection { url, ssh, proxy })
}

/// Resolve proxy configuration from CLI flag, profile, env vars.
fn resolve_proxy_config(
    connection_name: &str,
    proxy_url: Option<&str>,
    global_config: &GlobalConfig,
    url: &DatabaseUrl,
) -> Result<Option<ferrule_core::ProxyConfig>, CliError> {
    // 1. CLI flag
    if let Some(raw) = proxy_url {
        return ferrule_core::ProxyConfig::parse(raw)
            .map(Some)
            .map_err(|e| CliError::usage(format!("Invalid --proxy-url: {e}")));
    }

    // 2. Profile
    if let Some(profile) = global_config.connection.get(connection_name) {
        if let Some(raw) = &profile.proxy_url {
            return ferrule_core::ProxyConfig::parse(raw)
                .map(Some)
                .map_err(|e| CliError::usage(format!(
                    "Invalid proxy_url in profile for '{connection_name}': {e}"
                )));
        }
    }

    // 3. FERRULE_<NAME>_PROXY_URL env var
    let env_name = format!(
        "FERRULE_{}_PROXY_URL",
        connection_name.to_ascii_uppercase().replace('-', "_")
    );
    if let Ok(raw) = std::env::var(&env_name) {
        if !raw.is_empty() {
            return ferrule_core::ProxyConfig::parse(&raw)
                .map(Some)
                .map_err(|e| CliError::usage(format!(
                    "{env_name} is set but invalid: {e}"
                )));
        }
    }

    // 4. ALL_PROXY / HTTP_PROXY / HTTPS_PROXY env vars
    let target_scheme = url.scheme();
    if let Some(cfg) = ferrule_core::proxy::resolve_proxy_from_env(target_scheme) {
        if let Some(host) = url.host() {
            if ferrule_core::proxy::is_no_proxy(host) {
                return Ok(None);
            }
        }
        return Ok(Some(cfg));
    }

    Ok(None)
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
    let proxy = resolved.proxy.as_ref();
    if let Some(ssh) = resolved.ssh {
        return connect_via_ssh_tunnel(resolved.url, ssh, opts, proxy).await;
    }
    ferrule_core::connect(&resolved.url, opts, proxy,
    )
    .await
    .map_err(CliError::connection)
}

#[cfg(feature = "ssh")]
async fn connect_via_ssh_tunnel(
    url: DatabaseUrl,
    ssh: SshTunnelInputs,
    opts: &ferrule_core::ConnectOptions,
    proxy: Option<&ferrule_core::ProxyConfig>,
) -> Result<Box<dyn ferrule_core::Connection>, CliError> {
    let key_source = match &ssh.key_source {
        KeySource::File(path) => {
            match ferrule_core::tunnel::ssh_key_needs_passphrase(path) {
                Ok(false) => ferrule_core::KeySource::File(path.clone(), None),
                Ok(true) => {
                    let tty = is_terminal::IsTerminal::is_terminal(&std::io::stdin());
                    if !tty {
                        return Err(CliError::usage(format!(
                            "SSH key {} is encrypted. Passphrase prompting requires \
                             an interactive terminal.\n\
                             Use an SSH agent or decrypt the key on disk.",
                            path.display()
                        )));
                    }
                    let cloned = path.clone();
                    let passphrase = tokio::task::spawn_blocking(move || {
                        rpassword::prompt_password(format!(
                            "Enter passphrase for SSH key {}: ",
                            cloned.display()
                        ))
                    })
                    .await
                    .map_err(|e| CliError::usage(format!("Passphrase prompt failed: {e}")))?
                    .map_err(CliError::Io)?;
                    ferrule_core::KeySource::File(
                        path.clone(),
                        Some(secrecy::SecretString::new(passphrase.into())),
                    )
                }
                Err(e) => return Err(CliError::usage(e.to_string())),
            }
        }
        KeySource::Agent(path) => ferrule_core::KeySource::Agent(path.clone()),
    };
    match ferrule_core::connect_with_tunnel(&url, opts, &ssh.config, &key_source, proxy).await {
        Ok(conn) => Ok(conn),
        Err(ferrule_core::CoreError::SshUnknownHost { host, port, algorithm, fingerprint, key }) => {
            let tty = is_terminal::IsTerminal::is_terminal(&std::io::stdin());
            if !tty {
                return Err(CliError::usage(format!(
                    "SSH host {host}:{port} is not in ~/.ssh/known_hosts.\n\
                     To add it, run interactively once or use:\
                     \n  ssh-keyscan -p {port} {host} >> ~/.ssh/known_hosts"
                )));
            }
            eprintln!(
                "The authenticity of host '{host}:{port}' can't be established.\n\
                 {algorithm} key fingerprint is {fingerprint}.\n\
                 Are you sure you want to continue connecting (yes/no)? "
            );
            let mut answer = String::new();
            std::io::stdin()
                .read_line(&mut answer)
                .map_err(|e| CliError::Io(e))?;
            let trimmed = answer.trim().to_ascii_lowercase();
            if trimmed != "yes" && trimmed != "y" {
                return Err(CliError::usage(format!(
                    "Host {host}:{port} not accepted. Aborting."
                )));
            }
            ferrule_core::tunnel::learn_host_key(&host, port, &key,
            )
            .map_err(|e| CliError::connection(
                ferrule_core::CoreError::ConnectionFailed(e.to_string())
            ))?;
            // Retry once after writing the key.
            ferrule_core::connect_with_tunnel(&url, opts, &ssh.config, &key_source, proxy
            )
            .await
            .map_err(CliError::connection)
        }
        Err(ferrule_core::CoreError::SshHostKeyMismatch { host, port }) => {
            Err(CliError::connection(ferrule_core::CoreError::ConnectionFailed(format!(
                "SSH host key mismatch for {host}:{port}\n\
                 The key sent by the server does not match the one recorded \
                 in ~/.ssh/known_hosts.\n\
                 To resolve: verify the new fingerprint and remove the old key:\
                 \n  ssh-keygen -R {host} -f ~/.ssh/known_hosts"
            ))))
        }
        Err(other) => Err(CliError::connection(other)),
    }
}

#[cfg(not(feature = "ssh"))]
async fn connect_via_ssh_tunnel(
    _url: DatabaseUrl,
    ssh: SshTunnelInputs,
    _opts: &ferrule_core::ConnectOptions,
    _proxy: Option<&ferrule_core::ProxyConfig>,
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
