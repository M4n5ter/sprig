//! Immutable, content-defined chunk storage for revision manifests.
//!
//! Capturing requires a trusted, quiescent source. Leaf opens do not follow
//! symlinks, but this is not a sandbox against concurrent ancestor replacement.

use std::collections::{BTreeMap, BTreeSet};
#[cfg(unix)]
use std::ffi::OsString;
use std::fs::{self, File, FileTimes};
use std::io::{self, Cursor, Read, Write};
#[cfg(unix)]
use std::os::unix::ffi::{OsStrExt, OsStringExt};
#[cfg(unix)]
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use fastcdc::v2020::FastCDC;
use serde::{Deserialize, Serialize};

use crate::packed;
use crate::platform::{self, Metadata};

pub type Error = Box<dyn std::error::Error + Send + Sync>;
pub type Result<T> = std::result::Result<T, Error>;
pub type Manifest = BTreeMap<String, Entry>;
pub type Observed = BTreeMap<String, Observation>;

const MIN_CHUNK: usize = 16 * 1024;
const AVG_CHUNK: usize = 64 * 1024;
const MAX_CHUNK: usize = 256 * 1024;
/// Merge helpers deliberately refuse to load arbitrarily large files into RAM.
pub const MAX_READ_FILE: u64 = 16 * 1024 * 1024;
static NONCE: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Entry {
    pub id: String,
    pub mode: u32,
    /// Signed Unix nanoseconds; access times, ownership and ACLs are not tracked.
    #[serde(default)]
    pub modified_ns: i128,
    pub kind: EntryKind,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum EntryKind {
    Directory,
    File { size: u64, chunks: Vec<String> },
    Symlink { target: Vec<u8> },
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Observation {
    pub id: String,
    pub device: u64,
    pub inode: u64,
}

/// Captures all entries, including the root directory at the empty-string key.
pub fn capture(root: &Path, objects: &Path, previous: &Observed) -> Result<(Manifest, Observed)> {
    let root = normalized_root(root)?;
    let root_metadata = platform::symlink_metadata(&root)?;
    if !root_metadata.is_dir() || root_metadata.file_type().is_symlink() {
        return invalid("capture root must be a real directory, not a symbolic link");
    }
    prepare_objects(objects)?;
    if fs::canonicalize(objects)?.starts_with(fs::canonicalize(&root)?) {
        return invalid("object store must not be inside the captured tree");
    }
    let mut manifest = Manifest::new();
    let mut observed = Observed::new();
    // Reuse allocation only, never content: every file is read and verified.
    let mut batch = ObjectBatch::open(objects)?;
    capture_entry(&root, "", &mut batch, &mut manifest, &mut observed)?;
    assign_ids(&mut manifest, &mut observed, previous);
    validate_manifest(&manifest)?;
    // Flush all publications together before returning a usable manifest. Do
    // this even for dedup-only captures: an earlier failure may have left an
    // existing, fully synced chunk with an unsynced directory entry.
    batch.finish()?;
    Ok((manifest, observed))
}

fn capture_entry(
    path: &Path,
    relative: &str,
    batch: &mut ObjectBatch,
    manifest: &mut Manifest,
    observed: &mut Observed,
) -> Result<()> {
    validate_path(relative)?;
    let metadata = platform::symlink_metadata(path)?;
    platform::validate_capture_metadata(path, &metadata)?;
    let kind = if metadata.file_type().is_symlink() {
        EntryKind::Symlink {
            target: read_symlink(path)?,
        }
    } else if metadata.is_dir() {
        EntryKind::Directory
    } else if metadata.is_file() {
        let mut input = open_regular(path)?;
        let before = platform::file_metadata(&input)?;
        if !unchanged(&metadata, &before) {
            return invalid(format!("source changed during capture: {}", path.display()));
        }
        let kind = batch.put_reader(&mut input)?;
        let after = platform::file_metadata(&input)?;
        let EntryKind::File { size, .. } = &kind else {
            unreachable!()
        };
        if *size != before.len() || !unchanged(&before, &after) {
            return invalid(format!("file changed during capture: {}", path.display()));
        }
        kind
    } else {
        return invalid(format!("special file rejected: {}", path.display()));
    };
    manifest.insert(
        relative.to_owned(),
        Entry {
            id: String::new(),
            mode: metadata.mode() & 0o777,
            modified_ns: metadata.mtime() as i128 * 1_000_000_000 + metadata.mtime_nsec() as i128,
            kind,
        },
    );
    observed.insert(
        relative.to_owned(),
        Observation {
            id: String::new(),
            device: metadata.dev(),
            inode: metadata.ino(),
        },
    );
    if metadata.is_dir() {
        let mut children = fs::read_dir(path)?.collect::<io::Result<Vec<_>>>()?;
        children.sort_by_key(|entry| entry.file_name());
        for child in children {
            let name = child.file_name().into_string().map_err(|_| {
                io::Error::new(io::ErrorKind::InvalidData, "non-UTF-8 path rejected")
            })?;
            let child_relative = if relative.is_empty() {
                name
            } else {
                format!("{relative}/{name}")
            };
            capture_entry(&child.path(), &child_relative, batch, manifest, observed)?;
        }
        if !unchanged(&metadata, &platform::symlink_metadata(path)?) {
            return invalid(format!(
                "directory changed during capture: {}",
                path.display()
            ));
        }
    }
    Ok(())
}

fn assign_ids(manifest: &mut Manifest, observed: &mut Observed, previous: &Observed) {
    let mut used = BTreeSet::new();
    // Reserve same-path identities first, so an atomic editor save stays the
    // same logical file and a rename cannot steal an extant path's identity.
    for (path, current) in observed.iter_mut() {
        if let Some(old) = previous.get(path)
            && !old.id.is_empty()
            && used.insert(old.id.clone())
        {
            current.id.clone_from(&old.id);
        }
    }
    let mut old_inodes: BTreeMap<(u64, u64), Vec<&Observation>> = BTreeMap::new();
    for old in previous.values() {
        old_inodes
            .entry((old.device, old.inode))
            .or_default()
            .push(old);
    }
    let mut current_counts = BTreeMap::new();
    for current in observed.values() {
        *current_counts
            .entry((current.device, current.inode))
            .or_insert(0_usize) += 1;
    }
    for (path, current) in observed.iter_mut() {
        if current.id.is_empty() {
            let key = (current.device, current.inode);
            if current_counts[&key] == 1
                && let Some(candidates) = old_inodes.get(&key)
                && candidates.len() == 1
            {
                let id = &candidates[0].id;
                if !id.is_empty() && used.insert(id.clone()) {
                    current.id.clone_from(id);
                }
            }
            if current.id.is_empty() {
                loop {
                    let id = unique_id(path, current.device, current.inode);
                    if used.insert(id.clone()) {
                        current.id = id;
                        break;
                    }
                }
            }
        }
        manifest
            .get_mut(path)
            .expect("observation has an entry")
            .id
            .clone_from(&current.id);
    }
}

/// Writes a fresh tree only. The caller owns staging and atomic publication.
pub fn materialize(manifest: &Manifest, objects: &Path, target: &Path) -> Result<()> {
    validate_manifest(manifest)?;
    validate_native_manifest(manifest)?;
    let target = normalized_root(target)?;
    ensure_directory_chain(
        target
            .parent()
            .filter(|path| !path.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new(".")),
    )?;
    ensure_directory_chain(objects)?;
    // create_dir, not create_dir_all: an existing target is never modified.
    platform::create_dir(&target, 0o700)?;
    platform::set_path_mode(&target, 0o700)?;
    let reader = packed::Reader::open(objects)?;
    let mut directories = Vec::new();
    for (relative, entry) in manifest {
        let path = target.join(relative);
        match &entry.kind {
            EntryKind::Directory => {
                if !relative.is_empty() {
                    platform::create_dir(&path, 0o700)?;
                    platform::set_path_mode(&path, 0o700)?;
                }
                directories.push((path, entry.mode, entry.modified_ns));
            }
            EntryKind::File { size, chunks } => {
                let mut output = platform::create_new(&path, 0o600)?;
                let mut written = 0_u64;
                for hash in chunks {
                    let bytes = reader.read(hash)?;
                    written = written
                        .checked_add(bytes.len() as u64)
                        .ok_or_else(|| io::Error::other("file size overflow"))?;
                    if written > *size {
                        return invalid(format!("chunk lengths exceed file size: {relative}"));
                    }
                    output.write_all(&bytes)?;
                }
                if written != *size {
                    return invalid(format!("chunk lengths do not match file size: {relative}"));
                }
                platform::set_mode(&output, entry.mode)?;
                output
                    .set_times(FileTimes::new().set_modified(modified_time(entry.modified_ns)?))?;
                output.sync_all()?;
            }
            EntryKind::Symlink { target: link } => {
                create_symlink(link, &path)?;
                #[cfg(target_os = "macos")]
                set_symlink_permissions(&path, entry.mode)?;
                crate::copy_tree::set_symlink_modified(&path, entry.modified_ns)?;
            }
        }
    }
    // Child directories are finalized before parents, including mode-000 dirs.
    directories.sort_by_key(|(path, _, _)| std::cmp::Reverse(path.components().count()));
    for (path, mode, modified_ns) in directories {
        let directory = platform::open_directory(&path)?;
        platform::set_mode(&directory, mode)?;
        directory.set_times(FileTimes::new().set_modified(modified_time(modified_ns)?))?;
        platform::sync_directory_file(&directory)?;
    }
    platform::sync_directory(
        target
            .parent()
            .filter(|path| !path.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new(".")),
    )?;
    Ok(())
}

/// Read requested contents with per-chunk hash and whole-file size checks.
/// Unvisited index corruption is diagnosed by full-store validation, not this
/// point-read operation.
pub fn read_file(entry: &Entry, objects: &Path) -> Result<Vec<u8>> {
    ensure_directory_chain(objects)?;
    let mut reader = packed::PointReader::open(objects)?;
    read_file_with(entry, |hash| reader.read(hash))
}

pub(crate) fn read_file_from(entry: &Entry, reader: &packed::Reader) -> Result<Vec<u8>> {
    read_file_with(entry, |hash| reader.read(hash))
}

fn read_file_with(entry: &Entry, mut read: impl FnMut(&str) -> Result<Vec<u8>>) -> Result<Vec<u8>> {
    let EntryKind::File { size, chunks } = &entry.kind else {
        return invalid("entry is not a regular file");
    };
    if *size > MAX_READ_FILE {
        return invalid(format!(
            "file exceeds the {MAX_READ_FILE}-byte merge read limit"
        ));
    }
    let mut output = Vec::with_capacity(*size as usize);
    for hash in chunks {
        let bytes = read(hash)?;
        if output.len() as u64 + bytes.len() as u64 > *size {
            return invalid("chunk lengths exceed file size");
        }
        output.extend_from_slice(&bytes);
    }
    if output.len() as u64 != *size {
        return invalid("chunk lengths do not match file size");
    }
    Ok(output)
}

/// A bounded writer for one durability batch of immutable content objects.
///
/// Every existing object is read and hash-verified, including repeated chunks
/// within this batch. The allocation is reused, never trusted object content.
/// Callers must successfully call `finish` before publishing or returning a
/// manifest containing the entries returned by `put_reader`. Dropping a batch
/// does not make directory entries durable. An error may leave valid objects
/// behind; retrying verifies them and `finish` syncs dedup-only batches too.
#[must_use = "finish the batch before publishing its object references"]
pub struct ObjectBatch {
    batch: packed::Batch,
    buffer: Vec<u8>,
}

impl ObjectBatch {
    pub fn open(objects: &Path) -> Result<Self> {
        prepare_objects(objects)?;
        Ok(Self {
            batch: packed::Batch::open(objects)?,
            buffer: vec![0_u8; MAX_CHUNK],
        })
    }

    pub fn put_reader(&mut self, reader: &mut impl Read) -> Result<EntryKind> {
        put_reader(reader, &mut self.batch, &mut self.buffer)
    }

    /// Flush all publications, even if this batch only reused existing chunks.
    pub fn finish(self) -> Result<()> {
        self.batch.finish()
    }
}

pub fn put_bytes(bytes: &[u8], objects: &Path) -> Result<EntryKind> {
    let mut batch = ObjectBatch::open(objects)?;
    let kind = batch.put_reader(&mut Cursor::new(bytes))?;
    batch.finish()?;
    Ok(kind)
}

// Publishes fully synced chunk inodes; callers must sync the object directory
// before returning any references to them outside a capture/put_bytes batch.
fn put_reader(
    reader: &mut impl Read,
    batch: &mut packed::Batch,
    buffer: &mut [u8],
) -> Result<EntryKind> {
    debug_assert_eq!(buffer.len(), MAX_CHUNK);
    // Each file starts with no valid bytes; stale buffer tails are never chunked.
    // Refill to the maximum chunk length before cutting the first chunk. This
    // preserves CDC boundaries across read-buffer edges and bounds RAM usage.
    let mut filled = 0;
    let mut eof = false;
    let mut size = 0_u64;
    let mut chunks = Vec::new();
    loop {
        while filled < MAX_CHUNK && !eof {
            match reader.read(&mut buffer[filled..]) {
                Ok(0) => eof = true,
                Ok(length) => filled += length,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(error.into()),
            }
        }
        if filled == 0 {
            break;
        }
        let chunk = FastCDC::new(
            &buffer[..filled],
            MIN_CHUNK as _,
            AVG_CHUNK as _,
            MAX_CHUNK as _,
        )
        .next()
        .expect("nonempty buffer has a chunk");
        chunks.push(batch.put(&buffer[..chunk.length])?);
        size = size
            .checked_add(chunk.length as u64)
            .ok_or_else(|| io::Error::other("file size overflow"))?;
        buffer.copy_within(chunk.length..filled, 0);
        filled -= chunk.length;
    }
    Ok(EntryKind::File { size, chunks })
}

pub fn referenced_chunks(manifest: &Manifest) -> BTreeSet<String> {
    manifest
        .values()
        .filter_map(|entry| match &entry.kind {
            EntryKind::File { chunks, .. } => Some(chunks.iter().cloned()),
            _ => None,
        })
        .flatten()
        .collect()
}

pub fn validate_manifest(manifest: &Manifest) -> Result<()> {
    if !matches!(
        manifest.get("").map(|entry| &entry.kind),
        Some(EntryKind::Directory)
    ) {
        return invalid("manifest must contain a root directory at the empty path");
    }
    let mut identities = BTreeSet::new();
    for (path, entry) in manifest {
        validate_path(path)?;
        if entry.id.is_empty() || !identities.insert(&entry.id) {
            return invalid(format!("empty or duplicate logical identity at {path:?}"));
        }
        modified_time(entry.modified_ns)?;
        if entry.mode & !0o777 != 0 {
            return invalid(format!("unsafe permission bits at {path:?}"));
        }
        if !path.is_empty() {
            let parent = path.rsplit_once('/').map_or("", |(parent, _)| parent);
            if !matches!(
                manifest.get(parent).map(|entry| &entry.kind),
                Some(EntryKind::Directory)
            ) {
                return invalid(format!("missing or non-directory ancestor for {path:?}"));
            }
        }
        match &entry.kind {
            EntryKind::Directory => {}
            EntryKind::File { size, chunks } => {
                if (*size == 0) != chunks.is_empty() || chunks.len() as u64 > *size {
                    return invalid(format!("invalid chunk count at {path:?}"));
                }
                if *size > (chunks.len() as u64).saturating_mul(MAX_CHUNK as u64) {
                    return invalid(format!("file size exceeds chunk capacity at {path:?}"));
                }
                for hash in chunks {
                    validate_hash(hash)?;
                }
            }
            EntryKind::Symlink { target } => {
                if target.is_empty() || target.contains(&0) {
                    return invalid(format!("invalid symbolic link target at {path:?}"));
                }
            }
        }
    }
    Ok(())
}

fn modified_time(nanoseconds: i128) -> Result<SystemTime> {
    let absolute = nanoseconds.unsigned_abs();
    let seconds: u64 = (absolute / 1_000_000_000)
        .try_into()
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "mtime out of range"))?;
    let duration = Duration::new(seconds, (absolute % 1_000_000_000) as u32);
    let time = if nanoseconds < 0 {
        UNIX_EPOCH.checked_sub(duration)
    } else {
        UNIX_EPOCH.checked_add(duration)
    };
    time.ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "mtime out of range").into())
}

