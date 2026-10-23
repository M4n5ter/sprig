//! Mount-independent inode index and random-access reads, testable without FUSE.

use std::collections::{BTreeMap, HashMap};
use std::ffi::{OsStr, OsString};
use std::io::{self, Read};
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::content::{self, EntryKind, Manifest};
use fuser::{FileAttr, FileType};

use super::objects::{self as store, ChunkCache, ObjectStore};
use crate::content::Result;

pub const ROOT_INODE: u64 = 1;
pub const MAX_READ_BYTES: usize = 1024 * 1024;
/// Advisory source identity, not proof that an OverlayFS upper entry is unchanged.
pub const ORIGIN_XATTR: &str = "user.sprig.origin";
/// Fresh for each FUSE instance, even when the immutable manifest is unchanged.
pub const SESSION_XATTR: &str = "user.sprig.session";
// Conservative Linux readlink reply bound for the intended 4 KiB-page runner.
pub const MAX_SYMLINK_BYTES: usize = 4095;

struct ChunkSpan {
    hash: String,
    start: u64,
    end: u64,
}

enum Data {
    Directory { children: Vec<DirectoryEntry> },
    File { chunks: Vec<ChunkSpan> },
    Symlink { target: Vec<u8> },
}

struct Node {
    attr: FileAttr,
    parent: u64,
    data: Data,
    origin: Vec<u8>,
}

pub struct DirectoryEntry {
    pub name: OsString,
    pub inode: u64,
    pub kind: FileType,
}

/// Inodes follow lexicographically sorted manifest paths, rooted at 1. They are
/// stable across mounts of the same immutable manifest, not across revisions.
pub struct Projection {
    nodes: Vec<Node>,
    store: ObjectStore,
    cache: ChunkCache,
    logical_bytes: u64,
    unique_chunks: usize,
    read_requests: u64,
    returned_bytes: u64,
    session: String,
}

