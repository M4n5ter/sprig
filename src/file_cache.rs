//! Disposable, bounded whole-file bytes for native CoW materialization.
//! CAS is authoritative; cache hits verify every requested chunk before cloning.
//! Mutable workspaces never hard-link to the cache. Cache failures before target
//! creation fall back to CAS, not to unchecked cached bytes.

use crate::{Result, packed, platform};
use std::cell::{Cell, OnceCell, RefCell};
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

const BUDGET: u64 = 256 * 1024 * 1024;
const ENTRY_BUDGET: usize = 4096;
const LARGE_FILE_MIN: u64 = 64 * 1024;

// None means the requested distinct working set cannot fit. The planner has
// bounded memory and exits monotonically when either existing cache limit is
// exceeded. Logical charges are only an admission lower bound; actual allocated
// charges remain authoritative when filling the cache.
fn working_set(manifest: &crate::content::Manifest) -> Result<Option<BTreeSet<String>>> {
    let mut keys = BTreeSet::new();
    let mut charge = 0_u64;
    for entry in manifest.values() {
        let crate::content::EntryKind::File { size, chunks } = &entry.kind else {
            continue;
        };
        if !cache_eligible(*size) {
            continue;
        }
        let key = cache_key(*size, chunks)?;
        if keys.insert(key) {
            charge = charge
                .checked_add(size.checked_add(4096).ok_or("cache size overflow")?)
                .ok_or("cache size overflow")?;
            if keys.len() > ENTRY_BUDGET || charge > BUDGET {
                return Ok(None);
            }
        }
    }
    Ok(Some(keys))
}

fn cache_key(size: u64, chunks: &[String]) -> Result<String> {
    Ok(
        blake3::hash(&serde_json::to_vec(&("sprig-file-cache-1", size, chunks))?)
            .to_hex()
            .to_string(),
    )
}

#[cfg(test)]
#[derive(Default)]
struct Counters {
    probes: Cell<usize>,
    hits: Cell<usize>,
    fills: Cell<usize>,
    admission_declines: Cell<usize>,
}

/// One optional cache lease for a fresh native tree. Contention or an invalid
/// cache disables caching for this session, never the authoritative CAS reads.
/// Ineligible files do not create, inspect or lock the cache at all.
pub(crate) struct Session<'a> {
    objects: &'a Path,
    cache: OnceCell<Option<Cache>>,
    working_set: Option<BTreeSet<String>>,
    small_admission: Cell<bool>,
    no_existing_cache: Cell<bool>,
    #[cfg(test)]
    counters: Counters,
}

impl<'a> Session<'a> {
    pub(crate) fn for_manifest(
        objects: &'a Path,
        manifest: &crate::content::Manifest,
    ) -> Result<Self> {
        let working_set = working_set(manifest)?;
        let small_admission = Cell::new(working_set.is_some());
        Ok(Self {
            objects,
            cache: OnceCell::new(),
            working_set,
            small_admission,
            no_existing_cache: Cell::new(false),
            #[cfg(test)]
            counters: Counters::default(),
        })
    }

    // Existing low-level codec tests exercise unrestricted fills directly. All
    // production native-tree calls use for_manifest and its complete plan.
    #[cfg(test)]
    fn new(objects: &'a Path) -> Self {
        Self {
            objects,
            cache: OnceCell::new(),
            working_set: Some(BTreeSet::new()),
            small_admission: Cell::new(true),
            no_existing_cache: Cell::new(false),
            counters: Counters::default(),
        }
    }

    fn cache(&self, allow_creation: bool) -> Option<&Cache> {
        if !allow_creation && self.cache.get().is_none() {
            if self.no_existing_cache.get() {
                return None;
            }
            let root = self.objects.parent()?.join("file-cache");
            if platform::symlink_metadata(&root)
                .is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound)
            {
                // Do not initialize an empty cache for a rejected small working
                // set. This memoized absence does not block later large files.
                self.no_existing_cache.set(true);
                return None;
            }
        }
        self.cache
            .get_or_init(|| {
                open_cache(self.objects)
                    .and_then(|cache| {
                        if let Some(keys) = &self.working_set {
                            cache.inventory.borrow_mut().protect(keys.clone())?;
                        }
                        Ok(cache)
                    })
                    .ok()
            })
            .as_ref()
            .filter(|cache| !cache.disabled.get())
    }

    pub(crate) fn materialize(
        &self,
        reader: &packed::Reader,
        chunks: &[String],
        size: u64,
        target: &Path,
    ) -> Result<File> {
        if !cache_eligible(size) {
            return materialize_cas(reader, chunks, size, target);
        }
        let small = size < LARGE_FILE_MIN;
        let allow_population = !small || self.small_admission.get();
        let Some(cache) = self.cache(allow_population) else {
            return materialize_cas(reader, chunks, size, target);
        };
        let key = cache_key(size, chunks)?;
        // The lease-owned inventory cannot gain entries from another compliant
        // session. Closed admission still serves verified hits, without opening
        // a missing cache path for every file in an oversized tree.
        if allow_population || cache.inventory.borrow().charges.contains_key(&key) {
            #[cfg(test)]
            self.counters.probes.set(self.counters.probes.get() + 1);
            match cached(reader, chunks, size, cache, &key, target) {
                Ok(Some(output)) => {
                    #[cfg(test)]
                    self.counters.hits.set(self.counters.hits.get() + 1);
                    return Ok(output);
                }
                Ok(None) => {
                    let mut inventory = cache.inventory.borrow_mut();
                    if inventory.charges.contains_key(&key) {
                        inventory.directory_clean = false;
                    }
                    inventory.forget(&key)?;
                }
                Err(error) if target.try_exists()? => return Err(error),
                Err(_) => cache.disabled.set(true),
            }
        }
        let output = materialize_cas(reader, chunks, size, target)?;
        if allow_population && !cache.disabled.get() {
            #[cfg(test)]
            self.counters.fills.set(self.counters.fills.get() + 1);
            let protected = small && self.working_set.is_some();
            match populate(cache, &key, size, target, protected) {
                Ok(true) => {}
                Ok(false) => {
                    // Actual allocations cannot fit without evicting this
                    // tree's working set. Stop optional small fills, not reads.
                    self.small_admission.set(false);
                    #[cfg(test)]
                    self.counters
                        .admission_declines
                        .set(self.counters.admission_declines.get() + 1);
                }
                Err(_) => cache.disabled.set(true),
            }
        }
        Ok(output)
    }
}

fn cache_eligible(size: u64) -> bool {
    (1024..=BUDGET - 4096).contains(&size)
}

