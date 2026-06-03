//! Connection resolution — URL, credentials, proxy, SSH config.
//!
//! This module lives in `ferrule-core` so that non-CLI consumers
//! (daemons, REPLs, library embedders) can resolve connections the
//! same way the CLI does, without depending on `clap` or interactive
//! prompts.

use ferrule_sql::{resolve_proxy_from_env, DatabaseUrl, ProxyConfig, SqlError, SshConfig};
use secrecy::ExposeSecret;

/// Bundled output of connection resolution.
///
/// `url` has the resolved password injected (if any) so it remains a
/// complete connection string — the daemon path serializes this URL
/// over its socket, and several drivers parse the whole raw URL.
///
/// `secret` is the same resolved credential surfaced as a standalone
/// [`secrecy::SecretString`], so the in-process connect path can hand it to
/// `ferrule_sql::ConnectOptions::password` rather than relying on the
/// URL. Credential resolution itself (env var, OS keyring,
/// interactive prompt) stays here in the CLI/config layer; `ferrule-sql`
/// only ever receives the already-resolved secret. `secret` is `None`
/// when no password was resolved.
///
/// SSH config and proxy config are plain data — the caller (CLI) still
/// needs to resolve the actual SSH key source (file vs agent,
/// passphrase prompt) and set up the tunnel.
#[derive(Debug, Clone)]
pub struct ResolvedConnection {
    pub url: DatabaseUrl,
    pub secret: Option<secrecy::SecretString>,
    pub ssh_config: Option<SshConfig>,
    pub proxy: Option<ProxyConfig>,
}

/// Resolve a connection string into a [`ResolvedConnection`].
///
/// `password` is an explicit password (e.g. from `--password`).
/// `ssh_config` is already-merged SSH tunnel configuration (host,
/// port, user, key_path hint).  `proxy_url` is the optional
/// `--proxy-url` CLI flag.
pub fn resolve_connection(
    connection: &str,
    password: Option<String>,
    ssh_config: Option<SshConfig>,
    proxy_url: Option<&str>,
    global_config: &ferrule_config::profile::GlobalConfig,
) -> Result<ResolvedConnection, SqlError> {
    let url = resolve_url(connection, password, global_config)?;
    let proxy = resolve_proxy_config(connection, proxy_url, global_config, &url)?;
    // Surface the resolved credential as a standalone secret. It is
    // exactly the URL's password component after the credential stack
    // ran, so the in-process connect path (which threads it through
    // `ConnectOptions::password`) and the daemon path (which only sees
    // the serialized URL) authenticate identically.
    let secret = url.password();
    Ok(ResolvedConnection {
        url,
        secret,
        ssh_config,
        proxy,
    })
}

/// Resolve just the URL (and credential stack) without touching SSH
/// or proxy.
fn resolve_url(
    connection: &str,
    password: Option<String>,
    global_config: &ferrule_config::profile::GlobalConfig,
) -> Result<DatabaseUrl, SqlError> {
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
                    SqlError::InvalidUrl(format!(
                        "Invalid URL in profile for '{}': {}",
                        connection, e
                    ))
                })?;
                let resolved = ferrule_config::credentials::resolve_password_stack(
                    connection,
                    password.map(|p| secrecy::SecretString::new(p.into())),
                    profile.password_url.as_deref(),
                )
                .map_err(|e| SqlError::RegistryError(e.to_string()))?;
                if let Some(pwd) = resolved {
                    url.set_password(Some(pwd.expose_secret()));
                }
                return Ok(url);
            }

            // 2. Fall back to registry (connections.toml)
            let registry = ferrule_config::registry::ConnectionRegistry::load_default()
                .map_err(|e| SqlError::RegistryError(e.to_string()))?;
            let entry = registry.get(connection).ok_or_else(|| {
                SqlError::InvalidUrl(format!(
                    "Connection '{}' is not a valid URL and not found in registry or profile.",
                    connection
                ))
            })?;
            let mut url = DatabaseUrl::parse(&entry.url).map_err(|e| {
                SqlError::InvalidUrl(format!(
                    "Invalid URL in registry for '{}': {}",
                    connection, e
                ))
            })?;

            let resolved = ferrule_config::credentials::resolve_password_stack(
                connection,
                password.map(|p| secrecy::SecretString::new(p.into())),
                None,
            )
            .map_err(|e| SqlError::RegistryError(e.to_string()))?;
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
) -> Result<Option<ProxyConfig>, SqlError> {
    // 1. CLI flag
    if let Some(raw) = proxy_url {
        return ProxyConfig::parse(raw)
            .map(Some)
            .map_err(|e| SqlError::InvalidUrl(format!("Invalid --proxy-url: {e}")));
    }

    // 2. Profile
    if let Some(profile) = global_config.connection.get(connection_name) {
        if let Some(raw) = &profile.proxy_url {
            return ProxyConfig::parse(raw).map(Some).map_err(|e| {
                SqlError::InvalidUrl(format!(
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
                .map_err(|e| SqlError::InvalidUrl(format!("{env_name} is set but invalid: {e}")));
        }
    }

    // 4. ALL_PROXY / HTTP_PROXY / HTTPS_PROXY env vars
    let target_scheme = url.scheme();
    if let Some(cfg) = resolve_proxy_from_env(target_scheme) {
        if let Some(host) = url.host() {
            if ferrule_sql::is_no_proxy(host) {
                return Ok(None);
            }
        }
        return Ok(Some(cfg));
    }

    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use secrecy::ExposeSecret;

    /// A password embedded in a parseable URL is surfaced both on the
    /// URL (for the daemon-serialization path) and as a standalone
    /// `secret` (for the in-process `ConnectOptions::password` path),
    /// and the two agree. This is the consistency contract the rest of
    /// the credential plumbing relies on.
    #[test]
    fn url_password_is_surfaced_as_secret() {
        let cfg = ferrule_config::profile::GlobalConfig::default();
        let resolved = resolve_connection(
            "postgres://user:url_pw@localhost/db",
            None,
            None,
            None,
            &cfg,
        )
        .expect("resolve a plain URL");

        let secret = resolved.secret.expect("a surfaced secret");
        assert_eq!(secret.expose_secret(), "url_pw");
        assert_eq!(
            resolved
                .url
                .password()
                .map(|p| p.expose_secret().to_string()),
            Some("url_pw".to_string()),
        );
    }

    /// An explicit `--password` overrides the URL component, and that
    /// override is what gets surfaced as `secret`.
    #[test]
    fn explicit_password_overrides_url_and_is_surfaced() {
        let cfg = ferrule_config::profile::GlobalConfig::default();
        let resolved = resolve_connection(
            "postgres://user:url_pw@localhost/db",
            Some("flag_pw".to_string()),
            None,
            None,
            &cfg,
        )
        .expect("resolve with an explicit password");

        assert_eq!(
            resolved.secret.map(|s| s.expose_secret().to_string()),
            Some("flag_pw".to_string()),
        );
    }

    /// A passwordless URL yields no surfaced secret, so the SQL core
    /// connects without a password (trust/peer auth, local socket).
    #[test]
    fn passwordless_url_surfaces_no_secret() {
        let cfg = ferrule_config::profile::GlobalConfig::default();
        let resolved = resolve_connection("postgres://user@localhost/db", None, None, None, &cfg)
            .expect("resolve a passwordless URL");
        assert!(resolved.secret.is_none());
    }
}
