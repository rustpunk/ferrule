//! Shared CLI path utilities (used by `history` and `cache` for
//! `[history] path = "~/..."` / `[cache] path = "~/..."` resolution).

use std::path::PathBuf;

/// Expand a leading `~/` to the user's home directory. Returns the
/// input unchanged if it doesn't start with `~/` or if the home
/// directory can't be resolved.
pub fn expand_tilde(s: &str) -> PathBuf {
    if let Some(rest) = s.strip_prefix("~/") {
        if let Some(home) = dirs::home_dir() {
            return home.join(rest);
        }
    }
    PathBuf::from(s)
}
