//! `ferrule-config` — Connection registry, credential resolution, and profiles.

pub mod credentials;
pub mod error;
pub mod profile;
pub mod registry;

pub use credentials::{
    delete_keyring_password, resolve_env_password, resolve_keyring_password, set_keyring_password,
};
pub use profile::{ConnectionProfile, DefaultProfile, GlobalConfig};
pub use registry::{ConnectionEntry, ConnectionRegistry};
