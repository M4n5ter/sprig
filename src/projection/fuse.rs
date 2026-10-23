//! Read-only FUSE protocol implementation. Mount lifecycle is owned by callers.

use std::ffi::OsStr;
use std::io;
use std::path::Path;
use std::time::{Duration, SystemTime};

use fuser::{
    FileType, Filesystem, KernelConfig, MountOption, ReplyAttr, ReplyCreate, ReplyData,
    ReplyDirectory, ReplyEmpty, ReplyEntry, ReplyOpen, ReplyStatfs, ReplyWrite, ReplyXattr,
    Request, TimeOrNow,
};

use super::{MAX_READ_BYTES, ORIGIN_XATTR, Projection, ROOT_INODE, SESSION_XATTR};

const TTL: Duration = Duration::from_secs(3600);

/// Called after successful FUSE initialization, not merely after mount setup.
/// Returning an error rejects initialization and prevents a false ready signal.
pub type ReadySignal = Box<dyn FnOnce() -> io::Result<()> + Send>;

pub struct ReadOnlyFuse {
    projection: Projection,
    ready: Option<ReadySignal>,
}

impl ReadOnlyFuse {
    pub fn new(projection: Projection, ready: Option<ReadySignal>) -> Self {
        Self { projection, ready }
    }
}

/// Required policy for every projection mount. No writable or allow-other
/// option is exposed. Executable source files retain normal read/exec semantics.
pub fn mount_options() -> Vec<MountOption> {
    vec![
        MountOption::FSName("sprig-cas".into()),
        MountOption::RO,
        MountOption::DefaultPermissions,
        MountOption::NoSuid,
        MountOption::NoDev,
        MountOption::NoAtime,
    ]
}

fn errno(error: &io::Error) -> i32 {
    error.raw_os_error().unwrap_or(libc::EIO)
}

fn reply_xattr(reply: ReplyXattr, size: u32, value: &[u8]) {
    let Ok(length) = u32::try_from(value.len()) else {
        reply.error(libc::E2BIG);
        return;
    };
    if size == 0 {
        reply.size(length);
    } else if size < length {
        reply.error(libc::ERANGE);
    } else {
        reply.data(value);
    }
}

// The signatures are imposed by the FUSE protocol, not application API design.
#[allow(clippy::too_many_arguments)]
impl Filesystem for ReadOnlyFuse {
    fn init(
        &mut self,
        _request: &Request<'_>,
        config: &mut KernelConfig,
    ) -> std::result::Result<(), i32> {
        config
            .set_max_write(MAX_READ_BYTES as u32)
            .map_err(|_| libc::EINVAL)?;
        // Older kernels can advertise a smaller limit; keep their default then.
        let _ = config.set_max_readahead(MAX_READ_BYTES as u32);
        if let Some(ready) = self.ready.take() {
            ready().map_err(|error| errno(&error))?;
        }
        Ok(())
    }

    fn lookup(&mut self, _request: &Request<'_>, parent: u64, name: &OsStr, reply: ReplyEntry) {
        match self
            .projection
            .lookup(parent, name)
            .and_then(|inode| self.projection.getattr(inode))
        {
            Ok(attr) => reply.entry(&TTL, &attr, 1),
            Err(error) => reply.error(errno(&error)),
        }
    }

    fn getattr(
        &mut self,
        _request: &Request<'_>,
        inode: u64,
        _handle: Option<u64>,
        reply: ReplyAttr,
    ) {
        match self.projection.getattr(inode) {
            Ok(attr) => reply.attr(&TTL, &attr),
            Err(error) => reply.error(errno(&error)),
        }
    }

    fn readlink(&mut self, _request: &Request<'_>, inode: u64, reply: ReplyData) {
        match self.projection.readlink(inode) {
            Ok(bytes) => reply.data(bytes),
            Err(error) => reply.error(errno(&error)),
        }
    }

    fn open(&mut self, _request: &Request<'_>, inode: u64, flags: i32, reply: ReplyOpen) {
        match self.projection.open(inode, flags, false) {
            Ok(()) => reply.opened(0, 0),
            Err(error) => reply.error(errno(&error)),
        }
    }

