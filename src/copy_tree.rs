//! Copies quiescent, trusted trees without following child symbolic links.
//!
//! Native acceleration clones individual files, not an atomic filesystem tree.
//! Ancestor replacement by hostile concurrent writers is outside this contract.

use std::collections::BTreeMap;
#[cfg(unix)]
use std::fs::OpenOptions;
use std::fs::{self, File, FileTimes};
use std::io;
#[cfg(unix)]
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::platform::{self, Metadata};

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CopyMode {
    #[default]
    Auto,
    Copy,
    Clone,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct CopyStats {
    pub cloned: u64,
    pub copied: u64,
    #[serde(default)]
    pub hardlinks: u64,
    pub symlinks: u64,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(crate) struct PhysicalIdentity {
    pub device: u64,
    pub inode: u128,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum CopiedKind {
    Directory,
    File,
    Symlink,
}

/// Copy-time identity evidence only: this cannot describe or publish CAS content.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct CopiedIdentity {
    pub source: PhysicalIdentity,
    pub destination: PhysicalIdentity,
    pub kind: CopiedKind,
}

pub(crate) type CopyIdentities = BTreeMap<String, CopiedIdentity>;

struct CopiedFile {
    metadata: Metadata,
    target: PathBuf,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct NativeBackendInfo {
    pub platform: String,
    pub filesystem: String,
    pub clone_api: Option<String>,
    /// Windows volume flag; other platforms report no authoritative probe here.
    pub hardlink_support: Option<bool>,
    pub block_clone_support: Option<bool>,
    pub whole_tree_snapshots: bool,
    pub note: String,
}

pub fn copy_tree(source: &Path, target: &Path, mode: CopyMode) -> io::Result<CopyStats> {
    copy_tree_inner(source, target, mode, &mut None)
}

/// Returns source identities from the checked copy and independent destination
/// identities, including the root at "". Identity capture never reads file bytes
/// in addition to the ordinary copy/clone operation.
pub(crate) fn copy_tree_with_identities(
    source: &Path,
    target: &Path,
    mode: CopyMode,
) -> io::Result<(CopyStats, CopyIdentities)> {
    let mut identities = Some(CopyIdentities::new());
    let stats = copy_tree_inner(source, target, mode, &mut identities)?;
    let identities = identities.expect("identity collector was enabled");
    validate_copy_identities(&identities)?;
    Ok((stats, identities))
}

fn copy_tree_inner(
    source: &Path,
    target: &Path,
    mode: CopyMode,
    identities: &mut Option<CopyIdentities>,
) -> io::Result<CopyStats> {
    let source = normalized_root(source)?;
    let target = normalized_root(target)?;
    let metadata = platform::symlink_metadata(&source)?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "copy root must be a real directory, not a symbolic link",
        ));
    }
    ensure_parent_directories(&target)?;
    // Prevent a copy from recursively discovering its own freshly created tree.
    let source_absolute = fs::canonicalize(&source)?;
    let target_parent = fs::canonicalize(
        target
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new(".")),
    )?;
    if target_parent.starts_with(&source_absolute) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "copy target must not be inside source",
        ));
    }
    let mut stats = CopyStats::default();
    let mut files = BTreeMap::new();
    copy_entry(
        &source, &target, mode, &mut stats, &mut files, "", identities,
    )?;
    platform::sync_directory(
        target
            .parent()
            .filter(|path| !path.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new(".")),
    )?;
    Ok(stats)
}

