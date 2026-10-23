//! Incremental capture of a validated, quiescent OverlayFS view.
//!
//! This is not a writer freeze, lease, or hostile-tree sandbox. The caller must
//! keep the store, mount topology, and trees trusted and quiescent throughout.
//! mtime/inode equality detects some races; it NEVER authorizes content reuse.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::{CString, OsStr, OsString};
use std::fs::{self, File, Metadata, OpenOptions};
use std::io;
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use crate::content::{self, Entry, EntryKind, Manifest, Observation, Observed};
use serde::Serialize;

use crate::projection::ORIGIN_XATTR;
use crate::projection::objects::{self as store, ObjectStore, Result};

const MAX_ORIGIN: usize = 64 * 1024;
static NONCE: AtomicU64 = AtomicU64::new(0);

/// Seconds measured in-process, not attribution to kernel or remote services.
/// Walk categories are disjoint; walk_total_seconds includes them all. Lower
/// proof includes its own opens, metadata, origin and chunk metadata checks.
/// Unaccounted walk time includes readlink, validation, bookkeeping,
/// recursion, and timer overhead. Setup/finalize are outside walk.
#[derive(Debug, Default, Clone, Serialize)]
pub struct CaptureTimings {
    pub setup_seconds: f64,
    pub walk_total_seconds: f64,
    pub regular_merged_open_seconds: f64,
    pub directory_open_seconds: f64,
    pub descriptor_close_seconds: f64,
    pub metadata_seconds: f64,
    pub origin_xattr_seconds: f64,
    pub native_upper_state_seconds: f64,
    pub lower_reusable_proof_seconds: f64,
    pub content_hash_put_stream_seconds: f64,
    pub directory_enumeration_seconds: f64,
    /// Native upper certificate scans during walk; final rechecks are in finalize.
    pub certificate_inventory_seconds: f64,
    /// Base-entry cloning and per-file chunk length checks (capture-local cache).
    pub certificate_reuse_seconds: f64,
    pub finalize_seconds: f64,
}

#[derive(Debug, Default, Clone, Serialize)]
pub struct CaptureStats {
    pub timings: CaptureTimings,
    pub entries: u64,
    pub directories: u64,
    pub symlinks: u64,
    pub files_hashed: u64,
    pub files_reused: u64,
    pub certificate_subtrees_reused: u64,
    pub certificate_files_reused: u64,
    pub certificate_directories_reused: u64,
    pub certificate_symlinks_reused: u64,
    pub bytes_hashed: u64,
    pub bytes_reused: u64,
    /// Files hashed because lower provenance or upper absence was uncertain.
    pub fallback_files: u64,
    pub lower_reuse_enabled: bool,
    pub fallback_reason: Option<String>,
}

impl CaptureStats {
    pub fn json(&self) -> serde_json::Value {
        serde_json::to_value(self).expect("numeric capture statistics serialize")
    }
}

pub struct CaptureResult {
    pub manifest: Manifest,
    pub stats: CaptureStats,
    /// Merged-view identities for production rename and hardlink reconciliation.
    pub observed: Observed,
}

impl CaptureResult {
    /// Same-path identities win, followed by unambiguous merged device/inode
    /// matches, then advisory lower-origin IDs. Hardlinks never duplicate IDs.
    /// The previous observations must belong to this same mounted view; a
    /// remount requires observations captured in the replacement view instead.
    pub fn reconcile_identities(
        &mut self,
        previous: &Manifest,
        previous_observed: &Observed,
    ) -> Result<()> {
        content::validate_manifest(previous)?;
        content::validate_manifest(&self.manifest)?;
        let candidates: BTreeMap<_, _> = self
            .manifest
            .iter()
            .map(|(path, entry)| (path.clone(), entry.id.clone()))
            .collect();
        let mut old_inodes: BTreeMap<(u64, u64), Vec<&Observation>> = BTreeMap::new();
        for (path, old) in previous_observed {
            if previous.get(path).is_some_and(|entry| entry.id == old.id) {
                old_inodes
                    .entry((old.device, old.inode))
                    .or_default()
                    .push(old);
            }
        }
        let mut current_counts = BTreeMap::new();
        for current in self.observed.values() {
            *current_counts
                .entry((current.device, current.inode))
                .or_insert(0usize) += 1;
        }
        let mut used = BTreeSet::new();
        for (path, entry) in self.manifest.iter_mut() {
            entry.id.clear();
            if let Some(old) = previous.get(path)
                && used.insert(old.id.clone())
            {
                entry.id.clone_from(&old.id);
            }
        }
        // Reserve ALL unique inode matches before any origin candidate can
        // claim their IDs, just as same-path reservations precede renames.
        for (path, entry) in self.manifest.iter_mut() {
            if !entry.id.is_empty() {
                continue;
            }
            if let Some(current) = self.observed.get(path) {
                let key = (current.device, current.inode);
                if current_counts[&key] == 1
                    && let Some(old) = old_inodes.get(&key)
                    && old.len() == 1
                    && used.insert(old[0].id.clone())
                {
                    entry.id.clone_from(&old[0].id);
                }
            }
        }
        for (path, entry) in self.manifest.iter_mut() {
            if entry.id.is_empty() && used.insert(candidates[path].clone()) {
                entry.id.clone_from(&candidates[path]);
            }
            while entry.id.is_empty() {
                let id = fresh_id(path);
                if used.insert(id.clone()) {
                    entry.id = id;
                }
            }
            if let Some(current) = self.observed.get_mut(path) {
                current.id.clone_from(&entry.id);
            }
        }
        content::validate_manifest(&self.manifest)
    }
}

/// Produces a fresh manifest without publishing a head or changing the mount.
/// Only actual upper files or files without complete lower proof are read.
pub fn capture(
    base: &Manifest,
    objects: &Path,
    merged: &Path,
    upper: &Path,
    lower: &Path,
) -> Result<CaptureResult> {
    let setup_started = Instant::now();
    // SAFETY: geteuid is a side-effect-free query.
    if unsafe { libc::geteuid() } != 0 {
        return Err(store::invalid(
            "incremental capture requires a rootful trusted OverlayFS mount",
        )
        .into());
    }
    content::validate_manifest(base)?;
    let root = real_directory(objects)?;
    let merged = real_directory(merged)?;
    let upper = real_directory(upper)?;
    let lower = real_directory(lower)?;
    disjoint(&[&root, &merged, &upper, &lower])?;
    let binding = current_binding(&root, &merged, &upper, &lower)?;
    let store = ObjectStore::open(&root)?;
    let digest = blake3::hash(&serde_json::to_vec(&base)?)
        .to_hex()
        .to_string();
    let lower_fd = open_directory(&lower)?;
    let upper_fd = open_directory(&upper)?;
    let lower_origin_valid = origin(&lower_fd, &digest, base).as_deref() == Some("");
    let enabled = binding.metacopy_off && lower_origin_valid;
    let fallback_reason = if !binding.metacopy_off {
        Some("metacopy state cannot be proven off; every file will be hashed".into())
    } else if !lower_origin_valid {
        Some(
            "lower root origin does not authenticate the base manifest; every file will be hashed"
                .into(),
        )
    } else {
        None
    };
    let mut context = Capture {
        base,
        digest: &digest,
        store: &store,
        upper: &upper_fd,
        lower: &lower_fd,
        batch: content::ObjectBatch::open(&root)?,
        manifest: Manifest::new(),
        origins: BTreeMap::new(),
        certificate: enabled.then(|| Certificate::new(base)),
        stats: CaptureStats {
            lower_reuse_enabled: enabled,
            fallback_reason,
            ..Default::default()
        },
    };
    context.stats.timings.setup_seconds = setup_started.elapsed().as_secs_f64();
    let walk_started = Instant::now();
    context.walk(&merged, "")?;
    context.stats.timings.walk_total_seconds = walk_started.elapsed().as_secs_f64();
    let finalize_started = Instant::now();
    // Certificate reuse skips payloads and merged enumeration, not production
    // inode observations. Gather metadata only, and never follow leaf links.
    let observed = observe_manifest(&merged, &context.manifest)?;
    context.validate_inventories()?;
    assign_ids(&mut context.manifest, base, &context.origins);
    content::validate_manifest(&context.manifest)?;
    context.batch.finish()?;
    // Topology changes during traversal invalidate all reuse evidence.
    if current_binding(&root, &merged, &upper, &lower)? != binding
        || (enabled && origin(&lower_fd, &digest, base).as_deref() != Some(""))
    {
        return Err(store::invalid("mount or lower origin changed during capture").into());
    }
    context.stats.timings.finalize_seconds = finalize_started.elapsed().as_secs_f64();
    let mut observed = observed;
    for (path, observation) in &mut observed {
        observation.id.clone_from(&context.manifest[path].id);
    }
    Ok(CaptureResult {
        observed,
        manifest: context.manifest,
        stats: context.stats,
    })
}

fn observe_manifest(merged: &Path, manifest: &Manifest) -> Result<Observed> {
    let mut observed = Observed::new();
    for (path, entry) in manifest {
        let metadata = fs::symlink_metadata(merged.join(path))?;
        let same_kind = match &entry.kind {
            EntryKind::File { size, .. } => metadata.is_file() && metadata.len() == *size,
            EntryKind::Directory => metadata.is_dir(),
            EntryKind::Symlink { target } => {
                metadata.file_type().is_symlink()
                    && fs::read_link(merged.join(path))?.as_os_str().as_bytes() == target
            }
        };
        if !same_kind
            || metadata.mode() & 0o777 != entry.mode
            || modified_ns(&metadata) != entry.modified_ns
        {
            return Err(store::invalid("merged entry changed during observation").into());
        }
        observed.insert(
            path.clone(),
            Observation {
                id: entry.id.clone(),
                device: metadata.dev(),
                inode: metadata.ino(),
            },
        );
    }
    Ok(observed)
}

