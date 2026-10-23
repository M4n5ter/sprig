//! Linux FUSE adapter for the owned writable CAS view.
//!
//! The core is the only namespace and content authority. Its monotonic inode
//! numbers are used unchanged, including for unlinked-but-open nodes. This
//! adapter owns only FUSE lookup counts and open-handle protocol state. It does
//! not maintain a second path index, dirty-file map, or materialized file tree.
//!
//! Regular files use cached write-through I/O, never WRITEBACK_CACHE. Shared
//! mappings still generate background page writes; those enter the same journal
//! at explicit offsets. A durable session guard prevents known failed or
//! abandoned sessions from silently becoming checkpoints. Successful msync/fsync
//! is the mapped-data durability boundary, not a branch commit.

use std::cell::Cell;
use std::collections::HashMap;
use std::ffi::OsStr;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use fuser::{
    FileAttr, FileType, Filesystem, KernelConfig, MountOption, ReplyAttr, ReplyCreate, ReplyData,
    ReplyDirectory, ReplyEmpty, ReplyEntry, ReplyOpen, ReplyStatfs, ReplyWrite, ReplyXattr,
    Request, TimeOrNow,
};

use crate::content::EntryKind;
use crate::projection::ReadySignal;
use crate::writable::{NodeAttr, SetAttr, WritableCas};

const TTL: Duration = Duration::ZERO;
const MAX_IO_BYTES: usize = 1024 * 1024;

struct Handle {
    inode: u64,
    core: u64,
    directory: bool,
    readable: bool,
    writable: bool,
}

pub(crate) struct WritableFuse {
    core: WritableCas,
    uid: u32,
    gid: u32,
    ready: Option<ReadySignal>,
    lookups: HashMap<u64, u64>,
    handles: HashMap<u64, Handle>,
    next_handle: u64,
    session: crate::writable_session::SessionRecord,
    generation: PathBuf,
    initialized: bool,
    failed: Cell<bool>,
}

impl WritableFuse {
    pub(crate) fn new(
        core: WritableCas,
        uid: u32,
        gid: u32,
        ready: Option<ReadySignal>,
        generation: PathBuf,
        session: crate::writable_session::SessionRecord,
    ) -> Self {
        Self {
            core,
            uid,
            gid,
            ready,
            lookups: HashMap::new(),
            handles: HashMap::new(),
            next_handle: 1,
            session,
            generation,
            initialized: false,
            failed: Cell::new(false),
        }
    }

    fn record_io_failure(&self, error: &io::Error) {
        if matches!(
            error.kind(),
            io::ErrorKind::Other
                | io::ErrorKind::InvalidData
                | io::ErrorKind::StorageFull
                | io::ErrorKind::UnexpectedEof
                | io::ErrorKind::WriteZero
        ) {
            self.failed.set(true);
        }
    }

    fn attr(&self, node: &NodeAttr) -> io::Result<FileAttr> {
        let (kind, size) = match &node.entry.kind {
            EntryKind::Directory => (FileType::Directory, 0),
            EntryKind::File { size, .. } => (FileType::RegularFile, *size),
            EntryKind::Symlink { target } => (FileType::Symlink, target.len() as u64),
        };
        let modified = system_time(node.entry.modified_ns)?;
        Ok(FileAttr {
            ino: node.inode,
            size,
            blocks: size.div_ceil(512),
            atime: system_time(node.accessed_ns)?,
            mtime: modified,
            // Revision manifests do not track ctime or creation time. This
            // backend makes no claim to independent ctime/birthtime fidelity.
            ctime: modified,
            crtime: modified,
            kind,
            perm: node.entry.mode as u16 & 0o777,
            nlink: node.nlink,
            uid: self.uid,
            gid: self.gid,
            rdev: 0,
            flags: 0,
            blksize: 4096,
        })
    }

    fn remember(&mut self, inode: u64) {
        let count = self.lookups.entry(inode).or_default();
        *count = count.saturating_add(1);
    }

