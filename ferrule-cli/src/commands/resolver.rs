use crate::error::CliError;
use crate::ssh_keys::KeySource;
use ferrule_config::profile::GlobalConfig;
use ferrule_sql::tunnel::SshConfig;
use ferrule_sql::url::DatabaseUrl;

/// SSH inputs collected by [`resolve_connection`]: the merged SSH
/// configuration plus the resolved [`KeySource`]. The caller hands
/// these to the dispatch layer (which converts to the core-side
/// types and calls `connect_with_tunnel`).
#[derive(Debug, Clone)]
pub struct SshTunnelInputs {
    pub config: SshConfig,
    pub key_source: KeySource,
}

/// Bundled output of [`resolve_connection`]: the URL (with password
/// injected from the credential stack) plus the same resolved
/// credential surfaced as a standalone `secret`, and optional SSH
/// tunnel inputs.
///
/// `secret` is what `connect_resolved` hands to
/// `ferrule_sql::ConnectOptions::password` so the SQL core receives an
/// already-resolved credential instead of resolving one itself. When
/// `ssh` is `Some`, the dispatch layer sets up the tunnel before
/// connecting; when `None`, it connects directly.
#[derive(Debug, Clone)]
pub struct ResolvedConnection {
    pub url: DatabaseUrl,
    pub secret: Option<secrecy::SecretString>,
    pub ssh: Option<SshTunnelInputs>,
    pub proxy: Option<ferrule_sql::ProxyConfig>,
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
    // 1. Build SSH config from CLI flags + profile.
    let ssh_config =
        crate::ssh_flags::resolve_ssh_config(connection, ssh_tunnel, ssh_key, global_config)?;

    let ssh = match &ssh_config {
        Some(cfg) => {
            let key_source =
                crate::ssh_keys::resolve_key_source_default(connection, cfg.key_path.as_deref())?;
            Some(SshTunnelInputs {
                config: cfg.clone(),
                key_source,
            })
        }
        None => None,
    };

    // 2. Delegate URL, password, proxy resolution to ferrule-core.
    let core_resolved = ferrule_core::resolver::resolve_connection(
        connection,
        password,
        ssh_config,
        proxy_url,
        global_config,
    )
    .await
    .map_err(CliError::connection)?;

    Ok(ResolvedConnection {
        url: core_resolved.url,
        secret: core_resolved.secret,
        ssh,
        proxy: core_resolved.proxy,
    })
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

/// Establish a [`Connection`](ferrule_sql::Connection) from a
/// [`ResolvedConnection`]. Routes through the SSH tunnel when
/// `resolved.ssh` is `Some` (gated behind the `ssh` feature; without
/// it, this returns a "compiled without SSH support" diagnostic).
pub async fn connect_resolved(
    resolved: ResolvedConnection,
    opts: &ferrule_sql::ConnectOptions,
) -> Result<Box<dyn ferrule_sql::Connection>, CliError> {
    // Hand the SQL core the credential we already resolved (env var,
    // keyring, prompt) via `ConnectOptions::password` instead of
    // relying on the URL. The CLI owns credential resolution; the
    // resolved secret wins over any URL password component.
    let opts = ferrule_sql::ConnectOptions {
        password: resolved.secret.clone(),
        ..opts.clone()
    };
    let proxy = resolved.proxy.as_ref();
    if let Some(ssh) = resolved.ssh {
        return connect_via_ssh_tunnel(resolved.url, ssh, &opts, proxy).await;
    }
    ferrule_sql::connect(&resolved.url, &opts, proxy)
        .await
        .map_err(CliError::connection)
}

#[cfg(feature = "ssh")]
async fn connect_via_ssh_tunnel(
    url: DatabaseUrl,
    ssh: SshTunnelInputs,
    opts: &ferrule_sql::ConnectOptions,
    proxy: Option<&ferrule_sql::ProxyConfig>,
) -> Result<Box<dyn ferrule_sql::Connection>, CliError> {
    let key_source = match &ssh.key_source {
        KeySource::File(path) => match ferrule_sql::tunnel::ssh_key_needs_passphrase(path) {
            Ok(false) => ferrule_sql::KeySource::File(path.clone(), None),
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
                ferrule_sql::KeySource::File(
                    path.clone(),
                    Some(secrecy::SecretString::new(passphrase.into())),
                )
            }
            Err(e) => return Err(CliError::usage(e.to_string())),
        },
        KeySource::Agent(path) => ferrule_sql::KeySource::Agent(path.clone()),
    };
    match ferrule_sql::connect_with_tunnel(&url, opts, &ssh.config, &key_source, proxy).await {
        Ok(conn) => Ok(conn),
        Err(ferrule_sql::SqlError::SshUnknownHost {
            host,
            port,
            algorithm,
            fingerprint,
            key,
        }) => {
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
            ferrule_sql::tunnel::learn_host_key(&host, port, &key).map_err(|e| {
                CliError::connection(ferrule_sql::SqlError::ConnectionFailed(e.to_string()))
            })?;
            // Retry once after writing the key.
            ferrule_sql::connect_with_tunnel(&url, opts, &ssh.config, &key_source, proxy)
                .await
                .map_err(CliError::connection)
        }
        Err(ferrule_sql::SqlError::SshHostKeyMismatch { host, port }) => Err(CliError::connection(
            ferrule_sql::SqlError::ConnectionFailed(format!(
                "SSH host key mismatch for {host}:{port}\n\
                 The key sent by the server does not match the one recorded \
                 in ~/.ssh/known_hosts.\n\
                 To resolve: verify the new fingerprint and remove the old key:\
                 \n  ssh-keygen -R {host} -f ~/.ssh/known_hosts"
            )),
        )),
        Err(other) => Err(CliError::connection(other)),
    }
}

#[cfg(not(feature = "ssh"))]
async fn connect_via_ssh_tunnel(
    _url: DatabaseUrl,
    ssh: SshTunnelInputs,
    _opts: &ferrule_sql::ConnectOptions,
    _proxy: Option<&ferrule_sql::ProxyConfig>,
) -> Result<Box<dyn ferrule_sql::Connection>, CliError> {
    let _ = (&ssh.config, &ssh.key_source);
    Err(CliError::usage(
        "This ferrule binary was built without the `ssh` feature. \
         Rebuild with `cargo build --features ferrule-cli/ssh` (or `--features all`)."
            .to_string(),
    ))
}