struct Capture<'a> {
    base: &'a Manifest,
    digest: &'a str,
    store: &'a ObjectStore,
    upper: &'a File,
    lower: &'a File,
    batch: content::ObjectBatch,
    manifest: Manifest,
    origins: BTreeMap<String, String>,
    stats: CaptureStats,
    certificate: Option<Certificate>,
}

impl Capture<'_> {
    fn walk(&mut self, path: &Path, relative: &str) -> Result<()> {
        self.walk_certified(path, relative, self.certificate.is_some())
    }

    fn walk_certified(
        &mut self,
        path: &Path,
        relative: &str,
        allow_certificate: bool,
    ) -> Result<()> {
        let started = Instant::now();
        let before = fs::symlink_metadata(path)?;
        self.stats.timings.metadata_seconds += started.elapsed().as_secs_f64();
        let advisory;
        let kind = if before.is_file() {
            let started = Instant::now();
            let mut file = OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
                .open(path)?;
            self.stats.timings.regular_merged_open_seconds += started.elapsed().as_secs_f64();
            let started = Instant::now();
            let opened = file.metadata()?;
            self.stats.timings.metadata_seconds += started.elapsed().as_secs_f64();
            require_unchanged(path, &before, &opened)?;
            let started = Instant::now();
            advisory = origin(&file, self.digest, self.base);
            self.stats.timings.origin_xattr_seconds += started.elapsed().as_secs_f64();
            let started = Instant::now();
            let upper_status = upper_state(self.upper, relative);
            self.stats.timings.native_upper_state_seconds += started.elapsed().as_secs_f64();
            let reusable = if self.stats.lower_reuse_enabled && upper_status == UpperState::Absent {
                advisory.as_deref().and_then(|original| {
                    let started = Instant::now();
                    let reusable = self.reusable(original, &opened);
                    self.stats.timings.lower_reusable_proof_seconds +=
                        started.elapsed().as_secs_f64();
                    reusable
                })
            } else {
                None
            };
            let kind = if let Some(kind) = reusable {
                self.stats.files_reused += 1;
                self.stats.bytes_reused += opened.len();
                // A copied-up/faked origin cannot make an upper file reusable.
                let started = Instant::now();
                let upper_status = upper_state(self.upper, relative);
                self.stats.timings.native_upper_state_seconds += started.elapsed().as_secs_f64();
                if upper_status != UpperState::Absent {
                    return Err(store::invalid("upper changed during capture").into());
                }
                kind
            } else {
                if upper_status != UpperState::Present {
                    self.stats.fallback_files += 1;
                }
                let started = Instant::now();
                let kind = self.batch.put_reader(&mut file)?;
                self.stats.timings.content_hash_put_stream_seconds +=
                    started.elapsed().as_secs_f64();
                let EntryKind::File { size, .. } = &kind else {
                    unreachable!()
                };
                self.stats.files_hashed += 1;
                self.stats.bytes_hashed += *size;
                if *size != opened.len() {
                    return Err(store::invalid("file length changed during capture").into());
                }
                kind
            };
            let started = Instant::now();
            let after = file.metadata()?;
            self.stats.timings.metadata_seconds += started.elapsed().as_secs_f64();
            require_unchanged(path, &opened, &after)?;
            let started = Instant::now();
            drop(file);
            self.stats.timings.descriptor_close_seconds += started.elapsed().as_secs_f64();
            kind
        } else if before.is_dir() {
            self.stats.directories += 1;
            let started = Instant::now();
            let directory = open_directory(path)?;
            self.stats.timings.directory_open_seconds += started.elapsed().as_secs_f64();
            let started = Instant::now();
            let opened = directory.metadata()?;
            self.stats.timings.metadata_seconds += started.elapsed().as_secs_f64();
            require_unchanged(path, &before, &opened)?;
            let started = Instant::now();
            // Same-path directory IDs always win in assign_ids; an origin
            // query here is only an identity hint, never a reuse proof.
            advisory = if self.base.contains_key(relative) {
                None
            } else {
                origin(&directory, self.digest, self.base)
            };
            self.stats.timings.origin_xattr_seconds += started.elapsed().as_secs_f64();
            let started = Instant::now();
            drop(directory);
            self.stats.timings.descriptor_close_seconds += started.elapsed().as_secs_f64();
            EntryKind::Directory
        } else if before.file_type().is_symlink() {
            self.stats.symlinks += 1;
            // Linux forbids user.* xattrs on symlinks. Same-path IDs remain
            // stable, but symlink renames have no origin hint in this prototype.
            advisory = None;
            EntryKind::Symlink {
                target: fs::read_link(path)?.as_os_str().as_bytes().to_vec(),
            }
        } else {
            return Err(
                store::invalid(format!("special file rejected: {}", path.display())).into(),
            );
        };
        if let Some(original) = advisory {
            self.origins.insert(relative.to_owned(), original);
        }
        self.manifest.insert(
            relative.to_owned(),
            Entry {
                id: String::new(),
                mode: before.mode() & 0o777,
                modified_ns: modified_ns(&before),
                kind,
            },
        );
        self.stats.entries += 1;
        if before.is_dir() {
            let inventory = if allow_certificate {
                let started = Instant::now();
                let inventory = self.certificate.as_ref().and_then(|certificate| {
                    clean_inventory(self.upper, relative)
                        .and_then(|inventory| {
                            require_parent_inventory(
                                &certificate.inventories,
                                relative,
                                &inventory,
                            )?;
                            Ok(inventory)
                        })
                        .ok()
                });
                self.stats.timings.certificate_inventory_seconds += started.elapsed().as_secs_f64();
                inventory
            } else {
                None
            };
            if let Some(inventory) = inventory {
                // Complete native inventory is the ONLY source of absence
                // evidence. ENOENT while opening an inventory never proves it.
                let certificate = self.certificate.as_mut().expect("certificate enabled");
                let mut names = certificate
                    .children
                    .get(relative)
                    .cloned()
                    .unwrap_or_default();
                names.extend(inventory.children.keys().cloned());
                let upper_names: BTreeSet<_> = inventory.children.keys().cloned().collect();
                certificate
                    .inventories
                    .insert(relative.to_owned(), inventory);
                for name in names {
                    let child_relative = child_path(relative, &name);
                    if !upper_names.contains(&name) {
                        let started = Instant::now();
                        self.clone_subtree(&child_relative)?;
                        self.stats.timings.certificate_reuse_seconds +=
                            started.elapsed().as_secs_f64();
                    } else {
                        // Whiteouts and redirect semantics are interpreted by
                        // the kernel, never by guessing native upper encodings.
                        let child = path.join(&name);
                        match fs::symlink_metadata(&child) {
                            Ok(_) => self.walk_certified(&child, &child_relative, true)?,
                            Err(error) if error.raw_os_error() == Some(libc::ENOENT) => {}
                            Err(error) => return Err(error.into()),
                        }
                    }
                }
            } else {
                // Any unknown flag/read/enumeration disables certificates
                // throughout this subtree, including all redirected children.
                let started = Instant::now();
                let mut children = fs::read_dir(path)?.collect::<io::Result<Vec<_>>>()?;
                children.sort_by_key(|child| child.file_name());
                self.stats.timings.directory_enumeration_seconds += started.elapsed().as_secs_f64();
                for child in children {
                    let name = child
                        .file_name()
                        .into_string()
                        .map_err(|_| store::invalid("non-UTF-8 path rejected"))?;
                    self.walk_certified(&child.path(), &child_path(relative, &name), false)?;
                }
            }
        }
        let started = Instant::now();
        let after = fs::symlink_metadata(path)?;
        self.stats.timings.metadata_seconds += started.elapsed().as_secs_f64();
        require_unchanged(path, &before, &after)?;
        Ok(())
    }

    fn validate_inventories(&self) -> Result<()> {
        if let Some(certificate) = &self.certificate {
            validate_inventory_set(self.upper, &certificate.inventories)?;
        }
        Ok(())
    }

    fn clone_subtree(&mut self, relative: &str) -> Result<()> {
        // Separate exact node and slash-prefixed descendants: `a` must never
        // include sibling `ab`. Disjoint cloned ranges each visit nodes once.
        let entry = self
            .base
            .get(relative)
            .ok_or_else(|| store::invalid("certificate child missing from base"))?;
        self.clone_entry(relative, entry)?;
        let prefix = format!("{relative}/");
        for (path, entry) in self
            .base
            .range(prefix.clone()..)
            .take_while(|(path, _)| path.starts_with(&prefix))
        {
            self.clone_entry(path, entry)?;
        }
        self.stats.certificate_subtrees_reused += 1;
        Ok(())
    }

    fn clone_entry(&mut self, path: &str, entry: &Entry) -> Result<()> {
        match &entry.kind {
            EntryKind::File { size, chunks } => {
                let lengths = &mut self
                    .certificate
                    .as_mut()
                    .expect("certificate enabled")
                    .chunk_lengths;
                let mut total = 0u64;
                for hash in chunks {
                    let length = if let Some(length) = lengths.get(hash) {
                        *length
                    } else {
                        let length = self.store.chunk_length(hash)? as u64;
                        lengths.insert(hash.clone(), length);
                        length
                    };
                    total = total
                        .checked_add(length)
                        .ok_or_else(|| store::invalid("certificate file size overflow"))?;
                }
                if total != *size {
                    return Err(store::invalid(
                        "certificate object lengths disagree with file size",
                    )
                    .into());
                }
                self.stats.files_reused += 1;
                self.stats.certificate_files_reused += 1;
                self.stats.bytes_reused += size;
            }
            EntryKind::Directory => {
                self.stats.directories += 1;
                self.stats.certificate_directories_reused += 1;
            }
            EntryKind::Symlink { .. } => {
                self.stats.symlinks += 1;
                self.stats.certificate_symlinks_reused += 1;
            }
        }
        let mut cloned = entry.clone();
        cloned.id.clear(); // Normal same-path/origin ID policy runs once later.
        if self.manifest.insert(path.to_owned(), cloned).is_some() {
            return Err(store::invalid("certificate prefixes overlap").into());
        }
        self.stats.entries += 1;
        Ok(())
    }

    /// The merged payload is only a candidate. Independently query the exact
    /// original path on the validated immutable lower, without symlink traversal.
    fn reusable(&self, original: &str, merged: &Metadata) -> Option<EntryKind> {
        let entry = self.base.get(original)?;
        let EntryKind::File { size, chunks } = &entry.kind else {
            return None;
        };
        if merged.len() != *size {
            return None;
        }
        let lower = open_beneath(self.lower, original, false).ok()?;
        let before = lower.metadata().ok()?;
        if !before.is_file()
            || before.len() != *size
            || (before.mode() & 0o777) != entry.mode
            || modified_ns(&before) != entry.modified_ns
            || origin(&lower, self.digest, self.base).as_deref() != Some(original)
        {
            return None;
        }
        // Object metadata is checked, but lower content is intentionally not
        // reread or rehashed. Its immutable provenance is the reuse proof.
        let total = chunks.iter().try_fold(0_u64, |sum, hash| {
            sum.checked_add(self.store.chunk_length(hash).ok()? as u64)
        })?;
        if total != *size || !unchanged(&before, &lower.metadata().ok()?) {
            return None;
        }
        Some(entry.kind.clone())
    }
}

