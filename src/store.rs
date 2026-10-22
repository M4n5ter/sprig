//! A directory is the transaction: publish a fully written entry with one rename.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::copy_tree;
use crate::copy_tree::CopyStats;

const FORMAT: &str = "sprig-store-v1\n";
const AREAS: [&str; 4] = ["workspaces", "snapshots", "trash", "staging"];

pub(crate) struct Store {
    root: PathBuf,
    _lock: File,
}

impl Store {
    pub(crate) fn init(path: &Path) -> io::Result<()> {
        // Never adopt an existing directory, including an empty one or symlink.
        fs::DirBuilder::new().mode(0o700).create(path)?;
        let result = (|| {
            for area in AREAS {
                fs::create_dir(path.join(area))?;
                File::open(path.join(area))?.sync_all()?;
            }
            let mut marker = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(path.join("format"))?;
            marker.write_all(FORMAT.as_bytes())?;
            marker.sync_all()?;
            File::create(path.join("lock"))?.sync_all()?;
            File::open(path)?.sync_all()?;
            let absolute = path.canonicalize()?;
            let parent = absolute
                .parent()
                .ok_or_else(|| io::Error::other("store has no parent"))?;
            File::open(parent)?.sync_all()
        })();
        // Keep incomplete initialization for inspection rather than deleting data.
        result
    }

    pub(crate) fn open(path: &Path) -> io::Result<Self> {
        require_directory(path)?;
        let root = path.canonicalize()?;
        require_file(&root.join("format"))?;
        if fs::read_to_string(root.join("format"))? != FORMAT {
            return Err(io::Error::other("unsupported store format"));
        }
        for area in AREAS {
            require_directory(&root.join(area))?;
        }
        require_file(&root.join("lock"))?;
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .open(root.join("lock"))?;
        lock.try_lock().map_err(|error| {
            io::Error::other(format!("store is locked or locking failed: {error}"))
        })?;
        Ok(Self { root, _lock: lock })
    }

    pub(crate) fn import(&self, source: &Path, name: &str, plain: bool) -> io::Result<CopyStats> {
        require_directory(source)?;
        let source = source.canonicalize()?;
        if source.starts_with(&self.root) || self.root.starts_with(&source) {
            return Err(io::Error::other(
                "source and store must not contain one another",
            ));
        }
        self.publish(&source, "workspaces", name, plain)
    }

    pub(crate) fn duplicate(
        &self,
        from_area: &str,
        from: &str,
        to_area: &str,
        to: &str,
        plain: bool,
    ) -> io::Result<CopyStats> {
        let source = self.entry(from_area, from)?;
        self.publish(&source.join("tree"), to_area, to, plain)
    }

    fn publish(&self, source: &Path, area: &str, name: &str, plain: bool) -> io::Result<CopyStats> {
        validate_name(name)?;
        let target = self.root.join(area).join(name);
        ensure_absent(&target)?;
        let stage = self.root.join("staging").join(unique_id()?);
        fs::create_dir(&stage)?;
        // Failed or interrupted copies stay in staging for diagnosis. They are
        // never visible to list, fork, restore, or checkpoint as complete entries.
        let stats = copy_tree::copy_tree(source, &stage.join("tree"), plain)?;
        let mut record = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(stage.join("record"))?;
        writeln!(
            record,
            "sprig-entry-v1\ncloned={}\ncopied={}\nsymlinks={}",
            stats.cloned, stats.copied, stats.symlinks
        )?;
        record.sync_all()?;
        File::open(&stage)?.sync_all()?;
        ensure_absent(&target)?;
        fs::rename(&stage, &target)?;
        File::open(self.root.join(area))?.sync_all()?;
        File::open(self.root.join("staging"))?.sync_all()?;
        Ok(stats)
    }

