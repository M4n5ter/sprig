//! Disposable, bounded whole-file bytes for native CoW materialization.
//! CAS is authoritative; cache hits verify every requested chunk before cloning.
//! Mutable workspaces never hard-link to the cache. Cache failures before target
//! creation fall back to CAS, not to unchecked cached bytes.

use crate::{Result, packed, platform};
use std::cell::{Cell, OnceCell, RefCell};
use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

const BUDGET: u64 = 256 * 1024 * 1024;

/// One optional cache lease for a fresh native tree. Contention or an invalid
/// cache disables caching for this session, never the authoritative CAS reads.
/// Ineligible files do not create, inspect or lock the cache at all.
pub(crate) struct Session<'a> {
    objects: &'a Path,
    cache: OnceCell<Option<Cache>>,
}

impl<'a> Session<'a> {
    pub(crate) fn new(objects: &'a Path) -> Self {
        Self {
            objects,
            cache: OnceCell::new(),
        }
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
        let key = blake3::hash(&serde_json::to_vec(&("sprig-file-cache-1", size, chunks))?)
            .to_hex()
            .to_string();
        // Try once, lazily, and retain the same lock inode through the session.
        // The initial scan is shared; every fill still reserves its own budget.
        let cache = self.cache.get_or_init(|| open_cache(self.objects).ok());
        if let Some(cache) = cache.as_ref().filter(|cache| !cache.disabled.get()) {
            match cached(reader, chunks, size, &cache.root, &key, target) {
                Ok(Some(output)) => return Ok(output),
                Ok(None) => cache.inventory.borrow_mut().forget(&key),
                // A clone can fail after creating its destination. Never
                // overwrite an uncertain partial target or claim success.
                Err(error) if target.try_exists()? => return Err(error),
                Err(_) => cache.disabled.set(true),
            }
        }
        let output = materialize_cas(reader, chunks, size, target)?;
        if let Some(cache) = cache.as_ref().filter(|cache| !cache.disabled.get()) {
            // A failed fill can leave a pending entry. Keep its reservation and
            // disable this session; the next lock owner inventories it afresh.
            if populate(cache, &key, size, target).is_err() {
                cache.disabled.set(true);
            }
        }
        Ok(output)
    }
}

fn cache_eligible(size: u64) -> bool {
    (64 * 1024..=BUDGET - 4096).contains(&size)
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
    root: &Path,
    key: &str,
    target: &Path,
) -> Result<Option<File>> {
    let path = root.join(key);
    let mut input = match platform::open_regular(&path) {
        Ok(input) => input,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    if verify(&mut input, reader, chunks, size).is_err() {
        drop(input);
        // This is explicitly disposable derived content, never an object/ref.
        fs::remove_file(path)?;
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

fn populate(cache: &Cache, key: &str, size: u64, target: &Path) -> Result<()> {
    let root = &cache.root;
    if !cache_eligible(size) {
        return Ok(());
    }
    // Reserve before cloning: even a crash before completion cannot leave more
    // than the logical budget. Native allocation accounting remains separate.
    let metadata = platform::symlink_metadata(target)?;
    let charge = size
        .max(platform::allocated_bytes(target, &metadata)?)
        .checked_add(4096)
        .ok_or("cache size overflow")?;
    cache.inventory.borrow_mut().reserve(root, key, charge)?;
    let path = root.join(key);
    let input = platform::open_regular(target)?;
    let pending = root.join(format!("pending-{key}"));
    if let Some(cached) = crate::copy_tree::native_clone(&input, target, &pending)? {
        #[cfg(unix)]
        platform::set_mode(&cached, 0o444)?;
        cached.sync_all()?;
        drop(cached);
        platform::rename(&pending, &path)?;
        platform::sync_directory(root)?;
    } else {
        // Unsupported cloning removed its temporary target and wrote no cache.
        cache.inventory.borrow_mut().forget(key);
    }
    Ok(())
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
        platform::sync_directory(root)?;
        Ok(result)
    }

    fn forget(&mut self, key: &str) {
        if let Some(charge) = self.charges.remove(key) {
            self.total -= charge;
        }
    }

    fn evict(&mut self, root: &Path, additional: u64, additional_entries: usize) -> Result<()> {
        while self
            .total
            .checked_add(additional)
            .ok_or("cache size overflow")?
            > BUDGET
            || self.charges.len() + additional_entries > 4096
        {
            let (key, _) = self
                .charges
                .first_key_value()
                .ok_or("cache reservation exceeds budget")?;
            let key = key.clone();
            // No cached() input handle is alive here (important on Windows).
            // Only update accounting after a successful deletion.
            fs::remove_file(root.join(&key))?;
            self.forget(&key);
        }
        Ok(())
    }

    fn reserve(&mut self, root: &Path, key: &str, charge: u64) -> Result<()> {
        if self.charges.contains_key(key) {
            return Err("cache reservation already exists".into());
        }
        self.evict(root, charge, 1)?;
        // Preserve the existing eviction-before-publication durability boundary.
        platform::sync_directory(root)?;
        self.total = self
            .total
            .checked_add(charge)
            .ok_or("cache size overflow")?;
        self.charges.insert(key.to_owned(), charge);
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
        session.materialize(&reader, &chunks, size, &root.join("first"))?;
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
        assert!(!cache_eligible(64 * 1024 - 1));
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