    fn handle(&self, inode: u64, handle: u64, directory: bool) -> io::Result<&Handle> {
        self.handles
            .get(&handle)
            .filter(|handle| handle.inode == inode && handle.directory == directory)
            .ok_or_else(|| os_error(libc::EBADF))
    }

    fn open_handle(&mut self, inode: u64, flags: i32, directory: bool) -> io::Result<u64> {
        if !directory {
            validate_regular_flags(flags)?;
        }
        let access = flags & libc::O_ACCMODE;
        if !matches!(access, libc::O_RDONLY | libc::O_WRONLY | libc::O_RDWR) {
            return Err(os_error(libc::EINVAL));
        }
        if directory && (access != libc::O_RDONLY || flags & (libc::O_TRUNC | libc::O_APPEND) != 0)
        {
            return Err(os_error(libc::EISDIR));
        }
        let node = self.core.getattr(inode)?;
        match (&node.entry.kind, directory) {
            (EntryKind::Directory, true) | (EntryKind::File { .. }, false) => {}
            (_, true) => return Err(os_error(libc::ENOTDIR)),
            (EntryKind::Directory, false) => return Err(os_error(libc::EISDIR)),
            (EntryKind::Symlink { .. }, false) => return Err(os_error(libc::ELOOP)),
        }
        let writable = access != libc::O_RDONLY;
        if flags & libc::O_TRUNC != 0 && !writable {
            return Err(os_error(libc::EACCES));
        }
        let handle = self.next_handle;
        let next = handle
            .checked_add(1)
            .ok_or_else(|| os_error(libc::EMFILE))?;
        let core = self.core.open_handle(inode, writable)?;
        // FUSE_ATOMIC_O_TRUNC is not requested, so ordinary opens arrive after
        // the kernel's setattr. Handle an explicitly supplied flag defensively.
        if flags & libc::O_TRUNC != 0
            && let Err(error) = self.core.setattr(
                inode,
                SetAttr {
                    size: Some(0),
                    ..SetAttr::default()
                },
            )
        {
            let _ = self.core.release(core);
            return Err(error);
        }
        self.next_handle = next;
        self.handles.insert(
            handle,
            Handle {
                inode,
                core,
                directory,
                readable: access != libc::O_WRONLY,
                writable,
            },
        );
        Ok(handle)
    }

    fn close_handle(
        &mut self,
        inode: u64,
        handle: u64,
        directory: bool,
        flush: bool,
    ) -> io::Result<()> {
        if let Err(error) = self.handle(inode, handle, directory) {
            self.failed.set(true);
            return Err(error);
        }
        let handle = self
            .handles
            .remove(&handle)
            .ok_or_else(|| os_error(libc::EBADF))?;
        let synced = if flush { self.core.sync() } else { Ok(()) };
        let released = self.core.release(handle.core);
        let result = synced.and(released);
        if result.is_err() {
            self.failed.set(true);
        }
        result
    }

    fn sync_handle(&self, inode: u64, handle: u64, directory: bool) -> io::Result<()> {
        if let Err(error) = self.handle(inode, handle, directory) {
            self.failed.set(true);
            return Err(error);
        }
        let result = self.core.sync();
        if result.is_err() {
            self.failed.set(true);
        }
        result
    }
}

/// No allow-other, helper installation, or security-setting changes are used.
pub fn mount_options() -> Vec<MountOption> {
    vec![
        MountOption::FSName("sprig-writable-cas".into()),
        MountOption::RW,
        MountOption::DefaultPermissions,
        MountOption::NoSuid,
        MountOption::NoDev,
        MountOption::NoAtime,
    ]
}

fn os_error(errno: i32) -> io::Error {
    io::Error::from_raw_os_error(errno)
}

fn errno(error: &io::Error) -> i32 {
    error.raw_os_error().unwrap_or(match error.kind() {
        io::ErrorKind::NotFound => libc::ENOENT,
        io::ErrorKind::PermissionDenied => libc::EACCES,
        io::ErrorKind::AlreadyExists => libc::EEXIST,
        io::ErrorKind::InvalidInput => libc::EINVAL,
        io::ErrorKind::Unsupported => libc::EOPNOTSUPP,
        io::ErrorKind::StorageFull => libc::ENOSPC,
        io::ErrorKind::ReadOnlyFilesystem => libc::EROFS,
        io::ErrorKind::FileTooLarge => libc::EFBIG,
        _ => libc::EIO,
    })
}

