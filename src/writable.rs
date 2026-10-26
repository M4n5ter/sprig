//! Owned, mount-independent writable CAS generations.
//!
//! Namespace and file mutations are serialized and journaled before becoming
//! visible. A successful write is NOT a durability promise: like an ordinary
//! buffered filesystem, callers must fsync (or checkpoint) for that promise.
//! The journal has a hard 256 MiB limit and writes are at most 1 MiB. There are
//! at most one million lifetime namespace entries and 256 GiB logical growth
//! above the pinned base; detached tombstones count toward those quotas until
//! generation rotation. Hardlink aliases count file content only once.
//! Checkpoint streams each content-dirty file through whole-file CDC; it does not
//! hash only dirty ranges. It never edits or clears this generation. Publication belongs
//! to Store, which must durably publish a new clean generation and retain this
//! one until the state handoff succeeds.
//!
//! On the first read that needs original file bytes, all of that file's base
//! chunks are read and hash-verified to derive trustworthy chunk boundaries.
//! This can cost a whole-file read even for a small random read. Later reads
//! verify only selected chunk payloads, using the cached verified span map.
//!
//! A truncated final frame is recoverable only as a read-only prefix, preserving
//! the original bytes. A complete corrupt frame, sequence gap, or foreign
//! generation fails closed. Paths to backing files are never exposed. Backing
//! directories must remain trusted and quiescent; this is not a sandbox against
//! hostile ancestor replacement. Platform helpers preserve their existing
//! durability qualifications, notably for Windows directory publication.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::content::{self, Entry, EntryKind, Manifest, ObjectBatch};
use crate::{packed, platform};

pub(crate) const ROOT_INODE: u64 = 1;
pub(crate) const MAX_WRITE_BYTES: usize = 1024 * 1024;
const MAX_JOURNAL_BYTES: u64 = 256 * 1024 * 1024;
const MAX_METADATA_BYTES: usize = 64 * 1024;
const MAX_PATH_BYTES: usize = 4096;
const MAX_NAME_BYTES: usize = 255;
const MAX_NODES: u64 = 1_000_000;
const _: () = assert!(MAX_NODES <= u32::MAX as u64);
const MAX_LOGICAL_GROWTH: u64 = 256 * 1024 * 1024 * 1024;
const MAX_FILE_BYTES: u64 = i64::MAX as u64;
const MAGIC: &[u8; 8] = b"SPRGWR01";
const VERSION: u32 = 1;
const HEADER_BYTES: usize = 92;
const JOURNAL_NAME: &str = "writable.journal";
const LEASE_NAME: &str = "writable.lease";

#[derive(Clone, Debug)]
pub(crate) struct NodeAttr {
    pub inode: u64,
    /// Live metadata. For content-dirty files, chunks remain the base references;
    /// only snapshot() produces publishable current-content chunk references.
    pub entry: Entry,
    /// Access time is session/journal metadata, not part of revision identity.
    #[cfg_attr(
        not(all(target_os = "linux", feature = "projection")),
        allow(dead_code)
    )]
    pub accessed_ns: i128,
    #[cfg_attr(
        not(all(target_os = "linux", feature = "projection")),
        allow(dead_code)
    )]
    pub nlink: u32,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SetAttr {
    pub mode: Option<u32>,
    pub size: Option<u64>,
    pub modified_ns: Option<i128>,
    pub accessed_ns: Option<i128>,
}

#[derive(Clone, Debug)]
pub(crate) struct RecoveryInfo {
    pub read_only: bool,
    pub incomplete_tail: bool,
    pub verified_bytes: u64,
}

#[derive(Clone, Debug)]
pub(crate) struct SpaceStats {
    pub journal_bytes: u64,
    pub journal_limit: u64,
    pub logical_bytes: u64,
    pub logical_limit: u64,
    pub nodes: u64,
    pub node_limit: u64,
}

pub(crate) struct WritableCas {
    objects: PathBuf,
    inner: Mutex<Inner>,
}

struct Inner {
    // This separate stable lock file is never replaced or unlinked here.
    _lease: File,
    journal: File,
    reader: packed::Reader,
    binding: String,
    nodes: BTreeMap<u64, Node>,
    paths: BTreeMap<String, u64>,
    // Retain retired entry IDs so aliases cannot evade the metadata quota.
    entry_ids: BTreeSet<String>,
    handles: BTreeMap<u64, Handle>,
    next_inode: u64,
    next_handle: u64,
    next_sequence: u64,
    journal_bytes: u64,
    logical_bytes: u64,
    logical_limit: u64,
    dirty_paths: BTreeSet<String>,
    content_dirty: BTreeSet<u64>,
    poisoned: bool,
    incomplete_tail: bool,
    abandoned_session: bool,
}

struct Node {
    entry: Entry,
    accessed_ns: i128,
    // Only this private representation is narrow; public/journal inodes stay
    // u64. Validated base IDs are unique and base.len() <= MAX_NODES, so base
    // inodes (index + 1) fit. Each Create consumes one never-removed entry ID
    // and increments next_inode once; links only consume more IDs. Therefore
    // every assigned inode <= entry_ids.len() <= MAX_NODES. Create/Rename
    // validate an existing directory parent before apply(), including replay.
    parent: u32,
    // Derived from live, non-root directory nodes' stored parents, not paths.
    // Each child inode counts once even if it has multiple namespace links.
    child_directories: u32,
    // Each directory entry keeps its own identity; inode state is shared.
    links: BTreeMap<String, String>,
    file: Option<FileData>,
}

struct FileData {
    base_size: u64,
    // Once lowered, this never increases: truncate-then-grow cannot reveal old
    // base bytes. Journal extents are also permanently clipped on shrink.
    base_visible: u64,
    spans: Option<Vec<Span>>,
    extents: Vec<Extent>,
}

struct Span {
    start: u64,
    end: u64,
    hash: String,
}

struct Extent {
    start: u64,
    end: u64,
    payload_offset: u64,
    payload_length: usize,
    digest: [u8; 32],
}

struct Handle {
    inode: u64,
    writable: bool,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Record {
    binding: String,
    operation: Operation,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
enum Operation {
    Init,
    Create {
        parent: u64,
        name: String,
        inode: u64,
        entry: Entry,
    },
    Link {
        inode: u64,
        parent: u64,
        name: String,
        entry_id: String,
        now_ns: i128,
    },
    Write {
        inode: u64,
        offset: u64,
        modified_ns: i128,
    },
    Setattr {
        inode: u64,
        attrs: SetAttr,
        now_ns: i128,
    },
    Remove {
        parent: u64,
        name: String,
        directory: bool,
        now_ns: i128,
    },
    Rename {
        parent: u64,
        name: String,
        new_parent: u64,
        new_name: String,
        now_ns: i128,
    },
}

impl WritableCas {
    /// The caller creates the private generation directory first. A journal is
    /// created exclusively, and both it and its directory are flushed before
    /// this newly initialized generation is returned.
    pub(crate) fn create(
        objects: &Path,
        generation_dir: &Path,
        base: &Manifest,
        revision: &str,
        generation: &str,
    ) -> io::Result<Self> {
        validate_directory_chain(generation_dir)?;
        let lease = acquire_lease(generation_dir, true)?;
        let created = platform::create_new(&generation_dir.join(JOURNAL_NAME), 0o600)?;
        drop(created);
        let journal = platform::open_read_write(&generation_dir.join(JOURNAL_NAME))?;
        let mut inner = Inner::new(objects, base, revision, generation, lease, journal)?;
        inner.append(Operation::Init, &[])?;
        inner.journal.sync_all()?;
        platform::sync_directory(generation_dir)?;
        if let Some(parent) = generation_dir.parent() {
            platform::sync_directory(parent)?;
        }
        Ok(Self {
            objects: objects.to_owned(),
            inner: Mutex::new(inner),
        })
    }

    /// Opens one exclusive generation. A torn final frame yields a read-only
    /// recoverable view; its bytes remain untouched for inspection/recovery.
    pub(crate) fn open(
        objects: &Path,
        generation_dir: &Path,
        base: &Manifest,
        revision: &str,
        generation: &str,
    ) -> io::Result<Self> {
        validate_directory_chain(generation_dir)?;
        let lease = acquire_lease(generation_dir, false)?;
        let journal = platform::open_read_write(&generation_dir.join(JOURNAL_NAME))?;
        let mut inner = Inner::new(objects, base, revision, generation, lease, journal)?;
        inner.replay()?;
        inner.abandoned_session = crate::writable_session::has_session(generation_dir)?;
        Ok(Self {
            objects: objects.to_owned(),
            inner: Mutex::new(inner),
        })
    }

    fn lock(&self) -> io::Result<MutexGuard<'_, Inner>> {
        self.inner
            .lock()
            .map_err(|_| io::Error::other("writable generation mutex poisoned"))
    }

    pub(crate) fn recovery_info(&self) -> io::Result<RecoveryInfo> {
        let inner = self.lock()?;
        Ok(RecoveryInfo {
            read_only: inner.poisoned || inner.incomplete_tail || inner.abandoned_session,
            incomplete_tail: inner.incomplete_tail,
            verified_bytes: inner.journal_bytes,
        })
    }

    pub(crate) fn stats(&self) -> io::Result<SpaceStats> {
        let i = self.lock()?;
        Ok(SpaceStats {
            journal_bytes: i.journal_bytes,
            journal_limit: MAX_JOURNAL_BYTES,
            logical_bytes: i.logical_bytes,
            logical_limit: i.logical_limit,
            nodes: i.entry_ids.len() as u64,
            node_limit: MAX_NODES,
        })
    }

    pub(crate) fn dirty_paths(&self) -> io::Result<BTreeSet<String>> {
        Ok(self.lock()?.dirty_paths.clone())
    }

    #[cfg_attr(
        not(all(target_os = "linux", feature = "projection")),
        allow(dead_code)
    )]
    pub(crate) fn getattr(&self, inode: u64) -> io::Result<NodeAttr> {
        self.lock()?.attr(inode)
    }

    #[cfg_attr(
        not(all(target_os = "linux", feature = "projection")),
        allow(dead_code)
    )]
    pub(crate) fn lookup(&self, parent: u64, name: &str) -> io::Result<NodeAttr> {
        let inner = self.lock()?;
        let node = inner.directory(parent, false)?;
        match name {
            "." => inner.attr(parent),
            ".." => inner.attr(u64::from(node.parent)),
            _ => inner.attr_path(&inner.child_path(parent, name)?),
        }
    }

    pub(crate) fn lookup_path(&self, path: &str) -> io::Result<NodeAttr> {
        validate_path(path)?;
        let inner = self.lock()?;
        inner.attr_path(path)
    }

    pub(crate) fn paths(&self) -> io::Result<Vec<(String, NodeAttr)>> {
        let inner = self.lock()?;
        inner
            .paths
            .keys()
            .map(|p| Ok((p.clone(), inner.attr_path(p)?)))
            .collect()
    }

    #[cfg_attr(
        not(all(target_os = "linux", feature = "projection")),
        allow(dead_code)
    )]
    pub(crate) fn readdir(&self, inode: u64) -> io::Result<Vec<(String, NodeAttr)>> {
        let inner = self.lock()?;
        let directory = inner.directory(inode, false)?;
        let Some(path) = directory.links.keys().next() else {
            return Ok(vec![]);
        };
        inner
            .paths
            .keys()
            .filter(|p| !p.is_empty() && parent_path(p) == path)
            .map(|p| Ok((leaf(p).to_owned(), inner.attr_path(p)?)))
            .collect()
    }

    /// Read-only directory handles are allowed. Inodes are never reused, and
    /// detached nodes remain private tombstones for the generation's lifetime.
    pub(crate) fn open_handle(&self, inode: u64, writable: bool) -> io::Result<u64> {
        let mut inner = self.lock()?;
        let node = inner.nodes.get(&inode).ok_or_else(not_found)?;
        match node.entry.kind {
            EntryKind::Symlink { .. } => return Err(invalid_input()),
            EntryKind::Directory if writable => return Err(is_directory()),
            _ => {}
        }
        if writable {
            inner.can_mutate()?;
        }
        let handle = inner.next_handle;
        inner.next_handle = handle.checked_add(1).ok_or_else(no_space)?;
        inner.handles.insert(handle, Handle { inode, writable });
        Ok(handle)
    }

    pub(crate) fn release(&self, handle: u64) -> io::Result<()> {
        self.lock()?
            .handles
            .remove(&handle)
            .map(|_| ())
            .ok_or_else(bad_handle)
    }

    #[cfg_attr(
        not(all(target_os = "linux", feature = "projection")),
        allow(dead_code)
    )]
    pub(crate) fn read(&self, handle: u64, offset: u64, length: usize) -> io::Result<Vec<u8>> {
        let mut inner = self.lock()?;
        let inode = inner.handles.get(&handle).ok_or_else(bad_handle)?.inode;
        inner.read(inode, offset, length)
    }

    pub(crate) fn read_inode(&self, inode: u64, offset: u64, length: usize) -> io::Result<Vec<u8>> {
        self.lock()?.read(inode, offset, length)
    }

    pub(crate) fn write(&self, handle: u64, offset: u64, data: &[u8]) -> io::Result<usize> {
        self.write_impl(handle, Some(offset), data)
    }

    #[cfg_attr(
        not(all(target_os = "linux", feature = "projection")),
        allow(dead_code)
    )]
    pub(crate) fn write_append(&self, handle: u64, data: &[u8]) -> io::Result<usize> {
        self.write_impl(handle, None, data)
    }

    fn write_impl(&self, handle: u64, offset: Option<u64>, data: &[u8]) -> io::Result<usize> {
        let mut inner = self.lock()?;
        let h = inner.handles.get(&handle).ok_or_else(bad_handle)?;
        if !h.writable {
            return Err(bad_handle());
        }
        let inode = h.inode;
        let offset = offset.unwrap_or(inner.file_size(inode)?);
        inner.can_mutate()?;
        if data.is_empty() {
            return Ok(0);
        }
        inner.mutate(
            Operation::Write {
                inode,
                offset,
                modified_ns: now_ns(),
            },
            data,
        )?;
        Ok(data.len())
    }

    pub(crate) fn setattr(&self, inode: u64, attrs: SetAttr) -> io::Result<NodeAttr> {
        let mut inner = self.lock()?;
        inner.mutate(
            Operation::Setattr {
                inode,
                attrs,
                now_ns: now_ns(),
            },
            &[],
        )?;
        inner.attr(inode)
    }

    pub(crate) fn create_file(&self, parent: u64, name: &str, mode: u32) -> io::Result<NodeAttr> {
        self.create_node(
            parent,
            name,
            mode,
            EntryKind::File {
                size: 0,
                chunks: vec![],
            },
        )
    }

    pub(crate) fn mkdir(&self, parent: u64, name: &str, mode: u32) -> io::Result<NodeAttr> {
        self.create_node(parent, name, mode, EntryKind::Directory)
    }

    pub(crate) fn symlink(&self, parent: u64, name: &str, target: &[u8]) -> io::Result<NodeAttr> {
        self.create_node(
            parent,
            name,
            0o777,
            EntryKind::Symlink {
                target: target.to_vec(),
            },
        )
    }

    fn create_node(
        &self,
        parent: u64,
        name: &str,
        mode: u32,
        kind: EntryKind,
    ) -> io::Result<NodeAttr> {
        if mode & !0o777 != 0 {
            return Err(invalid_input());
        }
        if let EntryKind::Symlink { target } = &kind {
            if target.len() > MAX_PATH_BYTES {
                return Err(name_too_long());
            }
            if target.is_empty() || target.contains(&0) {
                return Err(invalid_input());
            }
        }
        let mut inner = self.lock()?;
        let inode = inner.next_inode;
        let entry = Entry {
            id: new_id(&inner.binding, inode),
            link_group: None,
            mode,
            modified_ns: now_ns(),
            kind,
        };
        inner.mutate(
            Operation::Create {
                parent,
                name: name.into(),
                inode,
                entry,
            },
            &[],
        )?;
        inner.attr(inode)
    }

    /// Link a live regular inode within this namespace. Symlink and directory
    /// hardlinks are deliberately unsupported; no backing filesystem path escapes.
    #[cfg_attr(
        not(all(target_os = "linux", feature = "projection")),
        allow(dead_code)
    )]
    pub(crate) fn link(&self, inode: u64, parent: u64, name: &str) -> io::Result<NodeAttr> {
        let mut inner = self.lock()?;
        let entry_id = new_link_id(&inner.binding, inner.next_sequence);
        inner.mutate(
            Operation::Link {
                inode,
                parent,
                name: name.into(),
                entry_id,
                now_ns: now_ns(),
            },
            &[],
        )?;
        inner.attr_path(&inner.child_path(parent, name)?)
    }

    pub(crate) fn unlink(&self, parent: u64, name: &str) -> io::Result<()> {
        self.lock()?.mutate(
            Operation::Remove {
                parent,
                name: name.into(),
                directory: false,
                now_ns: now_ns(),
            },
            &[],
        )
    }

    pub(crate) fn rmdir(&self, parent: u64, name: &str) -> io::Result<()> {
        self.lock()?.mutate(
            Operation::Remove {
                parent,
                name: name.into(),
                directory: true,
                now_ns: now_ns(),
            },
            &[],
        )
    }

    pub(crate) fn rename(
        &self,
        parent: u64,
        name: &str,
        new_parent: u64,
        new_name: &str,
    ) -> io::Result<()> {
        self.lock()?.mutate(
            Operation::Rename {
                parent,
                name: name.into(),
                new_parent,
                new_name: new_name.into(),
                now_ns: now_ns(),
            },
            &[],
        )
    }

    /// fsync flushes the shared journal, including metadata and all open files.
    pub(crate) fn sync(&self) -> io::Result<()> {
        let mut inner = self.lock()?;
        if let Err(error) = inner.journal.sync_all() {
            inner.poisoned = true;
            return Err(error);
        }
        Ok(())
    }

    /// A prepare operation only: sync the verified journal, stream whole changed
    /// namespace files to CAS, finish the batch, then return a valid manifest.
    /// No journal, dirty set, base identity, or namespace is reset on success.
    pub(crate) fn snapshot(&self) -> io::Result<Manifest> {
        let mut inner = self.lock()?;
        if inner.poisoned {
            return Err(io::Error::other(
                "reopen poisoned generation before checkpoint",
            ));
        }
        if let Err(error) = inner.journal.sync_all() {
            inner.poisoned = true;
            return Err(error);
        }
        let mut manifest = Manifest::new();
        let paths: Vec<_> = inner.paths.iter().map(|(p, i)| (p.clone(), *i)).collect();
        let mut batch = ObjectBatch::open(&self.objects).map_err(content_error)?;
        let mut published: BTreeMap<u64, EntryKind> = BTreeMap::new();
        for (path, inode) in paths {
            let mut entry = inner.attr_path(&path)?.entry;
            if inner.content_dirty.contains(&inode) && matches!(entry.kind, EntryKind::File { .. })
            {
                // Aliases share one byte stream and one CDC pass per checkpoint.
                if let Some(kind) = published.get(&inode) {
                    entry.kind = kind.clone();
                } else {
                    let mut reader = NodeReader {
                        inner: &mut inner,
                        inode,
                        offset: 0,
                    };
                    entry.kind = batch.put_reader(&mut reader).map_err(content_error)?;
                    published.insert(inode, entry.kind.clone());
                }
            }
            manifest.insert(path, entry);
        }
        content::validate_manifest(&manifest).map_err(content_error)?;
        batch.finish().map_err(content_error)?;
        Ok(manifest)
    }
}