impl Projection {
    pub fn new(objects: &Path, manifest: Manifest, uid: u32, gid: u32) -> Result<Self> {
        content::validate_manifest(&manifest)?;
        let store = ObjectStore::open(objects)?;
        let mut random = [0_u8; 32];
        std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open("/dev/urandom")?
            .read_exact(&mut random)?;
        let session = blake3::Hash::from_bytes(random).to_hex().to_string();
        // Hash the validated manifest value, not its filename, JSON whitespace,
        // or a branch name that may later refer to a different snapshot.
        let manifest_hash = blake3::hash(&serde_json::to_vec(&manifest)?)
            .to_hex()
            .to_string();
        let paths: BTreeMap<_, _> = manifest
            .keys()
            .enumerate()
            .map(|(index, path)| (path.clone(), index as u64 + ROOT_INODE))
            .collect();
        let mut lengths = HashMap::new();
        let mut nodes = Vec::with_capacity(manifest.len());
        let mut logical_bytes = 0_u64;
        for (path, entry) in manifest {
            if path.split('/').any(|name| name.len() > 255) {
                return Err(store::invalid("manifest component exceeds 255 bytes").into());
            }
            let inode = paths[&path];
            let parent_path = path.rsplit_once('/').map_or("", |(parent, _)| parent);
            let parent = paths[parent_path];
            let modified = modified_time(entry.modified_ns)?;
            let (kind, size, data) = match entry.kind {
                EntryKind::Directory => (
                    FileType::Directory,
                    0,
                    Data::Directory {
                        children: Vec::new(),
                    },
                ),
                EntryKind::File { size, chunks } => {
                    let mut offset = 0_u64;
                    let mut spans = Vec::with_capacity(chunks.len());
                    for hash in chunks {
                        let length = match lengths.get(&hash) {
                            Some(length) => *length,
                            None => {
                                let length = store.chunk_length(&hash)?;
                                lengths.insert(hash.clone(), length);
                                length
                            }
                        };
                        let end = offset
                            .checked_add(length as u64)
                            .ok_or_else(|| store::invalid("file size overflow"))?;
                        spans.push(ChunkSpan {
                            hash,
                            start: offset,
                            end,
                        });
                        offset = end;
                    }
                    if offset != size {
                        return Err(store::invalid(format!(
                            "chunk lengths do not match manifest size at {path:?}"
                        ))
                        .into());
                    }
                    logical_bytes = logical_bytes
                        .checked_add(size)
                        .ok_or_else(|| store::invalid("logical byte count overflow"))?;
                    (FileType::RegularFile, size, Data::File { chunks: spans })
                }
                EntryKind::Symlink { target } => {
                    if target.len() > MAX_SYMLINK_BYTES {
                        return Err(store::invalid(
                            "symlink target exceeds the 4095-byte Linux reply limit",
                        )
                        .into());
                    }
                    (
                        FileType::Symlink,
                        target.len() as u64,
                        Data::Symlink { target },
                    )
                }
            };
            let attr = FileAttr {
                ino: inode,
                size,
                blocks: size.div_ceil(512),
                atime: modified,
                mtime: modified,
                ctime: modified,
                crtime: modified,
                kind,
                perm: entry.mode as u16,
                nlink: if kind == FileType::Directory { 2 } else { 1 },
                uid,
                gid,
                rdev: 0,
                flags: 0,
                blksize: 4096,
            };
            nodes.push(Node {
                attr,
                parent,
                data,
                origin: serde_json::to_vec(&[manifest_hash.as_str(), path.as_str()])?,
            });
            if inode != ROOT_INODE {
                let parent = &mut nodes[(parent - ROOT_INODE) as usize];
                let Data::Directory { children } = &mut parent.data else {
                    unreachable!("manifest parents validated")
                };
                children.push(DirectoryEntry {
                    name: OsString::from(path.rsplit('/').next().expect("nonroot path has a name")),
                    inode,
                    kind,
                });
                if kind == FileType::Directory {
                    parent.attr.nlink = parent
                        .attr
                        .nlink
                        .checked_add(1)
                        .ok_or_else(|| store::invalid("directory link count overflow"))?;
                }
            }
        }
        // Full paths are ordered, but explicit sibling sorting makes the lookup
        // precondition independent from that implementation detail.
        for node in &mut nodes {
            if let Data::Directory { children } = &mut node.data {
                children.sort_by(|left, right| left.name.cmp(&right.name));
            }
        }
        Ok(Self {
            nodes,
            store,
            cache: ChunkCache::new(store::CACHE_BYTES),
            logical_bytes,
            unique_chunks: lengths.len(),
            read_requests: 0,
            returned_bytes: 0,
            session,
        })
    }

    fn node(&self, inode: u64) -> io::Result<&Node> {
        inode
            .checked_sub(ROOT_INODE)
            .and_then(|index| usize::try_from(index).ok())
            .and_then(|index| self.nodes.get(index))
            .ok_or_else(|| io::Error::from_raw_os_error(libc::ENOENT))
    }

    pub fn getattr(&self, inode: u64) -> io::Result<FileAttr> {
        Ok(self.node(inode)?.attr)
    }

    /// JSON `[manifest_digest, original_utf8_path]` for this immutable source.
    /// Copy-up may preserve this hint even after content changes: consumers must
    /// independently validate mount binding and upper-layer absence before reuse.
    pub fn origin(&self, inode: u64) -> io::Result<&[u8]> {
        let node = self.node(inode)?;
        // Linux forbids user.* attributes on symlinks. Advertising one would
        // make OverlayFS copy-up fail while copying its xattr list.
        if node.attr.kind == FileType::Symlink {
            return Err(io::Error::from_raw_os_error(libc::ENODATA));
        }
        Ok(&node.origin)
    }

    /// The nonce belongs to this running instance, not the persisted revision.
    /// It is exposed only on the immutable lower root, never on regular files.
    pub fn session(&self, inode: u64) -> io::Result<&[u8]> {
        self.node(inode)?;
        if inode != ROOT_INODE {
            return Err(io::Error::from_raw_os_error(libc::ENODATA));
        }
        Ok(self.session.as_bytes())
    }

    pub fn lookup(&self, parent: u64, name: &OsStr) -> io::Result<u64> {
        let node = self.node(parent)?;
        let Data::Directory { children } = &node.data else {
            return Err(io::Error::from_raw_os_error(libc::ENOTDIR));
        };
        if name == "." {
            return Ok(parent);
        }
        if name == ".." {
            return Ok(node.parent);
        }
        children
            .binary_search_by(|entry| entry.name.as_os_str().cmp(name))
            .map(|index| children[index].inode)
            .map_err(|_| io::Error::from_raw_os_error(libc::ENOENT))
    }

