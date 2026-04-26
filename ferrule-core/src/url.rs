use crate::error::CoreError;
use indexmap::IndexMap;
use secrecy::SecretString;
use url::Url;

/// Parsed database connection URL.
#[derive(Debug, Clone)]
pub struct DatabaseUrl {
    raw: String,
    parsed: Url,
}

/// Parsed components of an SSH-tunnelled connection URL.
///
/// Produced by [`DatabaseUrl::ssh_target`] when the URL scheme starts with
/// `ssh+` (e.g. `ssh+postgres://`). The `target_url` describes the inner
/// database endpoint as reachable from inside the SSH server; the tunnel
/// layer is responsible for substituting `127.0.0.1:<bound_port>` for the
/// host and port at connect time.
#[derive(Debug, Clone)]
pub struct SshTunnelSpec {
    /// SSH login username. `None` means "use the current OS user" — the
    /// tunnel layer resolves this default.
    pub ssh_user: Option<String>,
    /// SSH server hostname or IP. Required.
    pub ssh_host: String,
    /// SSH server port. Defaults to 22 when omitted from the URL.
    pub ssh_port: u16,
    /// Inner database URL — same scheme as the suffix after `ssh+`, with
    /// the SSH-specific query parameters stripped.
    pub target_url: DatabaseUrl,
}

impl DatabaseUrl {
    /// Parse a raw connection string.
    pub fn parse(raw: &str) -> Result<Self, CoreError> {
        let parsed = Url::parse(raw).map_err(|e| CoreError::InvalidUrl(format!("{e}")))?;
        let url = Self {
            raw: raw.to_string(),
            parsed,
        };
        // Validate SSH-tunnel URLs at parse time so `ssh_target()` is
        // infallible — any URL that survives `parse()` with an `ssh+`
        // scheme is guaranteed to have a usable tunnel spec.
        if url.is_ssh_tunnel() {
            url.validate_ssh()?;
        }
        Ok(url)
    }

    pub fn scheme(&self) -> &str {
        self.parsed.scheme()
    }

    pub fn username(&self) -> &str {
        self.parsed.username()
    }

    pub fn password(&self) -> Option<SecretString> {
        self.parsed.password().map(|p| SecretString::new(p.into()))
    }

    pub fn host(&self) -> Option<&str> {
        self.parsed.host_str()
    }

    pub fn port(&self) -> Option<u16> {
        self.parsed.port()
    }

    pub fn path(&self) -> &str {
        self.parsed.path()
    }

    pub fn database(&self) -> &str {
        // Drop the leading '/' from the path component
        self.parsed.path().trim_start_matches('/')
    }

    pub fn set_password(&mut self, password: Option<&str>) {
        let _ = self.parsed.set_password(password);
    }

    /// Query parameters as an ordered map.
    pub fn params(&self) -> IndexMap<String, String> {
        self.parsed
            .query_pairs()
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect()
    }

    /// Return a redacted display string for logging.
    pub fn redacted(&self) -> String {
        let mut url = self.parsed.clone();
        let _ = url.set_password(Some("***"));
        url.to_string()
    }

    /// Return the full raw URL string.
    pub fn as_str(&self) -> &str {
        &self.raw
    }

    /// Return the raw connection string.
    pub fn raw(&self) -> &str {
        &self.raw
    }

    /// True when the scheme starts with `ssh+` (e.g. `ssh+postgres`,
    /// `ssh+mysql`).
    pub fn is_ssh_tunnel(&self) -> bool {
        self.scheme().starts_with("ssh+")
    }

    /// Validate the SSH-specific portions of the URL. Called from
    /// [`DatabaseUrl::parse`] when the scheme starts with `ssh+`.
    fn validate_ssh(&self) -> Result<(), CoreError> {
        let params = self.params();
        if !params.contains_key("ssh_host") {
            return Err(CoreError::InvalidUrl(
                "ssh+<scheme>:// URLs require an ssh_host query parameter".into(),
            ));
        }
        if let Some(p) = params.get("ssh_port") {
            p.parse::<u16>().map_err(|e| {
                CoreError::InvalidUrl(format!("ssh_port query parameter is invalid: {e}"))
            })?;
        }
        // `validate_ssh` is only called when the scheme starts with
        // `ssh+`; `strip_prefix` therefore cannot return `None`.
        let inner_scheme = self.scheme().strip_prefix("ssh+").unwrap_or("");
        if inner_scheme.is_empty() {
            return Err(CoreError::InvalidUrl(
                "ssh+<scheme>:// requires a non-empty inner scheme after `ssh+`".into(),
            ));
        }
        Ok(())
    }