struct NodeReader<'a> {
    inner: &'a mut Inner,
    inode: u64,
    offset: u64,
}
impl Read for NodeReader<'_> {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        let bytes = self
            .inner
            .read(self.inode, self.offset, output.len().min(MAX_WRITE_BYTES))?;
        output[..bytes.len()].copy_from_slice(&bytes);
        self.offset += bytes.len() as u64;
        Ok(bytes.len())
    }
}

impl Inner {
    fn new(
        objects: &Path,
        base: &Manifest,
        revision: &str,
        generation: &str,
        lease: File,
        journal: File,
    ) -> io::Result<Self> {
        content::validate_manifest(base).map_err(content_error)?;
        if revision.is_empty()
            || revision.len() > 256
            || generation.is_empty()
            || generation.len() > 256
        {
            return Err(invalid("invalid writable generation identity"));
        }
        if base.len() as u64 > MAX_NODES {
            return Err(no_space());
        }
        let manifest_bytes = serde_json::to_vec(base).map_err(invalid_data)?;
        let binding = blake3::hash(
            &serde_json::to_vec(&(
                VERSION,
                revision,
                generation,
                blake3::hash(&manifest_bytes).to_hex().to_string(),
            ))
            .map_err(invalid_data)?,
        )
        .to_hex()
        .to_string();
        let mut nodes: BTreeMap<u64, Node> = BTreeMap::new();
        let mut paths = BTreeMap::new();
        let mut groups = BTreeMap::new();
        let mut entry_ids = BTreeSet::new();
        let mut logical_bytes = 0_u64;
        for (index, (path, entry)) in base.iter().enumerate() {
            validate_path(path)?;
            entry_ids.insert(entry.id.clone());
            if let Some(group) = &entry.link_group
                && let Some(inode) = groups.get(group)
            {
                // validate_manifest already verified group type and equality.
                nodes
                    .get_mut(inode)
                    .expect("existing link group")
                    .links
                    .insert(path.clone(), entry.id.clone());
                paths.insert(path.clone(), *inode);
                continue;
            }
            let inode = index as u64 + ROOT_INODE;
            if let Some(group) = &entry.link_group {
                groups.insert(group.clone(), inode);
            }
            let parent = if path.is_empty() {
                ROOT_INODE
            } else {
                *paths
                    .get(parent_path(path))
                    .ok_or_else(|| invalid("missing base parent"))?
            };
            let file = if let EntryKind::File { size, .. } = entry.kind {
                if size > MAX_FILE_BYTES {
                    return Err(invalid("base file exceeds signed offset range"));
                }
                logical_bytes = logical_bytes.checked_add(size).ok_or_else(no_space)?;
                Some(FileData {
                    base_size: size,
                    base_visible: size,
                    spans: None,
                    extents: vec![],
                })
            } else {
                None
            };
            nodes.insert(
                inode,
                Node {
                    entry: entry.clone(),
                    accessed_ns: entry.modified_ns,
                    parent: u32::try_from(parent)
                        .map_err(|_| invalid("base parent exceeds inode quota"))?,
                    child_directories: 0,
                    links: BTreeMap::from([(path.clone(), entry.id.clone())]),
                    file,
                },
            );
            if inode != ROOT_INODE && matches!(entry.kind, EntryKind::Directory) {
                nodes
                    .get_mut(&parent)
                    .expect("base parent")
                    .child_directories += 1;
            }
            paths.insert(path.clone(), inode);
        }
        Ok(Self {
            _lease: lease,
            journal,
            reader: packed::Reader::open(objects).map_err(content_error)?,
            binding,
            nodes,
            paths,
            entry_ids,
            handles: BTreeMap::new(),
            next_inode: base.len() as u64 + 1,
            next_handle: 1,
            next_sequence: 0,
            journal_bytes: 0,
            logical_bytes,
            logical_limit: logical_bytes
                .checked_add(MAX_LOGICAL_GROWTH)
                .ok_or_else(no_space)?,
            dirty_paths: BTreeSet::new(),
            content_dirty: BTreeSet::new(),
            poisoned: false,
            incomplete_tail: false,
            abandoned_session: false,
        })
    }

    fn can_mutate(&self) -> io::Result<()> {
        if self.incomplete_tail || self.abandoned_session {
            return Err(read_only());
        }
        if self.poisoned {
            return Err(io::Error::other(
                "journal append/sync failed; reopen generation before mutation",
            ));
        }
        Ok(())
    }

    fn attr(&self, inode: u64) -> io::Result<NodeAttr> {
        let node = self.nodes.get(&inode).ok_or_else(not_found)?;
        let nlink = if node.links.is_empty() {
            0
        } else if matches!(node.entry.kind, EntryKind::Directory) {
            2 + node.child_directories
        } else {
            node.links.len() as u32
        };
        Ok(NodeAttr {
            inode,
            entry: node.entry.clone(),
            accessed_ns: node.accessed_ns,
            nlink,
        })
    }

    fn attr_path(&self, path: &str) -> io::Result<NodeAttr> {
        let inode = *self.paths.get(path).ok_or_else(not_found)?;
        let mut attr = self.attr(inode)?;
        attr.entry.id = self.nodes[&inode].links[path].clone();
        Ok(attr)
    }

    fn directory(&self, inode: u64, linked: bool) -> io::Result<&Node> {
        let node = self.nodes.get(&inode).ok_or_else(not_found)?;
        if !matches!(node.entry.kind, EntryKind::Directory) {
            return Err(not_directory());
        }
        if linked && node.links.is_empty() {
            return Err(not_found());
        }
        Ok(node)
    }

    fn child_path(&self, parent: u64, name: &str) -> io::Result<String> {
        validate_name(name)?;
        let dir = self.directory(parent, true)?;
        let parent_path = dir.links.keys().next().expect("linked directory");
        let path = if parent_path.is_empty() {
            name.to_owned()
        } else {
            format!("{parent_path}/{name}")
        };
        validate_path(&path)?;
        Ok(path)
    }

    fn file_size(&self, inode: u64) -> io::Result<u64> {
        match self.nodes.get(&inode).ok_or_else(not_found)?.entry.kind {
            EntryKind::File { size, .. } => Ok(size),
            EntryKind::Directory => Err(is_directory()),
            _ => Err(invalid_input()),
        }
    }