// This state is capture-local and is created by the public entry point only
// after immutable-root and exact mount-binding proof. It is never persisted.
struct Certificate {
    children: BTreeMap<String, BTreeSet<String>>,
    inventories: BTreeMap<String, UpperInventory>,
    chunk_lengths: BTreeMap<String, u64>,
}

impl Certificate {
    fn new(base: &Manifest) -> Self {
        let mut children: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        for path in base.keys().filter(|path| !path.is_empty()) {
            let (parent, name) = path.rsplit_once('/').unwrap_or(("", path));
            children
                .entry(parent.to_owned())
                .or_default()
                .insert(name.to_owned());
        }
        Self {
            children,
            inventories: BTreeMap::new(),
            chunk_lengths: BTreeMap::new(),
        }
    }
}

fn child_path(parent: &str, name: &str) -> String {
    if parent.is_empty() {
        name.to_owned()
    } else {
        format!("{parent}/{name}")
    }
}

#[derive(Debug, Eq, PartialEq)]
struct UpperInventory {
    own: StatIdentity,
    // Includes every native child, without interpreting whiteouts or symlinks.
    children: BTreeMap<String, StatIdentity>,
}

#[derive(Debug, Eq, PartialEq)]
struct StatIdentity {
    dev: u64,
    ino: u64,
    mode: u32,
    size: i64,
    mtime: i64,
    mtime_ns: i64,
    ctime: i64,
    ctime_ns: i64,
}