    /// If this URL describes an SSH-tunnelled connection, return the
    /// parsed tunnel specification. Returns `None` for non-`ssh+` schemes.
    ///
    /// SSH parameters are read from query string keys `ssh_host`,
    /// `ssh_port`, and `ssh_user`. The returned `target_url` has the
    /// `ssh+` prefix stripped from the scheme and all `ssh_*` query
    /// parameters removed.
    pub fn ssh_target(&self) -> Option<SshTunnelSpec> {
        if !self.is_ssh_tunnel() {
            return None;
        }
        let params = self.params();
        // `validate_ssh` ensures ssh_host is present and ssh_port (if
        // present) is a valid u16 — these unwraps are safe for any URL
        // that parsed successfully.
        let ssh_host = params.get("ssh_host")?.clone();
        let ssh_port = params
            .get("ssh_port")
            .and_then(|p| p.parse::<u16>().ok())
            .unwrap_or(22);
        let ssh_user = params.get("ssh_user").cloned();

        let inner_scheme = self.scheme().strip_prefix("ssh+")?;
        let target = self.build_target_url(inner_scheme)?;

        Some(SshTunnelSpec {
            ssh_user,
            ssh_host,
            ssh_port,
            target_url: target,
        })
    }

    /// Construct the inner-scheme `DatabaseUrl` by stripping the `ssh+`
    /// scheme prefix and the `ssh_*` query parameters from the original
    /// raw URL. We rewrite the raw string rather than mutating
    /// `url::Url` so that the resulting `target_url.raw()` round-trips
    /// cleanly through any downstream driver — `Url::set_scheme` is
    /// finicky about special-vs-non-special transitions.
    fn build_target_url(&self, inner_scheme: &str) -> Option<DatabaseUrl> {
        let scheme_with_sep = format!("{}://", self.scheme());
        let after_scheme = self.raw.strip_prefix(&scheme_with_sep)?;
        let mut rebuilt = format!("{}://{}", inner_scheme, after_scheme);

        // Drop ssh_* query parameters from the rebuilt URL. We re-parse
        // and re-serialize to leverage the url crate's escaping rules
        // rather than hand-splicing query strings.
        let mut parsed_inner = Url::parse(&rebuilt).ok()?;
        let kept: Vec<(String, String)> = parsed_inner
            .query_pairs()
            .filter(|(k, _)| !k.starts_with("ssh_"))
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect();
        if kept.is_empty() {
            parsed_inner.set_query(None);
        } else {
            parsed_inner.query_pairs_mut().clear();
            for (k, v) in &kept {
                parsed_inner.query_pairs_mut().append_pair(k, v);
            }
        }
        rebuilt = parsed_inner.to_string();

        Some(DatabaseUrl {
            raw: rebuilt,
            parsed: parsed_inner,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ssh_target_none_for_plain_url() {
        let url = DatabaseUrl::parse("postgres://app:pwd@db.example.com:5432/myapp").unwrap();
        assert!(!url.is_ssh_tunnel());
        assert!(url.ssh_target().is_none());
    }

    #[test]
    fn ssh_target_parses_postgres_tunnel() {
        let raw = "ssh+postgres://app:pwd@10.0.0.5:5432/myapp\
                   ?ssh_host=bastion.example.com&ssh_user=ec2-user&ssh_port=2222";
        let url = DatabaseUrl::parse(raw).unwrap();
        assert!(url.is_ssh_tunnel());
        let spec = url.ssh_target().expect("ssh+postgres should have a tunnel spec");

        assert_eq!(spec.ssh_host, "bastion.example.com");
        assert_eq!(spec.ssh_port, 2222);
        assert_eq!(spec.ssh_user.as_deref(), Some("ec2-user"));

        // Inner URL: ssh+ stripped, ssh_* params removed, db creds preserved.
        assert_eq!(spec.target_url.scheme(), "postgres");
        assert_eq!(spec.target_url.username(), "app");
        assert_eq!(spec.target_url.host(), Some("10.0.0.5"));
        assert_eq!(spec.target_url.port(), Some(5432));
        assert_eq!(spec.target_url.database(), "myapp");
        assert!(
            !spec.target_url.params().contains_key("ssh_host"),
            "ssh_* params must be stripped from target_url"
        );
        assert!(!spec.target_url.params().contains_key("ssh_user"));
        assert!(!spec.target_url.params().contains_key("ssh_port"));
    }

    #[test]
    fn ssh_target_defaults_port_to_22() {
        let raw = "ssh+mysql://app:pwd@10.0.0.5:3306/db?ssh_host=bastion";
        let spec = DatabaseUrl::parse(raw).unwrap().ssh_target().unwrap();
        assert_eq!(spec.ssh_port, 22);
        assert_eq!(spec.ssh_user, None);
        assert_eq!(spec.target_url.scheme(), "mysql");
    }

    #[test]
    fn ssh_target_preserves_non_ssh_query_params() {
        let raw = "ssh+postgres://app:pwd@10.0.0.5:5432/myapp\
                   ?sslmode=disable&ssh_host=bastion&application_name=ferrule";
        let spec = DatabaseUrl::parse(raw).unwrap().ssh_target().unwrap();
        let params = spec.target_url.params();
        assert_eq!(params.get("sslmode").map(String::as_str), Some("disable"));
        assert_eq!(
            params.get("application_name").map(String::as_str),
            Some("ferrule")
        );
        assert!(!params.contains_key("ssh_host"));
    }

    #[test]
    fn ssh_url_without_ssh_host_fails_at_parse() {
        let raw = "ssh+postgres://app:pwd@10.0.0.5:5432/myapp";
        let err = DatabaseUrl::parse(raw).unwrap_err();
        match err {
            CoreError::InvalidUrl(msg) => assert!(
                msg.contains("ssh_host"),
                "diagnostic should mention ssh_host: {msg}"
            ),
            other => panic!("expected InvalidUrl, got {:?}", other),
        }
    }

    #[test]
    fn ssh_url_with_invalid_port_fails_at_parse() {
        let raw = "ssh+postgres://app:pwd@10.0.0.5:5432/myapp\
                   ?ssh_host=bastion&ssh_port=not-a-number";
        let err = DatabaseUrl::parse(raw).unwrap_err();
        match err {
            CoreError::InvalidUrl(msg) => assert!(
                msg.contains("ssh_port"),
                "diagnostic should mention ssh_port: {msg}"
            ),
            other => panic!("expected InvalidUrl, got {:?}", other),
        }
    }

    #[test]
    fn ssh_url_with_empty_inner_scheme_fails() {
        // `ssh+://...` is syntactically a URL but semantically has no
        // inner scheme. We reject it at parse.
        let raw = "ssh+://app@host?ssh_host=bastion";
        // `ssh+` parses as the scheme (everything before `:`); the
        // resulting parsed.scheme() is `ssh+`.
        let err = DatabaseUrl::parse(raw).unwrap_err();
        match err {
            CoreError::InvalidUrl(msg) => assert!(
                msg.contains("inner scheme") || msg.contains("ssh+"),
                "diagnostic should mention empty inner scheme: {msg}"
            ),
            other => panic!("expected InvalidUrl, got {:?}", other),
        }
    }

    #[test]
    fn ssh_target_redacted_strips_password() {
        let raw = "ssh+postgres://app:supersecret@10.0.0.5:5432/myapp?ssh_host=bastion";
        let url = DatabaseUrl::parse(raw).unwrap();
        let spec = url.ssh_target().unwrap();
        assert!(
            !spec.target_url.redacted().contains("supersecret"),
            "redacted form must not leak password"
        );
        assert!(
            !url.redacted().contains("supersecret"),
            "redacted form on the outer URL must not leak password either"
        );
    }
}
