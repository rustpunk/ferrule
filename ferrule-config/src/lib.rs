//! `ferrule-config` — Connection registry, credential resolution, and profiles.

pub mod credentials;
pub mod error;
pub mod profile;
pub mod registry;

pub use credentials::resolve_env_password;
pub use registry::{ConnectionEntry, ConnectionRegistry};
