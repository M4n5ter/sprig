//! Lossless storage for immutable JSON metadata.
//!
//! Revision identities belong to the uncompressed JSON bytes, not to their
//! compressed representation. Reads retain the complete decoded document for
//! fast slice-based parsing, just as plain JSON metadata did. Store contents
//! must remain trusted; document size and model allocations are not bounded.

use std::fs::OpenOptions;
use std::io::{self, BufRead, Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

use serde::de::DeserializeOwned;

use crate::Result;

// Bound codec working memory, not document size. The matching encoder setting
// permits arbitrarily large metadata without requiring a larger decode window.
const WINDOW_LOG: u32 = 20;

/// Encodes exactly these JSON bytes in one checksummed, fast zstd frame.
/// The caller remains responsible for durable, immutable publication.
pub(crate) fn encode(bytes: &[u8]) -> Result<Vec<u8>> {
    let mut encoder = zstd::stream::write::Encoder::new(Vec::new(), 1)?;
    encoder.window_log(WINDOW_LOG)?;
    encoder.include_checksum(true)?;
    encoder.set_pledged_src_size(Some(bytes.len() as u64))?;
    encoder.write_all(bytes)?;
    Ok(encoder.finish()?)
}

/// Reads one complete JSON document and returns its uncompressed BLAKE3 hash.
/// Plain JSON, incomplete frames, trailing bytes and multiple frames are errors.
pub(crate) fn read<T: DeserializeOwned>(path: &Path) -> Result<(T, blake3::Hash)> {
    // Do not follow a replaced leaf symlink or block opening a substituted FIFO.
    // Ancestor paths still require the store's trusted, single-user ownership.
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)?;
    if !file.metadata()?.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "metadata must be a regular file",
        )
        .into());
    }

    let mut decoder = zstd::stream::read::Decoder::new(file)?.single_frame();
    decoder.window_log_max(WINDOW_LOG)?;
    let mut bytes = Vec::new();
    // Read through decoded EOF to validate the frame epilogue and checksum,
    // including when the final JSON value precedes the end of the frame.
    decoder.read_to_end(&mut bytes)?;
    let mut compressed = decoder.finish();
    if !compressed.fill_buf()?.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "trailing data after metadata frame",
        )
        .into());
    }
    let hash = blake3::hash(&bytes);
    let value = serde_json::from_slice(&bytes)?;
    Ok((value, hash))
}