fn materialize_cas(
    reader: &packed::Reader,
    chunks: &[String],
    size: u64,
    target: &Path,
) -> Result<File> {
    let mut output = platform::create_new(target, 0o600)?;
    let mut written = 0_u64;
    for hash in chunks {
        let bytes = reader.read(hash)?;
        written = written
            .checked_add(bytes.len() as u64)
            .ok_or("file length overflow")?;
        if written > size {
            return Err("chunk lengths exceed file size".into());
        }
        output.write_all(&bytes)?;
    }
    if written != size {
        return Err("chunk lengths do not match file size".into());
    }
    // The caller applies final mode/mtime and fsyncs this private destination.
    // A cache clone has its own checked sync before publication.
    Ok(output)
}

struct Cache {
    root: PathBuf,
    _lock: File,
    inventory: RefCell<Inventory>,
    disabled: Cell<bool>,
}

fn open_cache(objects: &Path) -> Result<Cache> {
    let root = objects
        .parent()
        .ok_or("object directory has no parent")?
        .join("file-cache");
    match platform::create_dir(&root, 0o700) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(e.into()),
    }
    crate::content::ensure_directory_chain(&root)?;
    let lock = match platform::create_new(&root.join("lock"), 0o600) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            platform::open_read_write(&root.join("lock"))?
        }
        Err(e) => return Err(e.into()),
    };
    lock.try_lock()?;
    let inventory = Inventory::scan(&root)?;
    Ok(Cache {
        root,
        _lock: lock,
        inventory: RefCell::new(inventory),
        disabled: Cell::new(false),
    })
}

