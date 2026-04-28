//! HTTP CONNECT proxy support.
//!
//! Corporate firewalls often expose an HTTP CONNECT proxy (Squid,
//! Blue Coat, Zscaler, etc.). This module implements the client
//! side: open a TCP connection to the proxy, send `CONNECT
//! target:port HTTP/1.1`, parse the `200 Connection established`
//! response, and return the underlying `TcpStream` which is now
//! tunneled to the target.
//!
//! SOCKS5 is out of scope for Phase 1 — see `notes/TODO.md`.

use crate::error::CoreError;
use base64::Engine;
use secrecy::{ExposeSecret, SecretString};
use std::env;

/// Parsed proxy configuration.
#[derive(Debug, Clone)]
pub struct ProxyConfig {
    /// The original URL string (for diagnostics).
    pub url: String,
    /// Proxy hostname or IP.
    pub host: String,
    /// Proxy port.
    pub port: u16,
    /// Username for Basic auth (optional).
    pub username: Option<String>,
    /// Password for Basic auth (optional).
    pub password: Option<SecretString>,
}

impl ProxyConfig {
    /// Parse from a URL string like `http://proxy:8080` or
    /// `http://user:pass@proxy:8080`.
    pub fn parse(url: &str) -> Result<Self, CoreError> {
        let parsed = ::url::Url::parse(url)
            .map_err(|e| CoreError::InvalidUrl(format!("proxy URL: {e}")))?;

        let host = parsed
            .host_str()
            .ok_or_else(|| CoreError::InvalidUrl("proxy URL has no host".to_string()))?
            .to_string();
        let port = parsed.port().unwrap_or(8080);

        let (username, password) = if let Some(info) = parsed.password() {
            (
                Some(parsed.username().to_string()),
                Some(SecretString::new(info.to_string().into())),
            )
        } else {
            (None, None)
        };

        Ok(ProxyConfig {
            url: url.to_string(),
            host,
            port,
            username,
            password,
        })
    }
}

/// Check whether the target host is covered by the `NO_PROXY`
/// environment variable.
///
/// Supports `*` (all hosts), `.domain.com` (suffix match),
/// `domain.com` (exact or suffix match), and comma-separated
/// lists. Port numbers in patterns are ignored — ferrule matches
/// hostnames only.
pub fn is_no_proxy(target_host: &str) -> bool {
    let no_proxy = match env::var("NO_PROXY") {
        Ok(v) if !v.is_empty() => v,
        _ => return false,
    };

    let target_host = target_host.to_ascii_lowercase();

    for pattern in no_proxy.split(',') {
        let pattern = pattern.trim().to_ascii_lowercase();
        if pattern.is_empty() {
            continue;
        }

        // Match everything.
        if pattern == "*" {
            return true;
        }

        // Strip port if present (e.g. "host:8080" → "host").
        let pattern_host = pattern.split(':').next().unwrap_or(&pattern);

        // Suffix match: .domain.com matches any.sub.domain.com
        if pattern_host.starts_with('.') {
            if target_host.ends_with(pattern_host) {
                return true;
            }
        }
        // Exact match, or suffix match without leading dot.
        else if target_host == pattern_host
            || target_host.ends_with(&format!(".{pattern_host}"))
        {
            return true;
        }
    }

    false
}

/// Resolve proxy configuration from the standard environment
/// variable stack.
///
/// Checks `ALL_PROXY`, then `HTTP_PROXY` / `HTTPS_PROXY` based on
/// `target_scheme`. Skips `NO_PROXY` hosts at this layer — callers
/// should call `is_no_proxy` separately when they know the target
/// hostname.
pub fn resolve_proxy_from_env(_target_scheme: &str) -> Option<ProxyConfig> {
    let try_env = |name: &str| -> Option<ProxyConfig> {
        env::var(name)
            .ok()
            .filter(|s| !s.is_empty())
            .and_then(|url| ProxyConfig::parse(&url).ok())
    };

    try_env("ALL_PROXY").or_else(|| {
        // For database URLs the "scheme" is postgres/mysql/etc.
        // Corporate proxies are almost always HTTP CONNECT endpoints,
        // so we check both HTTP_PROXY and HTTPS_PROXY regardless of
        // the DB scheme.
        try_env("HTTPS_PROXY").or_else(|| try_env("HTTP_PROXY"))
    })
}