    fn opendir(&mut self, _request: &Request<'_>, inode: u64, flags: i32, reply: ReplyOpen) {
        match self.projection.open(inode, flags, true) {
            Ok(()) => reply.opened(0, 0),
            Err(error) => reply.error(errno(&error)),
        }
    }

    fn read(
        &mut self,
        _request: &Request<'_>,
        inode: u64,
        _handle: u64,
        offset: i64,
        size: u32,
        _flags: i32,
        _lock_owner: Option<u64>,
        reply: ReplyData,
    ) {
        match self.projection.read(inode, offset, size) {
            Ok(bytes) => reply.data(&bytes),
            Err(error) => {
                eprintln!(
                    "{}",
                    serde_json::json!({"event": "read_error", "inode": inode, "offset": offset, "error": error.to_string()})
                );
                reply.error(errno(&error));
            }
        }
    }

    fn readdir(
        &mut self,
        _request: &Request<'_>,
        inode: u64,
        _handle: u64,
        offset: i64,
        mut reply: ReplyDirectory,
    ) {
        let Ok(offset) = usize::try_from(offset) else {
            reply.error(libc::EINVAL);
            return;
        };
        let (parent, children) = match self.projection.directory(inode) {
            Ok(directory) => directory,
            Err(error) => {
                reply.error(errno(&error));
                return;
            }
        };
        let dots = [
            (inode, FileType::Directory, OsStr::new(".")),
            (parent, FileType::Directory, OsStr::new("..")),
        ];
        let entries = dots.into_iter().chain(
            children
                .iter()
                .map(|entry| (entry.inode, entry.kind, entry.name.as_os_str())),
        );
        for (index, (inode, kind, name)) in entries.enumerate().skip(offset) {
            if reply.add(inode, (index + 1) as i64, kind, name) {
                break;
            }
        }
        reply.ok();
    }

    fn access(&mut self, request: &Request<'_>, inode: u64, mask: i32, reply: ReplyEmpty) {
        match self
            .projection
            .access(inode, request.uid(), request.gid(), mask)
        {
            Ok(()) => reply.ok(),
            Err(error) => reply.error(errno(&error)),
        }
    }

    fn statfs(&mut self, _request: &Request<'_>, _inode: u64, reply: ReplyStatfs) {
        reply.statfs(
            self.projection.total_bytes().div_ceil(4096),
            0,
            0,
            self.projection.inode_count(),
            0,
            4096,
            255,
            4096,
        );
    }

    fn flush(
        &mut self,
        _request: &Request<'_>,
        _inode: u64,
        _handle: u64,
        _owner: u64,
        reply: ReplyEmpty,
    ) {
        reply.ok();
    }
    fn fsync(
        &mut self,
        _request: &Request<'_>,
        _inode: u64,
        _handle: u64,
        _datasync: bool,
        reply: ReplyEmpty,
    ) {
        reply.ok();
    }
    fn fsyncdir(
        &mut self,
        _request: &Request<'_>,
        _inode: u64,
        _handle: u64,
        _datasync: bool,
        reply: ReplyEmpty,
    ) {
        reply.ok();
    }

    fn getxattr(
        &mut self,
        _request: &Request<'_>,
        inode: u64,
        name: &OsStr,
        size: u32,
        reply: ReplyXattr,
    ) {
        if name == SESSION_XATTR {
            match self.projection.session(inode) {
                Ok(session) => reply_xattr(reply, size, session),
                Err(error) => reply.error(errno(&error)),
            }
            return;
        }
        match self.projection.origin(inode) {
            Ok(origin) if name == ORIGIN_XATTR => reply_xattr(reply, size, origin),
            Ok(_) => reply.error(libc::ENODATA),
            Err(error) => reply.error(errno(&error)),
        }
    }

    fn listxattr(&mut self, _request: &Request<'_>, inode: u64, size: u32, reply: ReplyXattr) {
        match self.projection.origin(inode) {
            Ok(_) => {
                let mut names = ORIGIN_XATTR.as_bytes().to_vec();
                names.push(0);
                if inode == ROOT_INODE {
                    names.extend_from_slice(SESSION_XATTR.as_bytes());
                    names.push(0);
                }
                reply_xattr(reply, size, &names);
            }
            Err(error) if error.raw_os_error() == Some(libc::ENODATA) => {
                reply_xattr(reply, size, &[])
            }
            Err(error) => reply.error(errno(&error)),
        }
    }

