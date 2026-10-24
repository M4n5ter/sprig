//! Native workspaces and content-addressed, immutable revision history.
//! All mutations share one commit engine and optimistic branch-head checks.
#![deny(unsafe_op_in_unsafe_fn)]

#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
compile_error!("Sprig supports Linux, macOS and Windows");

pub mod blame;
pub mod content;
pub mod copy_tree;
mod file_cache;
mod manifest_pages;
pub mod merge;
mod metadata;
mod model;
mod packed;
mod platform;
#[cfg(all(target_os = "linux", feature = "projection"))]
pub mod projection;
mod store;
pub mod writable;
#[cfg(all(target_os = "linux", feature = "projection"))]
pub mod writable_fuse;
#[cfg(all(target_os = "linux", feature = "projection"))]
pub mod writable_lifecycle;
mod writable_session;

pub use blame::{BlameLine, LineBlame as Blame};
pub use copy_tree::CopyMode;
pub use merge::MergeStrategy;
pub use model::*;
pub use store::Store;

/// Errors retain their underlying filesystem or validation diagnostics.
pub type Error = Box<dyn std::error::Error + Send + Sync>;
/// The result of a managed store operation.
pub type Result<T> = std::result::Result<T, Error>;