/// Perform an HTTP CONNECT handshake through the proxy, returning
/// a `TcpStream` that is tunneled to `target_host:target_port`.
pub async fn http_connect(
    proxy: &ProxyConfig,
    target_host: &str,
    target_port: u16,
) -> Result<tokio::net::TcpStream, CoreError> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let mut stream = tokio::net::TcpStream::connect((proxy.host.as_str(), proxy.port))
        .await
        .map_err(|e| {
            CoreError::ConnectionFailed(format!(
                "proxy connect to {}:{}: {e}",
                proxy.host, proxy.port
            ))
        })?;

    let mut request = format!(
        "CONNECT {target_host}:{target_port} HTTP/1.1\r\n\
         Host: {target_host}:{target_port}\r\n"
    );

    if let (Some(u), Some(p)) = (&proxy.username, &proxy.password) {
        let creds = format!("{}:{}", u, p.expose_secret());
        let encoded = base64::prelude::BASE64_STANDARD.encode(creds);
        request.push_str(&format!("Proxy-Authorization: Basic {encoded}\r\n"));
    }

    request.push_str("\r\n");

    stream
        .write_all(request.as_bytes())
        .await
        .map_err(|e| CoreError::ConnectionFailed(format!("proxy write: {e}")))?;

    // Read the response. HTTP CONNECT responses are short (a few
    // hundred bytes at most), so a 1 KB buffer is plenty.
    let mut buf = [0u8; 1024];
    let n = stream
        .read(&mut buf)
        .await
        .map_err(|e| CoreError::ConnectionFailed(format!("proxy read: {e}")))?;

    let response = std::str::from_utf8(&buf[..n])
        .map_err(|_| CoreError::ConnectionFailed(
            "proxy returned non-UTF-8 response".to_string()
        ))?;

    let status_line = response.lines().next().unwrap_or("").trim();
    if !status_line.starts_with("HTTP/1.1 200") && !status_line.starts_with("HTTP/1.0 200") {
        return Err(CoreError::ConnectionFailed(format!(
            "proxy error: {status_line}"
        )));
    }

    Ok(stream)
}

/// A connection that routes through a local TCP listener backed by
/// an HTTP CONNECT proxy. Used for backends (MySQL, MSSQL, Oracle)
/// that do not accept a pre-built stream.
///
/// When this struct is dropped, the listener is dropped and the
/// forwarder task exits naturally.
pub struct ProxiedConnection {
    pub inner: Box<dyn crate::Connection>,
    pub forwarder: Option<tokio::task::JoinHandle<()>>,
}

#[async_trait::async_trait]
impl crate::Connection for ProxiedConnection {
    async fn execute(
        &mut self,
        sql: &str,
    ) -> Result<crate::ExecutionSummary, crate::CoreError> {
        self.inner.execute(sql).await
    }

    async fn query(
        &mut self,
        sql: &str,
    ) -> Result<crate::QueryResult, crate::CoreError> {
        self.inner.query(sql).await
    }

    async fn execute_multi(
        &mut self,
        sql: &str,
    ) -> Result<Vec<crate::StatementResult>, crate::CoreError> {
        self.inner.execute_multi(sql).await
    }

    async fn ping(&mut self) -> Result<(), crate::CoreError> {
        self.inner.ping().await
    }

    async fn list_tables(
        &mut self,
        schema: Option<&str>,
    ) -> Result<Vec<String>, crate::CoreError> {
        self.inner.list_tables(schema).await
    }

    async fn describe_table(
        &mut self,
        schema: Option<&str>,
        table: &str,
    ) -> Result<crate::QueryResult, crate::CoreError> {
        self.inner.describe_table(schema, table).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_simple() {
        let cfg = ProxyConfig::parse("http://proxy:8080").unwrap();
        assert_eq!(cfg.host, "proxy");
        assert_eq!(cfg.port, 8080);
        assert_eq!(cfg.username, None);
        assert!(cfg.password.is_none());
    }

    #[test]
    fn test_parse_with_auth() {
        let cfg = ProxyConfig::parse("http://user:pass@proxy:3128").unwrap();
        assert_eq!(cfg.host, "proxy");
        assert_eq!(cfg.port, 3128);
        assert_eq!(cfg.username, Some("user".to_string()));
        assert_eq!(cfg.password.as_ref().unwrap().expose_secret(), "pass");
    }

    #[test]
    fn test_parse_no_port_uses_8080() {
        let cfg = ProxyConfig::parse("http://proxy").unwrap();
        assert_eq!(cfg.port, 8080);
    }

    #[test]
    fn test_is_no_proxy_star() {
        std::env::set_var("NO_PROXY", "*");
        assert!(is_no_proxy("anything"));
        std::env::remove_var("NO_PROXY");
    }

    #[test]
    fn test_is_no_proxy_exact() {
        std::env::set_var("NO_PROXY", "localhost");
        assert!(is_no_proxy("localhost"));
        assert!(!is_no_proxy("otherhost"));
        std::env::remove_var("NO_PROXY");
    }

    #[test]
    fn test_is_no_proxy_suffix() {
        std::env::set_var("NO_PROXY", ".example.com");
        assert!(is_no_proxy("db.example.com"));
        assert!(!is_no_proxy("example.com"));
        std::env::remove_var("NO_PROXY");
    }

    #[test]
    fn test_resolve_proxy_from_env_empty() {
        // Should return None when no env vars are set
        assert!(resolve_proxy_from_env("postgres").is_none());
    }
}
