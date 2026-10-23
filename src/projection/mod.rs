//! Linux-only, immutable CAS-backed filesystem projection.
//!
//! Uses the production content manifest and object directory directly. It does
//! not create an independent revision catalog or materialize regular files.
//! File content reads verify BLAKE3 addresses before entering a 16 MiB cache.
//! Logical identities are path entries, not hardlink groups: like ordinary
//! materialization, distinct captured hardlink paths have distinct inodes.

pub mod capture;
mod fuse;
pub mod lifecycle;
pub(crate) mod objects;
mod tree;

pub use fuse::{ReadOnlyFuse, ReadySignal, mount_options};
pub use tree::{
    DirectoryEntry, MAX_READ_BYTES, MAX_SYMLINK_BYTES, ORIGIN_XATTR, Projection, ROOT_INODE,
    SESSION_XATTR,
};