fn copy_entry(
    source: &Path,
    target: &Path,
    mode: CopyMode,
    stats: &mut CopyStats,
    files: &mut BTreeMap<(u64, u128), CopiedFile>,
    relative: &str,
    identities: &mut Option<CopyIdentities>,
) -> io::Result<()> {
    let metadata = platform::symlink_metadata(source)?;
    platform::validate_capture_metadata(source, &metadata)?;
    if identities.is_some() {
        validate_identity_path(relative)?;
        validate_modified_time(
            metadata.mtime() as i128 * 1_000_000_000 + metadata.mtime_nsec() as i128,
        )?;
    }
    if metadata.file_type().is_symlink() {
        copy_symlink(source, target, identities.is_some())?;
        #[cfg(target_os = "macos")]
        set_symlink_permissions(target, metadata.mode() & 0o777)?;
        set_symlink_modified(
            target,
            metadata.mtime() as i128 * 1_000_000_000 + metadata.mtime_nsec() as i128,
        )?;
        stats.symlinks += 1;
        if let Some(identities) = identities {
            record_identity(
                identities,
                relative,
                &metadata,
                &platform::symlink_metadata(target)?,
            )?;
        }
    } else if metadata.is_dir() {
        platform::create_dir(target, 0o700)?;
        platform::set_path_mode(target, 0o700)?;
        let directory = platform::open_directory(target)?;
        let mut children = fs::read_dir(source)?.collect::<io::Result<Vec<_>>>()?;
        children.sort_by_key(|entry| entry.file_name());
        for child in children {
            let child_relative = if identities.is_some() {
                let name = child.file_name().into_string().map_err(|_| {
                    io::Error::new(io::ErrorKind::InvalidData, "non-UTF-8 path rejected")
                })?;
                if relative.is_empty() {
                    name
                } else {
                    format!("{relative}/{name}")
                }
            } else {
                String::new()
            };
            copy_entry(
                &child.path(),
                &target.join(child.file_name()),
                mode,
                stats,
                files,
                &child_relative,
                identities,
            )?;
        }
        if !unchanged(&metadata, &platform::symlink_metadata(source)?) {
            return Err(io::Error::other(format!(
                "source changed during copy: {}",
                source.display()
            )));
        }
        platform::set_mode(&directory, metadata.mode() & 0o777)?;
        directory.set_times(FileTimes::new().set_modified(metadata.modified()?))?;
        platform::sync_directory_file(&directory)?;
        if let Some(identities) = identities {
            record_identity(
                identities,
                relative,
                &metadata,
                &platform::file_metadata(&directory)?,
            )?;
        }
    } else if metadata.is_file() {
        let mut input = platform::open_regular(source)?;
        let before = platform::file_metadata(&input)?;
        if !before.is_file() || !unchanged(&metadata, &before) {
            return Err(io::Error::other("source changed type while copying"));
        }
        let key = (before.dev(), platform::inode(&before));
        if let Some(copied) = files.get(&key) {
            if !unchanged(&copied.metadata, &before) {
                return Err(io::Error::other("linked source changed while copying"));
            }
            // Link only to our first independent destination copy/clone. A
            // source inode is never shared with a mutable destination tree.
            platform::hard_link(&copied.target, target)?;
            if !unchanged(&before, &platform::file_metadata(&input)?)
                || !unchanged(&before, &platform::symlink_metadata(source)?)
            {
                return Err(io::Error::other("linked source changed while copying"));
            }
            stats.hardlinks += 1;
            if let Some(identities) = identities {
                record_identity(
                    identities,
                    relative,
                    &before,
                    &platform::symlink_metadata(target)?,
                )?;
            }
            return Ok(());
        }
        let cloned = if mode == CopyMode::Copy {
            None
        } else {
            native_clone(&input, source, target)?
        };
        let output = if let Some(output) = cloned {
            stats.cloned += 1;
            output
        } else {
            if mode == CopyMode::Clone {
                return Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    "native file cloning is not supported for this source and destination",
                ));
            }
            let mut output = platform::create_new(target, 0o600)?;
            let copied = io::copy(&mut input, &mut output)?;
            if copied != before.len() {
                return Err(io::Error::other("source size changed while copying"));
            }
            stats.copied += 1;
            output
        };
        if !unchanged(&before, &platform::file_metadata(&input)?)
            || !unchanged(&before, &platform::symlink_metadata(source)?)
            || output.metadata()?.len() != before.len()
        {
            return Err(io::Error::other("source changed while copying"));
        }
        platform::set_mode(&output, metadata.mode() & 0o777)?;
        output.set_times(FileTimes::new().set_modified(metadata.modified()?))?;
        output.sync_all()?;
        if let Some(identities) = identities {
            record_identity(
                identities,
                relative,
                &before,
                &platform::file_metadata(&output)?,
            )?;
        }
        files.insert(
            key,
            CopiedFile {
                metadata: before,
                target: target.to_owned(),
            },
        );
    } else {
        return Err(io::Error::other(format!(
            "special file rejected: {}",
            source.display()
        )));
    }
    Ok(())
}