fn cached(
    reader: &packed::Reader,
    chunks: &[String],
    size: u64,
    cache: &Cache,
    key: &str,
    target: &Path,
) -> Result<Option<File>> {
    let path = cache.root.join(key);
    let mut input = match platform::open_regular(&path) {
        Ok(input) => input,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    if verify(&mut input, reader, chunks, size).is_err() {
        drop(input);
        // This is explicitly disposable derived content, never an object/ref.
        fs::remove_file(path)?;
        cache.inventory.borrow_mut().directory_clean = false;
        return Ok(None);
    }
    if let Some(output) = crate::copy_tree::native_clone(&input, &path, target)? {
        return Ok(Some(output));
    }
    input.seek(SeekFrom::Start(0))?;
    let mut output = platform::create_new(target, 0o600)?;
    std::io::copy(&mut input, &mut output)?;
    Ok(Some(output))
}

// false is a deliberate admission decline, not a clone or IO failure.
fn populate(cache: &Cache, key: &str, size: u64, target: &Path, protected: bool) -> Result<bool> {
    let root = &cache.root;
    if !cache_eligible(size) {
        return Ok(true);
    }
    // Reserve before cloning: even a crash before completion cannot leave more
    // than the logical budget. Native allocation accounting remains separate.
    let metadata = platform::symlink_metadata(target)?;
    let charge = size
        .max(platform::allocated_bytes(target, &metadata)?)
        .checked_add(4096)
        .ok_or("cache size overflow")?;
    if protected {
        if !cache
            .inventory
            .borrow_mut()
            .reserve_protected(root, key, charge)?
        {
            return Ok(false);
        }
    } else {
        cache.inventory.borrow_mut().reserve(root, key, charge)?;
    }
    #[cfg(test)]
    tests::inject_fill_fault()?;
    let path = root.join(key);
    let input = platform::open_regular(target)?;
    let pending = root.join(format!("pending-{key}"));
    if let Some(cached) = crate::copy_tree::native_clone(&input, target, &pending)? {
        #[cfg(unix)]
        platform::set_mode(&cached, 0o444)?;
        cached.sync_all()?;
        drop(cached);
        platform::rename(&pending, &path)?;
        cache.inventory.borrow_mut().sync_directory(root)?;
    } else {
        // Unsupported cloning removed its temporary target and wrote no cache.
        cache.inventory.borrow_mut().release_after_cleanup(key)?;
    }
    Ok(true)
}

fn verify(file: &mut File, reader: &packed::Reader, chunks: &[String], size: u64) -> Result<()> {
    if platform::file_metadata(file)?.len() != size {
        return Err("file cache length mismatch".into());
    }
    let mut total = 0_u64;
    for hash in chunks {
        let length = reader.length(hash)?;
        total = total
            .checked_add(length)
            .ok_or("file cache length overflow")?;
        if total > size || length > 256 * 1024 {
            return Err("invalid cached chunk extent".into());
        }
        let mut bytes = vec![0; length as usize];
        file.read_exact(&mut bytes)?;
        if blake3::hash(&bytes).to_hex().as_str() != hash {
            return Err("file cache content hash mismatch".into());
        }
    }
    if total != size {
        return Err("file cache chunk length mismatch".into());
    }
    Ok(())
}

// One inventory per exclusive cache lease, never shared across sessions. Only
// disposable cache files are listed or removed. Retained working trees and CAS
// are outside this directory and are never candidates for eviction.
#[derive(Default)]
struct Inventory {
    charges: BTreeMap<String, u64>,
    total: u64,
    protection: Option<Protection>,
    // True only after a successful directory barrier under this cache lease.
    // Capacity-releasing deletions invalidate it before another reservation.
    directory_clean: bool,
    #[cfg(test)]
    directory_syncs: usize,
    #[cfg(test)]
    protection_scanned_entries: usize,
    #[cfg(test)]
    protected_victim_queries: usize,
}

struct Protection {
    keys: BTreeSet<String>,
    bytes: u64,
    entries: usize,
    // Ordered victims avoid rescanning a protected lexical prefix per eviction.
    victims: BTreeSet<String>,
}

impl Inventory {
    fn scan(root: &Path) -> Result<Self> {
        let mut result = Self::default();
        for entry in fs::read_dir(root)? {
            let path = entry?.path();
            let name = path
                .file_name()
                .and_then(|v| v.to_str())
                .ok_or("invalid cache name")?;
            if name == "lock" {
                continue;
            }
            let hash = name.strip_prefix("pending-").unwrap_or(name);
            if hash.len() != 64 || !hash.bytes().all(|c| c.is_ascii_hexdigit()) {
                return Err("unexpected cache entry".into());
            }
            let metadata = platform::symlink_metadata(&path)?;
            if !metadata.is_file() || metadata.file_type().is_symlink() {
                return Err("invalid cache file".into());
            }
            if name.starts_with("pending-") {
                fs::remove_file(&path)?;
                continue;
            }
            let charge = metadata
                .len()
                .max(platform::allocated_bytes(&path, &metadata)?)
                .checked_add(4096)
                .ok_or("cache size overflow")?;
            result.total = result
                .total
                .checked_add(charge)
                .ok_or("cache size overflow")?;
            result.charges.insert(name.to_owned(), charge);
            // Bound memory even when a previous interrupted cache is oversized.
            result.evict(root, 0, 0)?;
        }
        result.sync_directory(root)?;
        Ok(result)
    }

    fn protect(&mut self, keys: BTreeSet<String>) -> Result<()> {
        if self.protection.is_some() || keys.len() > ENTRY_BUDGET {
            return Err("invalid cache protection initialization".into());
        }
        let mut protection = Protection {
            keys,
            bytes: 0,
            entries: 0,
            victims: BTreeSet::new(),
        };
        for (key, charge) in &self.charges {
            if protection.keys.contains(key) {
                protection.bytes = protection
                    .bytes
                    .checked_add(*charge)
                    .ok_or("cache size overflow")?;
                protection.entries += 1;
            } else {
                protection.victims.insert(key.clone());
            }
        }
        #[cfg(test)]
        {
            self.protection_scanned_entries += self.charges.len();
        }
        self.protection = Some(protection);
        Ok(())
    }

    fn insert_charge(&mut self, key: &str, charge: u64) -> Result<()> {
        if self.charges.contains_key(key) {
            return Err("cache reservation already exists".into());
        }
        let total = self
            .total
            .checked_add(charge)
            .ok_or("cache size overflow")?;
        if let Some(protection) = &mut self.protection {
            if protection.keys.contains(key) {
                let bytes = protection
                    .bytes
                    .checked_add(charge)
                    .ok_or("cache size overflow")?;
                let entries = protection
                    .entries
                    .checked_add(1)
                    .ok_or("cache entry overflow")?;
                protection.bytes = bytes;
                protection.entries = entries;
            } else {
                protection.victims.insert(key.to_owned());
            }
        }
        self.total = total;
        self.charges.insert(key.to_owned(), charge);
        Ok(())
    }

    fn forget(&mut self, key: &str) -> Result<()> {
        let Some(charge) = self.charges.get(key).copied() else {
            return Ok(());
        };
        let total = self
            .total
            .checked_sub(charge)
            .ok_or("cache accounting underflow")?;
        if let Some(protection) = &mut self.protection {
            if protection.keys.contains(key) {
                let bytes = protection
                    .bytes
                    .checked_sub(charge)
                    .ok_or("cache protection underflow")?;
                let entries = protection
                    .entries
                    .checked_sub(1)
                    .ok_or("cache protection underflow")?;
                protection.bytes = bytes;
                protection.entries = entries;
            } else if !protection.victims.remove(key) {
                return Err("cache victim index mismatch".into());
            }
        }
        self.total = total;
        self.charges.remove(key);
        Ok(())
    }

    fn evict(&mut self, root: &Path, additional: u64, additional_entries: usize) -> Result<()> {
        while self
            .total
            .checked_add(additional)
            .ok_or("cache size overflow")?
            > BUDGET
            || self.charges.len() + additional_entries > ENTRY_BUDGET
        {
            let (key, _) = self
                .charges
                .first_key_value()
                .ok_or("cache reservation exceeds budget")?;
            let key = key.clone();
            // No cached() input handle is alive here (important on Windows).
            // Only update accounting after a successful deletion.
            fs::remove_file(root.join(&key))?;
            self.directory_clean = false;
            self.forget(&key)?;
        }
        Ok(())
    }

    fn reserve_protected(&mut self, root: &Path, key: &str, charge: u64) -> Result<bool> {
        if self.charges.contains_key(key) {
            return Err("cache reservation already exists".into());
        }
        let protection = self
            .protection
            .as_ref()
            .ok_or("missing cache protection plan")?;
        // These counters are initialized once under the lease and maintained by
        // every reservation/removal, including the unchanged large-file policy.
        if protection
            .bytes
            .checked_add(charge)
            .ok_or("cache size overflow")?
            > BUDGET
            || protection
                .entries
                .checked_add(1)
                .ok_or("cache entry overflow")?
                > ENTRY_BUDGET
        {
            return Ok(false);
        }
        while self
            .total
            .checked_add(charge)
            .ok_or("cache size overflow")?
            > BUDGET
            || self.charges.len() + 1 > ENTRY_BUDGET
        {
            let victim = self
                .protection
                .as_ref()
                .unwrap()
                .victims
                .first()
                .ok_or("cache admission accounting mismatch")?
                .clone();
            #[cfg(test)]
            {
                self.protected_victim_queries += 1;
            }
            fs::remove_file(root.join(&victim))?;
            self.directory_clean = false;
            self.forget(&victim)?;
        }
        self.reserve(root, key, charge)?;
        Ok(true)
    }

    fn reserve(&mut self, root: &Path, key: &str, charge: u64) -> Result<()> {
        if self.charges.contains_key(key) {
            return Err("cache reservation already exists".into());
        }
        self.evict(root, charge, 1)?;
        // Only skip a proven no-op barrier: scan and every successful fill
        // already sync the directory. Deletions must be durable before reuse.
        if !self.directory_clean {
            self.sync_directory(root)?;
        }
        self.insert_charge(key, charge)
    }

    fn release_after_cleanup(&mut self, key: &str) -> Result<()> {
        // Unsupported cloning may have created then removed a pending target.
        // Its deletion must be durable before this charge can be reused.
        self.directory_clean = false;
        self.forget(key)
    }

    fn sync_directory(&mut self, root: &Path) -> Result<()> {
        // A failed/unknown sync must never establish a clean directory.
        self.directory_clean = false;
        #[cfg(test)]
        {
            self.directory_syncs += 1;
            if tests::FAIL_DIRECTORY_SYNC.get() {
                return Err(std::io::Error::other("injected cache directory sync failure").into());
            }
        }
        platform::sync_directory(root)?;
        self.directory_clean = true;
        Ok(())
    }
}

#[cfg(test)]
fn reserve(root: &Path, additional: u64) -> Result<()> {
    let mut inventory = Inventory::scan(root)?;
    inventory.evict(root, additional, 0)?;
    platform::sync_directory(root)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::content::{self, EntryKind};
    use std::time::{SystemTime, UNIX_EPOCH};

    thread_local! {
        static FAIL_FILL: Cell<bool> = const { Cell::new(false) };
        pub(super) static FAIL_DIRECTORY_SYNC: Cell<bool> = const { Cell::new(false) };
    }

    pub(super) fn inject_fill_fault() -> Result<()> {
        if FAIL_FILL.get() {
            return Err(std::io::Error::other("injected cache fill failure").into());
        }
        Ok(())
    }

    struct FillFault;
    impl Drop for FillFault {
        fn drop(&mut self) {
            FAIL_FILL.set(false);
        }
    }

    struct DirectorySyncFault;
    impl Drop for DirectorySyncFault {
        fn drop(&mut self) {
            FAIL_DIRECTORY_SYNC.set(false);
        }
    }

    #[test]
    fn clean_reservations_skip_only_redundant_directory_barriers() -> Result<()> {
        let root = std::env::temp_dir().join(format!(
            "sprig-clean-reserve-{}-{}",
            std::process::id(),
            SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos()
        ));
        fs::create_dir(&root)?;
        let mut inventory = Inventory::scan(&root)?;
        assert!(inventory.directory_clean);
        assert_eq!(inventory.directory_syncs, 1);
        for i in 0..32 {
            let key = format!("{i:064x}");
            inventory.reserve(&root, &key, 8192)?;
            // No-op reservation did not add a barrier. Publication still does.
            assert_eq!(inventory.directory_syncs, 1 + i);
            fs::write(root.join(key), [0; 4096])?;
            inventory.sync_directory(&root)?;
        }
        assert_eq!(inventory.directory_syncs, 33);
        // A real eviction must fence deletions before capacity is reserved.
        inventory.reserve(&root, &"f".repeat(64), BUDGET)?;
        assert_eq!(inventory.directory_syncs, 34);
        assert_eq!(inventory.charges.len(), 1);
        assert!(inventory.directory_clean);
        fs::remove_dir_all(root)?;
        Ok(())
    }

    #[test]
    fn unsupported_cleanup_requires_fence_before_next_reservation() -> Result<()> {
        let root = std::env::temp_dir().join(format!(
            "sprig-unsupported-fence-{}-{}",
            std::process::id(),
            SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos()
        ));
        fs::create_dir(&root)?;
        let mut inventory = Inventory::scan(&root)?;
        let key = "a".repeat(64);
        inventory.reserve(&root, &key, 8192)?;
        assert_eq!(inventory.directory_syncs, 1);
        let pending = root.join(format!("pending-{key}"));
        fs::write(&pending, b"partial clone")?;
        fs::remove_file(&pending)?;
        inventory.release_after_cleanup(&key)?;
        assert!(!inventory.directory_clean);
        assert_eq!(inventory.total, 0);
        inventory.reserve(&root, &"b".repeat(64), 8192)?;
        assert_eq!(inventory.directory_syncs, 2);
        assert!(inventory.directory_clean);
        fs::remove_dir_all(root)?;
        Ok(())
    }

    #[test]
    fn failed_deletion_fence_preserves_dirty_state_and_blocks_reservation() -> Result<()> {
        let root = std::env::temp_dir().join(format!(
            "sprig-reserve-fence-{}-{}",
            std::process::id(),
            SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos()
        ));
        fs::create_dir(&root)?;
        let key = "a".repeat(64);
        fs::write(root.join(&key), b"old")?;
        let mut inventory = Inventory::scan(&root)?;
        FAIL_DIRECTORY_SYNC.set(true);
        let fault = DirectorySyncFault;
        assert!(inventory.reserve(&root, &"b".repeat(64), BUDGET).is_err());
        assert!(!inventory.directory_clean);
        assert!(!inventory.charges.contains_key(&"b".repeat(64)));
        assert!(!root.join(key).exists());
        drop(fault);
        // Reopening always reestablishes the barrier, even with no entries.
        let mut reopened = Inventory::scan(&root)?;
        assert!(reopened.directory_clean);
        assert_eq!(reopened.directory_syncs, 1);
        reopened.reserve(&root, &"c".repeat(64), 8192)?;
        assert_eq!(reopened.directory_syncs, 1);
        fs::remove_dir_all(root)?;
        Ok(())
    }

    #[test]
    fn corrupt_hit_deletion_fence_failure_disables_fills_but_preserves_cas_output() -> Result<()> {
        for absent in [false, true] {
            let root = std::env::temp_dir().join(format!(
                "sprig-corrupt-fence-{}-{}",
                std::process::id(),
                SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos()
            ));
            fs::create_dir(&root)?;
            let root = root.canonicalize()?;
            let objects = root.join("objects");
            let bytes = vec![b'x'; 65536];
            let EntryKind::File { size, chunks } = content::put_bytes(&bytes, &objects)? else {
                unreachable!()
            };
            let key = cache_key(size, &chunks)?;
            let root_cache = root.join("file-cache");
            fs::create_dir(&root_cache)?;
            fs::write(root_cache.join(&key), b"corrupt")?;
            let session = Session::new(&objects);
            let cache = session.cache(true).unwrap();
            if absent {
                // Model an absent resident: never infer its deletion is durable.
                fs::remove_file(root_cache.join(&key))?;
            }
            let reader = packed::Reader::open(&objects)?;
            FAIL_DIRECTORY_SYNC.set(true);
            let fault = DirectorySyncFault;
            session.materialize(&reader, &chunks, size, &root.join("out"))?;
            assert_eq!(fs::read(root.join("out"))?, bytes);
            assert!(cache.disabled.get());
            assert!(!cache.inventory.borrow().directory_clean);
            assert_eq!(cache.inventory.borrow().directory_syncs, 2);
            assert!(!cache.inventory.borrow().charges.contains_key(&key));
            drop(fault);
            session.materialize(&reader, &chunks, size, &root.join("next"))?;
            assert_eq!(cache.inventory.borrow().directory_syncs, 2);
            drop(session);
            drop(reader);
            fs::remove_dir_all(root)?;
        }
        Ok(())
    }

    #[test]
    fn cache_is_disposable_bounded_verified_and_never_a_mutable_alias() -> Result<()> {
        let root = std::env::temp_dir().join(format!(
            "sprig-cache-{}-{}",
            std::process::id(),
            SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos()
        ));
        fs::create_dir(&root)?;
        let root = root.canonicalize()?;
        let objects = root.join("objects");
        let bytes = vec![b'c'; 65536];
        let EntryKind::File { size, chunks } = content::put_bytes(&bytes, &objects)? else {
            unreachable!()
        };
        let reader = packed::Reader::open(&objects)?;
        let lock = open_cache(&objects)?;
        let cache = lock.root.clone();
        // Contention bypasses cache, without weakening CAS validation.
        let busy = Session::new(&objects);
        busy.materialize(&reader, &chunks, size, &root.join("busy"))?;
        assert_eq!(fs::read(root.join("busy"))?, bytes);
        assert!(matches!(busy.cache.get(), Some(None)));
        drop(lock);
        // A busy session does not keep retrying the lock for later files.
        busy.materialize(&reader, &chunks, size, &root.join("still-bypassed"))?;
        assert!(matches!(busy.cache.get(), Some(None)));
        assert_eq!(fs::read_dir(&cache)?.count(), 1);
        drop(busy);
        let key = blake3::hash(&serde_json::to_vec(&("sprig-file-cache-1", size, &chunks))?)
            .to_hex()
            .to_string();
        fs::write(cache.join(format!("pending-{key}")), b"torn clone")?;
        fs::write(cache.join(&key), b"")?;
        let session = Session::new(&objects);
        session.materialize(&reader, &chunks, size, &root.join("empty-cache"))?;
        assert_eq!(fs::read(root.join("empty-cache"))?, bytes);
        assert!(!cache.join(format!("pending-{key}")).exists());
        if cache.join(&key).exists() {
            #[cfg(unix)]
            platform::set_path_mode(&cache.join(&key), 0o600)?;
        }
        fs::write(cache.join(&key), vec![b'!'; bytes.len()])?;
        session.materialize(&reader, &chunks, size, &root.join("repaired"))?;
        assert_eq!(fs::read(root.join("repaired"))?, bytes);
        // Supply a verified cache entry even on a copy-only test filesystem.
        if !cache.join(&key).exists() {
            fs::write(cache.join(&key), &bytes)?;
        }
        let pending = cache.join(format!("pending-{}", "e".repeat(64)));
        fs::write(&pending, b"disposable incomplete fill")?;
        session.materialize(&reader, &chunks, size, &root.join("hit"))?;
        // A verified hit does not repeat the initial directory scan.
        assert!(pending.exists());
        // The same session keeps its lease until all files are materialized.
        let contended = Session::new(&objects);
        contended.materialize(&reader, &chunks, size, &root.join("contended"))?;
        assert!(matches!(contended.cache.get(), Some(None)));
        drop(contended);
        // materialize normally applies the manifest mode after a cache clone.
        #[cfg(unix)]
        platform::set_path_mode(&root.join("hit"), 0o600)?;
        fs::write(root.join("hit"), b"changed workspace")?;
        assert_eq!(fs::read(cache.join(&key))?, bytes);
        assert_eq!(fs::read(root.join("repaired"))?, bytes);
        let existing = root.join("existing");
        fs::write(&existing, b"must not overwrite")?;
        assert!(
            session
                .materialize(&reader, &chunks, size, &existing)
                .is_err()
        );
        assert_eq!(fs::read(&existing)?, b"must not overwrite");
        // Logical-size reservations are bounded even for sparse/cold entries.
        let big = cache.join("f".repeat(64));
        let file = platform::create_new(&big, 0o600)?;
        file.set_len(BUDGET)?;
        drop(file);
        reserve(&cache, BUDGET)?;
        assert_eq!(fs::read_dir(&cache)?.count(), 1); // permanent lock only
        drop(session);
        // Unrecognized disposable-cache entries disable caching, not checkout.
        fs::write(cache.join("foreign-entry"), b"do not delete")?;
        let invalid = Session::new(&objects);
        invalid.materialize(&reader, &chunks, size, &root.join("bypass"))?;
        assert_eq!(fs::read(root.join("bypass"))?, bytes);
        assert!(cache.join("foreign-entry").exists());
        assert!(matches!(invalid.cache.get(), Some(None)));
        fs::remove_file(cache.join("foreign-entry"))?;
        invalid.materialize(&reader, &chunks, size, &root.join("still-invalid"))?;
        assert!(matches!(invalid.cache.get(), Some(None)));
        assert_eq!(fs::read_dir(&cache)?.count(), 1);
        drop(invalid);
        drop(reader);
        fs::remove_dir_all(root)?;
        Ok(())
    }

    #[test]
    fn lease_inventory_reserves_without_rescanning_and_bounds_eviction() -> Result<()> {
        let root = std::env::temp_dir().join(format!(
            "sprig-inventory-{}-{}",
            std::process::id(),
            SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos()
        ));
        fs::create_dir(&root)?;
        let mut inventory = Inventory::scan(&root)?;
        // A marker added after inventory creation proves reservations do not
        // rescan. It is not an eviction candidate and must remain untouched.
        fs::write(root.join("foreign-marker"), b"keep")?;
        for index in 0..32 {
            let key = format!("{index:064x}");
            inventory.reserve(&root, &key, 8192)?;
            fs::write(root.join(&key), b"cache")?;
        }
        assert_eq!(inventory.charges.len(), 32);
        assert_eq!(inventory.total, 32 * 8192);
        inventory.evict(&root, BUDGET, 0)?;
        assert_eq!(inventory.total, 0);
        assert!(inventory.charges.is_empty());
        assert_eq!(fs::read(root.join("foreign-marker"))?, b"keep");
        assert!(
            inventory
                .reserve(&root, &"e".repeat(64), BUDGET + 1)
                .is_err()
        );
        assert_eq!(inventory.total, 0);
        fs::remove_dir_all(root)?;
        Ok(())
    }

    #[test]
    fn failed_fill_retains_reservation_and_disables_session_without_deleting_evidence() -> Result<()>
    {
        let root = std::env::temp_dir().join(format!(
            "sprig-fill-failure-{}-{}",
            std::process::id(),
            SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos()
        ));
        fs::create_dir(&root)?;
        let root = root.canonicalize()?;
        let objects = root.join("objects");
        let bytes = vec![b'c'; 65536];
        let EntryKind::File { size, chunks } = content::put_bytes(&bytes, &objects)? else {
            unreachable!()
        };
        let reader = packed::Reader::open(&objects)?;
        let session = Session::new(&objects);
        assert!(session.cache.set(Some(open_cache(&objects)?)).is_ok());
        let cache = session.cache.get().unwrap().as_ref().unwrap();
        let key = blake3::hash(&serde_json::to_vec(&("sprig-file-cache-1", size, &chunks))?)
            .to_hex()
            .to_string();
        let pending = cache.root.join(format!("pending-{key}"));
        fs::create_dir(&pending)?;
        fs::write(pending.join("evidence"), b"keep")?;
        // NTFS may decline cloning before inspecting the destination. Inject a
        // real fill error after reservation deterministically on every platform,
        // rather than treating unsupported cloning as an IO failure.
        FAIL_FILL.set(true);
        let fault = FillFault;
        session.materialize(&reader, &chunks, size, &root.join("first"))?;
        drop(fault);
        assert_eq!(fs::read(root.join("first"))?, bytes);
        assert!(cache.disabled.get());
        assert!(cache.inventory.borrow().total >= size + 4096);
        session.materialize(&reader, &chunks, size, &root.join("second"))?;
        assert_eq!(fs::read(root.join("second"))?, bytes);
        assert_eq!(cache.inventory.borrow().charges.len(), 1);
        assert_eq!(fs::read(pending.join("evidence"))?, b"keep");
        drop(session);
        let next = Session::new(&objects);
        next.materialize(&reader, &chunks, size, &root.join("third"))?;
        assert!(matches!(next.cache.get(), Some(None)));
        assert_eq!(fs::read(pending.join("evidence"))?, b"keep");
        drop(next);
        drop(reader);
        fs::remove_dir_all(root)?;
        Ok(())
    }

    #[test]
    fn small_cached_files_verify_contents_and_keep_independent_writable_inodes() -> Result<()> {
        let root = std::env::temp_dir().join(format!(
            "sprig-small-cache-{}-{}",
            std::process::id(),
            SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos()
        ));
        fs::create_dir(&root)?;
        let root = root.canonicalize()?;
        let objects = root.join("objects");
        // Non-monotonic sizes avoid relying on traversal or fixture ordering.
        for size in [49152usize, 1024, 8192] {
            let bytes = vec![(size / 1024) as u8; size];
            let EntryKind::File { size, chunks } = content::put_bytes(&bytes, &objects)? else {
                unreachable!()
            };
            let reader = packed::Reader::open(&objects)?;
            let session = Session::new(&objects);
            let first = root.join(format!("first-{size}"));
            session.materialize(&reader, &chunks, size, &first)?;
            assert_eq!(fs::read(&first)?, bytes);
            let cache = session.cache.get().unwrap().as_ref().unwrap();
            let key = blake3::hash(&serde_json::to_vec(&("sprig-file-cache-1", size, &chunks))?)
                .to_hex()
                .to_string();
            let cached = cache.root.join(&key);
            // Exercise verified hits on copy-only filesystems too; whether the
            // platform actually shared physical blocks is measured separately.
            if !cached.exists() {
                fs::write(&cached, &bytes)?;
            }
            let second = root.join(format!("second-{size}"));
            session.materialize(&reader, &chunks, size, &second)?;
            assert_eq!(fs::read(&second)?, bytes);
            // The public materializer reapplies manifest permissions after a
            // clone; APFS clonefile also copies the cache inode's read-only mode.
            #[cfg(unix)]
            platform::set_path_mode(&second, 0o600)?;
            fs::write(&second, b"changed workspace")?;
            assert_eq!(fs::read(&cached)?, bytes);
            assert_eq!(fs::read(&first)?, bytes);
            #[cfg(unix)]
            platform::set_path_mode(&cached, 0o600)?;
            fs::write(&cached, vec![b'!'; bytes.len()])?;
            let repaired = root.join(format!("repaired-{size}"));
            session.materialize(&reader, &chunks, size, &repaired)?;
            assert_eq!(fs::read(repaired)?, bytes);
            drop(session);
            drop(reader);
        }
        fs::remove_dir_all(root)?;
        Ok(())
    }

    #[test]
    fn cache_inventory_entry_limit_is_independent_of_small_file_traversal() -> Result<()> {
        let root = std::env::temp_dir().join(format!(
            "sprig-cache-count-{}-{}",
            std::process::id(),
            SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos()
        ));
        fs::create_dir(&root)?;
        for index in (0..4100).rev() {
            fs::write(root.join(format!("{index:064x}")), b"")?;
        }
        let mut inventory = Inventory::scan(&root)?;
        assert_eq!(inventory.charges.len(), 4096);
        assert_eq!(fs::read_dir(&root)?.count(), 4096);
        assert_eq!(inventory.total, 4096 * 4096);
        let key = "f".repeat(64);
        inventory.reserve(&root, &key, 8192)?;
        fs::write(root.join(&key), b"cache")?;
        assert_eq!(inventory.charges.len(), 4096);
        assert_eq!(fs::read_dir(&root)?.count(), 4096);
        assert!(inventory.total <= BUDGET);
        fs::remove_dir_all(root)?;
        Ok(())
    }

    fn planned_manifest(count: usize, size: u64, distinct: bool) -> crate::content::Manifest {
        (0..count)
            .map(|index| {
                (
                    format!("file-{index:06}"),
                    content::Entry {
                        id: format!("id-{index}"),
                        link_group: None,
                        mode: 0o600,
                        modified_ns: 0,
                        kind: EntryKind::File {
                            size,
                            chunks: vec![format!("{:064x}", if distinct { index } else { 0 })],
                        },
                    },
                )
            })
            .collect()
    }

    #[test]
    fn complete_working_set_admission_has_capacity_boundaries_and_content_dedup() -> Result<()> {
        assert_eq!(
            working_set(&planned_manifest(ENTRY_BUDGET, 1024, true))?
                .unwrap()
                .len(),
            ENTRY_BUDGET
        );
        assert!(working_set(&planned_manifest(ENTRY_BUDGET + 1, 1024, true))?.is_none());
        assert!(working_set(&planned_manifest(ENTRY_BUDGET, LARGE_FILE_MIN, true))?.is_none());
        // Exactly one logical-charge budget fits; an additional distinct key does not.
        assert!(working_set(&planned_manifest(1, BUDGET - 4096, true))?.is_some());
        let mut over_bytes = planned_manifest(1, BUDGET - 4096, true);
        over_bytes.extend(planned_manifest(2, 1024, true).into_iter().skip(1));
        assert!(working_set(&over_bytes)?.is_none());
        let mut duplicate = planned_manifest(10_000, 8192, false);
        assert_eq!(working_set(&duplicate)?.unwrap().len(), 1);
        for entry in duplicate.values_mut() {
            entry.link_group = Some("same-inode".into());
        }
        assert_eq!(working_set(&duplicate)?.unwrap().len(), 1);
        // Relabel paths to reverse the value traversal: admission depends on the
        // complete set, never first-N paths or their lexical order.
        let ordered = planned_manifest(ENTRY_BUDGET + 1, 1024, true);
        let reversed: crate::content::Manifest = ordered
            .into_values()
            .rev()
            .enumerate()
            .map(|(i, entry)| (format!("reverse-{i:06}"), entry))
            .collect();
        assert!(working_set(&reversed)?.is_none());
        Ok(())
    }

    #[test]
    fn protected_admission_evicts_only_outside_working_set_and_declines_physical_overflow()
    -> Result<()> {
        let root = std::env::temp_dir().join(format!(
            "sprig-protected-cache-{}-{}",
            std::process::id(),
            SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos()
        ));
        fs::create_dir(&root)?;
        let mut inventory = Inventory::default();
        let current = "a".repeat(64);
        let old = "b".repeat(64);
        let incoming = "c".repeat(64);
        inventory.reserve(&root, &current, BUDGET / 2)?;
        fs::write(root.join(&current), b"current")?;
        inventory.reserve(&root, &old, BUDGET / 2)?;
        fs::write(root.join(&old), b"old")?;
        let protected = BTreeSet::from([current.clone(), incoming.clone()]);
        inventory.protect(protected)?;
        assert!(inventory.reserve_protected(&root, &incoming, 8192)?);
        fs::write(root.join(&incoming), b"incoming")?;
        assert!(!root.join(&old).exists());
        assert_eq!(fs::read(root.join(&current))?, b"current");
        let before = inventory.charges.clone();
        // Charges simulate a filesystem with much greater actual allocation
        // than the planner's logical lower bound. No current key may be pruned.
        assert!(!inventory.reserve_protected(&root, &"d".repeat(64), BUDGET)?);
        assert_eq!(inventory.charges, before);
        assert_eq!(fs::read(root.join(&incoming))?, b"incoming");
        fs::remove_dir_all(root)?;
        Ok(())
    }

    #[test]
    fn near_capacity_protection_counters_and_victim_index_are_incremental() -> Result<()> {
        let root = std::env::temp_dir().join(format!(
            "sprig-cache-index-{}-{}",
            std::process::id(),
            SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos()
        ));
        fs::create_dir(&root)?;
        let key = |index: usize| format!("{index:064x}");
        let half = ENTRY_BUDGET / 2;
        let mut inventory = Inventory::default();
        // Half are current protected keys, half obsolete. All have the same
        // small physical-accounting charge, so entry pressure is the boundary.
        for index in 0..ENTRY_BUDGET {
            inventory.insert_charge(&key(index), 8192)?;
            fs::write(root.join(key(index)), b"")?;
        }
        let protected: BTreeSet<_> = (0..half)
            .chain(ENTRY_BUDGET..ENTRY_BUDGET + half)
            .map(key)
            .collect();
        inventory.protect(protected)?;
        assert_eq!(inventory.protection_scanned_entries, ENTRY_BUDGET);
        for index in 0..half {
            let incoming = key(ENTRY_BUDGET + index);
            assert!(inventory.reserve_protected(&root, &incoming, 8192)?);
            fs::write(root.join(&incoming), b"")?;
            let protection = inventory.protection.as_ref().unwrap();
            assert_eq!(protection.entries, half + index + 1);
            assert_eq!(protection.bytes, (half + index + 1) as u64 * 8192);
            assert_eq!(protection.victims.len(), half - index - 1);
            assert_eq!(inventory.total, ENTRY_BUDGET as u64 * 8192);
            assert_eq!(inventory.protection_scanned_entries, ENTRY_BUDGET);
            assert_eq!(inventory.protected_victim_queries, index + 1);
            assert_eq!(inventory.directory_syncs, index + 1);
        }
        // Large policy is unchanged: it may evict the first protected key, but
        // both incremental counters and the unprotected-victim index must track it.
        let large = "f".repeat(64);
        inventory.reserve(&root, &large, 8192)?;
        fs::write(root.join(&large), b"")?;
        assert!(!root.join(key(0)).exists());
        let protection = inventory.protection.as_ref().unwrap();
        assert_eq!(protection.entries, ENTRY_BUDGET - 1);
        assert_eq!(protection.victims, BTreeSet::from([large.clone()]));
        fs::remove_file(root.join(&large))?;
        inventory.forget(&large)?;
        assert!(inventory.protection.as_ref().unwrap().victims.is_empty());
        assert!(inventory.reserve_protected(&root, &key(0), 8192)?);
        // Model a reserved fill that fails before publication: accounting keeps
        // the reservation until the session is disabled/dropped or a confirmed
        // unsupported clone explicitly forgets it.
        assert_eq!(inventory.protection.as_ref().unwrap().entries, ENTRY_BUDGET);
        inventory.forget(&key(0))?;
        assert_eq!(
            inventory.protection.as_ref().unwrap().entries,
            ENTRY_BUDGET - 1
        );
        assert_eq!(inventory.protection_scanned_entries, ENTRY_BUDGET);
        assert_eq!(inventory.protected_victim_queries, half);
        let protection = inventory.protection.as_ref().unwrap();
        assert_eq!(
            protection.bytes,
            inventory
                .charges
                .iter()
                .filter(|(k, _)| protection.keys.contains(*k))
                .map(|(_, charge)| charge)
                .sum::<u64>()
        );
        assert_eq!(inventory.total, inventory.charges.values().sum::<u64>());
        fs::remove_dir_all(root)?;
        Ok(())
    }

    #[test]
    fn oversized_unique_10k_plan_has_zero_small_probes_fills_and_keeps_large_policy() -> Result<()>
    {
        let root = std::env::temp_dir().join(format!(
            "sprig-admission-10k-{}-{}",
            std::process::id(),
            SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos()
        ));
        fs::create_dir(&root)?;
        let root = root.canonicalize()?;
        let objects = root.join("objects");
        let outputs = root.join("outputs");
        fs::create_dir(&outputs)?;
        let mut batch = content::ObjectBatch::open(&objects)?;
        let mut manifest = crate::content::Manifest::new();
        for index in 0..10_000u64 {
            let mut bytes = vec![0u8; 1024];
            bytes[..8].copy_from_slice(&index.to_le_bytes());
            let kind = batch.put_reader(&mut std::io::Cursor::new(bytes))?;
            manifest.insert(
                format!("file-{index:06}"),
                content::Entry {
                    id: format!("id-{index}"),
                    link_group: None,
                    mode: 0o600,
                    modified_ns: 0,
                    kind,
                },
            );
        }
        let large_bytes = vec![b'L'; LARGE_FILE_MIN as usize];
        let large = batch.put_reader(&mut std::io::Cursor::new(&large_bytes))?;
        manifest.insert(
            "large".into(),
            content::Entry {
                id: "large-id".into(),
                link_group: None,
                mode: 0o600,
                modified_ns: 0,
                kind: large.clone(),
            },
        );
        batch.finish()?;
        let reader = packed::Reader::open(&objects)?;
        let session = Session::for_manifest(&objects, &manifest)?;
        assert!(!session.small_admission.get());
        for (index, entry) in manifest.values().take(10_000).enumerate() {
            let EntryKind::File { size, chunks } = &entry.kind else {
                unreachable!()
            };
            let target = outputs.join(format!("{index}"));
            session.materialize(&reader, chunks, *size, &target)?;
            let bytes = fs::read(target)?;
            assert_eq!(bytes.len(), 1024);
            assert_eq!(&bytes[..8], &(index as u64).to_le_bytes());
        }
        assert!(session.no_existing_cache.get());
        assert!(session.cache.get().is_none());
        assert!(!root.join("file-cache").exists());
        assert_eq!(session.counters.probes.get(), 0);
        assert_eq!(session.counters.fills.get(), 0);
        let EntryKind::File { size, chunks } = large else {
            unreachable!()
        };
        session.materialize(&reader, &chunks, size, &outputs.join("large"))?;
        assert_eq!(fs::read(outputs.join("large"))?, large_bytes);
        assert!(session.cache.get().unwrap().is_some());
        assert_eq!(session.counters.probes.get(), 1);
        assert_eq!(session.counters.fills.get(), 1);
        drop(session);
        drop(reader);
        fs::remove_dir_all(root)?;
        Ok(())
    }

    #[test]
    fn actual_charge_decline_closes_small_population_but_preserves_verified_hits() -> Result<()> {
        let root = std::env::temp_dir().join(format!(
            "sprig-charge-decline-{}-{}",
            std::process::id(),
            SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos()
        ));
        fs::create_dir(&root)?;
        let root = root.canonicalize()?;
        let objects = root.join("objects");
        let present_bytes = vec![b'p'; 1024];
        let missing_bytes = vec![b'm'; 1024];
        let present = content::put_bytes(&present_bytes, &objects)?;
        let missing = content::put_bytes(&missing_bytes, &objects)?;
        let manifest: crate::content::Manifest =
            [("present", present.clone()), ("missing", missing.clone())]
                .into_iter()
                .map(|(name, kind)| {
                    (
                        name.into(),
                        content::Entry {
                            id: name.into(),
                            link_group: None,
                            mode: 0o600,
                            modified_ns: 0,
                            kind,
                        },
                    )
                })
                .collect();
        let EntryKind::File {
            size,
            chunks: present_chunks,
        } = present
        else {
            unreachable!()
        };
        let EntryKind::File {
            chunks: missing_chunks,
            ..
        } = missing
        else {
            unreachable!()
        };
        let key = cache_key(size, &present_chunks)?;
        let cache = open_cache(&objects)?;
        fs::write(cache.root.join(&key), &present_bytes)?;
        drop(cache);
        let reader = packed::Reader::open(&objects)?;
        let session = Session::for_manifest(&objects, &manifest)?;
        assert!(session.cache.set(Some(open_cache(&objects)?)).is_ok());
        let cache = session.cache.get().unwrap().as_ref().unwrap();
        // Deterministically model an unexpectedly large allocated charge without
        // requiring a special filesystem or a real 256 MiB test allocation.
        cache
            .inventory
            .borrow_mut()
            .charges
            .insert(key.clone(), BUDGET - 4096);
        cache.inventory.borrow_mut().total = BUDGET - 4096;
        cache
            .inventory
            .borrow_mut()
            .protect(session.working_set.clone().unwrap())?;
        session.materialize(&reader, &missing_chunks, size, &root.join("missing-one"))?;
        assert_eq!(fs::read(root.join("missing-one"))?, missing_bytes);
        assert!(!session.small_admission.get());
        assert!(!cache.disabled.get());
        assert_eq!(session.counters.admission_declines.get(), 1);
        assert_eq!(session.counters.fills.get(), 1);
        session.materialize(&reader, &missing_chunks, size, &root.join("missing-two"))?;
        assert_eq!(session.counters.fills.get(), 1);
        session.materialize(&reader, &present_chunks, size, &root.join("present"))?;
        assert_eq!(fs::read(root.join("present"))?, present_bytes);
        assert_eq!(session.counters.hits.get(), 1);
        assert_eq!(fs::read(cache.root.join(&key))?, present_bytes);
        assert_eq!(cache.inventory.borrow().charges.len(), 1);
        drop(session);
        drop(reader);
        fs::remove_dir_all(root)?;
        Ok(())
    }

    #[test]
    fn closed_admission_preserves_verified_hits_and_corruption_fallback_without_refill()
    -> Result<()> {
        let root = std::env::temp_dir().join(format!(
            "sprig-admission-hit-{}-{}",
            std::process::id(),
            SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos()
        ));
        fs::create_dir(&root)?;
        let root = root.canonicalize()?;
        let objects = root.join("objects");
        let bytes = vec![b'h'; 1024];
        let EntryKind::File { size, chunks } = content::put_bytes(&bytes, &objects)? else {
            unreachable!()
        };
        let reader = packed::Reader::open(&objects)?;
        let key = cache_key(size, &chunks)?;
        let cache = open_cache(&objects)?;
        fs::write(cache.root.join(&key), &bytes)?;
        drop(cache);
        let session = Session::for_manifest(&objects, &planned_manifest(5000, 1024, true))?;
        let target = root.join("hit");
        session.materialize(&reader, &chunks, size, &target)?;
        assert_eq!(fs::read(&target)?, bytes);
        assert_eq!(session.counters.hits.get(), 1);
        assert_eq!(session.counters.fills.get(), 0);
        let cached_path = root.join("file-cache").join(&key);
        #[cfg(unix)]
        platform::set_path_mode(&target, 0o600)?;
        fs::write(&target, b"private changed data")?;
        assert_eq!(fs::read(&cached_path)?, bytes);
        fs::write(&cached_path, vec![b'!'; 1024])?;
        session.materialize(&reader, &chunks, size, &root.join("repaired"))?;
        assert_eq!(fs::read(root.join("repaired"))?, bytes);
        assert!(!cached_path.exists());
        assert_eq!(session.counters.fills.get(), 0);
        assert_eq!(session.counters.hits.get(), 1);
        drop(session);
        drop(reader);
        fs::remove_dir_all(root)?;
        Ok(())
    }

    #[test]
    fn ineligible_files_never_initialize_the_cache() -> Result<()> {
        let root = std::env::temp_dir().join(format!(
            "sprig-cache-ineligible-{}-{}",
            std::process::id(),
            SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos()
        ));
        fs::create_dir(&root)?;
        let root = root.canonicalize()?;
        let objects = root.join("objects");
        let bytes = b"small native file";
        let EntryKind::File { size, chunks } = content::put_bytes(bytes, &objects)? else {
            unreachable!()
        };
        let reader = packed::Reader::open(&objects)?;
        let session = Session::new(&objects);
        for index in 0..3 {
            let target = root.join(format!("small-{index}"));
            session.materialize(&reader, &chunks, size, &target)?;
            assert_eq!(fs::read(target)?, bytes);
        }
        session.materialize(&reader, &[], 0, &root.join("empty"))?;
        // A declared oversize file also bypasses the cache, then still fails
        // the authoritative whole-file length check without allocating it.
        assert!(
            session
                .materialize(&reader, &chunks, BUDGET, &root.join("oversize"))
                .is_err()
        );
        assert!(session.cache.get().is_none());
        assert!(!root.join("file-cache").exists());
        assert!(!cache_eligible(1023));
        assert!(cache_eligible(1024));
        assert!(cache_eligible(64 * 1024 - 1));
        assert!(cache_eligible(64 * 1024));
        assert!(cache_eligible(BUDGET - 4096));
        assert!(!cache_eligible(BUDGET - 4095));
        // Exercise the native-tree caller too: a small-file checkout must not
        // eagerly open a cache merely because it creates a session.
        let source = root.join("source");
        fs::create_dir(&source)?;
        fs::write(source.join("small"), bytes)?;
        let (manifest, _) = content::capture(&source, &objects, &Default::default())?;
        let tree = root.join("tree");
        content::materialize(&manifest, &objects, &tree)?;
        assert_eq!(fs::read(tree.join("small"))?, bytes);
        assert!(!root.join("file-cache").exists());
        drop(session);
        drop(reader);
        fs::remove_dir_all(root)?;
        Ok(())
    }
}
