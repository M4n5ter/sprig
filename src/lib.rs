//! Native workspaces and content-addressed, immutable revision history.
//! All mutations share one commit engine and optimistic branch-head checks.
#![deny(unsafe_op_in_unsafe_fn)]

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
compile_error!("Sprig supports Linux and macOS");

pub mod content;
pub mod copy_tree;
pub mod merge;
mod model;
mod store;

pub use copy_tree::CopyMode;
pub use merge::MergeStrategy;
pub use model::*;
pub use store::Store;

/// Errors retain their underlying filesystem or validation diagnostics.
pub type Error = Box<dyn std::error::Error + Send + Sync>;
/// The result of a managed store operation.
pub type Result<T> = std::result::Result<T, Error>;