fn physical_identity(metadata: &Metadata) -> PhysicalIdentity {
    PhysicalIdentity {
        device: metadata.dev(),
        inode: platform::inode(metadata),
    }
}

fn copied_kind(metadata: &Metadata) -> io::Result<CopiedKind> {
    if metadata.file_type().is_symlink() {
        Ok(CopiedKind::Symlink)
    } else if metadata.is_dir() {
        Ok(CopiedKind::Directory)
    } else if metadata.is_file() {
        Ok(CopiedKind::File)
    } else {
        Err(io::Error::other("special file has no copy identity"))
    }
}

fn record_identity(
    identities: &mut CopyIdentities,
    relative: &str,
    source: &Metadata,
    destination: &Metadata,
) -> io::Result<()> {
    let kind = copied_kind(source)?;
    let source = physical_identity(source);
    let destination_kind = copied_kind(destination)?;
    let destination = physical_identity(destination);
    if kind != destination_kind || source == destination {
        return Err(io::Error::other(
            "copy destination changed type or shares the source inode",
        ));
    }
    if identities
        .insert(
            relative.to_owned(),
            CopiedIdentity {
                source,
                destination,
                kind,
            },
        )
        .is_some()
    {
        return Err(io::Error::other("duplicate copied path identity"));
    }
    Ok(())
}

fn validate_copy_identities(identities: &CopyIdentities) -> io::Result<()> {
    if identities.get("").map(|entry| entry.kind) != Some(CopiedKind::Directory) {
        return Err(io::Error::other("copy identities require a root directory"));
    }
    let mut source_files = BTreeMap::new();
    let mut destination_files = BTreeMap::new();
    for (path, entry) in identities {
        validate_identity_path(path)?;
        if !path.is_empty() {
            let parent = path.rsplit_once('/').map_or("", |(parent, _)| parent);
            if identities.get(parent).map(|entry| entry.kind) != Some(CopiedKind::Directory) {
                return Err(io::Error::other("copied path has no directory ancestor"));
            }
        }
        if entry.source == entry.destination {
            return Err(io::Error::other("copy shares a mutable source inode"));
        }
        if entry.kind == CopiedKind::File
            && (source_files
                .insert(entry.source, entry.destination)
                .is_some_and(|previous| previous != entry.destination)
                || destination_files
                    .insert(entry.destination, entry.source)
                    .is_some_and(|previous| previous != entry.source))
        {
            return Err(io::Error::other("copy changed regular-file link topology"));
        }
    }
    if destination_files
        .keys()
        .any(|identity| source_files.contains_key(identity))
    {
        return Err(io::Error::other("copy links to a source-tree file"));
    }
    Ok(())
}

fn validate_identity_path(path: &str) -> io::Result<()> {
    if path.is_empty() {
        return Ok(());
    }
    platform::validate_relative_path(path)?;
    if path.contains('\0')
        || path
            .split('/')
            .any(|part| part.is_empty() || part == "." || part == "..")
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid relative copy identity path",
        ));
    }
    Ok(())
}

fn validate_modified_time(nanoseconds: i128) -> io::Result<()> {
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
    time.ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "mtime out of range"))?;
    Ok(())
}

#[cfg(target_os = "linux")]
pub(crate) fn native_clone(
    source: &File,
    _source_path: &Path,
    target: &Path,
) -> io::Result<Option<File>> {
    use std::os::fd::AsRawFd;
    let output = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(target)?;
    // Linux UAPI FICLONE = _IOW(0x94, 9, int). Its third argument is an
    // integer fd, not a pointer; both owned File values remain live here.
    // SAFETY: this exact ioctl ABI accepts these valid open file descriptors.
    let result = unsafe {
        libc::ioctl(
            output.as_raw_fd(),
            0x4004_9409 as libc::c_ulong,
            source.as_raw_fd(),
        )
    };
    if result == 0 {
        return Ok(Some(output));
    }
    let error = io::Error::last_os_error();
    drop(output);
    fs::remove_file(target)?;
    if unsupported_clone(&error) {
        Ok(None)
    } else {
        Err(error)
    }
}

