//! `ferrule-config` — Connection registry, credential resolution, and profiles.

pub mod bookmarks;
pub mod credentials;
pub mod error;
pub mod profile;
pub mod registry;

pub use bookmarks::{Bookmark, BookmarkStore};
pub use credentials::resolve_password_stack;
pub use profile::{ConnectionProfile, DefaultProfile, GlobalConfig};
pub use registry::{ConnectionEntry, ConnectionRegistry};