    fn growth(&self, old: u64, new: u64) -> io::Result<()> {
        if new > MAX_FILE_BYTES
            || self
                .logical_bytes
                .checked_sub(old)
                .and_then(|n| n.checked_add(new))
                .is_none_or(|n| n > self.logical_limit)
        {
            return Err(no_space());
        }
        Ok(())
    }

    fn validate_operation(&self, op: &Operation, payload: &[u8]) -> io::Result<()> {
        if !matches!(op, Operation::Write { .. }) && !payload.is_empty() {
            return Err(invalid("unexpected journal payload"));
        }
        match op {
            Operation::Init => {
                if self.next_sequence != 0 {
                    return Err(invalid("duplicate journal initialization"));
                }
            }
            Operation::Create {
                parent,
                name,
                inode,
                entry,
            } => {
                let path = self.child_path(*parent, name)?;
                if self.paths.contains_key(&path) {
                    return Err(already_exists());
                }
                if *inode != self.next_inode || self.entry_ids.len() as u64 >= MAX_NODES {
                    return Err(no_space());
                }
                if entry.id != new_id(&self.binding, *inode)
                    || self.entry_ids.contains(&entry.id)
                    || self
                        .nodes
                        .values()
                        .any(|node| node.entry.link_group.as_ref() == Some(&entry.id))
                    || entry.link_group.is_some()
                    || entry.mode & !0o777 != 0
                {
                    return Err(invalid("invalid new node identity or permissions"));
                }
                validate_time(entry.modified_ns)?;
                match &entry.kind {
                    EntryKind::File { size: 0, chunks } if chunks.is_empty() => {}
                    EntryKind::Directory => {}
                    EntryKind::Symlink { target }
                        if !target.is_empty()
                            && target.len() <= MAX_PATH_BYTES
                            && !target.contains(&0) => {}
                    _ => return Err(invalid("unsupported new node type or invalid symlink")),
                }
            }
            Operation::Link {
                inode,
                parent,
                name,
                entry_id,
                now_ns,
            } => {
                let node = self.nodes.get(inode).ok_or_else(not_found)?;
                if node.links.is_empty() {
                    return Err(not_found());
                }
                match node.entry.kind {
                    EntryKind::File { .. } => {}
                    EntryKind::Directory => return Err(is_directory()),
                    EntryKind::Symlink { .. } => return Err(unsupported_link()),
                }
                let path = self.child_path(*parent, name)?;
                if self.paths.contains_key(&path) {
                    return Err(already_exists());
                }
                if self.entry_ids.len() as u64 >= MAX_NODES {
                    return Err(no_space());
                }
                if *entry_id != new_link_id(&self.binding, self.next_sequence)
                    || self.entry_ids.contains(entry_id)
                {
                    return Err(invalid("invalid hardlink entry identity"));
                }
                if node.entry.link_group.is_none() {
                    let group = &node.entry.id;
                    if self
                        .nodes
                        .values()
                        .any(|n| n.entry.link_group.as_ref() == Some(group))
                    {
                        return Err(invalid("hardlink group identity collision"));
                    }
                }
                validate_time(*now_ns)?;
            }
            Operation::Write {
                inode,
                offset,
                modified_ns,
            } => {
                let old = self.file_size(*inode)?;
                if payload.is_empty() || payload.len() > MAX_WRITE_BYTES {
                    return Err(invalid_input());
                }
                let end = offset
                    .checked_add(payload.len() as u64)
                    .ok_or_else(no_space)?;
                self.growth(old, old.max(end))?;
                validate_time(*modified_ns)?;
            }
            Operation::Setattr {
                inode,
                attrs,
                now_ns,
            } => {
                if !self.nodes.contains_key(inode) {
                    return Err(not_found());
                }
                if attrs.mode.is_some_and(|mode| mode & !0o777 != 0) {
                    return Err(invalid_input());
                }
                if let Some(size) = attrs.size {
                    self.growth(self.file_size(*inode)?, size)?;
                }
                validate_time(*now_ns)?;
                if let Some(time) = attrs.modified_ns {
                    validate_time(time)?;
                }
                if let Some(time) = attrs.accessed_ns {
                    validate_time(time)?;
                }
            }
            Operation::Remove {
                parent,
                name,
                directory,
                now_ns,
            } => {
                let path = self.child_path(*parent, name)?;
                let inode = self.paths.get(&path).ok_or_else(not_found)?;
                let is_dir = matches!(self.nodes[inode].entry.kind, EntryKind::Directory);
                if *directory && !is_dir {
                    return Err(not_directory());
                }
                if !directory && is_dir {
                    return Err(is_directory());
                }
                if is_dir && self.has_children(*inode) {
                    return Err(not_empty());
                }
                validate_time(*now_ns)?;
            }
            Operation::Rename {
                parent,
                name,
                new_parent,
                new_name,
                now_ns,
            } => {
                let old = self.child_path(*parent, name)?;
                let new = self.child_path(*new_parent, new_name)?;
                let inode = *self.paths.get(&old).ok_or_else(not_found)?;
                let source_is_dir = matches!(self.nodes[&inode].entry.kind, EntryKind::Directory);
                if old == new || self.paths.get(&new) == Some(&inode) {
                    return Ok(());
                }
                if source_is_dir && new.starts_with(&format!("{old}/")) {
                    return Err(invalid_input());
                }
                if let Some(victim) = self.paths.get(&new) {
                    let victim_is_dir =
                        matches!(self.nodes[victim].entry.kind, EntryKind::Directory);
                    if source_is_dir && !victim_is_dir {
                        return Err(not_directory());
                    }
                    if !source_is_dir && victim_is_dir {
                        return Err(is_directory());
                    }
                    if victim_is_dir && self.has_children(*victim) {
                        return Err(not_empty());
                    }
                }
                // The exact source and its slash-prefixed descendants are
                // separate ranges; similarly named siblings are not moved.
                let prefix = format!("{old}/");
                for path in std::iter::once(&old).chain(
                    self.paths
                        .range(prefix.clone()..)
                        .map(|(path, _)| path)
                        .take_while(|path| path.starts_with(&prefix)),
                ) {
                    validate_path(&format!("{new}{}", &path[old.len()..]))?;
                }
                validate_time(*now_ns)?;
            }
        }
        Ok(())
    }

    fn has_children(&self, inode: u64) -> bool {
        self.nodes[&inode]
            .links
            .keys()
            .next()
            .is_some_and(|directory| {
                self.paths
                    .keys()
                    .any(|path| !path.is_empty() && parent_path(path) == directory)
            })
    }

    fn mutate(&mut self, op: Operation, payload: &[u8]) -> io::Result<()> {
        self.can_mutate()?;
        self.validate_operation(&op, payload)?;
        self.append(op, payload)
    }

    fn append(&mut self, op: Operation, payload: &[u8]) -> io::Result<()> {
        let record = Record {
            binding: self.binding.clone(),
            operation: op,
        };
        let metadata = serde_json::to_vec(&record).map_err(invalid_data)?;
        if metadata.len() > MAX_METADATA_BYTES || payload.len() > MAX_WRITE_BYTES {
            return Err(no_space());
        }
        let length = HEADER_BYTES as u64 + metadata.len() as u64 + payload.len() as u64;
        let end = self
            .journal_bytes
            .checked_add(length)
            .filter(|n| *n <= MAX_JOURNAL_BYTES)
            .ok_or_else(no_space)?;
        let mut header = [0_u8; HEADER_BYTES];
        header[..8].copy_from_slice(MAGIC);
        header[8..12].copy_from_slice(&VERSION.to_le_bytes());
        header[12..16].copy_from_slice(&(metadata.len() as u32).to_le_bytes());
        header[16..20].copy_from_slice(&(payload.len() as u32).to_le_bytes());
        header[20..28].copy_from_slice(&self.next_sequence.to_le_bytes());
        // Authenticate lengths separately, before recovery decides whether a
        // frame is incomplete. Corrupt interior lengths must not hide later
        // complete frames by masquerading as a truncated final payload.
        let header_hash = blake3::hash(&header[..28]);
        header[28..60].copy_from_slice(header_hash.as_bytes());
        let mut hash = blake3::Hasher::new();
        hash.update(&header[..60]);
        hash.update(&metadata);
        hash.update(payload);
        header[60..].copy_from_slice(hash.finalize().as_bytes());
        let result = (|| {
            if platform::file_metadata(&self.journal)?.len() != self.journal_bytes {
                return Err(invalid(
                    "journal length changed outside the generation writer",
                ));
            }
            self.journal.seek(SeekFrom::Start(self.journal_bytes))?;
            self.journal.write_all(&header)?;
            self.journal.write_all(&metadata)?;
            self.journal.write_all(payload)
        })();
        if let Err(error) = result {
            self.poisoned = true;
            return Err(error);
        }
        let payload_offset = self.journal_bytes + HEADER_BYTES as u64 + metadata.len() as u64;
        self.apply(record.operation, payload_offset, payload);
        self.journal_bytes = end;
        self.next_sequence += 1;
        Ok(())
    }

    fn replay(&mut self) -> io::Result<()> {
        let length = platform::file_metadata(&self.journal)?.len();
        if length > MAX_JOURNAL_BYTES {
            return Err(invalid("journal exceeds generation quota"));
        }
        while self.journal_bytes < length {
            self.journal.seek(SeekFrom::Start(self.journal_bytes))?;
            let remaining = length - self.journal_bytes;
            let mut header = [0_u8; HEADER_BYTES];
            let count = remaining.min(HEADER_BYTES as u64) as usize;
            self.journal.read_exact(&mut header[..count])?;
            if header[..count.min(8)] != MAGIC[..count.min(8)] {
                return Err(invalid("invalid journal frame magic"));
            }
            if count >= 12 && u32::from_le_bytes(header[8..12].try_into().unwrap()) != VERSION {
                return Err(invalid("unsupported journal frame version"));
            }
            if count >= 60 && blake3::hash(&header[..28]).as_bytes() != &header[28..60] {
                return Err(invalid("journal header checksum mismatch"));
            }
            if count < HEADER_BYTES {
                self.incomplete_tail = true;
                break;
            }
            let metadata_length = u32::from_le_bytes(header[12..16].try_into().unwrap()) as usize;
            let payload_length = u32::from_le_bytes(header[16..20].try_into().unwrap()) as usize;
            let sequence = u64::from_le_bytes(header[20..28].try_into().unwrap());
            if metadata_length == 0
                || metadata_length > MAX_METADATA_BYTES
                || payload_length > MAX_WRITE_BYTES
                || sequence != self.next_sequence
            {
                return Err(invalid("invalid journal bounds or sequence"));
            }
            let frame_length = HEADER_BYTES as u64 + metadata_length as u64 + payload_length as u64;
            if frame_length > remaining {
                self.incomplete_tail = true;
                break;
            }
            let mut metadata = vec![0; metadata_length];
            let mut payload = vec![0; payload_length];
            self.journal.read_exact(&mut metadata)?;
            self.journal.read_exact(&mut payload)?;
            let mut hash = blake3::Hasher::new();
            hash.update(&header[..60]);
            hash.update(&metadata);
            hash.update(&payload);
            if hash.finalize().as_bytes() != &header[60..] {
                return Err(invalid("journal frame checksum mismatch"));
            }
            let record: Record = serde_json::from_slice(&metadata).map_err(invalid_data)?;
            if record.binding != self.binding {
                return Err(invalid("journal belongs to a different base or generation"));
            }
            if self.next_sequence == 0 && !matches!(record.operation, Operation::Init) {
                return Err(invalid("journal is missing its initialization frame"));
            }
            self.validate_operation(&record.operation, &payload)?;
            self.apply(
                record.operation,
                self.journal_bytes + HEADER_BYTES as u64 + metadata_length as u64,
                &payload,
            );
            self.journal_bytes += frame_length;
            self.next_sequence += 1;
        }
        if self.next_sequence == 0 {
            return Err(invalid("journal has no complete initialization frame"));
        }
        Ok(())
    }

