//! Disposable, bounded whole-file bytes for native CoW materialization.
//! CAS is authoritative; cache hits verify every requested chunk before cloning.
//! Mutable workspaces never hard-link to the cache. Cache failures before target
//! creation fall back to CAS, not to unchecked cached bytes.

use crate::{Result, packed, platform};
use std::cell::OnceCell;
use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

const BUDGET: u64 = 256 * 1024 * 1024;

/// One optional cache lease for a fresh native tree. Contention or an invalid
/// cache disables caching for this session, never the authoritative CAS reads.
/// Ineligible files do not create, inspect or lock the cache at all.
pub(crate) struct Session<'a> {
    objects: &'a Path,
    cache: OnceCell<Option<(PathBuf, File)>>,
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
        if let Some((root, _lock)) = cache {
            match cached(reader, chunks, size, root, &key, target) {
                Ok(Some(output)) => return Ok(output),
                Ok(None) => {}
                // A clone can fail after creating its destination. Never
                // overwrite an uncertain partial target or claim success.
                Err(error) if target.try_exists()? => return Err(error),
                Err(_) => {}
            }
        }
        let output = materialize_cas(reader, chunks, size, target)?;
        if let Some((root, _lock)) = cache {
            // Cache is optional. A failed fill/flush leaves only a disposable
            // entry, which the next use verifies; durable CAS is unchanged.
            let _ = populate(root, &key, size, target);
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

fn open_cache(objects: &Path) -> Result<(PathBuf, File)> {
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
    reserve(&root, 0)?;
    Ok((root, lock))
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

fn populate(root: &Path, key: &str, size: u64, target: &Path) -> Result<()> {
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
    reserve(root, charge)?;
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

fn reserve(root: &Path, additional: u64) -> Result<()> {
    let mut files = Vec::new();
    let mut total = additional;
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
        total = total.checked_add(charge).ok_or("cache size overflow")?;
        files.push((path, charge));
    }
    files.sort_by(|a, b| a.0.cmp(&b.0));
    let mut count = files.len();
    for (path, size) in files {
        if total <= BUDGET && count < 4096 {
            break;
        }
        fs::remove_file(path)?;
        total -= size;
        count -= 1;
    }
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
        let (cache, lock) = open_cache(&objects)?;
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
