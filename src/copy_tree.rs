//! Copies quiescent, trusted trees without following child symbolic links.
//!
//! Native acceleration clones individual files, not an atomic filesystem tree.
//! Ancestor replacement by hostile concurrent writers is outside this contract.

use std::fs::{self, DirBuilder, File, FileTimes, Metadata, OpenOptions};
use std::io;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};

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
    pub symlinks: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct NativeBackendInfo {
    pub platform: String,
    pub filesystem: String,
    pub clone_api: Option<String>,
    pub whole_tree_snapshots: bool,
    pub note: String,
}

pub fn copy_tree(source: &Path, target: &Path, mode: CopyMode) -> io::Result<CopyStats> {
    let source = normalized_root(source)?;
    let target = normalized_root(target)?;
    let metadata = fs::symlink_metadata(&source)?;
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
    copy_entry(&source, &target, mode, &mut stats)?;
    File::open(
        target
            .parent()
            .filter(|path| !path.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new(".")),
    )?
    .sync_all()?;
    Ok(stats)
}

fn copy_entry(
    source: &Path,
    target: &Path,
    mode: CopyMode,
    stats: &mut CopyStats,
) -> io::Result<()> {
    let metadata = fs::symlink_metadata(source)?;
    if metadata.file_type().is_symlink() {
        std::os::unix::fs::symlink(fs::read_link(source)?, target)?;
        #[cfg(target_os = "macos")]
        set_symlink_permissions(target, metadata.mode() & 0o777)?;
        set_symlink_modified(
            target,
            metadata.mtime() as i128 * 1_000_000_000 + metadata.mtime_nsec() as i128,
        )?;
        stats.symlinks += 1;
    } else if metadata.is_dir() {
        DirBuilder::new().mode(0o700).create(target)?;
        fs::set_permissions(target, fs::Permissions::from_mode(0o700))?;
        let directory = File::open(target)?;
        let mut children = fs::read_dir(source)?.collect::<io::Result<Vec<_>>>()?;
        children.sort_by_key(|entry| entry.file_name());
        for child in children {
            copy_entry(&child.path(), &target.join(child.file_name()), mode, stats)?;
        }
        if !unchanged(&metadata, &fs::symlink_metadata(source)?) {
            return Err(io::Error::other(format!(
                "source changed during copy: {}",
                source.display()
            )));
        }
        directory.set_permissions(fs::Permissions::from_mode(metadata.mode() & 0o777))?;
        directory.set_times(FileTimes::new().set_modified(metadata.modified()?))?;
        directory.sync_all()?;
    } else if metadata.is_file() {
        let mut input = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(source)?;
        let before = input.metadata()?;
        if !before.is_file() || !unchanged(&metadata, &before) {
            return Err(io::Error::other("source changed type while copying"));
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
            let mut output = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .custom_flags(libc::O_NOFOLLOW)
                .open(target)?;
            let copied = io::copy(&mut input, &mut output)?;
            if copied != before.len() {
                return Err(io::Error::other("source size changed while copying"));
            }
            stats.copied += 1;
            output
        };
        if !unchanged(&before, &input.metadata()?) || output.metadata()?.len() != before.len() {
            return Err(io::Error::other("source changed while copying"));
        }
        output.set_permissions(fs::Permissions::from_mode(metadata.mode() & 0o777))?;
        output.set_times(FileTimes::new().set_modified(metadata.modified()?))?;
        output.sync_all()?;
    } else {
        return Err(io::Error::other(format!(
            "special file rejected: {}",
            source.display()
        )));
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn native_clone(source: &File, _source_path: &Path, target: &Path) -> io::Result<Option<File>> {
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
fn native_clone(source: &File, _source_path: &Path, target: &Path) -> io::Result<Option<File>> {
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

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn native_clone(_source: &File, _source_path: &Path, _target: &Path) -> io::Result<Option<File>> {
    Ok(None)
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

pub fn native_backend_info(path: &Path) -> io::Result<NativeBackendInfo> {
    Ok(NativeBackendInfo {
        platform: std::env::consts::OS.to_owned(),
        filesystem: filesystem_name(path)?,
        clone_api: if cfg!(target_os = "linux") { Some("FICLONE".to_owned()) }
            else if cfg!(target_os = "macos") { Some("fclonefileat".to_owned()) } else { None },
        whole_tree_snapshots: false,
        note: "Per-file copy-on-write clones are attempted at copy time; filesystem detection alone does not guarantee support. No whole-tree native snapshot is used.".to_owned(),
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

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn filesystem_name(path: &Path) -> io::Result<String> {
    fs::metadata(path)?;
    Ok("unknown".to_owned())
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
        let metadata = fs::symlink_metadata(&current)?;
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
        && left.len() == right.len()
        && left.mtime() == right.mtime()
        && left.mtime_nsec() == right.mtime_nsec()
        && left.ctime() == right.ctime()
        && left.ctime_nsec() == right.ctime_nsec()
}

#[cfg(test)]
mod tests {
    use super::*;

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