fn name(name: &OsStr) -> io::Result<&str> {
    if name.len() > 255 {
        return Err(os_error(libc::ENAMETOOLONG));
    }
    name.to_str().ok_or_else(|| os_error(libc::EILSEQ))
}

fn validate_regular_flags(flags: i32) -> io::Result<()> {
    let access = flags & libc::O_ACCMODE;
    if access == libc::O_ACCMODE {
        return Err(os_error(libc::EINVAL));
    }
    if flags & libc::O_TMPFILE == libc::O_TMPFILE || flags & libc::O_PATH != 0 {
        return Err(os_error(libc::EOPNOTSUPP));
    }
    if flags & libc::O_DIRECTORY != 0 {
        return Err(os_error(libc::ENOTDIR));
    }
    if flags & libc::O_TRUNC != 0 && access == libc::O_RDONLY {
        return Err(os_error(libc::EACCES));
    }
    Ok(())
}

fn system_time(nanoseconds: i128) -> io::Result<SystemTime> {
    // fuser 0.16 cannot correctly encode negative fractional timestamps.
    if nanoseconds < 0 {
        return Err(os_error(libc::EOVERFLOW));
    }
    let seconds =
        u64::try_from(nanoseconds / 1_000_000_000).map_err(|_| os_error(libc::EOVERFLOW))?;
    UNIX_EPOCH
        .checked_add(Duration::new(seconds, (nanoseconds % 1_000_000_000) as u32))
        .ok_or_else(|| os_error(libc::EOVERFLOW))
}

fn timestamp(time: TimeOrNow) -> io::Result<i128> {
    let time = match time {
        TimeOrNow::SpecificTime(time) => time,
        TimeOrNow::Now => SystemTime::now(),
    };
    let duration = time
        .duration_since(UNIX_EPOCH)
        .map_err(|_| os_error(libc::EOVERFLOW))?;
    Ok(duration.as_secs() as i128 * 1_000_000_000 + duration.subsec_nanos() as i128)
}

fn reply_empty(reply: ReplyEmpty, result: io::Result<()>) {
    match result {
        Ok(()) => reply.ok(),
        Err(error) => reply.error(errno(&error)),
    }
}

// The callback argument lists are imposed by the FUSE protocol.
#[allow(clippy::too_many_arguments)]
impl Filesystem for WritableFuse {
    fn init(&mut self, _request: &Request<'_>, config: &mut KernelConfig) -> Result<(), i32> {
        config
            .set_max_write(MAX_IO_BYTES as u32)
            .map_err(|_| libc::EINVAL)?;
        let _ = config.set_max_readahead(MAX_IO_BYTES as u32);
        // fuser 0.16's Linux defaults request only ASYNC_READ, BIG_WRITES and,
        // where supported, MAX_PAGES. Do not add WRITEBACK_CACHE, POSIX_ACL,
        // ASYNC_DIO, or a shared-mmap/direct-I/O capability here.
        if let Some(ready) = self.ready.take() {
            ready().map_err(|error| errno(&error))?;
        }
        self.initialized = true;
        Ok(())
    }

    fn destroy(&mut self) {
        // fuser also invokes this callback after an abnormal session exit.
        // It is only a quiescence certificate, never authority to clear the
        // durable marker: the controller must itself complete normal unmount.
        let result = (|| {
            self.core.sync()?;
            if !self.initialized
                || self.failed.get()
                || !self.handles.is_empty()
                || self.core.recovery_info()?.read_only
            {
                return Err(io::Error::other("mapped session did not finish cleanly"));
            }
            if crate::writable_session::read_stop(&self.generation, &self.session)? {
                crate::writable_session::acknowledge(&self.generation, &self.session)?;
            }
            Ok(())
        })();
        if let Err(error) = result {
            self.failed.set(true);
            eprintln!(
                "{}",
                serde_json::json!({"event":"writable_session_retained","error":error.to_string()})
            );
        }
    }