    /// Called only after semantic validation and a complete checksummed frame.
    fn apply(&mut self, operation: Operation, payload_offset: u64, payload: &[u8]) {
        match operation {
            Operation::Init => {}
            Operation::Create {
                parent,
                name,
                inode,
                entry,
            } => {
                let path = self
                    .child_path(parent, &name)
                    .expect("validated child path");
                let file = matches!(entry.kind, EntryKind::File { .. }).then(|| FileData {
                    base_size: 0,
                    base_visible: 0,
                    spans: Some(vec![]),
                    extents: vec![],
                });
                let now = entry.modified_ns;
                let directory = matches!(entry.kind, EntryKind::Directory);
                if file.is_some() {
                    self.content_dirty.insert(inode);
                }
                self.entry_ids.insert(entry.id.clone());
                self.nodes.insert(
                    inode,
                    Node {
                        accessed_ns: now,
                        links: BTreeMap::from([(path.clone(), entry.id.clone())]),
                        entry,
                        parent: u32::try_from(parent).expect("validated parent within inode quota"),
                        child_directories: 0,
                        file,
                    },
                );
                if directory {
                    self.nodes.get_mut(&parent).unwrap().child_directories += 1;
                }
                self.paths.insert(path.clone(), inode);
                self.next_inode += 1;
                self.mark_path(&path);
                self.touch_parent(parent, now);
            }
            Operation::Link {
                inode,
                parent,
                name,
                entry_id,
                now_ns,
            } => {
                let path = self
                    .child_path(parent, &name)
                    .expect("validated child path");
                let node = self.nodes.get_mut(&inode).expect("validated regular inode");
                if node.entry.link_group.is_none() {
                    node.entry.link_group = Some(node.entry.id.clone());
                }
                node.links.insert(path.clone(), entry_id.clone());
                self.entry_ids.insert(entry_id);
                self.paths.insert(path, inode);
                // The first link adds group identity to every existing alias.
                self.mark_inode(inode);
                self.touch_parent(parent, now_ns);
            }
            Operation::Write {
                inode,
                offset,
                modified_ns,
            } => {
                let old = self.file_size(inode).expect("validated file");
                let end = offset + payload.len() as u64;
                let node = self.nodes.get_mut(&inode).expect("validated node");
                if let EntryKind::File { size, .. } = &mut node.entry.kind {
                    *size = old.max(end);
                }
                node.entry.modified_ns = modified_ns;
                node.file
                    .as_mut()
                    .expect("regular file")
                    .extents
                    .push(Extent {
                        start: offset,
                        end,
                        payload_offset,
                        payload_length: payload.len(),
                        digest: *blake3::hash(payload).as_bytes(),
                    });
                self.logical_bytes = self.logical_bytes - old + old.max(end);
                self.content_dirty.insert(inode);
                self.mark_inode(inode);
            }
            Operation::Setattr {
                inode,
                attrs,
                now_ns,
            } => {
                let old = attrs
                    .size
                    .map(|_| self.file_size(inode).expect("validated file"));
                let node = self.nodes.get_mut(&inode).expect("validated node");
                if let Some(mode) = attrs.mode {
                    node.entry.mode = mode;
                }
                if let Some(new_size) = attrs.size {
                    if let EntryKind::File { size, .. } = &mut node.entry.kind {
                        *size = new_size;
                    }
                    let file = node.file.as_mut().expect("regular file");
                    file.base_visible = file.base_visible.min(new_size);
                    for extent in &mut file.extents {
                        extent.end = extent.end.min(new_size);
                    }
                    file.extents.retain(|e| e.start < e.end);
                    node.entry.modified_ns = now_ns;
                    self.logical_bytes = self.logical_bytes - old.unwrap() + new_size;
                    self.content_dirty.insert(inode);
                }
                if let Some(time) = attrs.modified_ns {
                    node.entry.modified_ns = time;
                }
                if let Some(time) = attrs.accessed_ns {
                    node.accessed_ns = time;
                }
                self.mark_inode(inode);
            }
            Operation::Remove {
                parent,
                name,
                now_ns,
                ..
            } => {
                let path = self.child_path(parent, &name).expect("validated child");
                let inode = self.paths.remove(&path).expect("validated target");
                self.remove_link(inode, &path);
                self.mark_path(&path);
                self.touch_parent(parent, now_ns);
            }
            Operation::Rename {
                parent,
                name,
                new_parent,
                new_name,
                now_ns,
            } => {
                let old = self.child_path(parent, &name).expect("validated source");
                let new = self
                    .child_path(new_parent, &new_name)
                    .expect("validated target");
                let source = self.paths[&old];
                // POSIX rename of two names for the same inode leaves both names.
                if old == new || self.paths.get(&new) == Some(&source) {
                    return;
                }
                if let Some(victim) = self.paths.remove(&new) {
                    self.remove_link(victim, &new);
                }
                let prefix = format!("{old}/");
                let moved: Vec<_> = std::iter::once((old.clone(), source))
                    .chain(
                        self.paths
                            .range(prefix.clone()..)
                            .take_while(|(path, _)| path.starts_with(&prefix))
                            .map(|(path, inode)| (path.clone(), *inode)),
                    )
                    .collect();
                for (path, inode) in moved {
                    let renamed = format!("{new}{}", &path[old.len()..]);
                    self.paths.remove(&path);
                    self.paths.insert(renamed.clone(), inode);
                    let node = self.nodes.get_mut(&inode).unwrap();
                    let entry_id = node.links.remove(&path).expect("moved directory entry");
                    node.links.insert(renamed.clone(), entry_id);
                    self.mark_path(&path);
                    self.mark_path(&renamed);
                }
                self.move_parent(source, new_parent);
                self.touch_parent(parent, now_ns);
                self.touch_parent(new_parent, now_ns);
            }
        }
    }

    fn remove_link(&mut self, inode: u64, path: &str) {
        let node = self.nodes.get_mut(&inode).expect("validated target");
        node.links.remove(path);
        if inode != ROOT_INODE
            && node.links.is_empty()
            && matches!(node.entry.kind, EntryKind::Directory)
        {
            let parent = node.parent;
            self.nodes
                .get_mut(&u64::from(parent))
                .unwrap()
                .child_directories -= 1;
        }
    }

    fn move_parent(&mut self, inode: u64, parent: u64) {
        let parent = u32::try_from(parent).expect("validated parent within inode quota");
        let node = self.nodes.get_mut(&inode).expect("validated source");
        let old_parent = node.parent;
        node.parent = parent;
        if inode != ROOT_INODE
            && old_parent != parent
            && !node.links.is_empty()
            && matches!(node.entry.kind, EntryKind::Directory)
        {
            // A directory alias can leave its canonical path elsewhere. Match
            // the stored-parent semantics rather than infer parent from paths.
            self.nodes
                .get_mut(&u64::from(old_parent))
                .unwrap()
                .child_directories -= 1;
            self.nodes
                .get_mut(&u64::from(parent))
                .unwrap()
                .child_directories += 1;
        }
    }

    fn mark_path(&mut self, path: &str) {
        let mut current = path;
        loop {
            self.dirty_paths.insert(current.to_owned());
            if current.is_empty() {
                break;
            }
            current = parent_path(current);
        }
    }

    fn mark_inode(&mut self, inode: u64) {
        let paths: Vec<_> = self.nodes[&inode].links.keys().cloned().collect();
        for path in paths {
            self.mark_path(&path);
        }
    }

    fn touch_parent(&mut self, inode: u64, time: i128) {
        self.nodes
            .get_mut(&inode)
            .expect("parent exists")
            .entry
            .modified_ns = time;
        self.mark_inode(inode);
    }

    fn ensure_spans(&mut self, inode: u64) -> io::Result<()> {
        let node = self.nodes.get_mut(&inode).ok_or_else(not_found)?;
        let file = node
            .file
            .as_mut()
            .ok_or_else(|| invalid("not a regular file"))?;
        if file.spans.is_some() {
            return Ok(());
        }
        let EntryKind::File { chunks, .. } = &node.entry.kind else {
            unreachable!()
        };
        let mut offset = 0_u64;
        let mut spans = Vec::with_capacity(chunks.len());
        for hash in chunks {
            // Verify content, not metadata-only lengths: a corrupt object can
            // never shift the offsets used to select another requested chunk.
            let bytes = self.reader.read(hash).map_err(content_error)?;
            let end = offset
                .checked_add(bytes.len() as u64)
                .ok_or_else(|| invalid("chunk length overflow"))?;
            if bytes.is_empty() || end > file.base_size {
                return Err(invalid("base chunk lengths exceed file size"));
            }
            spans.push(Span {
                start: offset,
                end,
                hash: hash.clone(),
            });
            offset = end;
        }
        if offset != file.base_size {
            return Err(invalid("base chunk lengths do not match file size"));
        }
        file.spans = Some(spans);
        Ok(())
    }

    fn read(&mut self, inode: u64, offset: u64, length: usize) -> io::Result<Vec<u8>> {
        if length > MAX_WRITE_BYTES {
            return Err(invalid_input());
        }
        let size = self.file_size(inode)?;
        if offset >= size || length == 0 {
            return Ok(vec![]);
        }
        if offset
            < self.nodes[&inode]
                .file
                .as_ref()
                .expect("regular file")
                .base_visible
        {
            self.ensure_spans(inode)?;
        }
        let end = size.min(
            offset
                .checked_add(length as u64)
                .ok_or_else(invalid_input)?,
        );
        let mut output = vec![0_u8; (end - offset) as usize];
        let file = self.nodes[&inode].file.as_ref().expect("regular file");
        let spans = file.spans.as_deref().unwrap_or(&[]);
        let first = spans.partition_point(|span| span.end <= offset);
        for span in &spans[first..] {
            if span.start >= end.min(file.base_visible) {
                break;
            }
            let start = offset.max(span.start);
            let stop = end.min(span.end).min(file.base_visible);
            if start >= stop {
                continue;
            }
            let bytes = self.reader.read(&span.hash).map_err(content_error)?;
            if bytes.len() as u64 != span.end - span.start {
                return Err(invalid("immutable base chunk changed length"));
            }
            output[(start - offset) as usize..(stop - offset) as usize].copy_from_slice(
                &bytes[(start - span.start) as usize..(stop - span.start) as usize],
            );
        }
        for extent in &file.extents {
            let start = offset.max(extent.start);
            let stop = end.min(extent.end);
            if start >= stop {
                continue;
            }
            let mut bytes = vec![0; extent.payload_length];
            self.journal.seek(SeekFrom::Start(extent.payload_offset))?;
            self.journal.read_exact(&mut bytes)?;
            if blake3::hash(&bytes).as_bytes() != &extent.digest {
                return Err(invalid("journal write extent checksum mismatch"));
            }
            output[(start - offset) as usize..(stop - offset) as usize].copy_from_slice(
                &bytes[(start - extent.start) as usize..(stop - extent.start) as usize],
            );
        }
        Ok(output)
    }
}

fn acquire_lease(directory: &Path, create: bool) -> io::Result<File> {
    let path = directory.join(LEASE_NAME);
    if create {
        match platform::create_new(&path, 0o600) {
            Ok(file) => file.sync_all()?,
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
    }
    let lease = platform::open_lease(&path)?;
    lease.try_lock().map_err(|error| {
        io::Error::new(
            io::ErrorKind::WouldBlock,
            format!("writable generation busy: {error}"),
        )
    })?;
    Ok(lease)
}

fn validate_directory_chain(path: &Path) -> io::Result<()> {
    if path.as_os_str().is_empty() {
        return Err(invalid("empty generation path"));
    }
    let mut current = PathBuf::new();
    for component in path.components() {
        if matches!(component, Component::ParentDir) {
            return Err(invalid("generation path cannot contain '..'"));
        }
        current.push(component);
        if matches!(component, Component::Prefix(_)) {
            continue;
        }
        let metadata = platform::symlink_metadata(&current)?;
        if !metadata.is_dir() || metadata.file_type().is_symlink() {
            return Err(invalid("generation ancestor is a symlink or non-directory"));
        }
    }
    Ok(())
}

fn validate_name(name: &str) -> io::Result<()> {
    if name.len() > MAX_NAME_BYTES {
        return Err(name_too_long());
    }
    if name.is_empty() || name.contains('/') || name.contains('\0') || matches!(name, "." | "..") {
        return Err(invalid_input());
    }
    platform::validate_relative_path(name)
}

fn validate_path(path: &str) -> io::Result<()> {
    if path.len() > MAX_PATH_BYTES {
        return Err(name_too_long());
    }
    if path.is_empty() {
        return Ok(());
    }
    for name in path.split('/') {
        validate_name(name)?;
    }
    platform::validate_relative_path(path)
}

fn validate_time(ns: i128) -> io::Result<()> {
    let magnitude = ns.unsigned_abs();
    let seconds =
        u64::try_from(magnitude / 1_000_000_000).map_err(|_| invalid("time out of range"))?;
    let duration = Duration::new(seconds, (magnitude % 1_000_000_000) as u32);
    if (if ns < 0 {
        UNIX_EPOCH.checked_sub(duration)
    } else {
        UNIX_EPOCH.checked_add(duration)
    })
    .is_none()
    {
        return Err(invalid("time out of range"));
    }
    Ok(())
}

fn now_ns() -> i128 {
    match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(d) => d.as_nanos() as i128,
        Err(e) => -(e.duration().as_nanos() as i128),
    }
}
fn parent_path(path: &str) -> &str {
    path.rsplit_once('/').map_or("", |(p, _)| p)
}
#[cfg_attr(
    not(all(target_os = "linux", feature = "projection")),
    allow(dead_code)
)]
fn leaf(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or("")
}
fn new_id(binding: &str, inode: u64) -> String {
    let mut hash = blake3::Hasher::new();
    hash.update(binding.as_bytes());
    hash.update(&inode.to_le_bytes());
    hash.finalize().to_hex().to_string()
}
fn new_link_id(binding: &str, sequence: u64) -> String {
    let mut hash = blake3::Hasher::new();
    hash.update(b"sprig-hardlink-entry\0");
    hash.update(binding.as_bytes());
    hash.update(&sequence.to_le_bytes());
    hash.finalize().to_hex().to_string()
}