    pub fn directory(&self, inode: u64) -> io::Result<(u64, &[DirectoryEntry])> {
        let node = self.node(inode)?;
        match &node.data {
            Data::Directory { children } => Ok((node.parent, children)),
            _ => Err(io::Error::from_raw_os_error(libc::ENOTDIR)),
        }
    }

    pub fn readlink(&self, inode: u64) -> io::Result<&[u8]> {
        match &self.node(inode)?.data {
            Data::Symlink { target } => Ok(target),
            _ => Err(io::Error::from_raw_os_error(libc::EINVAL)),
        }
    }

    pub fn open(&self, inode: u64, flags: i32, directory: bool) -> io::Result<()> {
        let node = self.node(inode)?;
        if flags & libc::O_ACCMODE != libc::O_RDONLY
            || flags & (libc::O_TRUNC | libc::O_CREAT | libc::O_APPEND) != 0
        {
            return Err(io::Error::from_raw_os_error(libc::EROFS));
        }
        match (&node.data, directory) {
            (Data::Directory { .. }, true) | (Data::File { .. }, false) => Ok(()),
            (_, true) => Err(io::Error::from_raw_os_error(libc::ENOTDIR)),
            (Data::Directory { .. }, false) => Err(io::Error::from_raw_os_error(libc::EISDIR)),
            (Data::Symlink { .. }, false) => Err(io::Error::from_raw_os_error(libc::ELOOP)),
        }
    }

    /// The kernel checks permissions (including supplementary groups) via the
    /// mandatory default_permissions mount option. This fallback is conservative
    /// when called directly and checks the request's primary uid/gid only.
    pub fn access(&self, inode: u64, uid: u32, gid: u32, mask: i32) -> io::Result<()> {
        let attr = self.getattr(inode)?;
        if mask & !(libc::R_OK | libc::W_OK | libc::X_OK) != 0 {
            return Err(io::Error::from_raw_os_error(libc::EINVAL));
        }
        if mask & libc::W_OK != 0 {
            return Err(io::Error::from_raw_os_error(libc::EROFS));
        }
        let requested = mask as u16;
        let permission = if uid == attr.uid {
            attr.perm >> 6
        } else if gid == attr.gid {
            attr.perm >> 3
        } else {
            attr.perm
        } & 0o7;
        if uid == 0 {
            if mask & libc::X_OK == 0 || attr.kind == FileType::Directory || attr.perm & 0o111 != 0
            {
                return Ok(());
            }
        } else if permission & requested == requested {
            return Ok(());
        }
        Err(io::Error::from_raw_os_error(libc::EACCES))
    }

    /// Allocates only the requested response, never a whole file. Chunk lookup
    /// starts with binary search, then touches just the overlapping spans.
    pub fn read(&mut self, inode: u64, offset: i64, size: u32) -> io::Result<Vec<u8>> {
        if offset < 0 || size as usize > MAX_READ_BYTES {
            return Err(io::Error::from_raw_os_error(libc::EINVAL));
        }
        let index = inode
            .checked_sub(ROOT_INODE)
            .and_then(|n| usize::try_from(n).ok())
            .ok_or_else(|| io::Error::from_raw_os_error(libc::ENOENT))?;
        let node = self
            .nodes
            .get(index)
            .ok_or_else(|| io::Error::from_raw_os_error(libc::ENOENT))?;
        let chunks = match &node.data {
            Data::File { chunks } => chunks,
            Data::Directory { .. } => return Err(io::Error::from_raw_os_error(libc::EISDIR)),
            Data::Symlink { .. } => return Err(io::Error::from_raw_os_error(libc::EINVAL)),
        };
        self.read_requests = self.read_requests.saturating_add(1);
        let start = offset as u64;
        let end = start.saturating_add(size as u64).min(node.attr.size);
        if start >= end {
            return Ok(Vec::new());
        }
        let mut output = Vec::with_capacity((end - start) as usize);
        let first = chunks.partition_point(|chunk| chunk.end <= start);
        for chunk in chunks[first..].iter().take_while(|chunk| chunk.start < end) {
            let bytes = self.cache.get(&self.store, &chunk.hash)?;
            if bytes.len() as u64 != chunk.end - chunk.start {
                return Err(store::invalid("chunk length changed since indexing"));
            }
            let from = start.saturating_sub(chunk.start) as usize;
            let to = (end.min(chunk.end) - chunk.start) as usize;
            output.extend_from_slice(&bytes[from..to]);
        }
        self.returned_bytes = self.returned_bytes.saturating_add(output.len() as u64);
        Ok(output)
    }