    fn lookup(&mut self, _request: &Request<'_>, parent: u64, child: &OsStr, reply: ReplyEntry) {
        match name(child)
            .and_then(|name| self.core.lookup(parent, name))
            .and_then(|node| self.attr(&node))
        {
            Ok(attr) => {
                self.remember(attr.ino);
                reply.entry(&TTL, &attr, 1);
            }
            Err(error) => reply.error(errno(&error)),
        }
    }

    fn forget(&mut self, _request: &Request<'_>, inode: u64, count: u64) {
        if let Some(lookups) = self.lookups.get_mut(&inode) {
            *lookups = lookups.saturating_sub(count);
            if *lookups == 0 {
                self.lookups.remove(&inode);
            }
        }
        // Core node identities are never recycled, even after the final lookup
        // and handle disappear. Forget therefore cannot evict an open orphan.
    }

    fn getattr(
        &mut self,
        _request: &Request<'_>,
        inode: u64,
        handle: Option<u64>,
        reply: ReplyAttr,
    ) {
        let result = (|| {
            if let Some(handle) = handle
                && self
                    .handles
                    .get(&handle)
                    .is_none_or(|handle| handle.inode != inode)
            {
                return Err(os_error(libc::EBADF));
            }
            self.attr(&self.core.getattr(inode)?)
        })();
        match result {
            Ok(attr) => reply.attr(&TTL, &attr),
            Err(error) => reply.error(errno(&error)),
        }
    }

    fn setattr(
        &mut self,
        _request: &Request<'_>,
        inode: u64,
        mode: Option<u32>,
        uid: Option<u32>,
        gid: Option<u32>,
        size: Option<u64>,
        atime: Option<TimeOrNow>,
        mtime: Option<TimeOrNow>,
        _ctime: Option<SystemTime>,
        handle: Option<u64>,
        crtime: Option<SystemTime>,
        chgtime: Option<SystemTime>,
        bkuptime: Option<SystemTime>,
        flags: Option<u32>,
        reply: ReplyAttr,
    ) {
        let result = (|| {
            // Reject unsupported fields before applying any supported field.
            if uid.is_some()
                || gid.is_some()
                || crtime.is_some()
                || chgtime.is_some()
                || bkuptime.is_some()
                || flags.is_some()
                || mode.is_some_and(|mode| mode & 0o7000 != 0)
            {
                return Err(os_error(libc::EOPNOTSUPP));
            }
            if let Some(handle) = handle {
                let opened = self
                    .handles
                    .get(&handle)
                    .filter(|handle| handle.inode == inode)
                    .ok_or_else(|| os_error(libc::EBADF))?;
                if size.is_some() && !opened.writable {
                    return Err(os_error(libc::EBADF));
                }
            }
            let attrs = SetAttr {
                mode: mode.map(|mode| mode & 0o777),
                size,
                modified_ns: mtime.map(timestamp).transpose()?,
                accessed_ns: atime.map(timestamp).transpose()?,
            };
            self.attr(&self.core.setattr(inode, attrs)?)
        })();
        match result {
            Ok(attr) => reply.attr(&TTL, &attr),
            Err(error) => {
                self.record_io_failure(&error);
                reply.error(errno(&error));
            }
        }
    }

    fn readlink(&mut self, _request: &Request<'_>, inode: u64, reply: ReplyData) {
        match self.core.getattr(inode) {
            Ok(NodeAttr {
                entry:
                    crate::content::Entry {
                        kind: EntryKind::Symlink { target },
                        ..
                    },
                ..
            }) => {
                if target.len() > 4095 {
                    reply.error(libc::ENAMETOOLONG);
                } else {
                    reply.data(&target);
                }
            }
            Ok(_) => reply.error(libc::EINVAL),
            Err(error) => reply.error(errno(&error)),
        }
    }

    fn open(&mut self, _request: &Request<'_>, inode: u64, flags: i32, reply: ReplyOpen) {
        match self.open_handle(inode, flags, false) {
            Ok(handle) => reply.opened(handle, fuser::consts::FOPEN_KEEP_CACHE),
            Err(error) => reply.error(errno(&error)),
        }
    }

