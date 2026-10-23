//! Descriptor-relative, no-follow object reads and a bounded verified chunk cache.

use std::collections::HashMap;
use std::ffi::{CString, OsStr};
use std::fs::{File, OpenOptions};
use std::io::{self, Read};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Component, Path};

pub(crate) type Result<T> = crate::content::Result<T>;

pub(crate) const MAX_CHUNK_BYTES: usize = 256 * 1024;
pub(crate) const CACHE_BYTES: usize = 16 * 1024 * 1024;

/// Pins the object directory; every ancestor and object leaf is opened without
/// following symlinks. Published objects must remain immutable. Cache entries
/// contain only bytes already verified against their content address.
pub(crate) struct ObjectStore {
    objects: File,
}

impl ObjectStore {
    pub(crate) fn open(path: &Path) -> io::Result<Self> {
        if path.as_os_str().is_empty() {
            return Err(invalid("empty object directory path"));
        }
        let mut directory = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(if path.is_absolute() {
                Path::new("/")
            } else {
                Path::new(".")
            })?;
        for component in path.components() {
            match component {
                Component::RootDir | Component::CurDir => {}
                Component::Normal(name) => directory = open_at(&directory, name, true)?,
                _ => {
                    return Err(invalid(
                        "parent-directory syntax is forbidden in object paths",
                    ));
                }
            }
        }
        Ok(Self { objects: directory })
    }

    pub fn chunk_length(&self, hash: &str) -> io::Result<usize> {
        let length = self.open_chunk(hash)?.metadata()?.len();
        if length == 0 || length > MAX_CHUNK_BYTES as u64 {
            return Err(invalid("chunk length is outside 1..=256 KiB"));
        }
        Ok(length as usize)
    }

    fn open_chunk(&self, hash: &str) -> io::Result<File> {
        validate_hash(hash)?;
        open_at(&self.objects, OsStr::new(hash), false)
    }

    fn verified_chunk(&self, hash: &str) -> io::Result<Vec<u8>> {
        let file = self.open_chunk(hash)?;
        let length = file.metadata()?.len();
        if length == 0 || length > MAX_CHUNK_BYTES as u64 {
            return Err(invalid("invalid chunk length"));
        }
        let mut bytes = Vec::with_capacity(length as usize);
        file.take(MAX_CHUNK_BYTES as u64 + 1)
            .read_to_end(&mut bytes)?;
        if bytes.len() as u64 != length || blake3::hash(&bytes).to_hex().as_str() != hash {
            return Err(invalid(format!("corrupt content-addressed object: {hash}")));
        }
        Ok(bytes)
    }
}

fn open_at(directory: &File, name: &OsStr, is_directory: bool) -> io::Result<File> {
    let name = CString::new(name.as_bytes()).map_err(|_| invalid("NUL in filename"))?;
    let flags = libc::O_RDONLY
        | libc::O_CLOEXEC
        | libc::O_NOFOLLOW
        | libc::O_NONBLOCK
        | if is_directory { libc::O_DIRECTORY } else { 0 };
    // SAFETY: directory owns a live descriptor; name is a terminated CString.
    // A successful descriptor is adopted exactly once by File below.
    let descriptor = unsafe { libc::openat(directory.as_raw_fd(), name.as_ptr(), flags) };
    if descriptor < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: openat returned this new, uniquely owned valid descriptor.
    let file = unsafe { File::from_raw_fd(descriptor) };
    let metadata = file.metadata()?;
    if !(if is_directory {
        metadata.is_dir()
    } else {
        metadata.is_file()
    }) {
        return Err(invalid("unexpected non-regular object store entry"));
    }
    Ok(file)
}

fn validate_hash(hash: &str) -> io::Result<()> {
    if hash.len() != 64
        || !hash
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(invalid(
            "chunk address must be 64 lowercase hexadecimal bytes",
        ));
    }
    Ok(())
}

struct CachedChunk {
    bytes: Vec<u8>,
    last_used: u64,
}

/// Content-only memory is capped; metadata and one response buffer are separate.
pub(crate) struct ChunkCache {
    chunks: HashMap<String, CachedChunk>,
    bytes: usize,
    limit: usize,
    clock: u64,
    pub hits: u64,
    pub misses: u64,
    pub verified_bytes: u64,
}

impl ChunkCache {
    pub fn new(limit: usize) -> Self {
        Self {
            chunks: HashMap::new(),
            bytes: 0,
            limit,
            clock: 0,
            hits: 0,
            misses: 0,
            verified_bytes: 0,
        }
    }

    pub fn bytes(&self) -> usize {
        self.bytes
    }

    pub fn get<'a>(&'a mut self, store: &ObjectStore, hash: &str) -> io::Result<&'a [u8]> {
        // Validate even cache hits; no caller can use a non-canonical address.
        validate_hash(hash)?;
        self.clock = self
            .clock
            .checked_add(1)
            .ok_or_else(|| invalid("cache clock exhausted"))?;
        if !self.chunks.contains_key(hash) {
            self.misses = self.misses.saturating_add(1);
            let bytes = store.verified_chunk(hash)?;
            if bytes.len() > self.limit {
                return Err(invalid("cache limit is smaller than this chunk"));
            }
            while self.bytes + bytes.len() > self.limit {
                let oldest = self
                    .chunks
                    .iter()
                    .min_by_key(|(_, chunk)| chunk.last_used)
                    .map(|(hash, _)| hash.clone())
                    .expect("nonempty cache exceeds limit");
                self.bytes -= self
                    .chunks
                    .remove(&oldest)
                    .expect("oldest chunk exists")
                    .bytes
                    .len();
            }
            self.bytes += bytes.len();
            self.verified_bytes = self.verified_bytes.saturating_add(bytes.len() as u64);
            self.chunks.insert(
                hash.to_owned(),
                CachedChunk {
                    bytes,
                    last_used: self.clock,
                },
            );
        } else {
            self.hits = self.hits.saturating_add(1);
        }
        let cached = self
            .chunks
            .get_mut(hash)
            .expect("chunk inserted or already cached");
        cached.last_used = self.clock;
        Ok(&cached.bytes)
    }
}

pub(crate) fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}