fn content_error(error: content::Error) -> io::Error {
    match error.downcast::<io::Error>() {
        Ok(error) => *error,
        Err(error) => invalid(error.to_string()),
    }
}
fn invalid_data(error: impl std::fmt::Display) -> io::Error {
    invalid(error.to_string())
}
fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}
fn not_found() -> io::Error {
    io::Error::new(io::ErrorKind::NotFound, "namespace entry not found")
}
fn already_exists() -> io::Error {
    io::Error::new(
        io::ErrorKind::AlreadyExists,
        "namespace entry already exists",
    )
}
macro_rules! namespace_error {
    ($function:ident, $code:ident, $kind:ident, $message:literal) => {
        fn $function() -> io::Error {
            #[cfg(unix)]
            {
                io::Error::from_raw_os_error(libc::$code)
            }
            #[cfg(not(unix))]
            {
                io::Error::new(io::ErrorKind::$kind, $message)
            }
        }
    };
}
namespace_error!(
    invalid_input,
    EINVAL,
    InvalidInput,
    "invalid writable namespace operation"
);
namespace_error!(
    name_too_long,
    ENAMETOOLONG,
    InvalidInput,
    "namespace name or path exceeds its limit"
);
namespace_error!(
    no_space,
    ENOSPC,
    StorageFull,
    "writable generation quota exhausted"
);
namespace_error!(
    bad_handle,
    EBADF,
    InvalidInput,
    "invalid or non-writable handle"
);
namespace_error!(
    read_only,
    EROFS,
    ReadOnlyFilesystem,
    "recovered incomplete journal is read-only"
);
namespace_error!(
    unsupported_link,
    EOPNOTSUPP,
    Unsupported,
    "only regular-file hardlinks are supported"
);
namespace_error!(not_directory, ENOTDIR, NotADirectory, "not a directory");
namespace_error!(is_directory, EISDIR, IsADirectory, "is a directory");
namespace_error!(
    not_empty,
    ENOTEMPTY,
    DirectoryNotEmpty,
    "directory not empty"
);

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::{self, OpenOptions};

    // Keep the original arena-scan definition independent of the derived field.
    fn original_directory_child_count(inner: &Inner, inode: u64) -> u32 {
        inner
            .nodes
            .iter()
            .filter(|(i, n)| {
                **i != ROOT_INODE
                    && u64::from(n.parent) == inode
                    && !n.links.is_empty()
                    && matches!(n.entry.kind, EntryKind::Directory)
            })
            .count() as u32
    }

    fn assert_directory_counts(engine: &WritableCas) {
        let inner = engine.lock().unwrap();
        for (inode, node) in &inner.nodes {
            let children = original_directory_child_count(&inner, *inode);
            assert_eq!(node.child_directories, children, "child count at {inode}");
            let expected = if node.links.is_empty() {
                0
            } else if matches!(node.entry.kind, EntryKind::Directory) {
                2 + children
            } else {
                node.links.len() as u32
            };
            assert_eq!(inner.attr(*inode).unwrap().nlink, expected, "inode {inode}");
            for path in node.links.keys() {
                assert_eq!(inner.attr_path(path).unwrap().nlink, expected, "{path}");
            }
        }
    }

    fn directory_count_fixture(paths: &[&str]) -> (PathBuf, Manifest, WritableCas) {
        let root = std::env::temp_dir().join(format!(
            "sprig-directory-counts-{}-{}",
            std::process::id(),
            now_ns()
        ));
        fs::create_dir(&root).unwrap();
        let root = fs::canonicalize(root).unwrap();
        let objects = root.join("objects");
        let generation = root.join("generation");
        fs::create_dir(&objects).unwrap();
        fs::create_dir(&generation).unwrap();
        let mut base = Manifest::new();
        for path in paths {
            base.insert(
                (*path).into(),
                Entry {
                    id: format!("dir:{path}"),
                    link_group: None,
                    mode: 0o755,
                    modified_ns: 1,
                    kind: EntryKind::Directory,
                },
            );
        }
        for path in ["base-file", "base-file-alias"] {
            base.insert(
                path.into(),
                Entry {
                    id: path.into(),
                    link_group: Some("base-group".into()),
                    mode: 0o644,
                    modified_ns: 1,
                    kind: EntryKind::File {
                        size: 0,
                        chunks: vec![],
                    },
                },
            );
        }
        let engine = WritableCas::create(&objects, &generation, &base, "r", "g").unwrap();
        assert_directory_counts(&engine);
        (root, base, engine)
    }

    #[test]
    fn writable_narrow_parents_reject_large_public_identifiers_without_mutation() {
        let (root, base, engine) = directory_count_fixture(&["", "left", "left/child", "right"]);
        let left = engine.lookup_path("left").unwrap().inode;
        let right = engine.lookup_path("right").unwrap().inode;
        let child = engine.lookup_path("left/child").unwrap().inode;
        let file = engine.lookup_path("base-file").unwrap().inode;
        let snapshot = engine.snapshot().unwrap();
        let journal_path = root.join("generation").join(JOURNAL_NAME);
        let journal = fs::read(&journal_path).unwrap();
        let next_inode = engine.lock().unwrap().next_inode;
        macro_rules! missing {
            ($operation:expr) => {
                assert_eq!(
                    $operation.expect_err("large identifier rejected").kind(),
                    io::ErrorKind::NotFound
                )
            };
        }
        // These low bits name existing nodes, so a truncating conversion would
        // turn malformed requests into real mutations rather than just miss.
        for parent in [(1_u64 << 32) + ROOT_INODE, u64::MAX] {
            missing!(engine.mkdir(parent, "must-not-appear", 0o755));
            missing!(engine.rename(parent, "left", ROOT_INODE, "moved"));
            missing!(engine.rename(ROOT_INODE, "left", parent, "moved"));
            missing!(engine.lookup(parent, ".."));
        }
        for inode in [(1_u64 << 32) + file, u64::MAX] {
            missing!(engine.getattr(inode));
            missing!(engine.open_handle(inode, false));
            missing!(engine.link(inode, ROOT_INODE, "must-not-link"));
        }
        assert_eq!(engine.lock().unwrap().next_inode, next_inode);
        assert_eq!(fs::read(&journal_path).unwrap(), journal);
        assert_eq!(engine.snapshot().unwrap(), snapshot);
        assert_eq!(engine.lookup(child, "..").unwrap().inode, left);
        assert_directory_counts(&engine);

        // A subsequent valid parent change still widens back to the public
        // inode type and replays through the same checked constructor/update.
        let created = engine.mkdir(right, "created", 0o755).unwrap().inode;
        assert_eq!(engine.lookup(created, "..").unwrap().inode, right);
        engine.rename(left, "child", right, "moved").unwrap();
        assert_eq!(engine.lookup(child, "..").unwrap().inode, right);
        engine.sync().unwrap();
        drop(engine);
        let engine = WritableCas::open(
            &root.join("objects"),
            &root.join("generation"),
            &base,
            "r",
            "g",
        )
        .unwrap();
        assert_eq!(engine.lookup(child, "..").unwrap().inode, right);
        assert_eq!(engine.lookup(created, "..").unwrap().inode, right);
        assert_directory_counts(&engine);
        drop(engine);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn writable_narrow_parents_reject_large_checksummed_replay_identifiers() {
        let (root, base, engine) = directory_count_fixture(&["", "left", "right"]);
        let right = engine.lookup_path("right").unwrap().inode;
        let snapshot = engine.snapshot().unwrap();
        let (binding, sequence, next_inode) = {
            let inner = engine.lock().unwrap();
            (inner.binding.clone(), inner.next_sequence, inner.next_inode)
        };
        let journal_path = root.join("generation").join(JOURNAL_NAME);
        let journal = fs::read(&journal_path).unwrap();
        drop(engine);
        let create = |parent, inode| Operation::Create {
            parent,
            name: "must-not-appear".into(),
            inode,
            entry: Entry {
                id: new_id(&binding, inode),
                link_group: None,
                mode: 0o755,
                modified_ns: 1,
                kind: EntryKind::Directory,
            },
        };
        let rename = |parent, new_parent| Operation::Rename {
            parent,
            name: "left".into(),
            new_parent,
            new_name: "moved".into(),
            now_ns: 1,
        };
        for (operation, expected) in [
            (
                create((1_u64 << 32) + ROOT_INODE, next_inode),
                io::ErrorKind::NotFound,
            ),
            (create(u64::MAX, next_inode), io::ErrorKind::NotFound),
            (
                create(ROOT_INODE, (1_u64 << 32) + next_inode),
                io::ErrorKind::StorageFull,
            ),
            (create(ROOT_INODE, u64::MAX), io::ErrorKind::StorageFull),
            (
                rename((1_u64 << 32) + ROOT_INODE, right),
                io::ErrorKind::NotFound,
            ),
            (
                rename(ROOT_INODE, (1_u64 << 32) + right),
                io::ErrorKind::NotFound,
            ),
            (rename(ROOT_INODE, u64::MAX), io::ErrorKind::NotFound),
        ] {
            // Test-only construction of an authenticated frame: do not call
            // append/apply or bypass semantic validation in production code.
            let metadata = serde_json::to_vec(&Record {
                binding: binding.clone(),
                operation,
            })
            .unwrap();
            assert!(metadata.len() <= MAX_METADATA_BYTES);
            let mut header = [0_u8; HEADER_BYTES];
            header[..8].copy_from_slice(MAGIC);
            header[8..12].copy_from_slice(&VERSION.to_le_bytes());
            header[12..16].copy_from_slice(&(metadata.len() as u32).to_le_bytes());
            header[20..28].copy_from_slice(&sequence.to_le_bytes());
            let header_hash = blake3::hash(&header[..28]);
            header[28..60].copy_from_slice(header_hash.as_bytes());
            let mut hash = blake3::Hasher::new();
            hash.update(&header[..60]);
            hash.update(&metadata);
            header[60..].copy_from_slice(hash.finalize().as_bytes());
            let mut malformed = journal.clone();
            malformed.extend_from_slice(&header);
            malformed.extend_from_slice(&metadata);
            fs::write(&journal_path, &malformed).unwrap();
            let error = match WritableCas::open(
                &root.join("objects"),
                &root.join("generation"),
                &base,
                "r",
                "g",
            ) {
                Ok(_) => panic!("checksummed malformed identifier passed semantic validation"),
                Err(error) => error,
            };
            assert_eq!(error.kind(), expected);
            assert_eq!(fs::read(&journal_path).unwrap(), malformed);
        }
        fs::write(&journal_path, journal).unwrap();
        let engine = WritableCas::open(
            &root.join("objects"),
            &root.join("generation"),
            &base,
            "r",
            "g",
        )
        .unwrap();
        assert_eq!(engine.snapshot().unwrap(), snapshot);
        assert_directory_counts(&engine);
        drop(engine);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn writable_directory_counts_track_mutations_and_replay() {
        let (root, base, engine) = directory_count_fixture(&[
            "",
            "left",
            "left/base",
            "left/base/deep",
            "right",
            "right/victim",
            "sibling",
        ]);
        let left = engine.lookup_path("left").unwrap().inode;
        let right = engine.lookup_path("right").unwrap().inode;
        macro_rules! checked {
            ($operation:expr) => {{
                let result = $operation.unwrap();
                assert_directory_counts(&engine);
                result
            }};
        }
        let before = engine.stats().unwrap().journal_bytes;
        assert!(engine.rmdir(ROOT_INODE, "left").is_err());
        assert!(engine.mkdir(ROOT_INODE, "left", 0o755).is_err());
        assert!(engine.rename(ROOT_INODE, "left", left, "cycle").is_err());
        assert!(
            engine
                .rename(ROOT_INODE, "left", ROOT_INODE, "base-file")
                .is_err()
        );
        assert_eq!(engine.stats().unwrap().journal_bytes, before);
        assert_directory_counts(&engine);
        for index in 0..24 {
            let name = format!("tree-{index}");
            let tree = checked!(engine.mkdir(left, &name, 0o755)).inode;
            let child = checked!(engine.mkdir(tree, "子目录", 0o755)).inode;
            let held_tree = engine.open_handle(tree, false).unwrap();
            let file = checked!(engine.create_file(child, "file", 0o644)).inode;
            let held_file = engine.open_handle(file, true).unwrap();
            checked!(engine.link(file, right, "file-alias"));
            checked!(engine.write(held_file, 0, b"contents"));
            checked!(engine.setattr(
                file,
                SetAttr {
                    size: Some(3),
                    mode: Some(0o600),
                    ..Default::default()
                }
            ));
            checked!(engine.symlink(child, "symlink", b"file"));
            checked!(engine.rename(left, &name, right, "moved"));
            checked!(engine.rename(right, "moved", right, "renamed"));
            // Both same-path and same-inode alias renames are no-ops.
            checked!(engine.rename(right, "renamed", right, "renamed"));
            checked!(engine.rename(child, "file", right, "file-alias"));
            let victim = checked!(engine.mkdir(left, "victim", 0o755)).inode;
            let held_victim = engine.open_handle(victim, false).unwrap();
            checked!(engine.rename(right, "renamed", left, "victim"));
            assert_eq!(engine.getattr(victim).unwrap().nlink, 0);
            assert_eq!(engine.lookup(tree, "..").unwrap().inode, left);
            assert_eq!(engine.lookup(child, "..").unwrap().inode, tree);
            checked!(engine.unlink(child, "file"));
            assert_eq!(engine.read(held_file, 0, 99).unwrap(), b"con");
            checked!(engine.unlink(right, "file-alias"));
            checked!(engine.unlink(child, "symlink"));
            checked!(engine.rmdir(tree, "子目录"));
            checked!(engine.rmdir(left, "victim"));
            assert_eq!(engine.getattr(tree).unwrap().nlink, 0);
            assert_eq!(engine.getattr(child).unwrap().nlink, 0);
            assert!(engine.readdir(tree).unwrap().is_empty());
            // Reusing the path must not revive either retained directory inode.
            let replacement = checked!(engine.mkdir(left, "victim", 0o755)).inode;
            assert_ne!(replacement, victim);
            assert_ne!(replacement, tree);
            checked!(engine.rmdir(left, "victim"));
            for handle in [held_tree, held_file, held_victim] {
                engine.release(handle).unwrap();
            }
        }
        let snapshot = engine.snapshot().unwrap();
        let expected: BTreeMap<_, _> = {
            let inner = engine.lock().unwrap();
            inner
                .nodes
                .keys()
                .map(|inode| (*inode, inner.attr(*inode).unwrap().nlink))
                .collect()
        };
        engine.sync().unwrap();
        drop(engine);
        let objects = root.join("objects");
        let generation = root.join("generation");
        let engine = WritableCas::open(&objects, &generation, &base, "r", "g").unwrap();
        assert_directory_counts(&engine);
        for (inode, count) in expected {
            assert_eq!(engine.getattr(inode).unwrap().nlink, count);
        }
        assert_eq!(engine.snapshot().unwrap(), snapshot);
        let verified_bytes = engine.stats().unwrap().journal_bytes;
        engine.mkdir(ROOT_INODE, "incomplete", 0o755).unwrap();
        let length = engine.stats().unwrap().journal_bytes;
        drop(engine);
        OpenOptions::new()
            .write(true)
            .open(generation.join(JOURNAL_NAME))
            .unwrap()
            .set_len(length - 1)
            .unwrap();
        let engine = WritableCas::open(&objects, &generation, &base, "r", "g").unwrap();
        assert!(engine.recovery_info().unwrap().incomplete_tail);
        assert_eq!(
            engine.recovery_info().unwrap().verified_bytes,
            verified_bytes
        );
        assert_directory_counts(&engine);
        assert!(engine.lookup_path("incomplete").is_err());
        assert_eq!(engine.snapshot().unwrap(), snapshot);
        drop(engine);
        fs::remove_dir_all(root).unwrap();
    }

    fn add_directory_alias(engine: &WritableCas, inode: u64, path: &str) {
        // Directory aliases cannot be imported by validate_manifest or created
        // by link(). Still preserve the original internal-state semantics.
        let mut inner = engine.lock().unwrap();
        assert!(matches!(
            inner.nodes[&inode].entry.kind,
            EntryKind::Directory
        ));
        let id = format!("alias:{path}");
        assert!(inner.entry_ids.insert(id.clone()));
        assert!(inner.paths.insert(path.into(), inode).is_none());
        assert!(
            inner
                .nodes
                .get_mut(&inode)
                .unwrap()
                .links
                .insert(path.into(), id)
                .is_none()
        );
    }

    #[test]
    fn writable_directory_counts_preserve_alias_parent_and_unique_node_semantics() {
        let (root, _, engine) = directory_count_fixture(&[
            "",
            "a",
            "a/child",
            "a/child/deep",
            "b",
            "b/victim",
            "c",
            "c/victim",
            "z",
        ]);
        let a = engine.lookup_path("a").unwrap().inode;
        let b = engine.lookup_path("b").unwrap().inode;
        let c = engine.lookup_path("c").unwrap().inode;
        let child = engine.lookup_path("a/child").unwrap().inode;
        let deep = engine.lookup_path("a/child/deep").unwrap().inode;
        let victim = engine.lookup_path("b/victim").unwrap().inode;
        let held = engine.open_handle(child, false).unwrap();
        add_directory_alias(&engine, child, "b/alias");
        add_directory_alias(&engine, child, "c/alias");
        add_directory_alias(&engine, victim, "c/victim-alias");
        assert_directory_counts(&engine);
        assert_eq!(engine.getattr(a).unwrap().nlink, 3);
        assert_eq!(engine.getattr(b).unwrap().nlink, 3);
        assert_eq!(engine.getattr(c).unwrap().nlink, 3);
        macro_rules! checked {
            ($operation:expr) => {{
                let result = $operation.unwrap();
                assert_directory_counts(&engine);
                result
            }};
        }
        checked!(engine.rename(a, "child", b, "alias"));
        assert_eq!(engine.lookup(child, "..").unwrap().inode, a);
        // The source path is not canonical, and its parent is not Node.parent.
        checked!(engine.rename(b, "alias", c, "noncanonical"));
        assert_eq!(engine.lookup(child, "..").unwrap().inode, c);
        assert_eq!(engine.getattr(a).unwrap().nlink, 2);
        assert_eq!(engine.getattr(c).unwrap().nlink, 4);
        assert!(engine.lookup_path("a/child/deep").is_ok());
        checked!(engine.rename(a, "child", b, "moved"));
        assert_eq!(engine.lookup(deep, "..").unwrap().inode, child);
        assert_eq!(engine.lookup_path("b/moved/deep").unwrap().inode, deep);
        checked!(engine.rmdir(child, "deep"));
        // Overwriting one victim alias does not remove the victim contribution.
        checked!(engine.rename(b, "moved", b, "victim"));
        assert_eq!(engine.getattr(victim).unwrap().nlink, 2);
        assert_eq!(engine.getattr(b).unwrap().nlink, 4);
        // Removing the canonical link changes canonical path but not parent.
        checked!(engine.rmdir(b, "victim"));
        assert_eq!(engine.lookup(child, "..").unwrap().inode, b);
        let new_child = checked!(engine.mkdir(child, "new", 0o755)).inode;
        assert_eq!(engine.lookup_path("c/alias/new").unwrap().inode, new_child);
        checked!(engine.rmdir(child, "new"));
        checked!(engine.rmdir(c, "noncanonical"));
        checked!(engine.rmdir(c, "alias"));
        assert_eq!(engine.getattr(child).unwrap().nlink, 0);
        assert_eq!(engine.getattr(b).unwrap().nlink, 3);
        assert_eq!(engine.getattr(c).unwrap().nlink, 3);
        assert!(engine.readdir(child).unwrap().is_empty());
        // Final-link overwrite also subtracts the victim's stored parent (b),
        // not the target path's parent (c).
        checked!(engine.rename(c, "victim", c, "victim-alias"));
        assert_eq!(engine.getattr(victim).unwrap().nlink, 0);
        assert_eq!(engine.getattr(b).unwrap().nlink, 2);
        checked!(engine.rmdir(c, "victim-alias"));
        engine.release(held).unwrap();
        drop(engine);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn writable_directory_counts_preserve_root_aliases_self_parents_and_tombstones() {
        let (root, _, engine) = directory_count_fixture(&["", "a", "a/child", "b"]);
        let a = engine.lookup_path("a").unwrap().inode;
        let b = engine.lookup_path("b").unwrap().inode;
        let child = engine.lookup_path("a/child").unwrap().inode;
        add_directory_alias(&engine, ROOT_INODE, "b/root-alias");
        assert_directory_counts(&engine);
        engine.rename(b, "root-alias", a, "root-moved").unwrap();
        assert_directory_counts(&engine);
        assert_eq!(engine.getattr(a).unwrap().nlink, 3);
        assert_eq!(engine.getattr(b).unwrap().nlink, 2);
        assert_eq!(engine.lookup(ROOT_INODE, "..").unwrap().inode, a);
        // apply() is also checked for the root exclusion on link removal;
        // rmdir validation rejects this synthetic root alias as nonempty.
        engine.lock().unwrap().apply(
            Operation::Remove {
                parent: a,
                name: "root-moved".into(),
                directory: true,
                now_ns: 2,
            },
            0,
            &[],
        );
        assert_directory_counts(&engine);
        add_directory_alias(&engine, child, "b/alias");
        engine.rename(b, "alias", child, "self").unwrap();
        assert_directory_counts(&engine);
        assert_eq!(engine.lookup(child, "..").unwrap().inode, child);
        assert_eq!(engine.getattr(child).unwrap().nlink, 3);
        assert_eq!(engine.getattr(a).unwrap().nlink, 2);
        // Removing the self alias leaves the unique node live and self-parented.
        engine.rmdir(child, "self").unwrap_err();
        assert_directory_counts(&engine);
        engine.lock().unwrap().apply(
            Operation::Remove {
                parent: child,
                name: "self".into(),
                directory: true,
                now_ns: 3,
            },
            0,
            &[],
        );
        assert_directory_counts(&engine);
        assert_eq!(engine.getattr(child).unwrap().nlink, 3);
        engine.rmdir(a, "child").unwrap();
        assert_directory_counts(&engine);
        assert_eq!(engine.getattr(child).unwrap().nlink, 0);

        // An alias can expose an empty canonical path while a live child is
        // retained under a different path; a tombstone can still own children.
        let descendant = engine.mkdir(a, "descendant", 0o755).unwrap().inode;
        add_directory_alias(&engine, a, "0-empty");
        engine.rmdir(ROOT_INODE, "a").unwrap();
        assert_directory_counts(&engine);
        engine.rmdir(ROOT_INODE, "0-empty").unwrap();
        assert_directory_counts(&engine);
        assert_eq!(engine.getattr(a).unwrap().nlink, 0);
        assert_eq!(engine.lookup(descendant, "..").unwrap().inode, a);
        assert_eq!(engine.lock().unwrap().nodes[&a].child_directories, 1);
        // Even a synthetic final root-link removal must not subtract from the
        // root's stored parent, which still owns the live descendant above.
        {
            let mut inner = engine.lock().unwrap();
            assert_eq!(inner.paths.remove(""), Some(ROOT_INODE));
            inner.remove_link(ROOT_INODE, "");
        }
        assert_directory_counts(&engine);
        assert_eq!(engine.getattr(ROOT_INODE).unwrap().nlink, 0);
        assert_eq!(engine.lock().unwrap().nodes[&a].child_directories, 1);
        drop(engine);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn writable_generation_namespace_content_recovery_and_checkpoint() {
        let root = std::env::temp_dir().join(format!(
            "sprig-writable-{}-{}",
            std::process::id(),
            now_ns()
        ));
        fs::create_dir(&root).unwrap();
        let root = fs::canonicalize(root).unwrap();
        let objects = root.join("objects");
        fs::create_dir(&objects).unwrap();
        let generation = root.join("generation");
        fs::create_dir(&generation).unwrap();
        let make_entry = |id: &str, kind| Entry {
            id: id.into(),
            link_group: None,
            mode: 0o644,
            modified_ns: 123,
            kind,
        };
        let mut base = Manifest::new();
        base.insert(
            "".into(),
            Entry {
                id: "root".into(),
                link_group: None,
                mode: 0o755,
                modified_ns: 10,
                kind: EntryKind::Directory,
            },
        );
        base.insert(
            "dir".into(),
            Entry {
                id: "dir".into(),
                link_group: None,
                mode: 0o755,
                modified_ns: 11,
                kind: EntryKind::Directory,
            },
        );
        base.insert(
            "dir/file".into(),
            make_entry(
                "source",
                content::put_bytes(b"abcdefghij", &objects).unwrap(),
            ),
        );
        base.insert(
            "quiet".into(),
            make_entry("quiet", content::put_bytes(b"untouched", &objects).unwrap()),
        );
        base.insert(
            "victim".into(),
            make_entry("victim", content::put_bytes(b"victim", &objects).unwrap()),
        );
        let engine =
            WritableCas::create(&objects, &generation, &base, "revision", "generation").unwrap();
        assert!(WritableCas::open(&objects, &generation, &base, "revision", "generation").is_err());
        assert!(engine.dirty_paths().unwrap().is_empty());
        assert_eq!(engine.snapshot().unwrap(), base);
        let dir = engine.lookup(ROOT_INODE, "dir").unwrap().inode;
        let file = engine.lookup(dir, "file").unwrap().inode;
        let victim = engine.lookup(ROOT_INODE, "victim").unwrap().inode;
        let handle = engine.open_handle(file, true).unwrap();
        let victim_handle = engine.open_handle(victim, true).unwrap();
        assert_eq!(engine.read(handle, 0, 100).unwrap(), b"abcdefghij");
        engine.write(handle, 7, b"XYZ").unwrap();
        engine
            .setattr(
                file,
                SetAttr {
                    size: Some(5),
                    ..Default::default()
                },
            )
            .unwrap();
        engine
            .setattr(
                file,
                SetAttr {
                    size: Some(10),
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(engine.read(handle, 0, 100).unwrap(), b"abcde\0\0\0\0\0");
        engine.write(handle, 2, b"QQ").unwrap();
        assert_eq!(
            engine.dirty_paths().unwrap(),
            ["", "dir", "dir/file"]
                .into_iter()
                .map(String::from)
                .collect()
        );
        let before_rejected = engine.stats().unwrap().journal_bytes;
        assert!(
            engine
                .write(handle, 0, &vec![1; MAX_WRITE_BYTES + 1])
                .is_err()
        );
        let prior_metadata = engine.getattr(file).unwrap().entry;
        let prior_content = engine.read(handle, 0, 100).unwrap();
        let growth_error = engine
            .setattr(
                file,
                SetAttr {
                    size: Some(MAX_LOGICAL_GROWTH + 1000),
                    ..Default::default()
                },
            )
            .unwrap_err();
        #[cfg(unix)]
        assert_eq!(growth_error.raw_os_error(), Some(libc::ENOSPC));
        #[cfg(not(unix))]
        assert_eq!(growth_error.kind(), io::ErrorKind::StorageFull);
        // Inject only the quota accounting boundary, avoiding a 256 MiB test
        // fixture. A quota rejection must precede any write, poison, or apply.
        let journal_error = {
            let mut inner = engine.lock().unwrap();
            let bytes = inner.journal_bytes;
            inner.journal_bytes = MAX_JOURNAL_BYTES;
            let error = inner
                .mutate(
                    Operation::Write {
                        inode: file,
                        offset: 0,
                        modified_ns: now_ns(),
                    },
                    b"must-not-apply",
                )
                .unwrap_err();
            inner.journal_bytes = bytes;
            assert!(!inner.poisoned);
            error
        };
        #[cfg(unix)]
        assert_eq!(journal_error.raw_os_error(), Some(libc::ENOSPC));
        #[cfg(not(unix))]
        assert_eq!(journal_error.kind(), io::ErrorKind::StorageFull);
        assert_eq!(engine.getattr(file).unwrap().entry, prior_metadata);
        assert_eq!(engine.read(handle, 0, 100).unwrap(), prior_content);
        assert_eq!(
            fs::metadata(generation.join(JOURNAL_NAME)).unwrap().len(),
            before_rejected
        );
        assert!(engine.create_file(dir, "../escape", 0o644).is_err());
        assert!(engine.rmdir(ROOT_INODE, "dir").is_err());
        assert_eq!(engine.stats().unwrap().journal_bytes, before_rejected);
        engine.rename(dir, "file", ROOT_INODE, "victim").unwrap();
        assert_eq!(engine.lookup(ROOT_INODE, "victim").unwrap().inode, file);
        assert_eq!(engine.getattr(file).unwrap().entry.id, "source");
        assert_eq!(engine.read(victim_handle, 0, 100).unwrap(), b"victim");
        assert_eq!(engine.getattr(victim).unwrap().nlink, 0);
        engine.write(victim_handle, 0, b"V").unwrap();
        assert_eq!(engine.read(victim_handle, 0, 100).unwrap(), b"Victim");
        engine.write_append(handle, b"!").unwrap();
        engine
            .setattr(
                file,
                SetAttr {
                    mode: Some(0o640),
                    modified_ns: Some(999),
                    accessed_ns: Some(888),
                    ..Default::default()
                },
            )
            .unwrap();
        let nested = engine.mkdir(dir, "nested", 0o755).unwrap().inode;
        let created = engine.create_file(nested, "new", 0o600).unwrap();
        let created_handle = engine.open_handle(created.inode, true).unwrap();
        engine.write(created_handle, 3, b"new").unwrap();
        let link = engine.symlink(dir, "link", b"nested/new").unwrap();
        assert!(engine.create_file(link.inode, "outside", 0o644).is_err());
        engine
            .rename(ROOT_INODE, "dir", ROOT_INODE, "moved")
            .unwrap();
        assert_eq!(
            engine.lookup_path("moved/nested/new").unwrap().inode,
            created.inode
        );
        assert!(engine.lookup_path("dir/nested/new").is_err());
        assert_eq!(engine.lookup(nested, "..").unwrap().inode, dir);
        let dir_handle = engine.open_handle(dir, false).unwrap();
        assert!(engine.read(dir_handle, 0, 10).is_err());
        engine.release(dir_handle).unwrap();
        engine.sync().unwrap();
        let snapshot = engine.snapshot().unwrap();
        assert_eq!(snapshot["quiet"], base["quiet"]);
        assert_eq!(
            content::read_file(&snapshot["victim"], &objects).unwrap(),
            b"abQQe\0\0\0\0\0!"
        );
        assert_eq!(snapshot["victim"].mode, 0o640);
        assert_eq!(snapshot["victim"].modified_ns, 999);
        assert_eq!(
            content::read_file(&snapshot["moved/nested/new"], &objects).unwrap(),
            b"\0\0\0new"
        );
        assert_eq!(engine.getattr(file).unwrap().accessed_ns, 888);
        assert!(!snapshot.values().any(|e| e.id == "victim"));
        let journal_path = generation.join(JOURNAL_NAME);
        let complete_bytes = fs::read(&journal_path).unwrap();
        let dirty = engine.dirty_paths().unwrap();
        engine.release(handle).unwrap();
        engine.release(victim_handle).unwrap();
        engine.release(created_handle).unwrap();
        drop(engine);

        assert!(
            WritableCas::open(&objects, &generation, &base, "other-revision", "generation")
                .is_err()
        );
        let replayed =
            WritableCas::open(&objects, &generation, &base, "revision", "generation").unwrap();
        assert_eq!(replayed.snapshot().unwrap(), snapshot);
        assert_eq!(replayed.dirty_paths().unwrap(), dirty);
        assert!(
            replayed.read(handle, 0, 1).is_err(),
            "runtime handles must not replay"
        );
        assert_eq!(replayed.lookup_path("victim").unwrap().inode, file);
        let detached = replayed.open_handle(file, true).unwrap();
        replayed.unlink(ROOT_INODE, "victim").unwrap();
        replayed.write(detached, 0, b"D").unwrap();
        assert!(replayed.lookup_path("victim").is_err());
        assert!(
            !replayed
                .snapshot()
                .unwrap()
                .values()
                .any(|e| e.id == "source")
        );
        drop(replayed);
        let after_unlink = fs::read(&journal_path).unwrap();
        let replayed =
            WritableCas::open(&objects, &generation, &base, "revision", "generation").unwrap();
        assert!(replayed.lookup_path("victim").is_err());
        assert!(
            !replayed
                .snapshot()
                .unwrap()
                .values()
                .any(|e| e.id == "source")
        );
        drop(replayed);

        // An incomplete trailing frame is preserved, readable/checkpointable,
        // and never silently truncated or appended past.
        OpenOptions::new()
            .append(true)
            .open(&journal_path)
            .unwrap()
            .write_all(&MAGIC[..3])
            .unwrap();
        let torn_bytes = fs::read(&journal_path).unwrap();
        let recovered =
            WritableCas::open(&objects, &generation, &base, "revision", "generation").unwrap();
        let info = recovered.recovery_info().unwrap();
        assert!(info.read_only && info.incomplete_tail);
        assert_eq!(info.verified_bytes, after_unlink.len() as u64);
        assert!(
            recovered
                .create_file(ROOT_INODE, "must-not-appear", 0o644)
                .is_err()
        );
        recovered.snapshot().unwrap();
        assert_eq!(fs::read(&journal_path).unwrap(), torn_bytes);
        drop(recovered);

        // Neither interior corruption nor a complete corrupt last frame is
        // treated as a recoverable tail.
        for corrupt_offset in [HEADER_BYTES + 1, complete_bytes.len() - 1] {
            let mut corrupt = complete_bytes.clone();
            corrupt[corrupt_offset] ^= 1;
            fs::write(&journal_path, &corrupt).unwrap();
            assert!(
                WritableCas::open(&objects, &generation, &base, "revision", "generation").is_err()
            );
            assert_eq!(fs::read(&journal_path).unwrap(), corrupt);
        }
        // Corrupt an interior frame's declared length so it runs beyond EOF.
        // The separately verified header rejects it, rather than accepting a
        // false incomplete-tail prefix that silently discards later mutations.
        let second_frame =
            HEADER_BYTES + u32::from_le_bytes(complete_bytes[12..16].try_into().unwrap()) as usize;
        let mut corrupt_length = complete_bytes.clone();
        corrupt_length[second_frame + 12..second_frame + 16]
            .copy_from_slice(&(MAX_METADATA_BYTES as u32).to_le_bytes());
        fs::write(&journal_path, &corrupt_length).unwrap();
        assert!(WritableCas::open(&objects, &generation, &base, "revision", "generation").is_err());
        assert_eq!(fs::read(&journal_path).unwrap(), corrupt_length);
        fs::write(&journal_path, complete_bytes).unwrap();
        let intact =
            WritableCas::open(&objects, &generation, &base, "revision", "generation").unwrap();
        assert_eq!(intact.snapshot().unwrap(), snapshot);
        drop(intact);

        // Simulate an interrupted external append at a valid boundary. The
        // current writer detects the changed length and must poison itself,
        // rather than append past unknown bytes or publish its stale view.
        let poisoned =
            WritableCas::open(&objects, &generation, &base, "revision", "generation").unwrap();
        let mut header = [0_u8; HEADER_BYTES];
        header[..8].copy_from_slice(MAGIC);
        header[8..12].copy_from_slice(&VERSION.to_le_bytes());
        header[12..16].copy_from_slice(&100_u32.to_le_bytes());
        header[20..28].copy_from_slice(&poisoned.lock().unwrap().next_sequence.to_le_bytes());
        let header_hash = blake3::hash(&header[..28]);
        header[28..60].copy_from_slice(header_hash.as_bytes());
        let mut output = OpenOptions::new().append(true).open(&journal_path).unwrap();
        output.write_all(&header).unwrap();
        output.write_all(b"incomplete").unwrap();
        drop(output);
        let partial_body = fs::read(&journal_path).unwrap();
        assert!(
            poisoned
                .create_file(ROOT_INODE, "not-journaled", 0o600)
                .is_err()
        );
        assert!(poisoned.recovery_info().unwrap().read_only);
        assert!(
            poisoned
                .create_file(ROOT_INODE, "also-not-journaled", 0o600)
                .is_err()
        );
        assert!(poisoned.snapshot().is_err());
        assert_eq!(fs::read(&journal_path).unwrap(), partial_body);
        drop(poisoned);
        let recovered =
            WritableCas::open(&objects, &generation, &base, "revision", "generation").unwrap();
        assert!(recovered.recovery_info().unwrap().incomplete_tail);
        assert_eq!(recovered.snapshot().unwrap(), snapshot);
        assert_eq!(fs::read(&journal_path).unwrap(), partial_body);
        drop(recovered);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn writable_rename_ranges_preserve_namespace_handles_and_replay() {
        let root = std::env::temp_dir().join(format!(
            "sprig-writable-rename-ranges-{}-{}",
            std::process::id(),
            now_ns()
        ));
        fs::create_dir(&root).unwrap();
        let root = fs::canonicalize(root).unwrap();
        let objects = root.join("objects");
        let generation = root.join("generation");
        fs::create_dir(&objects).unwrap();
        fs::create_dir(&generation).unwrap();
        let mut base = Manifest::new();
        for path in [
            "",
            "old",
            "old/子目录",
            "old-extra",
            "old2",
            "destination",
            "destination/empty",
        ] {
            base.insert(
                path.into(),
                Entry {
                    id: format!("dir:{path}"),
                    link_group: None,
                    mode: 0o755,
                    modified_ns: 1,
                    kind: EntryKind::Directory,
                },
            );
        }
        let kind = content::put_bytes(b"rename contents", &objects).unwrap();
        for path in [
            "old/data",
            "old/子目录/文件",
            "old-extra/keep",
            "old2/keep",
            "old-file",
            "old-file-extra",
        ] {
            base.insert(
                path.into(),
                Entry {
                    id: path.into(),
                    link_group: None,
                    mode: 0o644,
                    modified_ns: 2,
                    kind: kind.clone(),
                },
            );
        }
        for index in 0..64 {
            let path = format!("unrelated-{index:03}");
            base.insert(
                path.clone(),
                Entry {
                    id: format!("dir:{path}"),
                    link_group: None,
                    mode: 0o755,
                    modified_ns: 1,
                    kind: EntryKind::Directory,
                },
            );
            let path = format!("{path}/keep");
            base.insert(
                path.clone(),
                Entry {
                    id: path,
                    link_group: None,
                    mode: 0o644,
                    modified_ns: 2,
                    kind: kind.clone(),
                },
            );
        }
        let engine = WritableCas::create(&objects, &generation, &base, "r", "g").unwrap();
        let original = engine.paths().unwrap();
        let source = engine.lookup_path("old").unwrap();
        let nested = engine.lookup_path("old/子目录").unwrap();
        let file = engine.lookup_path("old/子目录/文件").unwrap();
        let regular = engine.lookup_path("old-file").unwrap();
        let destination = engine.lookup_path("destination").unwrap();
        let replaced = engine.lookup_path("destination/empty").unwrap();
        let root_links = engine.getattr(ROOT_INODE).unwrap().nlink;
        let source_handle = engine.open_handle(source.inode, false).unwrap();
        let file_handle = engine.open_handle(file.inode, false).unwrap();
        let regular_handle = engine.open_handle(regular.inode, false).unwrap();
        let replaced_handle = engine.open_handle(replaced.inode, false).unwrap();

        engine
            .rename(ROOT_INODE, "old-file", ROOT_INODE, "renamed-文件")
            .unwrap();
        assert_eq!(
            engine.lookup_path("renamed-文件").unwrap().inode,
            regular.inode
        );
        engine
            .rename(ROOT_INODE, "old", destination.inode, "搬迁")
            .unwrap();
        assert_eq!(engine.getattr(ROOT_INODE).unwrap().nlink, root_links - 1);
        assert_eq!(engine.getattr(destination.inode).unwrap().nlink, 4);
        assert_eq!(
            engine.lookup(source.inode, "..").unwrap().inode,
            destination.inode
        );
        assert_eq!(
            engine.lookup(nested.inode, "..").unwrap().inode,
            source.inode
        );
        engine
            .rename(destination.inode, "搬迁", destination.inode, "empty")
            .unwrap();
        assert_eq!(engine.getattr(destination.inode).unwrap().nlink, 3);
        assert_eq!(engine.getattr(source.inode).unwrap().nlink, 3);
        assert_eq!(engine.getattr(replaced.inode).unwrap().nlink, 0);
        assert!(engine.readdir(replaced.inode).unwrap().is_empty());
        for handle in [file_handle, regular_handle] {
            assert_eq!(engine.read(handle, 0, 100).unwrap(), b"rename contents");
        }

        let expected: BTreeMap<_, _> = original
            .into_iter()
            .filter_map(|(path, node)| {
                let renamed = match path.as_str() {
                    "destination/empty" => return None,
                    "old-file" => "renamed-文件".to_owned(),
                    "old" => "destination/empty".to_owned(),
                    _ => path
                        .strip_prefix("old/")
                        .map(|suffix| format!("destination/empty/{suffix}"))
                        .unwrap_or(path),
                };
                Some((renamed, (node.inode, node.entry.id)))
            })
            .collect();
        let identities = |engine: &WritableCas| -> BTreeMap<_, _> {
            engine
                .paths()
                .unwrap()
                .into_iter()
                .map(|(path, node)| (path, (node.inode, node.entry.id)))
                .collect()
        };
        assert_eq!(identities(&engine), expected);
        let snapshot = engine.snapshot().unwrap();
        for (path, entry) in &base {
            if path.starts_with("unrelated-")
                || path.starts_with("old-extra")
                || path.starts_with("old2")
                || path == "old-file-extra"
            {
                assert_eq!(&snapshot[path], entry);
            }
        }
        let dirty = engine.dirty_paths().unwrap();
        for path in [
            "old-file",
            "renamed-文件",
            "old/子目录/文件",
            "destination/搬迁/子目录/文件",
            "destination/empty/子目录/文件",
        ] {
            assert!(dirty.contains(path));
        }
        for handle in [source_handle, file_handle, regular_handle, replaced_handle] {
            engine.release(handle).unwrap();
        }
        engine.sync().unwrap();
        drop(engine);

        let engine = WritableCas::open(&objects, &generation, &base, "r", "g").unwrap();
        assert_eq!(identities(&engine), expected);
        assert_eq!(engine.snapshot().unwrap(), snapshot);
        assert_eq!(engine.dirty_paths().unwrap(), dirty);
        assert_eq!(engine.getattr(replaced.inode).unwrap().nlink, 0);
        assert_eq!(
            engine.read_inode(file.inode, 0, 100).unwrap(),
            b"rename contents"
        );
        drop(engine);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn writable_hardlinks_share_inodes_and_replay_namespace_topology() {
        let root = std::env::temp_dir().join(format!(
            "sprig-writable-hardlinks-{}-{}",
            std::process::id(),
            now_ns()
        ));
        fs::create_dir(&root).unwrap();
        let root = fs::canonicalize(root).unwrap();
        let objects = root.join("objects");
        let generation = root.join("generation");
        fs::create_dir(&objects).unwrap();
        fs::create_dir(&generation).unwrap();
        let mut base = Manifest::new();
        for path in ["", "d", "e"] {
            base.insert(
                path.into(),
                Entry {
                    id: format!("dir:{path}"),
                    link_group: None,
                    mode: 0o755,
                    modified_ns: 1,
                    kind: EntryKind::Directory,
                },
            );
        }
        let kind = content::put_bytes(b"hello", &objects).unwrap();
        for (path, group) in [
            ("d/a", Some("shared")),
            ("e/b", Some("shared")),
            ("copy", None),
        ] {
            base.insert(
                path.into(),
                Entry {
                    id: path.into(),
                    link_group: group.map(String::from),
                    mode: 0o644,
                    modified_ns: 2,
                    kind: kind.clone(),
                },
            );
        }
        let engine = WritableCas::create(&objects, &generation, &base, "r", "g").unwrap();
        assert_eq!(engine.snapshot().unwrap(), base);
        assert_eq!(engine.stats().unwrap().logical_bytes, 10);
        let d = engine.lookup_path("d").unwrap().inode;
        let e = engine.lookup_path("e").unwrap().inode;
        let a = engine.lookup_path("d/a").unwrap();
        let b = engine.lookup_path("e/b").unwrap();
        let copy = engine.lookup_path("copy").unwrap();
        assert_eq!(a.inode, b.inode);
        assert_ne!(a.entry.id, b.entry.id);
        assert_eq!(a.nlink, 2);
        assert_ne!(a.inode, copy.inode, "equal bytes are not hardlinks");
        let handle = engine.open_handle(a.inode, true).unwrap();
        engine.write(handle, 0, b"H").unwrap();
        assert_eq!(engine.read_inode(b.inode, 0, 99).unwrap(), b"Hello");
        assert_eq!(engine.read_inode(copy.inode, 0, 99).unwrap(), b"hello");
        assert!(engine.dirty_paths().unwrap().contains("e/b"));
        engine
            .setattr(
                b.inode,
                SetAttr {
                    mode: Some(0o600),
                    size: Some(3),
                    modified_ns: Some(42),
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(engine.lookup_path("d/a").unwrap().entry.mode, 0o600);
        assert_eq!(engine.read(handle, 0, 99).unwrap(), b"Hel");
        assert_eq!(engine.stats().unwrap().logical_bytes, 8);
        let alias = engine.link(a.inode, ROOT_INODE, "alias").unwrap();
        assert_eq!(alias.inode, a.inode);
        assert_eq!(alias.nlink, 3);
        assert_ne!(alias.entry.id, a.entry.id);
        assert_eq!(engine.stats().unwrap().logical_bytes, 8);
        assert_eq!(engine.readdir(d).unwrap().len(), 1);
        assert_eq!(engine.readdir(e).unwrap().len(), 1);
        let symlink = engine.symlink(ROOT_INODE, "symbolic", b"alias").unwrap();
        let journal_before = engine.stats().unwrap().journal_bytes;
        for (inode, parent, name) in [
            (d, ROOT_INODE, "bad-directory"),
            (symlink.inode, ROOT_INODE, "bad-symlink"),
            (a.inode, ROOT_INODE, "alias"),
            (a.inode, d, "../escape"),
            (a.inode, copy.inode, "bad-parent"),
        ] {
            assert!(engine.link(inode, parent, name).is_err());
        }
        assert_eq!(engine.stats().unwrap().journal_bytes, journal_before);
        let before_noop = engine.snapshot().unwrap();
        engine.rename(d, "a", e, "b").unwrap();
        assert_eq!(engine.snapshot().unwrap(), before_noop);
        assert_eq!(engine.getattr(a.inode).unwrap().nlink, 3);
        engine.rename(ROOT_INODE, "d", ROOT_INODE, "moved").unwrap();
        assert_eq!(engine.lookup_path("moved/a").unwrap().entry.id, a.entry.id);
        assert_eq!(engine.lookup_path("alias").unwrap().inode, a.inode);
        engine.write(handle, 3, b"!").unwrap();
        assert!(engine.dirty_paths().unwrap().contains("moved/a"));
        assert_eq!(
            engine.lookup_path("e/b").unwrap().entry.modified_ns,
            engine.lookup_path("moved/a").unwrap().entry.modified_ns
        );
        // Replacing one alias of a different inode leaves its other alias alive.
        let victim_alias = engine.link(copy.inode, ROOT_INODE, "copy-alias").unwrap();
        assert_eq!(
            victim_alias.entry.link_group.as_deref(),
            Some(copy.entry.id.as_str())
        );
        let victim_handle = engine.open_handle(copy.inode, true).unwrap();
        engine
            .rename(ROOT_INODE, "alias", ROOT_INODE, "copy")
            .unwrap();
        assert_eq!(engine.getattr(copy.inode).unwrap().nlink, 1);
        assert_eq!(
            engine.lookup_path("copy-alias").unwrap().entry.id,
            victim_alias.entry.id
        );
        assert_eq!(engine.lookup_path("copy").unwrap().entry.id, alias.entry.id);
        engine.unlink(d, "a").unwrap();
        engine.unlink(e, "b").unwrap();
        assert_eq!(engine.getattr(a.inode).unwrap().nlink, 1);
        let singleton = engine.snapshot().unwrap();
        assert_eq!(singleton["copy"].link_group.as_deref(), Some("shared"));
        assert_eq!(singleton["copy"].id, alias.entry.id);
        assert_eq!(
            content::read_file(&singleton["copy"], &objects).unwrap(),
            b"Hel!"
        );
        assert!(singleton["copy-alias"].link_group.is_some());
        engine.sync().unwrap();
        let dirty = engine.dirty_paths().unwrap();
        drop(engine);
        let engine = WritableCas::open(&objects, &generation, &base, "r", "g").unwrap();
        assert_eq!(engine.snapshot().unwrap(), singleton);
        assert_eq!(engine.dirty_paths().unwrap(), dirty);
        assert_eq!(engine.getattr(a.inode).unwrap().nlink, 1);
        let handle = engine.open_handle(a.inode, true).unwrap();
        engine.unlink(ROOT_INODE, "copy").unwrap();
        assert_eq!(engine.getattr(a.inode).unwrap().nlink, 0);
        engine.write(handle, 0, b"J").unwrap();
        assert_eq!(engine.read(handle, 0, 99).unwrap(), b"Jel!");
        assert!(engine.link(a.inode, ROOT_INODE, "resurrect").is_err());
        assert!(
            !engine
                .snapshot()
                .unwrap()
                .values()
                .any(|entry| entry.link_group.as_deref() == Some("shared"))
        );
        engine.release(handle).unwrap();
        assert!(
            engine.release(victim_handle).is_err(),
            "handles never replay"
        );
        drop(engine);
        // A clean generation reconstructs aliases from group identity, preserving
        // a surviving singleton group rather than splitting the inode lineage.
        let clean = root.join("clean");
        fs::create_dir(&clean).unwrap();
        let clean = WritableCas::create(&objects, &clean, &singleton, "r2", "g2").unwrap();
        let restored = clean.lookup_path("copy").unwrap();
        assert_eq!(restored.nlink, 1);
        assert_eq!(restored.entry.link_group.as_deref(), Some("shared"));
        let added = clean
            .link(restored.inode, ROOT_INODE, "restored-alias")
            .unwrap();
        assert_eq!(added.nlink, 2);
        assert_eq!(added.entry.link_group, restored.entry.link_group);
        drop(clean);
        fs::remove_dir_all(root).unwrap();
    }
}
