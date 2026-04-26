use secrecy::ExposeSecret;
use secrecy::SecretString;

/// Resolve a password from the environment (`FERRULE_{NAME}_PASSWORD`).
/// Returns `None` if the variable is unset or empty.
pub fn resolve_env_password(name: &str) -> Option<SecretString> {
    let env_var = format!(
        "FERRULE_{}_PASSWORD",
        name.to_ascii_uppercase().replace('-', "_")
    );
    std::env::var(&env_var)
        .ok()
        .filter(|s| !s.is_empty())
        .map(|v| SecretString::new(v.into()))
}

/// Resolve a password from the OS keyring.
/// Returns `None` if no entry is found or keyring is unavailable.
#[cfg(feature = "keyring")]
pub fn resolve_keyring_password(name: &str) -> Option<SecretString> {
    let entry = keyring::Entry::new("ferrule", name).ok()?;
    match entry.get_password() {
        Ok(pwd) if !pwd.is_empty() => Some(SecretString::new(pwd.into())),
        _ => None,
    }
}

#[cfg(not(feature = "keyring"))]
pub fn resolve_keyring_password(_name: &str) -> Option<SecretString> {
    None
}

/// Store a password in the OS keyring.
#[cfg(feature = "keyring")]
pub fn set_keyring_password(
    name: &str,
    password: &SecretString,
) -> Result<(), crate::error::ConfigError> {
    let entry = keyring::Entry::new("ferrule", name)
        .map_err(|e| crate::error::ConfigError::KeyringError(e.to_string()))?;
    entry
        .set_password(password.expose_secret())
        .map_err(|e| crate::error::ConfigError::KeyringError(e.to_string()))?;
    Ok(())
}

#[cfg(not(feature = "keyring"))]
pub fn set_keyring_password(
    _name: &str,
    _password: &SecretString,
) -> Result<(), crate::error::ConfigError> {
    Err(crate::error::ConfigError::KeyringError(
        "keyring support is not enabled in this build".into(),
    ))
}

/// Delete a password from the OS keyring.
#[cfg(feature = "keyring")]
pub fn delete_keyring_password(name: &str) -> Result<(), crate::error::ConfigError> {
    let entry = keyring::Entry::new("ferrule", name)
        .map_err(|e| crate::error::ConfigError::KeyringError(e.to_string()))?;
    entry
        .delete_credential()
        .map_err(|e| crate::error::ConfigError::KeyringError(e.to_string()))?;
    Ok(())
}

#[cfg(not(feature = "keyring"))]
pub fn delete_keyring_password(_name: &str) -> Result<(), crate::error::ConfigError> {
    Err(crate::error::ConfigError::KeyringError(
        "keyring support is not enabled in this build".into(),
    ))
}
