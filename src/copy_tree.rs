//! Copy trees without following symbolic links or sharing writable inodes.

use std::fs::{self, File, OpenOptions};
use std::io;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::Path;

#[derive(Default)]
pub(crate) struct CopyStats {
    pub(crate) cloned: u64,
    pub(crate) copied: u64,
    pub(crate) symlinks: u64,
}

pub(crate) fn copy_tree(source: &Path, target: &Path, plain: bool) -> io::Result<CopyStats> {
    let mut stats = CopyStats::default();
    copy_entry(source, target, plain, &mut stats)?;
    Ok(stats)
}

fn copy_entry(source: &Path, target: &Path, plain: bool, stats: &mut CopyStats) -> io::Result<()> {
    let metadata = fs::symlink_metadata(source)?;
    let kind = metadata.file_type();
    if kind.is_symlink() {
        std::os::unix::fs::symlink(fs::read_link(source)?, target)?;
        stats.symlinks += 1;
    } else if kind.is_dir() {
        fs::create_dir(target)?;
        for entry in fs::read_dir(source)? {
            let entry = entry?;
            copy_entry(&entry.path(), &target.join(entry.file_name()), plain, stats)?;
        }
        fs::set_permissions(
            target,
            fs::Permissions::from_mode(metadata.permissions().mode() & 0o777),
        )?;
        File::open(target)?.sync_all()?;
    } else if kind.is_file() {
        // O_NOFOLLOW prevents a replaced leaf symlink from being opened. The caller
        // must still quiesce external writers: ancestors can change during a walk.
        let mut input = OpenOptions::new()
            .read(true)
            .custom_flags(0x20000)
            .open(source)?;
        if !input.metadata()?.is_file() {
            return Err(io::Error::other("source changed type while copying"));
        }
        let mut output = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(target)?;
        if !plain && try_clone(&input, &output) {
            stats.cloned += 1;
        } else {
            // A failed ioctl must not leave partial bytes in the fallback output.
            output.set_len(0)?;
            io::copy(&mut input, &mut output)?;
            stats.copied += 1;
        }
        output.set_permissions(fs::Permissions::from_mode(
            metadata.permissions().mode() & 0o777,
        ))?;
        output.sync_all()?;
    } else {
        return Err(io::Error::other(format!(
            "special file rejected: {}",
            source.display()
        )));
    }
    Ok(())
}

fn try_clone(source: &File, target: &File) -> bool {
    use std::os::fd::AsRawFd;
    use std::os::raw::{c_int, c_ulong};

    unsafe extern "C" {
        fn ioctl(fd: c_int, request: c_ulong, ...) -> c_int;
    }
    // Linux UAPI FICLONE = _IOW(0x94, 9, int). The ioctl consumes an fd,
    // not a pointer. Both File values keep their descriptors live for the call.
    // SAFETY: valid owned file descriptors and the exact integer ioctl ABI.
    unsafe { ioctl(target.as_raw_fd(), 0x4004_9409, source.as_raw_fd()) == 0 }
}