fn validate_path(path: &str) -> Result<()> {
    if path.is_empty() {
        return Ok(());
    }
    platform::validate_relative_path(path)?;
    if path.contains('\0')
        || path
            .split('/')
            .any(|part| part.is_empty() || part == "." || part == "..")
    {
        return invalid(format!("invalid relative manifest path: {path:?}"));
    }
    Ok(())
}

fn validate_hash(hash: &str) -> Result<()> {
    if hash.len() != 64
        || !hash
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return invalid(format!("invalid chunk address: {hash:?}"));
    }
    Ok(())
}

fn normalized_root(path: &Path) -> Result<PathBuf> {
    if path.as_os_str().is_empty() {
        return invalid("empty filesystem path");
    }
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::ParentDir => {
                return invalid("parent-directory syntax is not allowed in tree roots");
            }
            Component::CurDir => {}
            other => normalized.push(other.as_os_str()),
        }
    }
    if normalized.as_os_str().is_empty() {
        normalized.push(".");
    }
    Ok(normalized)
}

fn ensure_directory_chain(path: &Path) -> Result<()> {
    let path = normalized_root(path)?;
    let mut current = PathBuf::new();
    for component in path.components() {
        current.push(component.as_os_str());
        if matches!(component, Component::Prefix(_)) {
            continue;
        }
        let metadata = platform::symlink_metadata(&current)?;
        if !metadata.is_dir() || metadata.file_type().is_symlink() {
            return invalid(format!(
                "directory path contains a symbolic link or non-directory: {}",
                current.display()
            ));
        }
    }
    Ok(())
}