    fn setattr(
        &mut self,
        _request: &Request<'_>,
        _inode: u64,
        _mode: Option<u32>,
        _uid: Option<u32>,
        _gid: Option<u32>,
        _size: Option<u64>,
        _atime: Option<TimeOrNow>,
        _mtime: Option<TimeOrNow>,
        _ctime: Option<SystemTime>,
        _handle: Option<u64>,
        _crtime: Option<SystemTime>,
        _chgtime: Option<SystemTime>,
        _bkuptime: Option<SystemTime>,
        _flags: Option<u32>,
        reply: ReplyAttr,
    ) {
        reply.error(libc::EROFS);
    }
    fn mknod(
        &mut self,
        _request: &Request<'_>,
        _parent: u64,
        _name: &OsStr,
        _mode: u32,
        _umask: u32,
        _rdev: u32,
        reply: ReplyEntry,
    ) {
        reply.error(libc::EROFS);
    }
    fn mkdir(
        &mut self,
        _request: &Request<'_>,
        _parent: u64,
        _name: &OsStr,
        _mode: u32,
        _umask: u32,
        reply: ReplyEntry,
    ) {
        reply.error(libc::EROFS);
    }
    fn unlink(&mut self, _request: &Request<'_>, _parent: u64, _name: &OsStr, reply: ReplyEmpty) {
        reply.error(libc::EROFS);
    }
    fn rmdir(&mut self, _request: &Request<'_>, _parent: u64, _name: &OsStr, reply: ReplyEmpty) {
        reply.error(libc::EROFS);
    }
    fn symlink(
        &mut self,
        _request: &Request<'_>,
        _parent: u64,
        _name: &OsStr,
        _target: &Path,
        reply: ReplyEntry,
    ) {
        reply.error(libc::EROFS);
    }
    fn rename(
        &mut self,
        _request: &Request<'_>,
        _parent: u64,
        _name: &OsStr,
        _newparent: u64,
        _newname: &OsStr,
        _flags: u32,
        reply: ReplyEmpty,
    ) {
        reply.error(libc::EROFS);
    }
    fn link(
        &mut self,
        _request: &Request<'_>,
        _inode: u64,
        _parent: u64,
        _name: &OsStr,
        reply: ReplyEntry,
    ) {
        reply.error(libc::EROFS);
    }
    fn write(
        &mut self,
        _request: &Request<'_>,
        _inode: u64,
        _handle: u64,
        _offset: i64,
        _data: &[u8],
        _write_flags: u32,
        _flags: i32,
        _owner: Option<u64>,
        reply: ReplyWrite,
    ) {
        reply.error(libc::EROFS);
    }
    fn create(
        &mut self,
        _request: &Request<'_>,
        _parent: u64,
        _name: &OsStr,
        _mode: u32,
        _umask: u32,
        _flags: i32,
        reply: ReplyCreate,
    ) {
        reply.error(libc::EROFS);
    }
    fn setxattr(
        &mut self,
        _request: &Request<'_>,
        _inode: u64,
        _name: &OsStr,
        _value: &[u8],
        _flags: i32,
        _position: u32,
        reply: ReplyEmpty,
    ) {
        reply.error(libc::EROFS);
    }
    fn removexattr(
        &mut self,
        _request: &Request<'_>,
        _inode: u64,
        _name: &OsStr,
        reply: ReplyEmpty,
    ) {
        reply.error(libc::EROFS);
    }
    fn fallocate(
        &mut self,
        _request: &Request<'_>,
        _inode: u64,
        _handle: u64,
        _offset: i64,
        _length: i64,
        _mode: i32,
        reply: ReplyEmpty,
    ) {
        reply.error(libc::EROFS);
    }
    fn copy_file_range(
        &mut self,
        _request: &Request<'_>,
        _inode_in: u64,
        _handle_in: u64,
        _offset_in: i64,
        _inode_out: u64,
        _handle_out: u64,
        _offset_out: i64,
        _length: u64,
        _flags: u32,
        reply: ReplyWrite,
    ) {
        reply.error(libc::EROFS);
    }
}