    pub(crate) fn drop_entry(&self, area: &str, name: &str) -> io::Result<String> {
        let source = self.entry(area, name)?;
        let token = format!("{}-{}", area, unique_id()?);
        let target = self.root.join("trash").join(&token);
        ensure_absent(&target)?;
        fs::rename(source, &target)?;
        File::open(self.root.join("trash"))?.sync_all()?;
        File::open(self.root.join(area))?.sync_all()?;
        Ok(token)
    }

    pub(crate) fn list(&self) -> io::Result<()> {
        for area in ["workspaces", "snapshots", "trash"] {
            let mut names = Vec::new();
            for entry in fs::read_dir(self.root.join(area))? {
                let entry = entry?;
                let name = entry
                    .file_name()
                    .into_string()
                    .map_err(|_| io::Error::other("invalid store entry name"))?;
                self.entry(area, &name)?;
                names.push(name);
            }
            names.sort();
            for name in names {
                println!("{area}\t{name}");
            }
        }
        Ok(())
    }

    pub(crate) fn status(&self) -> io::Result<()> {
        println!("store: {}", self.root.display());
        println!("backend: Linux FICLONE attempted per file; ordinary copy on failure");
        println!("snapshot consistency: stop all external writers first");
        println!(
            "staging entries: {} (unfinished/unpublished; inspect manually)",
            fs::read_dir(self.root.join("staging"))?.count()
        );
        self.list()
    }

    pub(crate) fn show(&self, area: &str, name: &str) -> io::Result<()> {
        let entry = self.entry(area, name)?;
        println!("{}", entry.join("tree").display());
        print!("{}", fs::read_to_string(entry.join("record"))?);
        Ok(())
    }

    fn entry(&self, area: &str, name: &str) -> io::Result<PathBuf> {
        validate_name(name)?;
        let entry = self.root.join(area).join(name);
        require_directory(&entry)?;
        require_directory(&entry.join("tree"))?;
        require_file(&entry.join("record"))?;
        let record = fs::read_to_string(entry.join("record"))?;
        if !valid_record(&record) {
            return Err(io::Error::other("invalid entry record"));
        }
        Ok(entry)
    }
}

fn unique_id() -> io::Result<String> {
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(io::Error::other)?
        .as_nanos();
    Ok(format!("{timestamp}-{}", std::process::id()))
}

fn ensure_absent(path: &Path) -> io::Result<()> {
    match fs::symlink_metadata(path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
        Ok(_) => Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!("already exists: {}", path.display()),
        )),
    }
}

fn require_directory(path: &Path) -> io::Result<()> {
    // Remove trailing slash / dot syntax, which otherwise makes the kernel
    // follow a final symlink even when symlink_metadata is requested.
    let normalized: PathBuf = path.components().collect();
    if !fs::symlink_metadata(&normalized)?.is_dir() {
        return Err(io::Error::other(format!(
            "not a real directory: {}",
            path.display()
        )));
    }
    Ok(())
}

fn require_file(path: &Path) -> io::Result<()> {
    if !fs::symlink_metadata(path)?.is_file() {
        return Err(io::Error::other(format!(
            "not a regular file: {}",
            path.display()
        )));
    }
    Ok(())
}

fn validate_name(name: &str) -> io::Result<()> {
    if name.is_empty()
        || name.len() > 96
        || !name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
    {
        return Err(io::Error::other(
            "names must be 1–96 ASCII letters, digits, '-' or '_'",
        ));
    }
    Ok(())
}

fn valid_record(record: &str) -> bool {
    let mut lines = record.lines();
    if lines.next() != Some("sprig-entry-v1") {
        return false;
    }
    for key in ["cloned=", "copied=", "symlinks="] {
        let Some(value) = lines.next().and_then(|line| line.strip_prefix(key)) else {
            return false;
        };
        if value.is_empty()
            || !value.bytes().all(|byte| byte.is_ascii_digit())
            || value.parse::<u64>().is_err()
        {
            return false;
        }
    }
    lines.next().is_none() && record.ends_with('\n')
}