fn prepare_objects(objects: &Path) -> Result<()> {
    // Check existing ancestors before creation, then verify the complete chain.
    let objects = normalized_root(objects)?;
    let mut current = PathBuf::new();
    for component in objects.components() {
        current.push(component.as_os_str());
        if matches!(component, Component::Prefix(_)) {
            continue;
        }
        match platform::symlink_metadata(&current) {
            Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {}
            Ok(_) => return invalid("object store path contains a symbolic link or non-directory"),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                platform::create_dir(&current, 0o700)?;
                platform::sync_directory(
                    current
                        .parent()
                        .filter(|path| !path.as_os_str().is_empty())
                        .unwrap_or_else(|| Path::new(".")),
                )?;
            }
            Err(error) => return Err(error.into()),
        }
    }
    ensure_directory_chain(&objects)
}

#[cfg(target_os = "macos")]
fn set_symlink_permissions(path: &Path, mode: u32) -> Result<()> {
    let path = std::ffi::CString::new(path.as_os_str().as_bytes())?;
    // SAFETY: a live NUL-terminated path and ordinary permission bits; lchmod
    // changes the link itself, never its potentially external target.
    if unsafe { libc::lchmod(path.as_ptr(), mode as libc::mode_t) } != 0 {
        return Err(io::Error::last_os_error().into());
    }
    Ok(())
}