    pub fn stats(&self) -> serde_json::Value {
        serde_json::json!({"entries": self.nodes.len(), "logical_bytes": self.logical_bytes, "unique_chunks": self.unique_chunks, "read_requests": self.read_requests, "returned_bytes": self.returned_bytes, "cache_bytes": self.cache.bytes(), "cache_limit_bytes": store::CACHE_BYTES, "cache_hits": self.cache.hits, "cache_misses": self.cache.misses, "verified_chunk_bytes": self.cache.verified_bytes})
    }

    pub fn total_bytes(&self) -> u64 {
        self.logical_bytes
    }
    pub fn inode_count(&self) -> u64 {
        self.nodes.len() as u64
    }
}

fn modified_time(nanoseconds: i128) -> io::Result<SystemTime> {
    // fuser 0.16 does not normalize negative fractional timestamps correctly.
    // Fail closed for all pre-epoch times rather than expose incorrect metadata.
    if nanoseconds < 0 {
        return Err(store::invalid(
            "pre-1970 mtime is unsupported by this fuser 0.16 projection",
        ));
    }
    let seconds = u64::try_from(nanoseconds / 1_000_000_000)
        .map_err(|_| store::invalid("mtime out of range"))?;
    let duration = Duration::new(seconds, (nanoseconds % 1_000_000_000) as u32);
    UNIX_EPOCH
        .checked_add(duration)
        .ok_or_else(|| store::invalid("mtime out of range"))
}

#[cfg(test)]
mod tests {
    use std::ffi::{OsStr, OsString};
    use std::fs::{self, File, FileTimes};
    use std::os::unix::ffi::OsStringExt;
    use std::os::unix::fs::{PermissionsExt, symlink};
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{Duration, UNIX_EPOCH};

    use super::super::objects::{self as store, ChunkCache, ObjectStore};
    use super::{MAX_READ_BYTES, Projection, ROOT_INODE};
    use crate::content::{self, Entry, EntryKind, Manifest, Observed};
    use fuser::FileType;

    static NEXT: AtomicU64 = AtomicU64::new(0);

    struct Fixture {
        path: PathBuf,
    }
    impl Fixture {
        fn new() -> Self {
            let path = Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("target/test-work")
                .join(format!(
                    "{}-{}",
                    std::process::id(),
                    NEXT.fetch_add(1, Ordering::Relaxed)
                ));
            fs::create_dir_all(&path).expect("create test-owned fixture");
            Self { path }
        }
        fn source(&self) -> PathBuf {
            self.path.join("source")
        }
        fn library(&self) -> PathBuf {
            self.path.join("library")
        }
        fn manifest(&self) -> Manifest {
            serde_json::from_slice(&fs::read(self.library().join("manifest.json")).unwrap())
                .unwrap()
        }
        fn load(&self) -> Projection {
            let manifest = self.manifest();
            let digest = blake3::hash(&serde_json::to_vec(&manifest).unwrap())
                .to_hex()
                .to_string();
            let projection = Projection::new(
                &self.library().join("objects"),
                manifest.clone(),
                1000,
                1000,
            )
            .unwrap();
            for (index, (path, entry)) in manifest.iter().enumerate() {
                let origin = projection.origin(index as u64 + ROOT_INODE);
                if matches!(entry.kind, EntryKind::Symlink { .. }) {
                    assert_eq!(origin.unwrap_err().raw_os_error(), Some(libc::ENODATA));
                } else {
                    let actual: (String, String) = serde_json::from_slice(origin.unwrap()).unwrap();
                    assert_eq!(actual, (digest.clone(), path.clone()));
                }
            }
            projection
        }
        fn import(&self) {
            fs::create_dir_all(self.library()).unwrap();
            let (manifest, _) = content::capture(
                &self.source(),
                &self.library().join("objects"),
                &Observed::new(),
            )
            .unwrap();
            fs::write(
                self.library().join("manifest.json"),
                serde_json::to_vec(&manifest).unwrap(),
            )
            .unwrap();
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    fn data(length: usize) -> Vec<u8> {
        let mut state = 0x5a17_9b13_291d_u64;
        (0..length)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                state as u8
            })
            .collect()
    }