#[cfg(target_os = "macos")]
pub(crate) fn native_clone(
    source: &File,
    _source_path: &Path,
    target: &Path,
) -> io::Result<Option<File>> {
    use std::ffi::CString;
    use std::os::fd::AsRawFd;
    use std::os::unix::ffi::OsStrExt;
    let destination = CString::new(target.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "NUL in destination path"))?;
    // The fd-based clonefile-family API pins the already verified regular source
    // inode. The destination must not exist, and no writable inode is shared.
    // SAFETY: the source fd and destination C string remain valid for the call;
    // AT_FDCWD resolves the destination path using the ordinary process cwd.
    let result =
        unsafe { libc::fclonefileat(source.as_raw_fd(), libc::AT_FDCWD, destination.as_ptr(), 0) };
    if result == 0 {
        let output = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(target)?;
        if !output.metadata()?.is_file() {
            return Err(io::Error::other("source changed type during native clone"));
        }
        return Ok(Some(output));
    }
    let error = io::Error::last_os_error();
    if unsupported_clone(&error) {
        Ok(None)
    } else {
        Err(error)
    }
}

#[cfg(target_os = "macos")]
pub(crate) fn set_symlink_permissions(path: &Path, mode: u32) -> io::Result<()> {
    use std::os::unix::ffi::OsStrExt;
    let path = std::ffi::CString::new(path.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "NUL in symbolic link path"))?;
    // SAFETY: a live path and ordinary permission bits. lchmod operates on the
    // symbolic link itself rather than following its target.
    if unsafe { libc::lchmod(path.as_ptr(), mode as libc::mode_t) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(windows)]
pub(crate) fn native_clone(
    source: &File,
    source_path: &Path,
    target: &Path,
) -> io::Result<Option<File>> {
    platform::native_clone(source, source_path, target)
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn unsupported_clone(error: &io::Error) -> bool {
    // EINVAL is an established unsupported reflink case for otherwise-valid
    // whole regular-file requests. Permission, quota, I/O and space errors are
    // deliberately NOT hidden by an ordinary-copy fallback.
    matches!(error.raw_os_error(), Some(code) if code == libc::EXDEV
        || code == libc::EOPNOTSUPP || code == libc::ENOTTY
        || code == libc::ENOSYS || code == libc::EINVAL)
}

/// Restores a symlink's mtime without touching the target, including dangling
/// or absolute links. atime is intentionally left unchanged.
#[cfg(unix)]
pub(crate) fn set_symlink_modified(path: &Path, modified_ns: i128) -> io::Result<()> {
    use std::os::unix::ffi::OsStrExt;
    let path = std::ffi::CString::new(path.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "NUL in symbolic link path"))?;
    let seconds: libc::time_t = modified_ns
        .div_euclid(1_000_000_000)
        .try_into()
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "mtime out of range"))?;
    let times = [
        libc::timespec {
            tv_sec: 0,
            tv_nsec: libc::UTIME_OMIT,
        },
        libc::timespec {
            tv_sec: seconds,
            tv_nsec: modified_ns.rem_euclid(1_000_000_000) as _,
        },
    ];
    // SAFETY: valid C path, two initialized timespec values, and the POSIX
    // NOFOLLOW flag explicitly applies the timestamp to the link inode.
    if unsafe {
        libc::utimensat(
            libc::AT_FDCWD,
            path.as_ptr(),
            times.as_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    } != 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(windows)]
pub(crate) fn set_symlink_modified(_path: &Path, _modified_ns: i128) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "symbolic links are not supported on Windows",
    ))
}

#[cfg(unix)]
fn copy_symlink(source: &Path, target: &Path, validate_target: bool) -> io::Result<()> {
    use std::os::unix::ffi::OsStrExt;
    let link = fs::read_link(source)?;
    if validate_target {
        let bytes = link.as_os_str().as_bytes();
        if bytes.is_empty() || bytes.contains(&0) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid symbolic link target",
            ));
        }
    }
    std::os::unix::fs::symlink(link, target)
}

