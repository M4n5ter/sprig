//! Bounded workspace barriers. Every file finishes before bottom-up directories.
//! This does not infer a write set: unchanged files are flushed too.

use std::fs;
use std::io;
#[cfg(unix)]
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use crate::platform::{self, Metadata};

struct Entry {
    path: PathBuf,
    metadata: Metadata,
}

pub(crate) fn sync(root: &Path, allow_whiteouts: bool) -> io::Result<()> {
    let mut files = Vec::new();
    let mut directories = Vec::<Vec<Entry>>::new();
    let mut opaque = Vec::new();
    collect(
        root,
        0,
        allow_whiteouts,
        &mut files,
        &mut directories,
        &mut opaque,
    )?;
    let parallel = files.len() >= 16 || directories.iter().any(|level| level.len() >= 16);
    if parallel {
        flush_batch(&files)?;
        for level in directories.iter().rev() {
            flush_batch(level)?;
        }
    } else {
        // Preserve the original platform checks for trees too small to benefit
        // from workers; do not add descriptor-stat overhead to this path.
        for entry in &files {
            platform::sync_file(&entry.path)?;
        }
        for level in directories.iter().rev() {
            for entry in level {
                platform::sync_directory(&entry.path)?;
            }
        }
    }
    // Symlinks and whiteouts are namespace entries, not devices to open.
    for entry in opaque {
        verify(&entry.metadata, &platform::symlink_metadata(&entry.path)?)?;
    }
    Ok(())
}

fn collect(
    path: &Path,
    depth: usize,
    allow_whiteouts: bool,
    files: &mut Vec<Entry>,
    directories: &mut Vec<Vec<Entry>>,
    opaque: &mut Vec<Entry>,
) -> io::Result<()> {
    let metadata = platform::symlink_metadata(path)?;
    if metadata.is_dir() {
        if directories.len() <= depth {
            directories.push(Vec::new());
        }
        for child in fs::read_dir(path)? {
            collect(
                &child?.path(),
                depth + 1,
                allow_whiteouts,
                files,
                directories,
                opaque,
            )?;
        }
        directories[depth].push(Entry {
            path: path.to_owned(),
            metadata,
        });
    } else if metadata.is_file() {
        files.push(Entry {
            path: path.to_owned(),
            metadata,
        });
    } else if metadata.file_type().is_symlink() || allow_whiteouts && is_whiteout(&metadata) {
        opaque.push(Entry {
            path: path.to_owned(),
            metadata,
        });
    } else {
        return Err(io::Error::other(
            "unsupported special file while syncing workspace",
        ));
    }
    Ok(())
}

fn is_whiteout(metadata: &Metadata) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::FileTypeExt;
        metadata.file_type().is_char_device() && metadata.rdev() == 0
    }
    #[cfg(windows)]
    {
        let _ = metadata;
        false
    }
}

fn verify(before: &Metadata, after: &Metadata) -> io::Result<()> {
    if before.dev() != after.dev()
        || before.ino() != after.ino()
        || before.mode() != after.mode()
        || before.len() != after.len()
        || before.mtime() != after.mtime()
        || before.mtime_nsec() != after.mtime_nsec()
        || before.ctime() != after.ctime()
        || before.ctime_nsec() != after.ctime_nsec()
    {
        return Err(io::Error::other("workspace changed during synchronization"));
    }
    Ok(())
}

fn flush(entry: &Entry) -> io::Result<()> {
    let file = if entry.metadata.is_dir() {
        platform::open_directory(&entry.path)?
    } else {
        #[cfg(unix)]
        {
            platform::open_regular(&entry.path)?
        }
        #[cfg(windows)]
        {
            platform::open_read_write(&entry.path)?
        }
    };
    verify(&entry.metadata, &platform::file_metadata(&file)?)?;
    #[cfg(test)]
    if FAIL_ROOT
        .lock()
        .unwrap()
        .as_ref()
        .is_some_and(|root| entry.path.starts_with(root))
    {
        return Err(io::Error::from_raw_os_error(libc::EIO));
    }
    if entry.metadata.is_dir() {
        platform::sync_directory_file(&file)?;
    } else {
        file.sync_all()?;
    }
    verify(&entry.metadata, &platform::file_metadata(&file)?)
}