    fn entry(id: &str, kind: EntryKind) -> Entry {
        Entry {
            id: id.into(),
            mode: 0o755,
            modified_ns: 0,
            kind,
        }
    }

    fn handbuilt(fixture: &Fixture, chunks: &[Vec<u8>]) -> Vec<String> {
        let root = fixture.library();
        fs::create_dir_all(root.join("objects")).expect("create objects");
        let hashes: Vec<String> = chunks
            .iter()
            .map(|bytes| {
                let hash = blake3::hash(bytes).to_hex().to_string();
                fs::write(root.join("objects").join(&hash), bytes).expect("write chunk");
                hash
            })
            .collect();
        let manifest = Manifest::from([
            (String::new(), entry("root", EntryKind::Directory)),
            (
                "file".into(),
                entry(
                    "file",
                    EntryKind::File {
                        size: chunks.iter().map(|bytes| bytes.len() as u64).sum(),
                        chunks: hashes.clone(),
                    },
                ),
            ),
        ]);
        fs::write(
            root.join("manifest.json"),
            serde_json::to_vec(&manifest).expect("serialize manifest"),
        )
        .expect("write manifest");
        hashes
    }

    fn import_preserves_bytes_modes_times_symlink_and_stable_inode_layout() {
        let fixture = Fixture::new();
        fs::create_dir_all(fixture.source().join("nested/empty"))
            .expect("create fixture directories");
        let bytes = data(3 * MAX_READ_BYTES + 137);
        fs::write(fixture.source().join("nested/file"), &bytes).expect("write fixture file");
        fs::write(fixture.source().join("zero"), []).expect("write empty file");
        fs::hard_link(
            fixture.source().join("nested/file"),
            fixture.source().join("alias"),
        )
        .unwrap();
        fs::set_permissions(
            fixture.source().join("nested/file"),
            fs::Permissions::from_mode(0o751),
        )
        .expect("set fixture mode");
        let time = UNIX_EPOCH + Duration::new(1_700_000_000, 123_456_789);
        File::open(fixture.source().join("nested/file"))
            .expect("open fixture")
            .set_times(FileTimes::new().set_modified(time))
            .expect("set fixture mtime");
        let target = OsString::from_vec(b"nested/\xff-target".to_vec());
        symlink(&target, fixture.source().join("link")).expect("write byte symlink");
        fixture.import();
        let mut projection = fixture.load();
        let second = fixture.load();
        let nonce = projection.session(ROOT_INODE).unwrap();
        assert_eq!(nonce.len(), 64);
        assert!(
            nonce
                .iter()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(byte))
        );
        assert_ne!(nonce, second.session(ROOT_INODE).unwrap());
        assert_eq!(
            projection.session(2).unwrap_err().raw_os_error(),
            Some(libc::ENODATA)
        );
        let nested = projection
            .lookup(ROOT_INODE, OsStr::new("nested"))
            .expect("lookup directory");
        let file = projection
            .lookup(nested, OsStr::new("file"))
            .expect("lookup file");
        assert_eq!(
            file,
            second
                .lookup(
                    second
                        .lookup(ROOT_INODE, OsStr::new("nested"))
                        .expect("nested"),
                    OsStr::new("file")
                )
                .expect("file")
        );
        assert_eq!(
            projection.lookup(nested, OsStr::new(".")).expect("dot"),
            nested
        );
        assert_eq!(
            projection.lookup(nested, OsStr::new("..")).expect("dotdot"),
            ROOT_INODE
        );
        assert_eq!(
            projection
                .lookup(ROOT_INODE, OsStr::new(".."))
                .expect("root parent"),
            ROOT_INODE
        );
        let attr = projection.getattr(file).expect("file attributes");
        let alias = projection.lookup(ROOT_INODE, OsStr::new("alias")).unwrap();
        assert_ne!(
            alias, file,
            "manifest paths do not preserve hardlink groups"
        );
        assert_eq!(projection.getattr(alias).unwrap().nlink, 1);
        assert_eq!(attr.kind, FileType::RegularFile);
        assert_eq!(attr.perm, 0o751);
        assert_eq!(attr.mtime, time);
        assert_eq!(attr.size, bytes.len() as u64);
        assert_eq!(
            projection
                .getattr(nested)
                .expect("directory attributes")
                .nlink,
            3
        );
        let link = projection
            .lookup(ROOT_INODE, OsStr::new("link"))
            .expect("lookup link");
        assert_eq!(
            projection.readlink(link).expect("read link"),
            b"nested/\xff-target"
        );
        let (_, children) = projection.directory(nested).expect("directory listing");
        assert_eq!(
            children
                .iter()
                .map(|entry| entry.name.as_os_str())
                .collect::<Vec<_>>(),
            [OsStr::new("empty"), OsStr::new("file")]
        );
        for (offset, size) in [
            (0, 0),
            (0, 1),
            (15_999, 400_001),
            (262_100, 512_000),
            (MAX_READ_BYTES - 1, MAX_READ_BYTES),
            (bytes.len() - 11, 999),
            (bytes.len(), 20),
            (bytes.len() + 20, 20),
        ] {
            assert_eq!(
                projection
                    .read(file, offset as i64, size as u32)
                    .expect("bounded random read"),
                bytes[offset.min(bytes.len())..(offset + size).min(bytes.len())]
            );
        }
        let zero = projection
            .lookup(ROOT_INODE, OsStr::new("zero"))
            .expect("lookup zero");
        assert!(projection.read(zero, 0, 100).expect("read zero").is_empty());
        assert_eq!(
            projection
                .read(file, -1, 1)
                .expect_err("negative offset")
                .raw_os_error(),
            Some(libc::EINVAL)
        );
        assert_eq!(
            projection
                .read(file, 0, MAX_READ_BYTES as u32 + 1)
                .expect_err("oversized read")
                .raw_os_error(),
            Some(libc::EINVAL)
        );
        assert_eq!(
            projection
                .read(ROOT_INODE, 0, 1)
                .expect_err("directory read")
                .raw_os_error(),
            Some(libc::EISDIR)
        );
        assert!(projection.getattr(0).is_err());
        assert!(projection.lookup(file, OsStr::new("x")).is_err());
    }

    fn reading_a_file_larger_than_cache_is_bounded_and_does_not_materialize_it() {
        let fixture = Fixture::new();
        fs::create_dir(fixture.source()).expect("create source");
        let bytes = data(store::CACHE_BYTES + 3 * MAX_READ_BYTES);
        fs::write(fixture.source().join("large"), &bytes).expect("write large source");
        fixture.import();
        // The projection remains independent of the original tree.
        fs::remove_dir_all(fixture.source()).expect("remove test-owned source");
        let mut projection = fixture.load();
        assert_eq!(projection.stats()["verified_chunk_bytes"], 0);
        let inode = projection
            .lookup(ROOT_INODE, OsStr::new("large"))
            .expect("lookup large");
        let mut digest = blake3::Hasher::new();
        for offset in (0..bytes.len()).step_by(MAX_READ_BYTES) {
            let result = projection
                .read(inode, offset as i64, MAX_READ_BYTES as u32)
                .expect("stream read");
            assert!(result.len() <= MAX_READ_BYTES);
            digest.update(&result);
            assert!(
                projection.stats()["cache_bytes"]
                    .as_u64()
                    .expect("cache bytes")
                    <= store::CACHE_BYTES as u64
            );
        }
        assert_eq!(digest.finalize(), blake3::hash(&bytes));
        let mut entries: Vec<_> = fs::read_dir(fixture.library())
            .expect("library listing")
            .map(|entry| entry.expect("library entry").file_name())
            .collect();
        entries.sort();
        assert_eq!(
            entries,
            [OsString::from("manifest.json"), OsString::from("objects")]
        );
    }

    fn chunk_lru_checks_hashes_before_caching_and_evicts_the_least_recently_used() {
        let fixture = Fixture::new();
        let chunks = [vec![1; 65_536], vec![2; 65_536], vec![3; 65_536]];
        let hashes = handbuilt(&fixture, &chunks);
        let store = ObjectStore::open(&fixture.library().join("objects")).expect("load store");
        let mut cache = ChunkCache::new(131_072);
        assert_eq!(cache.get(&store, &hashes[0]).expect("A"), chunks[0]);
        cache.get(&store, &hashes[1]).expect("B");
        cache.get(&store, &hashes[0]).expect("A hit");
        cache.get(&store, &hashes[2]).expect("C evicts B");
        assert_eq!(cache.bytes(), 131_072);
        assert_eq!((cache.hits, cache.misses), (1, 3));
        cache.get(&store, &hashes[1]).expect("B reload");
        assert_eq!(cache.misses, 4);
        // Cached data is the previously verified immutable content, not a new disk read.
        fs::write(
            fixture.library().join("objects").join(&hashes[1]),
            vec![9; 65_536],
        )
        .expect("corrupt object");
        assert_eq!(
            cache.get(&store, &hashes[1]).expect("cached verified B"),
            chunks[1]
        );
        let mut cold = ChunkCache::new(131_072);
        assert!(cold.get(&store, &hashes[1]).is_err());
        assert_eq!(cold.bytes(), 0);
        for hash in [
            "../escape".to_owned(),
            "A".repeat(64),
            "0".repeat(63),
            format!("{}\0", "0".repeat(63)),
        ] {
            assert!(cold.get(&store, &hash).is_err());
        }
    }

    fn missing_symlink_oversize_and_length_mismatch_objects_are_rejected() {
        let fixture = Fixture::new();
        let hashes = handbuilt(&fixture, &[vec![1; 32]]);
        let object = fixture.library().join("objects").join(&hashes[0]);
        fs::remove_file(&object).expect("remove test chunk");
        assert!(
            Projection::new(&fixture.library().join("objects"), fixture.manifest(), 0, 0).is_err()
        );
        fs::write(fixture.path.join("outside"), vec![1; 32]).expect("write symlink target");
        symlink(fixture.path.join("outside"), &object).expect("replace object with symlink");
        assert!(
            Projection::new(&fixture.library().join("objects"), fixture.manifest(), 0, 0).is_err()
        );
        fs::remove_file(&object).expect("remove test symlink");
        fs::write(&object, vec![1; 33]).expect("wrong length chunk");
        assert!(
            Projection::new(&fixture.library().join("objects"), fixture.manifest(), 0, 0).is_err()
        );
        fs::write(&object, vec![1; store::MAX_CHUNK_BYTES + 1]).expect("oversize chunk");
        assert!(
            Projection::new(&fixture.library().join("objects"), fixture.manifest(), 0, 0).is_err()
        );
        let linked = fixture.path.join("linked");
        symlink(fixture.library(), &linked).unwrap();
        let mut invalid_manifest = fixture.manifest();
        invalid_manifest.get_mut("file").unwrap().id = "root".into();
        assert!(
            Projection::new(&fixture.library().join("objects"), invalid_manifest, 0, 0).is_err()
        );
        assert!(ObjectStore::open(&linked.join("objects")).is_err());
        assert!(ObjectStore::open(&fixture.library().join("objects/../objects")).is_err());
    }

    fn readonly_permissions_reject_every_write_intent_and_respect_mode() {
        let fixture = Fixture::new();
        handbuilt(&fixture, &[vec![1; 32]]);
        let projection = fixture.load();
        let inode = projection
            .lookup(ROOT_INODE, OsStr::new("file"))
            .expect("lookup");
        assert!(projection.open(inode, libc::O_RDONLY, false).is_ok());
        for flag in [
            libc::O_WRONLY,
            libc::O_RDWR,
            libc::O_RDONLY | libc::O_TRUNC,
            libc::O_RDONLY | libc::O_APPEND,
            libc::O_RDONLY | libc::O_CREAT,
        ] {
            assert_eq!(
                projection
                    .open(inode, flag, false)
                    .expect_err("write rejected")
                    .raw_os_error(),
                Some(libc::EROFS)
            );
        }
        assert!(projection.open(ROOT_INODE, libc::O_RDONLY, true).is_ok());
        assert!(projection.open(inode, libc::O_RDONLY, true).is_err());
        assert!(
            projection
                .access(inode, 1001, 1001, libc::R_OK | libc::X_OK)
                .is_ok()
        );
        assert_eq!(
            projection
                .access(inode, 0, 0, libc::W_OK)
                .expect_err("root write rejected")
                .raw_os_error(),
            Some(libc::EROFS)
        );
    }

    fn pre_epoch_timestamps_fail_closed_without_changing_the_manifest() {
        let fixture = Fixture::new();
        handbuilt(&fixture, &[vec![1; 32]]);
        let path = fixture.library().join("manifest.json");
        let mut manifest: Manifest =
            serde_json::from_slice(&fs::read(&path).expect("read manifest"))
                .expect("decode manifest");
        for nanoseconds in [-1_000_000_000, -500_000_000, -1] {
            manifest.get_mut("file").expect("file entry").modified_ns = nanoseconds;
            let encoded = serde_json::to_vec(&manifest).expect("encode timestamp fixture");
            fs::write(&path, &encoded).expect("write timestamp fixture");
            let error =
                match Projection::new(&fixture.library().join("objects"), fixture.manifest(), 0, 0)
                {
                    Ok(_) => panic!("pre-epoch metadata must not be exposed incorrectly"),
                    Err(error) => error,
                };
            assert!(error.to_string().contains("pre-1970 mtime"));
            assert_eq!(fs::read(&path).expect("unchanged manifest"), encoded);
        }
        for nanoseconds in [0, 1, 500_000_000] {
            manifest.get_mut("file").expect("file entry").modified_ns = nanoseconds;
            fs::write(
                &path,
                serde_json::to_vec(&manifest).expect("encode timestamp"),
            )
            .expect("write valid timestamp");
            let projection = fixture.load();
            let inode = projection
                .lookup(ROOT_INODE, OsStr::new("file"))
                .expect("lookup");
            assert_eq!(
                projection.getattr(inode).expect("attributes").mtime,
                UNIX_EPOCH + Duration::from_nanos(nanoseconds as u64)
            );
        }
    }

    fn symlink_targets_fit_the_linux_readlink_reply() {
        let fixture = Fixture::new();
        handbuilt(&fixture, &[vec![1; 32]]);
        let path = fixture.library().join("manifest.json");
        let mut manifest: Manifest =
            serde_json::from_slice(&fs::read(&path).expect("read manifest"))
                .expect("decode manifest");
        for length in [4095, 4096] {
            manifest.insert(
                "link".into(),
                entry(
                    "link",
                    EntryKind::Symlink {
                        target: vec![b'x'; length],
                    },
                ),
            );
            fs::write(
                &path,
                serde_json::to_vec(&manifest).expect("encode symlink"),
            )
            .expect("write symlink fixture");
            let result =
                Projection::new(&fixture.library().join("objects"), fixture.manifest(), 0, 0);
            if length == 4095 {
                let projection = result.expect("maximum safe readlink reply accepted");
                let inode = projection
                    .lookup(ROOT_INODE, OsStr::new("link"))
                    .expect("lookup link");
                assert_eq!(projection.readlink(inode).expect("read link").len(), length);
            } else {
                let error = match result {
                    Ok(_) => panic!("oversized readlink reply must be rejected before mount"),
                    Err(error) => error,
                };
                assert!(error.to_string().contains("4095-byte"));
            }
        }
    }

    // Keep the public test surface small while grouping related invariant checks.
    #[test]
    fn projection_roundtrip_and_readonly_contract() {
        import_preserves_bytes_modes_times_symlink_and_stable_inode_layout();
        reading_a_file_larger_than_cache_is_bounded_and_does_not_materialize_it();
        readonly_permissions_reject_every_write_intent_and_respect_mode();
    }

    #[test]
    fn bounded_cache_and_invalid_store_contract() {
        chunk_lru_checks_hashes_before_caching_and_evicts_the_least_recently_used();
        missing_symlink_oversize_and_length_mismatch_objects_are_rejected();
        pre_epoch_timestamps_fail_closed_without_changing_the_manifest();
        symlink_targets_fit_the_linux_readlink_reply();
    }
}