    fn opendir(&mut self, _request: &Request<'_>, inode: u64, flags: i32, reply: ReplyOpen) {
        match self.open_handle(inode, flags, true) {
            Ok(handle) => reply.opened(handle, 0),
            Err(error) => reply.error(errno(&error)),
        }
    }

    fn read(
        &mut self,
        _request: &Request<'_>,
        inode: u64,
        handle: u64,
        offset: i64,
        size: u32,
        _flags: i32,
        _owner: Option<u64>,
        reply: ReplyData,
    ) {
        let result = (|| {
            let offset = u64::try_from(offset).map_err(|_| os_error(libc::EINVAL))?;
            if size as usize > MAX_IO_BYTES {
                return Err(os_error(libc::EINVAL));
            }
            let handle = self.handle(inode, handle, false)?;
            if !handle.readable {
                return Err(os_error(libc::EBADF));
            }
            self.core.read(handle.core, offset, size as usize)
        })();
        match result {
            Ok(bytes) => reply.data(&bytes),
            Err(error) => {
                self.record_io_failure(&error);
                reply.error(errno(&error));
            }
        }
    }

    fn write(
        &mut self,
        _request: &Request<'_>,
        inode: u64,
        handle: u64,
        offset: i64,
        data: &[u8],
        write_flags: u32,
        flags: i32,
        _owner: Option<u64>,
        reply: ReplyWrite,
    ) {
        let result = (|| {
            let offset = u64::try_from(offset).map_err(|_| os_error(libc::EINVAL))?;
            if data.len() > MAX_IO_BYTES {
                return Err(os_error(libc::EFBIG));
            }

            let handle = self.handle(inode, handle, false)?;
            if !handle.writable {
                return Err(os_error(libc::EBADF));
            }
            // Use current file flags, not the original open flags: F_SETFL can
            // enable or disable O_APPEND without another FUSE open callback.
            let written = if write_flags & fuser::consts::FUSE_WRITE_CACHE == 0
                && flags & libc::O_APPEND != 0
            {
                self.core.write_append(handle.core, data)?
            } else {
                self.core.write(handle.core, offset, data)?
            };
            if flags & (libc::O_SYNC | libc::O_DSYNC) != 0 {
                self.core.sync()?;
            }
            Ok(written as u32)
        })();
        match result {
            Ok(length) => reply.written(length),
            Err(error) => {
                self.failed.set(true);
                reply.error(errno(&error));
            }
        }
    }

    fn readdir(
        &mut self,
        _request: &Request<'_>,
        inode: u64,
        handle: u64,
        offset: i64,
        mut reply: ReplyDirectory,
    ) {
        let result = (|| {
            self.handle(inode, handle, true)?;
            let offset = usize::try_from(offset).map_err(|_| os_error(libc::EINVAL))?;
            let parent = self.core.lookup(inode, "..")?.inode;
            Ok((offset, parent, self.core.readdir(inode)?))
        })();
        let (offset, parent, children) = match result {
            Ok(entries) => entries,
            Err(error) => {
                reply.error(errno(&error));
                return;
            }
        };
        // Cookies are positions in the core's path-ordered listing. As on an
        // ordinary directory, mutation while enumerating has unspecified order;
        // no stale namespace snapshot becomes authoritative in this adapter.
        let dots = [
            (inode, FileType::Directory, "."),
            (parent, FileType::Directory, ".."),
        ];
        let entries = dots.into_iter().chain(children.iter().map(|(name, node)| {
            let kind = match node.entry.kind {
                EntryKind::Directory => FileType::Directory,
                EntryKind::File { .. } => FileType::RegularFile,
                EntryKind::Symlink { .. } => FileType::Symlink,
            };
            (node.inode, kind, name.as_str())
        }));
        for (index, (inode, kind, name)) in entries.enumerate().skip(offset) {
            if reply.add(inode, (index + 1) as i64, kind, name) {
                break;
            }
        }
        reply.ok();
    }