fn flush_batch(entries: &[Entry]) -> io::Result<()> {
    // Tiny trees avoid thread startup. At most four workers exist at a time;
    // joining a level is a barrier before any parent directory is synchronized.
    if entries.len() < 16 {
        return entries.iter().try_for_each(flush);
    }
    std::thread::scope(|scope| {
        let mut workers = Vec::new();
        let mut errors = Vec::new();
        for worker in 0..4 {
            match std::thread::Builder::new().spawn_scoped(scope, move || {
                entries
                    .iter()
                    .skip(worker)
                    .step_by(4)
                    .try_for_each(|entry| {
                        flush(entry).map_err(|error| {
                            io::Error::other(format!("{}: {error}", entry.path.display()))
                        })
                    })
            }) {
                Ok(handle) => workers.push(handle),
                Err(error) => {
                    errors.push(error.to_string());
                    break;
                }
            }
        }
        // Join even after a failure: no background flush can outlive the lease.
        for worker in workers {
            let outcome = worker
                .join()
                .unwrap_or_else(|_| Err(io::Error::other("workspace sync worker panicked")));
            if let Err(error) = outcome {
                errors.push(error.to_string());
            }
        }
        if errors.is_empty() {
            Ok(())
        } else {
            Err(io::Error::other(errors.join("; ")))
        }
    })
}

#[cfg(test)]
static FAIL_ROOT: std::sync::Mutex<Option<PathBuf>> = std::sync::Mutex::new(None);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{CopyMode, Store};
    use std::time::{SystemTime, UNIX_EPOCH};

    struct Fixture(PathBuf);
    impl Drop for Fixture {
        fn drop(&mut self) {
            *FAIL_ROOT.lock().unwrap() = None;
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn parallel_barrier_rejects_replacement_and_never_publishes_failed_flush() -> crate::Result<()>
    {
        let fixture = Fixture(std::env::temp_dir().join(format!(
            "sprig-sync-{}-{}",
            std::process::id(),
            SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos()
        )));
        fs::create_dir(&fixture.0)?;
        let source = fixture.0.join("source");
        fs::create_dir(&source)?;
        for index in 0..16 {
            fs::write(source.join(format!("file-{index}")), b"initial")?;
        }
        let mut files = Vec::new();
        collect(
            &source,
            0,
            false,
            &mut files,
            &mut Vec::new(),
            &mut Vec::new(),
        )?;
        for entry in &files[..2] {
            let replacement = source.join("replacement");
            fs::write(&replacement, b"initial")?;
            fs::File::options()
                .write(true)
                .open(&replacement)?
                .set_times(fs::FileTimes::new().set_modified(entry.metadata.modified()?))?;
            fs::rename(replacement, &entry.path)?;
        }
        let error = flush_batch(&files).unwrap_err().to_string();
        assert!(error.contains(files[0].path.to_str().unwrap()));
        assert!(error.contains(files[1].path.to_str().unwrap()));
        let store_path = fixture.0.join("store");
        let store = Store::init(&store_path)?;
        let workspace = store.import(&source, "main", CopyMode::Copy)?;
        let tree = PathBuf::from(&workspace.path);
        fs::write(tree.join("file-0"), b"changed")?;
        let before = fs::read(store_path.join("state.json"))?;
        *FAIL_ROOT.lock().unwrap() = Some(tree);
        let error = store
            .checkpoint("main", "must not publish", None)
            .unwrap_err()
            .to_string();
        *FAIL_ROOT.lock().unwrap() = None;
        assert_eq!(error.matches("os error 5").count(), 4);
        assert_eq!(fs::read(store_path.join("state.json"))?, before);
        assert!(
            store
                .checkpoint("main", "after injected failure", None)?
                .committed
        );
        assert_ne!(fs::read(store_path.join("state.json"))?, before);
        assert_eq!(
            fs::read(PathBuf::from(&workspace.path).join("file-0"))?,
            b"changed"
        );
        let tiny = fixture.0.join("tiny");
        fs::create_dir(&tiny)?;
        fs::write(tiny.join("one"), b"one")?;
        sync(&tiny, false)?;
        Ok(())
    }
}
