#![allow(dead_code, unused_variables, unused_imports)]

use secrecy::SecretString;

/// Resolve a password from the environment (`FERRULE_{NAME}_PASSWORD`).
/// Returns `None` if the variable is unset or empty.
pub fn resolve_env_password(name: &str) -> Option<SecretString> {
    let env_var = format!("FERRULE_{}_PASSWORD", name.to_ascii_uppercase().replace('-', "_"));
    std::env::var(&env_var)
        .ok()
        .filter(|s| !s.is_empty())
        .map(|v| SecretString::new(v.into()))
}