fn stat_identity(parent: &File, name: &OsStr) -> io::Result<StatIdentity> {
    let name = CString::new(name.as_bytes()).map_err(|_| store::invalid("NUL path"))?;
    let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
    // SAFETY: parent/name are live; output points to a stat. Never follows a child symlink.
    if unsafe {
        libc::fstatat(
            parent.as_raw_fd(),
            name.as_ptr(),
            stat.as_mut_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    } != 0
    {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: successful fstatat initialized the structure.
    let stat = unsafe { stat.assume_init() };
    Ok(StatIdentity {
        dev: stat.st_dev,
        ino: stat.st_ino,
        mode: stat.st_mode,
        size: stat.st_size,
        mtime: stat.st_mtime,
        mtime_ns: stat.st_mtime_nsec,
        ctime: stat.st_ctime,
        ctime_ns: stat.st_ctime_nsec,
    })
}

fn flag_is_absent(result: isize, errno: Option<i32>) -> bool {
    result == -1 && errno == Some(libc::ENODATA)
}

fn require_clean_flags(directory: &File) -> io::Result<()> {
    for name in [c"trusted.overlay.redirect", c"trusted.overlay.opaque"] {
        // Query existence, not value: even an empty/x/unknown value blocks reuse.
        // SAFETY: owned fd and static C string; null zero-length output is legal.
        let result = unsafe {
            libc::fgetxattr(
                directory.as_raw_fd(),
                name.as_ptr(),
                std::ptr::null_mut(),
                0,
            )
        };
        let error = if result < 0 {
            io::Error::last_os_error().raw_os_error()
        } else {
            None
        };
        if !flag_is_absent(result, error) {
            return Err(store::invalid(
                "native upper overlay flags are present or uncertain",
            ));
        }
    }
    Ok(())
}

fn validate_inventory_set(
    upper: &File,
    inventories: &BTreeMap<String, UpperInventory>,
) -> io::Result<()> {
    for (relative, expected) in inventories {
        require_parent_inventory(inventories, relative, expected)?;
        if clean_inventory(upper, relative)? != *expected {
            return Err(store::invalid(
                "native upper inventory changed during capture",
            ));
        }
    }
    Ok(())
}

// Immediate-parent evidence makes the saved inventory set prefix-closed. At
// insertion the parent is already certified; final validation checks every
// saved prefix again. Native child identity also binds each edge of the tree.
fn require_parent_inventory(
    inventories: &BTreeMap<String, UpperInventory>,
    relative: &str,
    inventory: &UpperInventory,
) -> io::Result<()> {
    if relative.is_empty() {
        return Ok(());
    }
    let (parent, name) = relative.rsplit_once('/').unwrap_or(("", relative));
    let expected = inventories
        .get(parent)
        .and_then(|parent| parent.children.get(name));
    if expected != Some(&inventory.own) || inventory.own.mode & libc::S_IFMT != libc::S_IFDIR {
        return Err(store::invalid(
            "missing or changed native upper parent evidence",
        ));
    }
    Ok(())
}

fn clean_inventory(upper: &File, relative: &str) -> io::Result<UpperInventory> {
    let directory = open_inventory_directory(upper, relative)?;
    require_clean_flags(&directory)?;
    inventory_from_clean_directory(&directory)
}

fn inventory_from_clean_directory(directory: &File) -> io::Result<UpperInventory> {
    let own = stat_identity(directory, OsStr::new("."))?;
    let mut children = BTreeMap::new();
    // The proc link is to our owned FD, not a user path. The FD stays alive
    // through enumeration. Child metadata remains descriptor-relative/nofollow.
    for child in fs::read_dir(format!("/proc/self/fd/{}", directory.as_raw_fd()))? {
        let name = child?.file_name();
        let identity = stat_identity(directory, &name)?;
        let name = name
            .into_string()
            .map_err(|_| store::invalid("non-UTF-8 upper name"))?;
        if children.insert(name, identity).is_some() {
            return Err(store::invalid("duplicate native upper name"));
        }
    }
    require_clean_flags(directory)?;
    if stat_identity(directory, OsStr::new("."))? != own {
        return Err(store::invalid("native upper changed during enumeration"));
    }
    Ok(UpperInventory { own, children })
}

fn modified_ns(metadata: &Metadata) -> i128 {
    metadata.mtime() as i128 * 1_000_000_000 + metadata.mtime_nsec() as i128
}

fn unchanged(left: &Metadata, right: &Metadata) -> bool {
    left.dev() == right.dev()
        && left.ino() == right.ino()
        && left.mode() == right.mode()
        && left.len() == right.len()
        && left.mtime() == right.mtime()
        && left.mtime_nsec() == right.mtime_nsec()
        && left.ctime() == right.ctime()
        && left.ctime_nsec() == right.ctime_nsec()
}

fn require_unchanged(path: &Path, before: &Metadata, after: &Metadata) -> io::Result<()> {
    if unchanged(before, after) {
        Ok(())
    } else {
        Err(store::invalid(format!(
            "source changed during capture: {}",
            path.display()
        )))
    }
}

fn origin(file: &File, digest: &str, base: &Manifest) -> Option<String> {
    let name = CString::new(ORIGIN_XATTR).ok()?;
    // One bounded read. ENODATA, EACCES, ERANGE, malformed JSON, and every other
    // failure are uncertainty, never evidence of an absent upper inode.
    let mut bytes = vec![0_u8; MAX_ORIGIN];
    // SAFETY: file owns its descriptor; name and writable buffer remain live.
    let length = unsafe {
        libc::fgetxattr(
            file.as_raw_fd(),
            name.as_ptr(),
            bytes.as_mut_ptr().cast(),
            bytes.len(),
        )
    };
    if length < 0 {
        return None;
    }
    bytes.truncate(length as usize);
    parse_origin(&bytes, digest, base)
}

fn parse_origin(bytes: &[u8], digest: &str, base: &Manifest) -> Option<String> {
    let [found, original]: [String; 2] = serde_json::from_slice(bytes).ok()?;
    if found != digest || !base.contains_key(&original) {
        return None;
    }
    Some(original)
}

#[derive(Debug, Eq, PartialEq)]
enum UpperState {
    Absent,
    Present,
    Unknown,
}

fn upper_state(upper: &File, relative: &str) -> UpperState {
    let mut parent = match upper.try_clone() {
        Ok(file) => file,
        Err(_) => return UpperState::Unknown,
    };
    let parts: Vec<_> = relative.split('/').collect();
    if relative.is_empty() || parts.iter().any(|part| matches!(*part, "" | "." | "..")) {
        return UpperState::Unknown;
    }
    for (index, part) in parts.iter().enumerate() {
        let name = match CString::new(*part) {
            Ok(name) => name,
            Err(_) => return UpperState::Unknown,
        };
        if index + 1 == parts.len() {
            let mut metadata = std::mem::MaybeUninit::<libc::stat>::uninit();
            // SAFETY: parent/name are valid and the output points to a stat.
            let result = unsafe {
                libc::fstatat(
                    parent.as_raw_fd(),
                    name.as_ptr(),
                    metadata.as_mut_ptr(),
                    libc::AT_SYMLINK_NOFOLLOW,
                )
            };
            return if result == 0 {
                UpperState::Present
            } else {
                absence_or_unknown(io::Error::last_os_error())
            };
        }
        match open_at(&parent, OsStr::new(part), true) {
            Ok(directory) => parent = directory,
            Err(error) => return absence_or_unknown(error),
        }
    }
    UpperState::Unknown
}

fn absence_or_unknown(error: io::Error) -> UpperState {
    // ENOTDIR may mean an upper symlink ancestor. Never traverse it or mistake
    // it, EACCES, EIO, or ELOOP for positive absence evidence.
    if error.raw_os_error() == Some(libc::ENOENT) {
        UpperState::Absent
    } else {
        UpperState::Unknown
    }
}

fn open_at(parent: &File, name: &OsStr, directory: bool) -> io::Result<File> {
    let name = CString::new(name.as_bytes()).map_err(|_| store::invalid("NUL path"))?;
    let flags = libc::O_RDONLY
        | libc::O_CLOEXEC
        | libc::O_NOFOLLOW
        | libc::O_NONBLOCK
        | if directory { libc::O_DIRECTORY } else { 0 };
    // SAFETY: valid descriptor and terminated name; returned fd adopted once.
    let fd = unsafe { libc::openat(parent.as_raw_fd(), name.as_ptr(), flags) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: openat returned a new owned descriptor.
    let file = unsafe { File::from_raw_fd(fd) };
    let metadata = file.metadata()?;
    if !(if directory {
        metadata.is_dir()
    } else {
        metadata.is_file()
    }) {
        return Err(store::invalid("unexpected entry type"));
    }
    Ok(file)
}

fn open_directory(path: &Path) -> io::Result<File> {
    OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)
}

// No userspace ancestor FD cache: each open owns only its resulting FD.
// openat2 resolves every component beneath the root and rejects all symlinks,
// including magic links. ENOSYS alone permits the equivalent legacy walk.
fn open_inventory_directory(root: &File, relative: &str) -> io::Result<File> {
    open_inventory_directory_with(root, relative, |name| {
        // SAFETY: open_how is an integer-only C struct; zeroing initializes all
        // fields and padding, including mode (no creation is requested).
        let mut how: libc::open_how = unsafe { std::mem::zeroed() };
        how.flags =
            (libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOFOLLOW) as u64;
        how.resolve = libc::RESOLVE_BENEATH | libc::RESOLVE_NO_SYMLINKS;
        // SAFETY: root/name/how remain live for the syscall; size is exact.
        let fd = unsafe {
            libc::syscall(
                libc::SYS_openat2,
                root.as_raw_fd(),
                name.as_ptr(),
                &how,
                std::mem::size_of::<libc::open_how>(),
            )
        };
        if fd < 0 {
            Err(io::Error::last_os_error())
        } else {
            // SAFETY: successful openat2 returned a new owned descriptor.
            Ok(unsafe { File::from_raw_fd(fd as libc::c_int) })
        }
    })
}

fn open_inventory_directory_with(
    root: &File,
    relative: &str,
    open: impl FnOnce(&std::ffi::CStr) -> io::Result<File>,
) -> io::Result<File> {
    if relative.is_empty() {
        return root.try_clone();
    }
    // Reject noncanonical components rather than silently normalize paths.
    if relative
        .split('/')
        .any(|part| part.is_empty() || part == "." || part == "..")
    {
        return Err(store::invalid("invalid native upper path"));
    }
    let name = CString::new(relative).map_err(|_| store::invalid("NUL path"))?;
    match open(&name) {
        Err(error) if error.raw_os_error() == Some(libc::ENOSYS) => {
            open_beneath(root, relative, true)
        }
        result => result,
    }
}

fn open_beneath(root: &File, relative: &str, directory: bool) -> io::Result<File> {
    let mut file = root.try_clone()?;
    let parts: Vec<_> = Path::new(relative).components().collect();
    for (index, component) in parts.iter().enumerate() {
        let Component::Normal(name) = component else {
            return Err(store::invalid("invalid origin path"));
        };
        file = open_at(&file, name, index + 1 != parts.len() || directory)?;
    }
    Ok(file)
}

fn real_directory(path: &Path) -> io::Result<PathBuf> {
    let absolute = if path.is_absolute() {
        path.to_owned()
    } else {
        std::env::current_dir()?.join(path)
    };
    let mut directory = open_directory(Path::new("/"))?;
    for component in absolute.components() {
        match component {
            Component::RootDir | Component::CurDir => {}
            Component::Normal(name) => directory = open_at(&directory, name, true)?,
            _ => return Err(store::invalid("parent components are unsupported")),
        }
    }
    fs::canonicalize(absolute)
}

fn disjoint(paths: &[&Path]) -> io::Result<()> {
    for (index, left) in paths.iter().enumerate() {
        for right in paths.iter().skip(index + 1) {
            if left.starts_with(right) || right.starts_with(left) {
                return Err(store::invalid(
                    "store, merged, upper, lower, and work trees must be disjoint",
                ));
            }
        }
    }
    Ok(())
}

fn assign_ids(manifest: &mut Manifest, base: &Manifest, origins: &BTreeMap<String, String>) {
    let mut used = BTreeSet::new();
    // Same-name entries have first claim, including atomic editor replacements.
    for (path, entry) in manifest.iter_mut() {
        if let Some(old) = base.get(path)
            && used.insert(old.id.clone())
        {
            entry.id.clone_from(&old.id);
        }
    }
    // Origins are identity hints only, including copied-up origins. Duplicate
    // hints can never duplicate an ID or steal a surviving same-path identity.
    // A counterfeit upper hint can claim an otherwise removed base identity;
    // this experimental advisory identity scheme is not an authenticated rename
    // journal. Such hints can never authorize skipping upper content hashing.
    for (path, entry) in manifest.iter_mut() {
        if entry.id.is_empty()
            && let Some(old) = origins.get(path).and_then(|original| base.get(original))
            && used.insert(old.id.clone())
        {
            entry.id.clone_from(&old.id);
        }
        while entry.id.is_empty() {
            let id = fresh_id(path);
            if used.insert(id.clone()) {
                entry.id = id;
            }
        }
    }
}

fn fresh_id(path: &str) -> String {
    let mut hash = blake3::Hasher::new();
    hash.update(path.as_bytes());
    hash.update(&std::process::id().to_le_bytes());
    hash.update(&NONCE.fetch_add(1, Ordering::Relaxed).to_le_bytes());
    hash.update(
        &SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
            .to_le_bytes(),
    );
    hash.finalize().to_hex().to_string()
}

#[derive(Debug, Clone, Eq, PartialEq)]
struct Mount {
    id: String,
    root: PathBuf,
    point: PathBuf,
    options: BTreeSet<String>,
    filesystem: String,
    source: String,
    super_options: BTreeMap<String, String>,
}

#[derive(Debug, Eq, PartialEq)]
struct Binding {
    overlay: Mount,
    lower: Mount,
    metacopy_off: bool,
}

fn current_binding(root: &Path, merged: &Path, upper: &Path, lower: &Path) -> Result<Binding> {
    // Linux ovl_show_options prints metacopy only when it differs from the
    // current module default. An omitted option alone therefore proves nothing.
    // https://github.com/torvalds/linux/blob/master/fs/overlayfs/params.c
    let before = fs::read_to_string("/sys/module/overlay/parameters/metacopy").ok();
    let mounts = parse_mountinfo(&fs::read_to_string("/proc/self/mountinfo")?)?;
    let after = fs::read_to_string("/sys/module/overlay/parameters/metacopy").ok();
    let default_off = before.as_deref().map(str::trim) == Some("N") && before == after;
    let binding = validate_binding(&mounts, root, merged, upper, lower, default_off)?;
    let work = Path::new(&binding.overlay.super_options["workdir"]);
    if real_directory(work)? != work || fs::metadata(work)?.dev() != fs::metadata(upper)?.dev() {
        return Err(store::invalid("invalid overlay work directory").into());
    }
    Ok(binding)
}

fn validate_binding(
    mounts: &[Mount],
    root: &Path,
    merged: &Path,
    upper: &Path,
    lower: &Path,
    default_off: bool,
) -> io::Result<Binding> {
    let exact = |point: &Path| -> io::Result<Mount> {
        let mut matches = mounts.iter().filter(|mount| mount.point == point);
        let mount = matches
            .next()
            .ok_or_else(|| store::invalid("path is not an exact mountpoint"))?;
        if matches.next().is_some() {
            return Err(store::invalid("stacked mounts unsupported"));
        }
        Ok(mount.clone())
    };
    let overlay = exact(merged)?;
    let lower_mount = exact(lower)?;
    let options = &overlay.super_options;
    if overlay.filesystem != "overlay"
        || overlay.root != Path::new("/")
        || options.contains_key("userxattr")
        || options.contains_key("volatile")
        || options
            .get("fsync")
            .is_some_and(|value| value == "volatile")
        || options.contains_key("lowerdir+")
        || options.contains_key("datadir+")
        || options.get("lowerdir").map(Path::new) != Some(lower)
        || options.get("upperdir").map(Path::new) != Some(upper)
        || lower_mount.filesystem != "fuse"
        || lower_mount.source != "sprig-cas"
        || lower_mount.super_options.get("user_id").map(String::as_str) != Some("0")
        || lower_mount.root != Path::new("/")
        || !lower_mount.options.contains("ro")
    {
        return Err(store::invalid("unsupported overlay/lower binding"));
    }
    let work = Path::new(
        options
            .get("workdir")
            .ok_or_else(|| store::invalid("overlay workdir missing"))?,
    );
    disjoint(&[root, merged, upper, lower, work])?;
    for path in [root, merged, upper, lower, work] {
        if !path.is_absolute()
            || path
                .as_os_str()
                .as_bytes()
                .iter()
                .any(|byte| matches!(byte, b':' | b',' | b'\\'))
        {
            return Err(store::invalid("ambiguous overlay path unsupported"));
        }
        if mounts
            .iter()
            .any(|mount| mount.point != path && mount.point.starts_with(path))
        {
            return Err(store::invalid(
                "nested mounts within capture trees unsupported",
            ));
        }
    }
    let metacopy_off = match options.get("metacopy").map(String::as_str) {
        Some("off") => true,
        None => default_off,
        _ => {
            return Err(store::invalid(
                "metacopy enabled or unknown option unsupported",
            ));
        }
    };
    Ok(Binding {
        overlay,
        lower: lower_mount,
        metacopy_off,
    })
}

fn parse_mountinfo(input: &str) -> io::Result<Vec<Mount>> {
    input
        .lines()
        .map(|line| {
            let (left, right) = line
                .split_once(" - ")
                .ok_or_else(|| store::invalid("invalid mountinfo"))?;
            let left: Vec<_> = left.split_ascii_whitespace().collect();
            let right: Vec<_> = right.split_ascii_whitespace().collect();
            if left.len() < 6 || right.len() != 3 {
                return Err(store::invalid("invalid mountinfo fields"));
            }
            let mut super_options = BTreeMap::new();
            for part in right[2].split(',') {
                let (key, value) = part.split_once('=').unwrap_or((part, ""));
                let value = decode_mount(value)?
                    .into_string()
                    .map_err(|_| store::invalid("non-UTF-8 mount option"))?;
                if super_options.insert(key.to_owned(), value).is_some() {
                    return Err(store::invalid("duplicate mount option unsupported"));
                }
            }
            Ok(Mount {
                id: left[0].into(),
                root: PathBuf::from(decode_mount(left[3])?),
                point: PathBuf::from(decode_mount(left[4])?),
                options: left[5].split(',').map(str::to_owned).collect(),
                filesystem: right[0].into(),
                source: decode_mount(right[1])?
                    .into_string()
                    .map_err(|_| store::invalid("non-UTF-8 mount source"))?,
                super_options,
            })
        })
        .collect()
}

fn decode_mount(input: &str) -> io::Result<OsString> {
    let bytes = input.as_bytes();
    let mut output = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'\\' {
            if index + 3 >= bytes.len()
                || !bytes[index + 1..index + 4]
                    .iter()
                    .all(|byte| (b'0'..=b'7').contains(byte))
            {
                return Err(store::invalid("invalid mountinfo escape"));
            }
            let value = (bytes[index + 1] - b'0') as u16 * 64
                + (bytes[index + 2] - b'0') as u16 * 8
                + (bytes[index + 3] - b'0') as u16;
            if value == 0 || value > 255 {
                return Err(store::invalid("invalid mountinfo escape byte"));
            }
            output.push(value as u8);
            index += 4;
        } else {
            output.push(bytes[index]);
            index += 1;
        }
    }
    Ok(OsString::from_vec(output))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Cursor, Read, Write};

    const MIN_CHUNK: usize = 16 * 1024;
    const AVG_CHUNK: usize = 64 * 1024;
    const MAX_CHUNK: usize = 256 * 1024;
    use std::os::unix::fs::symlink;

    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            let path = Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("target/test-work")
                .join(format!(
                    "capture-{}-{}",
                    std::process::id(),
                    NONCE.fetch_add(1, Ordering::Relaxed)
                ));
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn entry(id: &str, kind: EntryKind) -> Entry {
        Entry {
            id: id.into(),
            mode: 0o644,
            modified_ns: 0,
            kind,
        }
    }

    #[test]
    fn mount_binding_is_exact_and_metacopy_is_not_assumed() {
        let text = "1 0 0:1 / / rw - tmpfs tmpfs rw\n\
            2 1 0:2 / /lower ro,nosuid,nodev - fuse sprig-cas ro,user_id=0,group_id=0\n\
            3 1 0:3 / /merged rw - overlay overlay rw,lowerdir=/lower,upperdir=/upper,workdir=/work,nouserxattr\n";
        let mounts = parse_mountinfo(text).unwrap();
        let check = |mounts: &[Mount], default| {
            validate_binding(
                mounts,
                Path::new("/store"),
                Path::new("/merged"),
                Path::new("/upper"),
                Path::new("/lower"),
                default,
            )
        };
        assert!(!check(&mounts, false).unwrap().metacopy_off);
        assert!(check(&mounts, true).unwrap().metacopy_off);
        let mut changed = mounts.clone();
        changed[2]
            .super_options
            .insert("metacopy".into(), "off".into());
        assert!(check(&changed, false).unwrap().metacopy_off);
        for (key, value) in [
            ("metacopy", "on"),
            ("userxattr", ""),
            ("lowerdir", "/lower:/second"),
            ("upperdir", "/other"),
            ("workdir", "/store/work"),
        ] {
            let mut changed = mounts.clone();
            changed[2].super_options.insert(key.into(), value.into());
            assert!(check(&changed, true).is_err(), "accepted {key}={value}");
        }
        let mut changed = mounts.clone();
        changed[1].options.remove("ro");
        assert!(check(&changed, true).is_err());
        let mut changed = mounts.clone();
        changed[1].super_options.remove("user_id");
        assert!(check(&changed, true).is_err());
        changed[1]
            .super_options
            .insert("user_id".into(), "1000".into());
        assert!(check(&changed, true).is_err());
        let mut changed = mounts.clone();
        let mut child = changed[0].clone();
        child.point = PathBuf::from("/upper/nested");
        changed.push(child);
        assert!(check(&changed, true).is_err());
        assert_eq!(
            decode_mount(r"/a\040b\134c").unwrap(),
            OsStr::new("/a b\\c")
        );
        assert!(decode_mount(r"/a\999").is_err());
    }

    #[test]
    fn upper_absence_never_follows_symlinks_or_accepts_permission_errors() {
        let fixture = Fixture::new();
        let upper_path = fixture.0.join("upper");
        fs::create_dir(&upper_path).unwrap();
        fs::create_dir(upper_path.join("dir")).unwrap();
        fs::write(upper_path.join("dir/file"), "upper").unwrap();
        symlink("dir", upper_path.join("alias")).unwrap();
        symlink("missing", upper_path.join("dangling")).unwrap();
        let upper = open_directory(&upper_path).unwrap();
        assert_eq!(upper_state(&upper, "dir/file"), UpperState::Present);
        assert_eq!(upper_state(&upper, "dir/new"), UpperState::Absent);
        assert_eq!(upper_state(&upper, "missing/child"), UpperState::Absent);
        assert_eq!(upper_state(&upper, "alias/absent"), UpperState::Unknown);
        assert_eq!(upper_state(&upper, "dangling/child"), UpperState::Unknown);
        assert_eq!(upper_state(&upper, "dangling"), UpperState::Present);
        assert_eq!(
            absence_or_unknown(io::Error::from_raw_os_error(libc::EACCES)),
            UpperState::Unknown
        );
        assert_eq!(
            absence_or_unknown(io::Error::from_raw_os_error(libc::EIO)),
            UpperState::Unknown
        );
        assert!(real_directory(&upper_path.join("alias")).is_err());
        synthetic_origin_proof();
        synthetic_certificate_proof();
        synthetic_deep_inventory_proof();
    }

    fn synthetic_deep_inventory_proof() {
        let fixture = Fixture::new();
        let upper_path = fixture.0.join("deep-upper");
        fs::create_dir(&upper_path).unwrap();
        let mut paths = vec![String::new()];
        for _ in 0..128 {
            let next = child_path(paths.last().unwrap(), "d");
            fs::create_dir(upper_path.join(&next)).unwrap();
            paths.push(next);
        }
        let upper = open_directory(&upper_path).unwrap();
        // The exact original algorithm, retained only for differential proof
        // and local unmounted traversal measurements (not production tuning).
        let original = |relative: &str| {
            let mut directory = upper.try_clone()?;
            require_clean_flags(&directory)?;
            for component in Path::new(relative).components() {
                let Component::Normal(name) = component else {
                    return Err(store::invalid("invalid native upper path"));
                };
                directory = open_at(&directory, name, true)?;
                require_clean_flags(&directory)?;
            }
            inventory_from_clean_directory(&directory)
        };
        let mut inventories = BTreeMap::new();
        for path in &paths {
            let inventory = clean_inventory(&upper, path).unwrap();
            assert_eq!(inventory, original(path).unwrap());
            require_parent_inventory(&inventories, path, &inventory).unwrap();
            inventories.insert(path.clone(), inventory);
        }
        validate_inventory_set(&upper, &inventories).unwrap();
        let root = inventories.remove("").unwrap();
        assert!(validate_inventory_set(&upper, &inventories).is_err());
        assert!(require_parent_inventory(&inventories, "d", &inventories["d"]).is_err());
        inventories.insert(String::new(), root);
        let child = inventories.get_mut("d").unwrap();
        child.own.ino ^= 1;
        assert!(require_parent_inventory(&inventories, "d", &inventories["d"]).is_err());
        inventories.get_mut("d").unwrap().own.ino ^= 1;
        let original_mode = inventories["d"].own.mode;
        inventories.get_mut("d").unwrap().own.mode = libc::S_IFREG;
        // Even mutually matching forged edge evidence cannot certify a file.
        inventories
            .get_mut("")
            .unwrap()
            .children
            .get_mut("d")
            .unwrap()
            .mode = libc::S_IFREG;
        assert!(require_parent_inventory(&inventories, "d", &inventories["d"]).is_err());
        inventories.get_mut("d").unwrap().own.mode = original_mode;
        inventories
            .get_mut("")
            .unwrap()
            .children
            .get_mut("d")
            .unwrap()
            .mode = original_mode;
        // Ancestor identity/metadata mutation must invalidate final proof even
        // though each descendant's own flags remain clean.
        fs::write(upper_path.join("d/late"), "mutation").unwrap();
        assert!(validate_inventory_set(&upper, &inventories).is_err());
        fs::remove_file(upper_path.join("d/late")).unwrap();
        symlink("d", upper_path.join("alias")).unwrap();
        for path in [
            "/d", "../d", "d/../d", "./d", "d//d", "d/", "d\0", "alias", "alias/d",
        ] {
            assert!(
                open_inventory_directory(&upper, path).is_err(),
                "accepted {path:?}"
            );
            assert!(
                open_inventory_directory_with(&upper, path, |_| {
                    Err(io::Error::from_raw_os_error(libc::ENOSYS))
                })
                .is_err(),
                "fallback accepted {path:?}"
            );
        }
        let fallback = open_inventory_directory_with(&upper, paths.last().unwrap(), |_| {
            Err(io::Error::from_raw_os_error(libc::ENOSYS))
        })
        .unwrap();
        assert_eq!(
            stat_identity(&fallback, OsStr::new(".")).unwrap(),
            clean_inventory(&upper, paths.last().unwrap()).unwrap().own
        );
        for errno in [
            libc::EACCES,
            libc::EIO,
            libc::EINVAL,
            libc::EAGAIN,
            libc::EXDEV,
            libc::EPERM,
        ] {
            assert_eq!(
                open_inventory_directory_with(&upper, "d", |_| {
                    Err(io::Error::from_raw_os_error(errno))
                })
                .unwrap_err()
                .raw_os_error(),
                Some(errno)
            );
        }
        fs::remove_file(upper_path.join("alias")).unwrap();
        // Opt-in repetitions remain inside the existing grouped test. Timings
        // exclude fixture construction and verify exact inventories every pass.
        if std::env::var_os("SPRIG_INVENTORY_AB").is_some() {
            for repeat in 0..7 {
                let started = Instant::now();
                let old: Vec<_> = paths.iter().map(|path| original(path).unwrap()).collect();
                let old_seconds = started.elapsed().as_secs_f64();
                let started = Instant::now();
                let mut new = BTreeMap::new();
                for (path, expected) in paths.iter().zip(&old) {
                    let inventory = clean_inventory(&upper, path).unwrap();
                    assert_eq!(&inventory, expected);
                    require_parent_inventory(&new, path, &inventory).unwrap();
                    new.insert(path.clone(), inventory);
                }
                let new_seconds = started.elapsed().as_secs_f64();
                eprintln!(
                    "inventory_ab repeat={} directories={} old_seconds={old_seconds:.6} new_seconds={new_seconds:.6}",
                    repeat + 1,
                    paths.len()
                );
            }
        }
    }

    // A private proof-engine differential test, NOT a real OverlayFS test.
    // The caller constructs equivalent merged/upper trees, with no mount or
    // special-file/xattr privilege. Real redirects/whiteouts need runner tests.
    fn synthetic_certificate_proof() {
        use std::os::unix::fs::PermissionsExt;
        let fixture = Fixture::new();
        let source = fixture.0.join("source");
        let merged = fixture.0.join("merged");
        let upper = fixture.0.join("upper");
        let library = fixture.0.join("library");
        fs::create_dir_all(source.join("a/deep/nested")).unwrap();
        fs::create_dir(&upper).unwrap();
        for path in ["a/deep/nested/file", "a/duplicate", "ab", "edited"] {
            fs::write(source.join(path), "same immutable bytes").unwrap();
        }
        fs::set_permissions(source.join("a/deep"), fs::Permissions::from_mode(0o751)).unwrap();
        File::open(source.join("a/deep/nested/file"))
            .unwrap()
            .set_times(
                fs::FileTimes::new()
                    .set_modified(UNIX_EPOCH + std::time::Duration::new(1234, 56789)),
            )
            .unwrap();
        symlink(
            OsString::from_vec(vec![0xff, b'/', b'x']),
            source.join("a/link"),
        )
        .unwrap();
        let (base, _) =
            content::capture(&source, &library.join("objects"), &Observed::new()).unwrap();
        let store = ObjectStore::open(&library.join("objects")).unwrap();
        let digest = blake3::hash(&serde_json::to_vec(&base).unwrap())
            .to_hex()
            .to_string();
        content::materialize(&base, &library.join("objects"), &merged).unwrap();
        fs::write(merged.join("edited"), "changed sibling").unwrap();
        fs::write(upper.join("edited"), "changed sibling").unwrap();
        // The root's own metadata must come from merged, not base.
        fs::set_permissions(&merged, fs::Permissions::from_mode(0o750)).unwrap();
        let upper_fd = open_directory(&upper).unwrap();
        let lower_fd = open_directory(&source).unwrap();
        let make_context = |enabled: bool| Capture {
            base: &base,
            digest: &digest,
            store: &store,
            upper: &upper_fd,
            lower: &lower_fd,
            batch: content::ObjectBatch::open(&library.join("objects")).unwrap(),
            manifest: Manifest::new(),
            origins: BTreeMap::new(),
            certificate: enabled.then(|| Certificate::new(&base)),
            stats: CaptureStats::default(),
        };
        let mut certified = make_context(true);
        certified.walk(&merged, "").unwrap();
        certified.validate_inventories().unwrap();
        assert_eq!(certified.stats.certificate_subtrees_reused, 2); // a, ab
        assert_eq!(certified.stats.certificate_files_reused, 3);
        assert_eq!(certified.stats.certificate_directories_reused, 3);
        assert_eq!(certified.stats.certificate_symlinks_reused, 1);
        assert_eq!(certified.stats.files_hashed, 1);
        assert_eq!(
            certified.certificate.as_ref().unwrap().chunk_lengths.len(),
            1
        );
        assert_eq!(certified.manifest[""].mode, 0o750);
        let mut full = make_context(false);
        full.walk(&merged, "").unwrap();
        assert_eq!(certified.manifest, full.manifest);
        assert_eq!(certified.stats.entries, full.stats.entries);
        assert_eq!(certified.manifest["a/link"].kind, base["a/link"].kind);
        assert_eq!(certified.manifest["a/deep"].mode, 0o751);
        assert_eq!(
            certified.manifest["a/deep/nested/file"].modified_ns,
            1_234_000_056_789
        );
        // An unchanged inventory validates; any added name invalidates it.
        fs::write(upper.join("late"), "late writer").unwrap();
        assert!(certified.validate_inventories().is_err());
        fs::remove_file(upper.join("late")).unwrap();
        // Failure to open/scan a native upper directory never creates evidence.
        assert!(clean_inventory(&upper_fd, "missing").is_err());
        symlink("missing", upper.join("alias")).unwrap();
        assert!(clean_inventory(&upper_fd, "alias").is_err());
        fs::remove_file(upper.join("alias")).unwrap();
        assert!(flag_is_absent(-1, Some(libc::ENODATA)));
        for errno in [
            libc::EACCES,
            libc::EIO,
            libc::ENOTSUP,
            libc::ERANGE,
            libc::ENOENT,
        ] {
            assert!(!flag_is_absent(-1, Some(errno)));
        }
        for length in [0, 1, 4096] {
            assert!(!flag_is_absent(length, None));
        }
        // Explicit fallback is transitive, even with available certificate state.
        let mut fallback = make_context(true);
        fallback.walk_certified(&merged, "", false).unwrap();
        assert_eq!(fallback.manifest, full.manifest);
        assert_eq!(fallback.stats.certificate_subtrees_reused, 0);
        assert!(
            fallback
                .certificate
                .as_ref()
                .unwrap()
                .inventories
                .is_empty()
        );
        // Normal identity assignment produces the same snapshot as fullwalk;
        // reconciling a prior checkpoint preserves cloned-node IDs exactly.
        let mut certified_manifest = certified.manifest.clone();
        let mut full_manifest = full.manifest.clone();
        assign_ids(&mut certified_manifest, &base, &certified.origins);
        assign_ids(&mut full_manifest, &base, &full.origins);
        assert_eq!(certified_manifest, full_manifest);
        for path in ["a", "a/deep", "a/deep/nested/file", "a/link", "ab"] {
            assert_eq!(certified_manifest[path].id, base[path].id);
        }
        let mut result = CaptureResult {
            observed: Observed::new(),
            manifest: certified_manifest,
            stats: CaptureStats::default(),
        };
        result
            .reconcile_identities(&full_manifest, &Observed::new())
            .unwrap();
        assert_eq!(result.manifest, full_manifest); // identical/no-op candidate
        full_manifest.get_mut("a").unwrap().id = "previous-checkpoint-directory".into();
        full_manifest.get_mut("a/link").unwrap().id = "previous-checkpoint-symlink".into();
        result
            .reconcile_identities(&full_manifest, &Observed::new())
            .unwrap();
        assert_eq!(result.manifest, full_manifest);
        content::validate_manifest(&result.manifest).unwrap();
        // Each file still checks its aggregate length even when all hashes
        // have already been validated and cached for another cloned file.
        let mut wrong_size = base["ab"].clone();
        let EntryKind::File { size, .. } = &mut wrong_size.kind else {
            unreachable!()
        };
        *size += 1;
        assert!(certified.clone_entry("bad-size", &wrong_size).is_err());
        // Missing referenced objects are fatal, not silently trusted or omitted.
        let EntryKind::File { chunks, .. } = &base["ab"].kind else {
            unreachable!()
        };
        let object = library.join("objects").join(&chunks[0]);
        fs::remove_file(&object).unwrap();
        assert!(make_context(true).walk(&merged, "").is_err());
        // A fresh capture must reject symlinks and directories, not reuse a
        // previous capture's successful length cache or follow object symlinks.
        symlink(source.join("ab"), &object).unwrap();
        assert!(make_context(true).walk(&merged, "").is_err());
        fs::remove_file(&object).unwrap();
        fs::create_dir(&object).unwrap();
        assert!(make_context(true).walk(&merged, "").is_err());
        fs::remove_dir(&object).unwrap();
        // Empty/oversized chunks violate the object contract; a legal but
        // incorrect length must still fail the per-file aggregate check.
        for length in [0, MAX_CHUNK + 1, 1] {
            fs::write(&object, vec![b'x'; length]).unwrap();
            assert!(make_context(true).walk(&merged, "").is_err());
            fs::remove_file(&object).unwrap();
        }
    }

    // Isolated proof-engine fixture only: no mount creation and no public
    // constructor that bypasses runtime mount validation.
    fn synthetic_origin_proof() {
        let fixture = Fixture::new();
        let lower = fixture.0.join("lower");
        let merged = fixture.0.join("merged");
        let upper = fixture.0.join("upper");
        let library = fixture.0.join("library");
        fs::create_dir_all(lower.join("old")).unwrap();
        fs::create_dir(&upper).unwrap();
        fs::write(lower.join("old/file"), "original").unwrap();
        symlink(
            OsString::from_vec(vec![0xff, b'/', b'x']),
            lower.join("link"),
        )
        .unwrap();
        let (base, _) =
            content::capture(&lower, &library.join("objects"), &Observed::new()).unwrap();
        let store = ObjectStore::open(&library.join("objects")).unwrap();
        let digest = blake3::hash(&serde_json::to_vec(&base).unwrap())
            .to_hex()
            .to_string();
        content::materialize(&base, &library.join("objects"), &merged).unwrap();
        let write_origin = |path: &Path, original: &str| {
            let bytes = serde_json::to_vec(&[digest.as_str(), original]).unwrap();
            let file = File::open(path).unwrap();
            let name = CString::new(ORIGIN_XATTR).unwrap();
            // SAFETY: live descriptor, CString and readable bytes; test-owned file.
            let result = unsafe {
                libc::fsetxattr(
                    file.as_raw_fd(),
                    name.as_ptr(),
                    bytes.as_ptr().cast(),
                    bytes.len(),
                    0,
                )
            };
            assert_eq!(result, 0, "{}", io::Error::last_os_error());
        };
        write_origin(&lower.join("old/file"), "old/file");
        write_origin(&merged.join("old/file"), "old/file");
        write_origin(&merged.join("old"), "old");
        fs::rename(merged.join("old"), merged.join("new")).unwrap();
        let upper_fd = open_directory(&upper).unwrap();
        let lower_fd = open_directory(&lower).unwrap();
        let run = || {
            let mut context = Capture {
                base: &base,
                digest: &digest,
                store: &store,
                upper: &upper_fd,
                lower: &lower_fd,
                batch: content::ObjectBatch::open(&library.join("objects")).unwrap(),
                manifest: Manifest::new(),
                origins: BTreeMap::new(),
                certificate: None,
                stats: CaptureStats {
                    lower_reuse_enabled: true,
                    ..Default::default()
                },
            };
            let started = Instant::now();
            context.walk(&merged, "").unwrap();
            let elapsed = started.elapsed().as_secs_f64();
            let json = context.stats.json();
            let timings = json["timings"].as_object().unwrap();
            assert_eq!(timings.len(), 14);
            let sum: f64 = timings
                .values()
                .map(|value| {
                    let seconds = value.as_f64().unwrap();
                    assert!(seconds.is_finite() && seconds >= 0.0);
                    seconds
                })
                .sum();
            // Setup, walk_total and finalize are zero in this direct walk
            // fixture. The nine disjoint walk categories fit within elapsed.
            assert!(sum <= elapsed);
            assign_ids(&mut context.manifest, &base, &context.origins);
            content::validate_manifest(&context.manifest).unwrap();
            let observed = observe_manifest(&merged, &context.manifest).unwrap();
            assert_eq!(observed.len(), context.manifest.len());
            assert_eq!(
                observed["new/file"].inode,
                fs::symlink_metadata(merged.join("new/file")).unwrap().ino()
            );
            CaptureResult {
                observed,
                manifest: context.manifest,
                stats: context.stats,
            }
        };
        let captured = run();
        assert_eq!(captured.stats.files_reused, 1);
        assert_eq!(captured.stats.bytes_hashed, 0);
        assert_eq!(captured.manifest["new/file"].kind, base["old/file"].kind);
        assert_eq!(captured.manifest["new/file"].id, base["old/file"].id);
        assert_eq!(captured.manifest["new"].id, base["old"].id);
        assert_eq!(
            captured.manifest["link"].kind,
            EntryKind::Symlink {
                target: vec![0xff, b'/', b'x']
            }
        );
        // Simulate a copied-up file retaining the exact origin, size and mtime.
        // Its payload is changed, so relying on those hints would be unsound.
        let modified = fs::metadata(merged.join("new/file"))
            .unwrap()
            .modified()
            .unwrap();
        fs::write(merged.join("new/file"), "modified").unwrap();
        File::open(merged.join("new/file"))
            .unwrap()
            .set_times(std::fs::FileTimes::new().set_modified(modified))
            .unwrap();
        fs::create_dir(upper.join("new")).unwrap();
        fs::write(upper.join("new/file"), "modified").unwrap();
        write_origin(&upper.join("new/file"), "old/file");
        let captured = run();
        assert_eq!(captured.stats.files_reused, 0);
        assert_eq!(captured.stats.bytes_hashed, 8);
        assert_ne!(captured.manifest["new/file"].kind, base["old/file"].kind);
        assert_eq!(captured.manifest["new/file"].id, base["old/file"].id);
        // Even with proven upper absence, an unvalidated lower origin cannot
        // authorize reuse. Here a wrong-but-well-formed original path is used.
        fs::remove_file(upper.join("new/file")).unwrap();
        write_origin(&lower.join("old/file"), "old");
        let captured = run();
        assert_eq!(captured.stats.files_reused, 0);
        assert_eq!(captured.stats.bytes_hashed, 8);
        assert_eq!(captured.stats.fallback_files, 1);
        assert!(parse_origin(br#"["wrong","old/file"]"#, &digest, &base).is_none());
        assert!(parse_origin(br#"[1,2]"#, &digest, &base).is_none());
    }

    #[test]
    fn production_chunk_boundaries_match_and_existing_corruption_is_rejected() {
        let fixture = Fixture::new();
        let objects = fixture.0.join("objects");
        let mut random = 0x523c_2d34_a88a_u64;
        let data: Vec<u8> = (0..(MAX_CHUNK * 9 + 31))
            .map(|_| {
                random ^= random << 13;
                random ^= random >> 7;
                random ^= random << 17;
                random as u8
            })
            .collect();
        let mut batch = content::ObjectBatch::open(&objects).unwrap();
        for size in [
            0,
            1,
            MIN_CHUNK - 1,
            MIN_CHUNK,
            AVG_CHUNK + 1,
            MAX_CHUNK,
            data.len(),
        ] {
            let expected = content::put_bytes(&data[..size], &objects).unwrap();
            let direct = EntryKind::File {
                size: size as u64,
                chunks: fastcdc::v2020::FastCDC::new(
                    &data[..size],
                    MIN_CHUNK as _,
                    AVG_CHUNK as _,
                    MAX_CHUNK as _,
                )
                .map(|chunk| {
                    blake3::hash(&data[chunk.offset..chunk.offset + chunk.length])
                        .to_hex()
                        .to_string()
                })
                .collect(),
            };
            assert_eq!(expected, direct);
            assert_eq!(
                batch.put_reader(&mut Cursor::new(&data[..size])).unwrap(),
                expected
            );
        }
        // Arbitrary short reads and EINTR must not change CDC boundaries.
        struct Fragmented<'a> {
            remaining: &'a [u8],
            interrupt: bool,
        }
        impl Read for Fragmented<'_> {
            fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
                self.interrupt = !self.interrupt;
                if self.interrupt {
                    return Err(io::ErrorKind::Interrupted.into());
                }
                let count = output.len().min(self.remaining.len()).min(97);
                output[..count].copy_from_slice(&self.remaining[..count]);
                self.remaining = &self.remaining[count..];
                Ok(count)
            }
        }
        assert_eq!(
            batch
                .put_reader(&mut Fragmented {
                    remaining: &data,
                    interrupt: false
                })
                .unwrap(),
            content::put_bytes(&data, &objects).unwrap(),
        );
        // A failed reader may have published complete chunks, but returns no
        // file entry. Reusing the batch must not retain stale buffered bytes.
        struct Fails;
        impl Read for Fails {
            fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
                Err(io::Error::other("injected read failure"))
            }
        }
        let mut failing = Cursor::new(&data[..MAX_CHUNK + 100]).chain(Fails);
        assert!(batch.put_reader(&mut failing).is_err());
        assert_eq!(
            batch.put_reader(&mut Cursor::new(b"fresh")).unwrap(),
            content::put_bytes(b"fresh", &objects).unwrap()
        );
        batch.finish().unwrap();
        let mut batch = content::ObjectBatch::open(&objects).unwrap();
        // Stream a file beyond the production merge helper's 16 MiB limit. No
        // whole-file read/allocation is used by either capture implementation.
        let source = fixture.0.join("source");
        fs::create_dir(&source).unwrap();
        let mut file = File::create(source.join("large")).unwrap();
        for _ in 0..75 {
            file.write_all(&data[..MAX_CHUNK]).unwrap();
        }
        drop(file);
        let (expected, _) = content::capture(&source, &objects, &content::Observed::new()).unwrap();
        let streamed = batch
            .put_reader(&mut File::open(source.join("large")).unwrap())
            .unwrap();
        assert_eq!(streamed, expected["large"].kind);
        let EntryKind::File { chunks, .. } = streamed else {
            unreachable!()
        };
        let target = objects.join(&chunks[0]);
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&target, fs::Permissions::from_mode(0o600)).unwrap();
        let original = fs::read(&target).unwrap();
        // Same-length corruption is still rejected after reuse in this batch.
        let mut corrupt = original.clone();
        corrupt[0] ^= 1;
        fs::write(&target, &corrupt).unwrap();
        assert!(
            batch
                .put_reader(&mut File::open(source.join("large")).unwrap())
                .is_err()
        );
        fs::remove_file(&target).unwrap();
        let alternate = fixture.0.join("alternate");
        fs::write(&alternate, &original).unwrap();
        std::os::unix::fs::symlink(&alternate, &target).unwrap();
        assert!(batch.put_reader(&mut Cursor::new(&original)).is_err());
        fs::remove_file(&target).unwrap();
        fs::create_dir(&target).unwrap();
        assert!(batch.put_reader(&mut Cursor::new(&original)).is_err());
        fs::remove_dir(&target).unwrap();
        batch.put_reader(&mut Cursor::new(&original)).unwrap();
        batch.finish().unwrap();
        assert!(
            fs::read_dir(&objects).unwrap().all(|item| !item
                .unwrap()
                .file_name()
                .as_bytes()
                .starts_with(b".tmp-"))
        );
        // Even empty/dedup-only batches must execute a fallible final barrier.
        let empty = fixture.0.join("empty");
        let empty_batch = content::ObjectBatch::open(&empty).unwrap();
        fs::remove_dir(&empty).unwrap();
        assert!(empty_batch.finish().is_err());
        let mut dedup_batch = content::ObjectBatch::open(&objects).unwrap();
        dedup_batch.put_reader(&mut Cursor::new(&original)).unwrap();
        fs::rename(&objects, fixture.0.join("moved-objects")).unwrap();
        assert!(dedup_batch.finish().is_err());
    }

    #[test]
    fn same_path_identity_wins_and_origin_duplicates_get_fresh_ids() {
        let base = Manifest::from([
            ("".into(), entry("root", EntryKind::Directory)),
            (
                "a".into(),
                entry(
                    "a-id",
                    EntryKind::File {
                        size: 0,
                        chunks: vec![],
                    },
                ),
            ),
            ("old".into(), entry("rename-id", EntryKind::Directory)),
        ]);
        let mut fresh = Manifest::from([
            ("".into(), entry("", EntryKind::Directory)),
            (
                "a".into(),
                entry(
                    "",
                    EntryKind::File {
                        size: 0,
                        chunks: vec![],
                    },
                ),
            ),
            (
                "copy".into(),
                entry(
                    "",
                    EntryKind::File {
                        size: 0,
                        chunks: vec![],
                    },
                ),
            ),
            ("new".into(), entry("", EntryKind::Directory)),
            ("new-copy".into(), entry("", EntryKind::Directory)),
        ]);
        let origins = BTreeMap::from([
            ("copy".into(), "a".into()),
            ("new".into(), "old".into()),
            ("new-copy".into(), "old".into()),
        ]);
        assign_ids(&mut fresh, &base, &origins);
        assert_eq!(fresh["a"].id, "a-id");
        assert_eq!(fresh["new"].id, "rename-id");
        assert_ne!(fresh["copy"].id, "a-id");
        assert_ne!(fresh["new-copy"].id, "rename-id");
        content::validate_manifest(&fresh).unwrap();

        // The immutable lower and logical previous checkpoint are different
        // bases after the first checkpoint. Same-path upper-only IDs must not
        // change on repeated captures against that still-mounted lower.
        fresh.insert(
            "upper-only".into(),
            entry(
                "random-capture-id",
                EntryKind::File {
                    size: 0,
                    chunks: vec![],
                },
            ),
        );
        let mut previous = fresh.clone();
        previous.get_mut("upper-only").unwrap().id = "checkpoint-id".into();
        previous.remove("new");
        previous.get_mut("a").unwrap().id = "rename-id".into();
        let mut captured = CaptureResult {
            observed: Observed::new(),
            manifest: fresh,
            stats: CaptureStats::default(),
        };
        captured
            .reconcile_identities(&previous, &Observed::new())
            .unwrap();
        assert_eq!(captured.manifest["upper-only"].id, "checkpoint-id");
        assert_eq!(captured.manifest["a"].id, "rename-id");
        assert_ne!(captured.manifest["new"].id, "rename-id");
        let checkpoint = captured.manifest.clone();
        // Simulate fresh capture IDs, retaining unchanged content and metadata.
        for (path, entry) in captured.manifest.iter_mut() {
            entry.id = fresh_id(path);
        }
        captured
            .reconcile_identities(&checkpoint, &Observed::new())
            .unwrap();
        assert_eq!(captured.manifest, checkpoint);

        // Production observations preserve upper-only renames without origin
        // hints; shared inode/hardlink ambiguity must not steal logical IDs.
        let old = Manifest::from([
            ("".into(), entry("root", EntryKind::Directory)),
            (
                "old".into(),
                entry(
                    "upper-id",
                    EntryKind::File {
                        size: 0,
                        chunks: vec![],
                    },
                ),
            ),
        ]);
        let old_observed = Observed::from([
            (
                "".into(),
                Observation {
                    id: "root".into(),
                    device: 7,
                    inode: 1,
                },
            ),
            (
                "old".into(),
                Observation {
                    id: "upper-id".into(),
                    device: 7,
                    inode: 2,
                },
            ),
        ]);
        let mut renamed = CaptureResult {
            manifest: Manifest::from([
                ("".into(), entry("capture-root", EntryKind::Directory)),
                (
                    "new".into(),
                    entry(
                        "capture-new",
                        EntryKind::File {
                            size: 0,
                            chunks: vec![],
                        },
                    ),
                ),
            ]),
            observed: Observed::from([
                (
                    "".into(),
                    Observation {
                        id: "capture-root".into(),
                        device: 7,
                        inode: 1,
                    },
                ),
                (
                    "new".into(),
                    Observation {
                        id: "capture-new".into(),
                        device: 7,
                        inode: 2,
                    },
                ),
            ]),
            stats: CaptureStats::default(),
        };
        renamed.reconcile_identities(&old, &old_observed).unwrap();
        assert_eq!(renamed.manifest["new"].id, "upper-id");
        assert_eq!(renamed.observed["new"].id, "upper-id");
        renamed.manifest.get_mut("new").unwrap().id = "capture-new".into();
        renamed.manifest.insert(
            "link".into(),
            entry(
                "capture-link",
                EntryKind::File {
                    size: 0,
                    chunks: vec![],
                },
            ),
        );
        renamed.observed.insert(
            "link".into(),
            Observation {
                id: "capture-link".into(),
                device: 7,
                inode: 2,
            },
        );
        renamed.reconcile_identities(&old, &old_observed).unwrap();
        assert_ne!(renamed.manifest["new"].id, "upper-id");
        assert_ne!(renamed.manifest["link"].id, "upper-id");
        assert_ne!(renamed.manifest["new"].id, renamed.manifest["link"].id);
    }
}