#[cfg(windows)]
fn copy_symlink(_source: &Path, _target: &Path, _validate_target: bool) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "symbolic links are not supported on Windows",
    ))
}

pub fn native_backend_info(path: &Path) -> io::Result<NativeBackendInfo> {
    Ok(NativeBackendInfo {
        platform: std::env::consts::OS.to_owned(),
        filesystem: filesystem_name(path)?,
        clone_api: if cfg!(target_os = "linux") { Some("FICLONE".to_owned()) }
            else if cfg!(target_os = "macos") { Some("fclonefileat".to_owned()) }
            else if cfg!(windows) { Some("FSCTL_DUPLICATE_EXTENTS_TO_FILE (ReFS)".to_owned()) } else { None },
        hardlink_support: {
            #[cfg(windows)]
            { Some(platform::supports_hard_links(path)?) }
            #[cfg(unix)]
            { None }
        },
        block_clone_support: {
            #[cfg(windows)]
            { Some(platform::supports_block_cloning(path)?) }
            #[cfg(unix)]
            { None }
        },
        whole_tree_snapshots: false,
        note: if cfg!(windows) {
            "Windows supports local NTFS and ReFS. Same-volume ReFS files may use block cloning; other supported files use ordinary copies. Linux projection is unsupported. Reparse points and read-only files are rejected. ACLs are inherited, not reproduced. File flushes and write-through replacement do not provide POSIX directory-fsync power-loss guarantees."
        } else {
            "Per-file copy-on-write clones are attempted at copy time; filesystem detection alone does not guarantee support. No whole-tree native snapshot is used."
        }.to_owned(),
    })
}

#[cfg(target_os = "linux")]
fn filesystem_name(path: &Path) -> io::Result<String> {
    use std::mem::MaybeUninit;
    use std::os::fd::AsRawFd;
    let file = File::open(path)?;
    let mut info = MaybeUninit::<libc::statfs>::uninit();
    // SAFETY: fstatfs initializes the complete struct on success, and the fd and
    // writable output pointer are valid for the duration of the call.
    if unsafe { libc::fstatfs(file.as_raw_fd(), info.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: successful fstatfs initialized the value.
    let magic = unsafe { info.assume_init() }.f_type as u64;
    Ok(match magic {
        0x9123_683e => "btrfs".to_owned(),
        0x5846_5342 => "xfs".to_owned(),
        0x0000_ef53 => "ext-family".to_owned(),
        0x794c_7630 => "overlayfs".to_owned(),
        0x0102_1994 => "tmpfs".to_owned(),
        0x0000_6969 => "nfs".to_owned(),
        0x2fc1_2fc1 => "zfs".to_owned(),
        other => format!("linux-statfs-0x{other:x}"),
    })
}

#[cfg(target_os = "macos")]
fn filesystem_name(path: &Path) -> io::Result<String> {
    use std::ffi::CStr;
    use std::mem::MaybeUninit;
    use std::os::fd::AsRawFd;
    let file = File::open(path)?;
    let mut info = MaybeUninit::<libc::statfs>::uninit();
    // SAFETY: live fd and writable struct pointer; inspected only on success.
    if unsafe { libc::fstatfs(file.as_raw_fd(), info.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: successful fstatfs initializes the struct and its filesystem name
    // is a NUL-terminated fixed-size array according to the Darwin ABI.
    let info = unsafe { info.assume_init() };
    let name = unsafe { CStr::from_ptr(info.f_fstypename.as_ptr()) };
    Ok(name.to_string_lossy().into_owned())
}

#[cfg(windows)]
fn filesystem_name(path: &Path) -> io::Result<String> {
    platform::filesystem_name(path).map(str::to_owned)
}

fn normalized_root(path: &Path) -> io::Result<PathBuf> {
    if path.as_os_str().is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "empty tree root",
        ));
    }
    let mut output = PathBuf::new();
    for component in path.components() {
        match component {
            Component::ParentDir => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "parent-directory syntax is not allowed in tree roots",
                ));
            }
            Component::CurDir => {}
            other => output.push(other.as_os_str()),
        }
    }
    if output.as_os_str().is_empty() {
        output.push(".");
    }
    Ok(output)
}