    fn access(&mut self, request: &Request<'_>, inode: u64, mask: i32, reply: ReplyEmpty) {
        let result = (|| {
            let attr = self.attr(&self.core.getattr(inode)?)?;
            if mask & !(libc::R_OK | libc::W_OK | libc::X_OK) != 0 {
                return Err(os_error(libc::EINVAL));
            }
            if mask & libc::W_OK != 0 && self.core.recovery_info()?.read_only {
                return Err(os_error(libc::EROFS));
            }
            let requested = mask as u16;
            let permission = if request.uid() == attr.uid {
                attr.perm >> 6
            } else if request.gid() == attr.gid {
                attr.perm >> 3
            } else {
                attr.perm
            } & 0o7;
            // default_permissions performs the real kernel permission check,
            // including supplementary groups. This fallback is conservative.
            if request.uid() == 0 {
                if mask & libc::X_OK == 0
                    || attr.kind == FileType::Directory
                    || attr.perm & 0o111 != 0
                {
                    return Ok(());
                }
            } else if permission & requested == requested {
                return Ok(());
            }
            Err(os_error(libc::EACCES))
        })();
        reply_empty(reply, result);
    }

    fn statfs(&mut self, _request: &Request<'_>, _inode: u64, reply: ReplyStatfs) {
        match self.core.stats() {
            Ok(stats) => {
                // An advisory upper bound only: a write also needs journal
                // framing and object-store space. No reservation is implied.
                let available = stats
                    .logical_limit
                    .saturating_sub(stats.logical_bytes)
                    .min(stats.journal_limit.saturating_sub(stats.journal_bytes));
                reply.statfs(
                    stats.logical_limit / 4096,
                    available / 4096,
                    available / 4096,
                    stats.node_limit,
                    stats.node_limit.saturating_sub(stats.nodes),
                    4096,
                    255,
                    4096,
                );
            }
            Err(error) => reply.error(errno(&error)),
        }
    }

    fn flush(
        &mut self,
        _request: &Request<'_>,
        inode: u64,
        handle: u64,
        _owner: u64,
        reply: ReplyEmpty,
    ) {
        reply_empty(reply, self.sync_handle(inode, handle, false));
    }

    fn fsync(
        &mut self,
        _request: &Request<'_>,
        inode: u64,
        handle: u64,
        _datasync: bool,
        reply: ReplyEmpty,
    ) {
        reply_empty(reply, self.sync_handle(inode, handle, false));
    }

    fn fsyncdir(
        &mut self,
        _request: &Request<'_>,
        inode: u64,
        handle: u64,
        _datasync: bool,
        reply: ReplyEmpty,
    ) {
        reply_empty(reply, self.sync_handle(inode, handle, true));
    }

    fn release(
        &mut self,
        _request: &Request<'_>,
        inode: u64,
        handle: u64,
        _flags: i32,
        _owner: Option<u64>,
        flush: bool,
        reply: ReplyEmpty,
    ) {
        reply_empty(reply, self.close_handle(inode, handle, false, flush));
    }

    fn releasedir(
        &mut self,
        _request: &Request<'_>,
        inode: u64,
        handle: u64,
        _flags: i32,
        reply: ReplyEmpty,
    ) {
        reply_empty(reply, self.close_handle(inode, handle, true, false));
    }

    fn create(
        &mut self,
        _request: &Request<'_>,
        parent: u64,
        child: &OsStr,
        mode: u32,
        umask: u32,
        flags: i32,
        reply: ReplyCreate,
    ) {
        let result = (|| {
            if mode & 0o7000 != 0 {
                return Err(os_error(libc::EOPNOTSUPP));
            }
            validate_regular_flags(flags)?;
            self.next_handle
                .checked_add(1)
                .ok_or_else(|| os_error(libc::EMFILE))?;
            let node = self
                .core
                .create_file(parent, name(child)?, mode & !umask & 0o777)?;
            let attr = self.attr(&node)?;
            // A later resource/journal failure is returned explicitly. Do not
            // disguise it as success or attempt an unsafe compensating unlink.
            let handle = self.open_handle(node.inode, flags, false)?;
            Ok((attr, handle))
        })();
        match result {
            Ok((attr, handle)) => {
                self.remember(attr.ino);
                reply.created(&TTL, &attr, 1, handle, fuser::consts::FOPEN_KEEP_CACHE);
            }
            Err(error) => reply.error(errno(&error)),
        }
    }