fn open_regular(path: &Path) -> Result<File> {
    Ok(platform::open_regular(path)?)
}

#[cfg(unix)]
fn read_symlink(path: &Path) -> Result<Vec<u8>> {
    Ok(fs::read_link(path)?.as_os_str().as_bytes().to_vec())
}

#[cfg(windows)]
fn read_symlink(_path: &Path) -> Result<Vec<u8>> {
    invalid("symbolic links are not supported on Windows")
}

#[cfg(unix)]
fn create_symlink(target: &[u8], path: &Path) -> Result<()> {
    Ok(std::os::unix::fs::symlink(
        OsString::from_vec(target.to_vec()),
        path,
    )?)
}

#[cfg(windows)]
fn create_symlink(_target: &[u8], _path: &Path) -> Result<()> {
    invalid("symbolic links are not supported on Windows")
}

fn validate_native_manifest(manifest: &Manifest) -> Result<()> {
    #[cfg(unix)]
    let _ = manifest;
    #[cfg(windows)]
    for (path, entry) in manifest {
        let expected_mode = match entry.kind {
            EntryKind::Directory => 0o755,
            EntryKind::File { .. } => 0o644,
            EntryKind::Symlink { .. } => {
                return invalid("symbolic links are not supported on Windows");
            }
        };
        if entry.mode != expected_mode || entry.modified_ns % 100 != 0 {
            return invalid(format!(
                "Windows cannot preserve this entry's POSIX mode or sub-100ns timestamp: {path:?}"
            ));
        }
    }
    Ok(())
}

