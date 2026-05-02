//! Connection resolution — URL, credentials, proxy, SSH config.
//!
//! This module lives in `ferrule-core` so that non-CLI consumers
//! (daemons, REPLs, library embedders) can resolve connections the
//! same way the CLI does, without depending on `clap` or interactive
//! prompts.

use crate::error::CoreError;
use crate::proxy::{resolve_proxy_from_env, ProxyConfig};
use crate::tunnel::SshConfig;
use crate::url::DatabaseUrl;
use secrecy::ExposeSecret;

/// Bundled output of connection resolution.
///
/// The URL has the password injected (if any).  SSH config and proxy
/// config are plain data — the caller (CLI) still needs to resolve
/// the actual SSH key source (file vs agent, passphrase prompt) and
/// set up the tunnel.
#[derive(Debug, Clone)]
pub struct ResolvedConnection {
    pub url: DatabaseUrl,
    pub ssh_config: Option<SshConfig>,
    pub proxy: Option<ProxyConfig>,
}

/// Resolve a connection string into a [`ResolvedConnection`].
///
/// `password` is an explicit password (e.g. from `--password`).
/// `ssh_config` is already-merged SSH tunnel configuration (host,
/// port, user, key_path hint).  `proxy_url` is the optional
/// `--proxy-url` CLI flag.
pub async fn resolve_connection(
    connection: &str,
    password: Option<String>,
    ssh_config: Option<SshConfig>,
    proxy_url: Option<&str>,
    global_config: &ferrule_config::profile::GlobalConfig,
) -> Result<ResolvedConnection, CoreError> {
    let url = resolve_url(connection, password, global_config).await?;
    let proxy = resolve_proxy_config(connection, proxy_url, global_config, &url)?;
    Ok(ResolvedConnection {
        url,
        ssh_config,
        proxy,
    })
}

/// Resolve just the URL (and credential stack) without touching SSH
/// or proxy.
async fn resolve_url(
    connection: &str,
    password: Option<String>,
    global_config: &ferrule_config::profile::GlobalConfig,
) -> Result<DatabaseUrl, CoreError> {
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
                    CoreError::InvalidUrl(format!(
                        "Invalid URL in profile for '{}': {}",
                        connection, e
                    ))
                })?;
                let resolved = ferrule_config::credentials::resolve_password_stack(
                    connection,
                    password.map(|p| secrecy::SecretString::new(p.into())),
                    profile.password_url.as_deref(),
                )
                .map_err(|e| CoreError::RegistryError(e.to_string()))?;
                if let Some(pwd) = resolved {
                    url.set_password(Some(pwd.expose_secret()));
                }
                return Ok(url);
            }

            // 2. Fall back to registry (connections.toml)
            let registry = ferrule_config::registry::ConnectionRegistry::load_default()
                .map_err(|e| CoreError::RegistryError(e.to_string()))?;
            let entry = registry.get(connection).ok_or_else(|| {
                CoreError::InvalidUrl(format!(
                    "Connection '{}' is not a valid URL and not found in registry or profile.",
                    connection
                ))
            })?;
            let mut url = DatabaseUrl::parse(&entry.url).map_err(|e| {
                CoreError::InvalidUrl(format!(
                    "Invalid URL in registry for '{}': {}",
                    connection, e
                ))
            })?;

            let resolved = ferrule_config::credentials::resolve_password_stack(
                connection,
                password.map(|p| secrecy::SecretString::new(p.into())),
                None,
            )
            .map_err(|e| CoreError::RegistryError(e.to_string()))?;
            if let Some(pwd) = resolved {
                url.set_password(Some(pwd.expose_secret()));
            }
            Ok(url)
        }
    }
}

/// Resolve proxy configuration from explicit flag, profile, or env.
fn resolve_proxy_config(
    connection_name: &str,
    proxy_url: Option<&str>,
    global_config: &ferrule_config::profile::GlobalConfig,
    url: &DatabaseUrl,
) -> Result<Option<ProxyConfig>, CoreError> {
    // 1. CLI flag
    if let Some(raw) = proxy_url {
        return ProxyConfig::parse(raw)
            .map(Some)
            .map_err(|e| CoreError::InvalidUrl(format!("Invalid --proxy-url: {e}")));
    }

    // 2. Profile
    if let Some(profile) = global_config.connection.get(connection_name) {
        if let Some(raw) = &profile.proxy_url {
            return ProxyConfig::parse(raw).map(Some).map_err(|e| {
                CoreError::InvalidUrl(format!(
                    "Invalid proxy_url in profile for '{connection_name}': {e}"
                ))
            });
        }
    }

    // 3. FERRULE_<NAME>_PROXY_URL env var
    let env_name = format!(
        "FERRULE_{}_PROXY_URL",
        connection_name.to_ascii_uppercase().replace('-', "_")
    );
    if let Ok(raw) = std::env::var(&env_name) {
        if !raw.is_empty() {
            return ProxyConfig::parse(&raw)
                .map(Some)
                .map_err(|e| CoreError::InvalidUrl(format!("{env_name} is set but invalid: {e}")));
        }
    }

    // 4. ALL_PROXY / HTTP_PROXY / HTTPS_PROXY env vars
    let target_scheme = url.scheme();
    if let Some(cfg) = resolve_proxy_from_env(target_scheme) {
        if let Some(host) = url.host() {
            if crate::proxy::is_no_proxy(host) {
                return Ok(None);
            }
        }
        return Ok(Some(cfg));
    }

    Ok(None)
}