fn ensure_parent_directories(target: &Path) -> io::Result<()> {
    let mut current = PathBuf::new();
    for component in target
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .components()
    {
        current.push(component.as_os_str());
        if matches!(component, Component::Prefix(_)) {
            continue;
        }
        let metadata = platform::symlink_metadata(&current)?;
        if !metadata.is_dir() || metadata.file_type().is_symlink() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "copy destination ancestors must be real directories",
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
        && left.mode() == right.mode()
        && left.len() == right.len()
        && left.mtime() == right.mtime()
        && left.mtime_nsec() == right.mtime_nsec()
        && left.ctime() == right.ctime()
        && left.ctime_nsec() == right.ctime_nsec()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::SystemTime;

    struct Fixture(PathBuf);

    impl Fixture {
        fn new() -> io::Result<Self> {
            static NONCE: AtomicU64 = AtomicU64::new(0);
            let path = fs::canonicalize(std::env::temp_dir())?.join(format!(
                "sprig-copy-identities-{}-{}-{}",
                std::process::id(),
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_nanos(),
                NONCE.fetch_add(1, Ordering::Relaxed),
            ));
            fs::create_dir(&path)?;
            Ok(Self(path))
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn copy_report_tracks_each_node_and_preserves_link_isolation() -> io::Result<()> {
        let fixture = Fixture::new()?;
        let source = fixture.0.join("source");
        let target = fixture.0.join("target");
        fs::create_dir_all(source.join("nested"))?;
        fs::write(source.join("a"), b"shared bytes")?;
        fs::hard_link(source.join("a"), source.join("nested/b"))?;
        fs::write(source.join("independent"), b"shared bytes")?;
        fs::write(source.join("empty"), [])?;
        // An alias outside the copied tree must not become part of its report.
        fs::hard_link(source.join("independent"), fixture.0.join("outside"))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(source.join("a"), fs::Permissions::from_mode(0o751))?;
            std::os::unix::fs::symlink("missing-target", source.join("dangling"))?;
        }

        let (stats, identities) = copy_tree_with_identities(&source, &target, CopyMode::Copy)?;
        assert_eq!(stats.copied, 3);
        assert_eq!(stats.cloned, 0);
        assert_eq!(stats.hardlinks, 1);
        assert_eq!(identities.len(), if cfg!(unix) { 7 } else { 6 });
        assert_eq!(identities[""].kind, CopiedKind::Directory);
        assert_eq!(identities["nested"].kind, CopiedKind::Directory);
        assert_eq!(identities["empty"].kind, CopiedKind::File);
        assert_eq!(identities["a"].source, identities["nested/b"].source);
        assert_eq!(
            identities["a"].destination,
            identities["nested/b"].destination
        );
        assert_ne!(identities["a"].source, identities["independent"].source);
        assert_ne!(
            identities["a"].destination,
            identities["independent"].destination
        );
        for (path, identity) in &identities {
            let original = platform::symlink_metadata(source.join(path))?;
            let copied = platform::symlink_metadata(target.join(path))?;
            assert_eq!(identity.source, physical_identity(&original));
            assert_eq!(identity.destination, physical_identity(&copied));
            assert_eq!(identity.kind, copied_kind(&original)?);
            assert_eq!(identity.kind, copied_kind(&copied)?);
            assert_ne!(identity.source, identity.destination);
            assert_eq!(original.mode() & 0o777, copied.mode() & 0o777);
            assert_eq!(original.mtime(), copied.mtime());
            assert_eq!(original.mtime_nsec(), copied.mtime_nsec());
        }
        #[cfg(unix)]
        {
            assert_eq!(stats.symlinks, 1);
            assert_eq!(identities["dangling"].kind, CopiedKind::Symlink);
            assert_eq!(
                fs::read_link(target.join("dangling"))?,
                Path::new("missing-target")
            );
        }
        fs::write(target.join("a"), b"fork edit")?;
        assert_eq!(fs::read(target.join("nested/b"))?, b"fork edit");
        assert_eq!(fs::read(source.join("a"))?, b"shared bytes");
        assert_eq!(fs::read(source.join("nested/b"))?, b"shared bytes");
        assert_eq!(fs::read(target.join("independent"))?, b"shared bytes");
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn identity_capture_rejects_non_utf8_names_without_changing_raw_copy() -> io::Result<()> {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt;

        let fixture = Fixture::new()?;
        let source = fixture.0.join("source");
        fs::create_dir(&source)?;
        let name = OsString::from_vec(vec![b'f', 0xff]);
        let created = fs::write(source.join(&name), b"bytes");
        #[cfg(target_os = "macos")]
        if let Err(error) = &created {
            // APFS rejects this filename before any copy operation. Check that
            // exact filesystem boundary, not an arbitrary fixture failure.
            assert_eq!(error.raw_os_error(), Some(libc::EILSEQ), "{error:?}");
            assert_eq!(fs::read_dir(&source)?.count(), 0);
            return Ok(());
        }
        created?;
        let raw_target = fixture.0.join("raw");
        assert_eq!(copy_tree(&source, &raw_target, CopyMode::Copy)?.copied, 1);
        assert_eq!(fs::read(raw_target.join(&name))?, b"bytes");
        assert!(
            copy_tree_with_identities(&source, &fixture.0.join("reported"), CopyMode::Copy)
                .is_err()
        );
        Ok(())
    }

    #[test]
    fn identity_validation_rejects_bad_paths_topology_and_times() {
        for path in ["/absolute", "a/../b", "a/./b", "a//b", "a/", "nul\0name"] {
            assert!(validate_identity_path(path).is_err(), "{path:?}");
        }
        assert!(validate_identity_path("").is_ok());
        assert!(validate_identity_path("space name/中文").is_ok());
        assert!(validate_modified_time(0).is_ok());
        assert!(validate_modified_time(1_000_000_001).is_ok());
        assert!(validate_modified_time(i128::MAX).is_err());
        assert!(validate_modified_time(i128::MIN).is_err());

        let identity = |inode| PhysicalIdentity { device: 7, inode };
        let node = |source, destination, kind| CopiedIdentity {
            source: identity(source),
            destination: identity(destination),
            kind,
        };
        // Equal low 64 bits must not collapse distinct Windows file IDs.
        let high = 1_u128 << 100;
        let valid = BTreeMap::from([
            (String::new(), node(1, 101, CopiedKind::Directory)),
            ("a".into(), node(2, 102, CopiedKind::File)),
            ("b".into(), node(2, 102, CopiedKind::File)),
            ("c".into(), node(high + 2, high + 102, CopiedKind::File)),
        ]);
        assert!(validate_copy_identities(&valid).is_ok());

        let mut bad = valid.clone();
        bad.remove("");
        assert!(validate_copy_identities(&bad).is_err());
        let mut bad = valid.clone();
        bad.get_mut("").unwrap().kind = CopiedKind::File;
        assert!(validate_copy_identities(&bad).is_err());
        let mut bad = valid.clone();
        bad.insert("a/child".into(), node(3, 103, CopiedKind::File));
        assert!(validate_copy_identities(&bad).is_err());
        let mut bad = valid.clone();
        bad.get_mut("b").unwrap().destination = identity(103);
        assert!(validate_copy_identities(&bad).is_err());
        let mut bad = valid.clone();
        bad.get_mut("c").unwrap().destination = identity(102);
        assert!(validate_copy_identities(&bad).is_err());
        let mut bad = valid;
        bad.get_mut("c").unwrap().destination = identity(2);
        assert!(validate_copy_identities(&bad).is_err());
    }

    #[test]
    fn validates_root_syntax_and_clone_error_classification() {
        assert_eq!(
            normalized_root(Path::new("link/./")).unwrap(),
            PathBuf::from("link")
        );
        assert!(normalized_root(Path::new("link/../other")).is_err());
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        {
            assert!(unsupported_clone(&io::Error::from_raw_os_error(
                libc::EXDEV
            )));
            assert!(!unsupported_clone(&io::Error::from_raw_os_error(libc::EIO)));
            assert!(!unsupported_clone(&io::Error::from_raw_os_error(
                libc::ENOSPC
            )));
            assert!(!unsupported_clone(&io::Error::from_raw_os_error(
                libc::EPERM
            )));
        }
    }
}
