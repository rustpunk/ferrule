//! `ferrule-config` — Connection registry, credential resolution, and profiles.

pub mod bookmarks;
pub mod credentials;
pub mod error;
pub mod parse;
pub mod profile;
pub mod registry;

pub use bookmarks::{Bookmark, BookmarkStore};
pub use credentials::resolve_password_stack;
pub use profile::{
    CacheConfig, ConnectionProfile, DefaultProfile, GlobalConfig, HistoryConfig, SlowLogConfig,
};
pub use registry::{ConnectionEntry, ConnectionRegistry};