fn same_node(left: &Metadata, right: &Metadata) -> bool {
    left.dev() == right.dev() && left.ino() == right.ino() && left.file_type() == right.file_type()
}

fn unchanged(left: &Metadata, right: &Metadata) -> bool {
    same_node(left, right)
        && left.len() == right.len()
        && left.mtime() == right.mtime()
        && left.mtime_nsec() == right.mtime_nsec()
        && left.ctime() == right.ctime()
        && left.ctime_nsec() == right.ctime_nsec()
}

fn unique_id(path: &str, device: u64, inode: u64) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(
        &SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
            .to_le_bytes(),
    );
    hasher.update(&std::process::id().to_le_bytes());
    hasher.update(&NONCE.fetch_add(1, Ordering::Relaxed).to_le_bytes());
    hasher.update(&device.to_le_bytes());
    hasher.update(&inode.to_le_bytes());
    hasher.update(path.as_bytes());
    hasher.finalize().to_hex().to_string()
}

fn invalid<T>(message: impl Into<String>) -> Result<T> {
    Err(io::Error::new(io::ErrorKind::InvalidData, message.into()).into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_paths_addresses_and_root_syntax() {
        for path in ["/absolute", "a/../b", "a/./b", "a//b", "a/"] {
            assert!(validate_path(path).is_err());
        }
        assert!(validate_path("").is_ok());
        assert!(validate_path("space name/中文").is_ok());
        assert!(validate_hash(&"f".repeat(64)).is_ok());
        assert!(validate_hash(&"F".repeat(64)).is_err());

        assert_eq!(
            normalized_root(Path::new("link/./")).unwrap(),
            PathBuf::from("link")
        );
        assert!(normalized_root(Path::new("link/../other")).is_err());
    }
}
