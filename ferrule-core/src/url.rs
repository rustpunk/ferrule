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

impl DatabaseUrl {
    /// Parse a raw connection string.
    pub fn parse(raw: &str) -> Result<Self, CoreError> {
        let parsed = Url::parse(raw)
            .map_err(|e| CoreError::InvalidUrl(format!("{e}")))?;
        Ok(Self {
            raw: raw.to_string(),
            parsed,
        })
    }

    pub fn scheme(&self) -> &str {
        self.parsed.scheme()
    }

    pub fn username(&self) -> &str {
        self.parsed.username()
    }

    pub fn password(&self) -> Option<SecretString> {
        self.parsed
            .password()
            .map(|p| SecretString::new(p.into()))
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

    /// Return the raw connection string.
    pub fn raw(&self) -> &str {
        &self.raw
    }
}
