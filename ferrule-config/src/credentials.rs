use hasp::Store;
use secrecy::SecretString;

/// Resolve credentials through the hasp unified stack.
///
/// Resolution order:
/// 1. `explicit` (CLI `--password` flag)
/// 2. `password_url` via `hasp::Store::get()`
/// 3. `env://FERRULE_{NAME}_PASSWORD` via hasp
/// 4. `keyring://ferrule/{name}` via hasp
///
/// Returns `Ok(None)` when no credential is found so the caller can
/// fall back to an interactive prompt.
pub fn resolve_password_stack(
    name: &str,
    explicit: Option<SecretString>,
    password_url: Option<&str>,
) -> Result<Option<SecretString>, crate::error::ConfigError> {
    if let Some(pwd) = explicit {
        return Ok(Some(pwd));
    }

    let store = Store::with_defaults();

    // 2. password_url from profile
    if let Some(url) = password_url {
        match store.get(url) {
            Ok(secret) => return Ok(Some(secret)),
            Err(ref e) => {
                if let Some(err) = warn_or_fail(url, e) {
                    return Err(err);
                }
            }
        }
    }

    // 3. Legacy env var via hasp
    let env_url = format!(
        "env://FERRULE_{}_PASSWORD",
        name.to_ascii_uppercase().replace('-', "_")
    );
    match store.get(&env_url) {
        Ok(secret) => return Ok(Some(secret)),
        Err(ref e) => {
            if let Some(err) = warn_or_fail(&env_url, e) {
                return Err(err);
            }
        }
    }

    // 4. OS keyring via hasp
    let keyring_url = format!("keyring://ferrule/{}", name);
    match store.get(&keyring_url) {
        Ok(secret) => return Ok(Some(secret)),
        Err(ref e) => {
            if let Some(err) = warn_or_fail(&keyring_url, e) {
                return Err(err);
            }
        }
    }

    Ok(None)
}

fn warn_or_fail(url: &str, err: &hasp::Error) -> Option<crate::error::ConfigError> {
    match err {
        hasp::Error::NotFound(_) => None,
        hasp::Error::PermissionDenied(_) => {
            eprintln!("Warning: hasp permission denied for {}", url);
            None
        }
        hasp::Error::AuthenticationFailed(_) => {
            eprintln!("Warning: hasp authentication failed for {}", url);
            None
        }
        e if e.is_transient() => {
            eprintln!("Warning: hasp transient failure for {}, retrying...", url);
            None
        }
        hasp::Error::InvalidUrl(msg) => Some(crate::error::ConfigError::HaspError(format!(
            "invalid hasp URL '{}': {}",
            url, msg
        ))),
        hasp::Error::UrlParse(e) => Some(crate::error::ConfigError::HaspError(format!(
            "invalid hasp URL '{}': {}",
            url, e
        ))),
        e => {
            eprintln!("Warning: hasp lookup failed for {}: {}", url, e);
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use secrecy::ExposeSecret;

    #[test]
    fn explicit_password_returned_immediately() {
        let secret = SecretString::new("hunter2".into());
        let result = resolve_password_stack(
            "prod",
            Some(secret.clone()),
            Some("env://SHOULD_NOT_BE_READ"),
        );
        assert_eq!(result.unwrap().unwrap().expose_secret(), "hunter2");
    }

    #[test]
    fn password_url_resolved_via_hasp_env() {
        std::env::set_var("FERRULE_TEST_HASP_PWD", "from_env");
        let result = resolve_password_stack("test", None, Some("env://FERRULE_TEST_HASP_PWD"));
        assert_eq!(result.unwrap().unwrap().expose_secret(), "from_env");
        std::env::remove_var("FERRULE_TEST_HASP_PWD");
    }

    #[test]
    fn legacy_env_var_via_hasp() {
        std::env::set_var("FERRULE_LEGACY_ENV_PASSWORD", "legacy");
        let result = resolve_password_stack("legacy-env", None, None);
        assert_eq!(result.unwrap().unwrap().expose_secret(), "legacy");
        std::env::remove_var("FERRULE_LEGACY_ENV_PASSWORD");
    }

    #[test]
    fn not_found_falls_through_to_none() {
        std::env::remove_var("FERRULE_NONEXISTENT_PASSWORD");
        let result = resolve_password_stack("nonexistent", None, None).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn invalid_password_url_returns_error() {
        let result = resolve_password_stack("test", None, Some("not-a-url"));
        assert!(result.is_err());
    }
}