    fn mkdir(
        &mut self,
        _request: &Request<'_>,
        parent: u64,
        child: &OsStr,
        mode: u32,
        umask: u32,
        reply: ReplyEntry,
    ) {
        let result = (|| {
            if mode & 0o7000 != 0 {
                return Err(os_error(libc::EOPNOTSUPP));
            }
            self.attr(
                &self
                    .core
                    .mkdir(parent, name(child)?, mode & !umask & 0o777)?,
            )
        })();
        match result {
            Ok(attr) => {
                self.remember(attr.ino);
                reply.entry(&TTL, &attr, 1);
            }
            Err(error) => reply.error(errno(&error)),
        }
    }

    fn symlink(
        &mut self,
        _request: &Request<'_>,
        parent: u64,
        child: &OsStr,
        target: &Path,
        reply: ReplyEntry,
    ) {
        if target.as_os_str().len() > 4095 {
            reply.error(libc::ENAMETOOLONG);
            return;
        }
        let result = name(child)
            .and_then(|name| {
                self.core
                    .symlink(parent, name, target.as_os_str().as_bytes())
            })
            .and_then(|node| self.attr(&node));
        match result {
            Ok(attr) => {
                self.remember(attr.ino);
                reply.entry(&TTL, &attr, 1);
            }
            Err(error) => reply.error(errno(&error)),
        }
    }

    fn unlink(&mut self, _request: &Request<'_>, parent: u64, child: &OsStr, reply: ReplyEmpty) {
        reply_empty(
            reply,
            name(child).and_then(|name| self.core.unlink(parent, name)),
        );
    }

    fn rmdir(&mut self, _request: &Request<'_>, parent: u64, child: &OsStr, reply: ReplyEmpty) {
        reply_empty(
            reply,
            name(child).and_then(|name| self.core.rmdir(parent, name)),
        );
    }

    fn rename(
        &mut self,
        _request: &Request<'_>,
        parent: u64,
        child: &OsStr,
        new_parent: u64,
        new_child: &OsStr,
        flags: u32,
        reply: ReplyEmpty,
    ) {
        let result = (|| {
            if flags != 0 {
                return Err(os_error(libc::EOPNOTSUPP));
            }
            self.core
                .rename(parent, name(child)?, new_parent, name(new_child)?)
        })();
        reply_empty(reply, result);
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
        reply.error(libc::EOPNOTSUPP);
    }

    fn link(
        &mut self,
        _request: &Request<'_>,
        inode: u64,
        parent: u64,
        child: &OsStr,
        reply: ReplyEntry,
    ) {
        let result = (|| {
            let node = self.core.link(inode, parent, name(child)?)?;
            self.attr(&node)
        })();
        match result {
            Ok(attr) => {
                self.remember(attr.ino);
                reply.entry(&TTL, &attr, 0);
            }
            Err(error) => reply.error(errno(&error)),
        }
    }

    fn getxattr(
        &mut self,
        _request: &Request<'_>,
        _inode: u64,
        _name: &OsStr,
        _size: u32,
        reply: ReplyXattr,
    ) {
        reply.error(libc::EOPNOTSUPP);
    }

    fn listxattr(&mut self, _request: &Request<'_>, _inode: u64, _size: u32, reply: ReplyXattr) {
        reply.error(libc::EOPNOTSUPP);
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
        reply.error(libc::EOPNOTSUPP);
    }

    fn removexattr(
        &mut self,
        _request: &Request<'_>,
        _inode: u64,
        _name: &OsStr,
        reply: ReplyEmpty,
    ) {
        reply.error(libc::EOPNOTSUPP);
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
        reply.error(libc::EOPNOTSUPP);
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
        reply.error(libc::EOPNOTSUPP);
    }
}
