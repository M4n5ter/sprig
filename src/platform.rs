//! Filesystem operations with explicit platform guarantees.
//!
//! Windows supports ordinary writable files and directories on local NTFS/ReFS
//! disks. File identities retain all 128 bits on Windows; ReFS block clones
//! share physical extents, never a mutable file identity.
//! Existing reparse-point leaves and ancestors are rejected. Paths must stay
//! trusted and quiescent: this is not a sandbox against concurrent ancestor
//! replacement. Windows has no unprivileged
//! POSIX directory-fsync equivalent: file contents are flushed and publication
//! uses write-through rename or checked file flushes around immutable-index
//! replacement. Equal power-loss namespace durability is not claimed. Directory
//! permissions inherit the user's existing ACLs.

use std::fs::{self, File, OpenOptions};
use std::io;
use std::path::Path;

#[cfg(unix)]
pub(crate) use std::fs::Metadata;
#[cfg(unix)]
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
#[cfg(windows)]
pub(crate) use windows::Metadata;

/// Full physical identity, widened losslessly on Unix and native 128-bit on Windows.
pub(crate) fn inode(metadata: &Metadata) -> u128 {
    #[cfg(unix)]
    {
        u128::from(std::os::unix::fs::MetadataExt::ino(metadata))
    }
    #[cfg(windows)]
    {
        metadata.ino()
    }
}

pub(crate) fn symlink_metadata(path: impl AsRef<Path>) -> io::Result<Metadata> {
    #[cfg(unix)]
    {
        fs::symlink_metadata(path)
    }
    #[cfg(windows)]
    {
        windows::metadata(&windows::open_node(
            path.as_ref(),
            windows::READ_ATTRIBUTES,
        )?)
    }
}

pub(crate) fn file_metadata(file: &File) -> io::Result<Metadata> {
    #[cfg(unix)]
    {
        file.metadata()
    }
    #[cfg(windows)]
    {
        windows::metadata(file)
    }
}

pub(crate) fn create_dir(path: &Path, mode: u32) -> io::Result<()> {
    #[cfg(unix)]
    {
        fs::DirBuilder::new().mode(mode).create(path)
    }
    #[cfg(windows)]
    {
        let _ = mode;
        windows::validate_ancestors(path)?;
        fs::create_dir(path)
    }
}

pub(crate) fn open_regular(path: &Path) -> io::Result<File> {
    #[cfg(unix)]
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)?;
    #[cfg(windows)]
    let file = windows::open_node(path, windows::GENERIC_READ)?;
    if !file_metadata(&file)?.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "expected a regular file",
        ));
    }
    Ok(file)
}

pub(crate) fn open_read_write(path: &Path) -> io::Result<File> {
    #[cfg(unix)]
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)?;
    #[cfg(windows)]
    let file = windows::open_node(path, windows::GENERIC_READ | windows::GENERIC_WRITE)?;
    if !file_metadata(&file)?.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "expected a regular file",
        ));
    }
    Ok(file)
}

/// A Windows share-deny handle remains exclusive in inheriting processes even
/// after the process that acquired a byte-range lock exits.
pub(crate) fn open_lease(path: &Path) -> io::Result<File> {
    #[cfg(unix)]
    {
        open_read_write(path)
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        windows::validate_ancestors(path)?;
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .share_mode(0)
            .custom_flags(windows::OPEN_REPARSE_POINT)
            .open(path)?;
        if !file_metadata(&file)?.is_file() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "lease must be a regular file",
            ));
        }
        Ok(file)
    }
}

/// Serializes Sprig-managed Windows spawns and inherits only a short-lived
/// duplicate of the lease. Stable Rust Command inherits marked handles. An
/// unrelated SDK-host thread spawning outside this mutex can also inherit the
/// duplicate and conservatively prolong workspace_busy. Never force-unlock it.
#[cfg(windows)]
pub(crate) fn spawn_leased(
    command: &mut std::process::Command,
    lease: &File,
) -> io::Result<std::process::Child> {
    static SPAWN_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _guard = SPAWN_LOCK
        .lock()
        .map_err(|_| io::Error::other("spawn lock poisoned"))?;
    let inherited = lease.try_clone()?;
    windows::make_inheritable(&inherited)?;
    let result = command.spawn();
    drop(inherited);
    result
}

pub(crate) fn create_new(path: &Path, mode: u32) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    options.mode(mode).custom_flags(libc::O_NOFOLLOW);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        let _ = mode;
        windows::validate_ancestors(path)?;
        options.custom_flags(windows::OPEN_REPARSE_POINT);
    }
    let file = options.open(path)?;
    if !file_metadata(&file)?.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "expected a new regular file",
        ));
    }
    Ok(file)
}

/// Creates a new name for a regular file in a trusted, quiescent tree. Callers
/// must choose a source within the fresh destination, never another workspace.
pub(crate) fn hard_link(source: &Path, target: &Path) -> io::Result<()> {
    #[cfg(unix)]
    use std::os::unix::fs::MetadataExt;

    let before = symlink_metadata(source)?;
    if !before.is_file() || before.file_type().is_symlink() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "hard-link source must be a regular file",
        ));
    }
    validate_capture_metadata(source, &before)?;
    #[cfg(windows)]
    windows::validate_ancestors(target)?;
    // std::fs::hard_link requires a new destination name. Neither platform is
    // allowed to follow a reparse/symbolic-link source in this checked flow.
    fs::hard_link(source, target)?;
    let linked = symlink_metadata(target)?;
    if !linked.is_file()
        || linked.file_type().is_symlink()
        || linked.dev() != before.dev()
        || linked.ino() != before.ino()
        || linked.len() != before.len()
        || linked.mode() != before.mode()
        || linked.mtime() != before.mtime()
        || linked.mtime_nsec() != before.mtime_nsec()
    {
        return Err(io::Error::other("source changed while creating hard link"));
    }
    Ok(())
}

pub(crate) fn open_directory(path: &Path) -> io::Result<File> {
    #[cfg(unix)]
    let file = File::open(path)?;
    #[cfg(windows)]
    let file = windows::open_node(path, windows::READ_ATTRIBUTES | windows::WRITE_ATTRIBUTES)?;
    if !file_metadata(&file)?.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "expected a real directory",
        ));
    }
    Ok(file)
}

pub(crate) fn set_mode(file: &File, mode: u32) -> io::Result<()> {
    #[cfg(unix)]
    {
        file.set_permissions(fs::Permissions::from_mode(mode))
    }
    #[cfg(windows)]
    {
        // Only staging modes and the canonical Windows modes are accepted.
        // These are not an ACL emulation; chmod is separately unsupported.
        let metadata = file_metadata(file)?;
        if (metadata.is_dir() && matches!(mode, 0o700 | 0o755))
            || (metadata.is_file() && matches!(mode, 0o600 | 0o644))
        {
            Ok(())
        } else {
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "POSIX permissions are not supported on Windows",
            ))
        }
    }
}

pub(crate) fn set_path_mode(path: &Path, mode: u32) -> io::Result<()> {
    #[cfg(unix)]
    {
        fs::set_permissions(path, fs::Permissions::from_mode(mode))
    }
    #[cfg(windows)]
    {
        set_mode(&windows::open_node(path, windows::READ_ATTRIBUTES)?, mode)
    }
}

pub(crate) fn protect_object(file: &File) -> io::Result<()> {
    #[cfg(unix)]
    {
        set_mode(file, 0o444)
    }
    #[cfg(windows)]
    {
        // Windows read-only attributes also prevent deletion of temporary hard
        // links. Immutability is enforced by the store, not by an ACL change.
        file_metadata(file).map(|_| ())
    }
}

pub(crate) fn sync_directory(path: &Path) -> io::Result<()> {
    let directory = open_directory(path)?;
    sync_directory_file(&directory)
}

pub(crate) fn sync_directory_file(directory: &File) -> io::Result<()> {
    #[cfg(unix)]
    directory.sync_all()?;
    #[cfg(windows)]
    if !file_metadata(directory)?.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "expected a directory",
        ));
    }
    Ok(())
}

pub(crate) fn sync_file(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    let file = open_regular(path)?;
    #[cfg(windows)]
    let file = open_read_write(path)?; // FlushFileBuffers requires GENERIC_WRITE.
    file.sync_all()
}

pub(crate) fn rename(source: &Path, target: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        fs::rename(source, target)
    }
    #[cfg(windows)]
    {
        windows::rename(source, target)
    }
}

/// Publishes an immutable index without invalidating readers of its old file.
/// Windows falls back to POSIX replacement with checked file/metadata flushes,
/// not a directory-fsync guarantee. An error may follow successful publication.
pub(crate) fn replace_immutable_file(source: &Path, target: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        rename(source, target)
    }
    #[cfg(windows)]
    {
        match rename(source, target) {
            Ok(()) => return Ok(()),
            Err(error) if error.raw_os_error() == Some(5) => {}
            Err(error) => return Err(error),
        }
        // These helpers check ancestors, reject reparse points, and require
        // local NTFS/ReFS. Keep the source handle through publication and its flush.
        let source_file = open_read_write(source)?;
        let source_metadata = file_metadata(&source_file)?;
        let target_metadata = symlink_metadata(target)?;
        if !target_metadata.is_file() || target_metadata.permissions().readonly() {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "immutable index replacement requires a writable regular target file",
            ));
        }
        if source_metadata.dev() != target_metadata.dev() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "immutable index replacement requires the same local volume",
            ));
        }
        source_file.sync_all()?;
        // Rust 1.89+ uses FileRenameInfoEx POSIX semantics after AccessDenied.
        // Windows 10 1607+ with filesystem support is required; errors are not
        // emulated by copying, deleting, or closing pinned readers. Std opens
        // its own rename handle, so the explicit post-rename flush is required.
        fs::rename(source, target)?;
        source_file.sync_all()
    }
}

pub(crate) fn validate_relative_path(path: &str) -> io::Result<()> {
    #[cfg(unix)]
    let _ = path;
    #[cfg(windows)]
    for part in path.split('/').filter(|part| !part.is_empty()) {
        windows::validate_component(part)?;
    }
    Ok(())
}

pub(crate) fn validate_capture_metadata(path: &Path, metadata: &Metadata) -> io::Result<()> {
    #[cfg(unix)]
    let _ = (path, metadata);
    #[cfg(windows)]
    {
        if metadata.is_file() && metadata.permissions().readonly() {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "read-only files are not supported by the Windows native backend",
            ));
        }
        windows::reject_named_streams(path)?;
    }
    Ok(())
}

/// Referenced allocated bytes, not exclusive physical extents on CoW filesystems.
pub(crate) fn allocated_bytes(path: &Path, metadata: &Metadata) -> io::Result<u64> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let _ = path;
        metadata
            .blocks()
            .checked_mul(512)
            .ok_or_else(|| io::Error::other("allocation overflow"))
    }
    #[cfg(windows)]
    {
        let _ = metadata;
        windows::allocation(path)
    }
}

/// Attempts an independent ReFS block clone, preserving the supplied source
/// handle's cursor. A subcluster tail is copied, never cloned beyond source EOF.
/// Nonempty subcluster files are unsupported rather than counted as clones.
/// `None` permits ordinary-copy fallback; operational/partial-clone errors do
/// not. Targets are always create-new, including after a fallback.
#[cfg(windows)]
pub(crate) fn native_clone(
    source: &File,
    source_path: &Path,
    target: &Path,
) -> io::Result<Option<File>> {
    windows::native_clone(source, source_path, target)
}

#[cfg(windows)]
pub(crate) fn filesystem_name(path: &Path) -> io::Result<&'static str> {
    let file = windows::open_node(path, windows::READ_ATTRIBUTES)?;
    Ok(windows::volume_info(&file)?.name)
}

#[cfg(windows)]
pub(crate) fn supports_hard_links(path: &Path) -> io::Result<bool> {
    let file = windows::open_node(path, windows::READ_ATTRIBUTES)?;
    Ok(windows::volume_info(&file)?.flags & 0x0040_0000 != 0)
}

#[cfg(windows)]
pub(crate) fn supports_block_cloning(path: &Path) -> io::Result<bool> {
    let file = windows::open_node(path, windows::READ_ATTRIBUTES)?;
    let volume = windows::volume_info(&file)?;
    Ok(volume.name == "ReFS" && volume.flags & 0x0800_0000 != 0)
}

#[cfg(windows)]
mod windows {
    use super::*;
    use std::ffi::c_void;
    use std::ops::Deref;
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::fs::{FileExt, MetadataExt, OpenOptionsExt};
    use std::os::windows::io::AsRawHandle;
    use std::path::{Component, Prefix};

    pub(super) const GENERIC_READ: u32 = 0x8000_0000;
    pub(super) const GENERIC_WRITE: u32 = 0x4000_0000;
    pub(super) const READ_ATTRIBUTES: u32 = 0x0080;
    pub(super) const WRITE_ATTRIBUTES: u32 = 0x0100;
    pub(super) const OPEN_REPARSE_POINT: u32 = 0x0020_0000;
    const BACKUP_SEMANTICS: u32 = 0x0200_0000;
    const REPARSE_POINT: u32 = 0x0400;
    const SPARSE_FILE: u32 = 0x0200;
    const DELETE_ACCESS: u32 = 0x0001_0000;
    const SHARE_READ: u32 = 1;
    const SUPPORTS_BLOCK_REFCOUNTING: u32 = 0x0800_0000;
    const FSCTL_DUPLICATE_EXTENTS_TO_FILE: u32 = 0x0009_8344;
    const FSCTL_GET_INTEGRITY_INFORMATION: u32 = 0x0009_027c;
    const FSCTL_SET_SPARSE: u32 = 0x0009_00c4;
    const EPOCH_TICKS: i128 = 116_444_736_000_000_000;

    #[repr(C)]
    #[derive(Default)]
    struct FileIdInfo {
        volume: u64,
        identifier: [u8; 16],
    }
    #[repr(C)]
    #[derive(Default)]
    struct BasicInfo {
        creation: i64,
        access: i64,
        write: i64,
        change: i64,
        attributes: u32,
    }

    #[repr(C)]
    #[derive(Default)]
    struct IntegrityInfo {
        checksum_algorithm: u16,
        reserved: u16,
        flags: u32,
        checksum_chunk_size: u32,
        cluster_size: u32,
    }

    #[repr(C)]
    struct DuplicateExtents {
        source: *mut c_void,
        source_offset: i64,
        target_offset: i64,
        byte_count: i64,
    }

    const _: () = {
        assert!(std::mem::size_of::<FileIdInfo>() == 24);
        assert!(std::mem::offset_of!(FileIdInfo, identifier) == 8);
        assert!(std::mem::size_of::<IntegrityInfo>() == 16);
        assert!(std::mem::offset_of!(IntegrityInfo, cluster_size) == 12);
        assert!(std::mem::size_of::<DuplicateExtents>() == 32);
        assert!(std::mem::offset_of!(DuplicateExtents, source_offset) == 8);
        assert!(std::mem::offset_of!(DuplicateExtents, target_offset) == 16);
        assert!(std::mem::offset_of!(DuplicateExtents, byte_count) == 24);
    };

    pub(super) struct VolumeInfo {
        pub(super) name: &'static str,
        pub(super) flags: u32,
    }

    #[repr(C)]
    struct StreamInfo {
        size: i64,
        name: [u16; 296], // MAX_PATH + 36, per WIN32_FIND_STREAM_DATA.
    }

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetFileInformationByHandleEx(
            handle: *mut c_void,
            class: i32,
            info: *mut c_void,
            size: u32,
        ) -> i32;
        fn SetFileInformationByHandle(
            handle: *mut c_void,
            class: i32,
            info: *const c_void,
            size: u32,
        ) -> i32;
        fn DeviceIoControl(
            handle: *mut c_void,
            control: u32,
            input: *const c_void,
            input_size: u32,
            output: *mut c_void,
            output_size: u32,
            returned: *mut u32,
            overlapped: *mut c_void,
        ) -> i32;
        fn MoveFileExW(source: *const u16, target: *const u16, flags: u32) -> i32;
        fn SetHandleInformation(handle: *mut c_void, mask: u32, flags: u32) -> i32;
        fn GetVolumeInformationByHandleW(
            handle: *mut c_void,
            name: *mut u16,
            name_size: u32,
            serial: *mut u32,
            max_component: *mut u32,
            flags: *mut u32,
            filesystem: *mut u16,
            filesystem_size: u32,
        ) -> i32;
        fn FindFirstStreamW(
            path: *const u16,
            level: i32,
            info: *mut StreamInfo,
            flags: u32,
        ) -> *mut c_void;
        fn FindNextStreamW(handle: *mut c_void, info: *mut StreamInfo) -> i32;
        fn FindClose(handle: *mut c_void) -> i32;
    }

    pub(crate) struct Metadata {
        inner: fs::Metadata,
        device: u64,
        inode: u128,
        modified_ns: i128,
        changed_ns: i128,
    }
    impl Deref for Metadata {
        type Target = fs::Metadata;
        fn deref(&self) -> &Self::Target {
            &self.inner
        }
    }
    impl Metadata {
        pub(crate) fn dev(&self) -> u64 {
            self.device
        }
        pub(crate) fn ino(&self) -> u128 {
            self.inode
        }
        pub(crate) fn mode(&self) -> u32 {
            if self.is_dir() {
                0o755
            } else if self.permissions().readonly() {
                0o444
            } else {
                0o644
            }
        }
        pub(crate) fn mtime(&self) -> i64 {
            self.modified_ns.div_euclid(1_000_000_000) as i64
        }
        pub(crate) fn mtime_nsec(&self) -> i64 {
            self.modified_ns.rem_euclid(1_000_000_000) as i64
        }
        pub(crate) fn ctime(&self) -> i64 {
            self.changed_ns.div_euclid(1_000_000_000) as i64
        }
        pub(crate) fn ctime_nsec(&self) -> i64 {
            self.changed_ns.rem_euclid(1_000_000_000) as i64
        }
    }

    pub(super) fn metadata(file: &File) -> io::Result<Metadata> {
        let inner = file.metadata()?;
        if inner.file_attributes() & REPARSE_POINT != 0 {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "Windows reparse points, symlinks and junctions are not supported",
            ));
        }
        volume_info(file)?;
        let mut info = FileIdInfo::default();
        let mut basic = BasicInfo::default();
        // SAFETY: both outputs have the documented Win32 layout and remain
        // writable; the File keeps its HANDLE alive throughout both calls.
        if unsafe {
            GetFileInformationByHandleEx(
                file.as_raw_handle(),
                18, // FileIdInfo; legacy BY_HANDLE_FILE_INFORMATION truncates ReFS IDs.
                (&mut info as *mut FileIdInfo).cast(),
                std::mem::size_of::<FileIdInfo>() as u32,
            )
        } == 0
            || unsafe {
                GetFileInformationByHandleEx(
                    file.as_raw_handle(),
                    0,
                    (&mut basic as *mut BasicInfo).cast(),
                    std::mem::size_of::<BasicInfo>() as u32,
                )
            } == 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok(Metadata {
            modified_ns: (basic.write as i128 - EPOCH_TICKS) * 100,
            changed_ns: (basic.change as i128 - EPOCH_TICKS) * 100,
            inner,
            device: info.volume,
            inode: u128::from_le_bytes(info.identifier),
        })
    }

    pub(super) fn allocation(path: &Path) -> io::Result<u64> {
        #[repr(C)]
        #[derive(Default)]
        struct StandardInfo {
            allocation_size: i64,
            end_of_file: i64,
            number_of_links: u32,
            delete_pending: u8,
            directory: u8,
        }
        const _: () = {
            assert!(std::mem::size_of::<StandardInfo>() == 24);
            assert!(std::mem::offset_of!(StandardInfo, allocation_size) == 0);
            assert!(std::mem::offset_of!(StandardInfo, end_of_file) == 8);
            assert!(std::mem::offset_of!(StandardInfo, number_of_links) == 16);
            assert!(std::mem::offset_of!(StandardInfo, delete_pending) == 20);
            assert!(std::mem::offset_of!(StandardInfo, directory) == 21);
        };
        let file = open_node(path, READ_ATTRIBUTES)?;
        let mut info = StandardInfo::default();
        // SAFETY: class FileStandardInfo (1) expects this repr(C) layout;
        // the initialized buffer and owned no-follow handle remain live.
        if unsafe {
            GetFileInformationByHandleEx(
                file.as_raw_handle(),
                1,
                (&mut info as *mut StandardInfo).cast(),
                std::mem::size_of::<StandardInfo>() as u32,
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        u64::try_from(info.allocation_size)
            .map_err(|_| io::Error::other("negative file allocation"))
    }

    pub(super) fn volume_info(file: &File) -> io::Result<VolumeInfo> {
        let mut filesystem = [0u16; 32];
        let mut flags = 0;
        // SAFETY: File owns the live handle, the filesystem-name buffer is
        // writable, and all unused optional output pointers are null.
        if unsafe {
            GetVolumeInformationByHandleW(
                file.as_raw_handle(),
                std::ptr::null_mut(),
                0,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &mut flags,
                filesystem.as_mut_ptr(),
                filesystem.len() as u32,
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        let length = filesystem
            .iter()
            .position(|&unit| unit == 0)
            .unwrap_or(filesystem.len());
        let name = match &filesystem[..length] {
            [78, 84, 70, 83] => "NTFS",
            [82, 101, 70, 83] => "ReFS",
            _ => {
                return Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    "the Windows native backend requires local NTFS or ReFS",
                ));
            }
        };
        Ok(VolumeInfo { name, flags })
    }

    fn same_snapshot(left: &Metadata, right: &Metadata) -> bool {
        left.dev() == right.dev()
            && left.ino() == right.ino()
            && left.is_file() == right.is_file()
            && left.len() == right.len()
            && left.mode() == right.mode()
            && left.modified_ns == right.modified_ns
            && left.changed_ns == right.changed_ns
    }

    fn unsupported_clone(error: &io::Error) -> bool {
        // ERROR_INVALID_FUNCTION and ERROR_NOT_SUPPORTED explicitly report a
        // missing filesystem operation. INVALID_PARAMETER is deliberately not
        // accepted: it can indicate a bug in offsets, alignment or handles.
        // Access, sharing/lock, quota, space and I/O failures must propagate.
        matches!(error.raw_os_error(), Some(1 | 50))
    }

    fn integrity_info(file: &File) -> io::Result<IntegrityInfo> {
        let mut info = IntegrityInfo::default();
        let mut returned = 0;
        // SAFETY: synchronous live file handle, no input, and a correctly sized
        // FSCTL_GET_INTEGRITY_INFORMATION_BUFFER output with its documented ABI.
        if unsafe {
            DeviceIoControl(
                file.as_raw_handle(),
                FSCTL_GET_INTEGRITY_INFORMATION,
                std::ptr::null(),
                0,
                (&mut info as *mut IntegrityInfo).cast(),
                std::mem::size_of::<IntegrityInfo>() as u32,
                &mut returned,
                std::ptr::null_mut(),
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        if returned != std::mem::size_of::<IntegrityInfo>() as u32
            || !info.cluster_size.is_power_of_two()
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid ReFS integrity or cluster-size information",
            ));
        }
        Ok(info)
    }

    fn require_absent(path: &Path) -> io::Result<()> {
        match fs::symlink_metadata(path) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error),
            Ok(_) => Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "clone target already exists",
            )),
        }
    }

    /// Deletes only our create-new handle, not a potentially replaced pathname.
    /// Failure is fatal: the caller must never overwrite a leftover target.
    fn discard_clone(output: File, target: &Path) -> io::Result<Option<File>> {
        let delete_file: u8 = 1; // FILE_DISPOSITION_INFO.DeleteFile is BOOLEAN.
        // SAFETY: the owned handle has DELETE access, and FileDispositionInfo
        // (4) takes this one-byte BOOLEAN. No unrelated path is deleted.
        if unsafe {
            SetFileInformationByHandle(
                output.as_raw_handle(),
                4,
                (&delete_file as *const u8).cast(),
                1,
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        drop(output);
        // Another still-open reader can delay final deletion. Do not authorize
        // fallback until the old target really is gone, even in that case.
        require_absent(target)?;
        Ok(None)
    }

    pub(super) fn native_clone(
        original_source: &File,
        source_path: &Path,
        target: &Path,
    ) -> io::Result<Option<File>> {
        validate_ancestors(source_path)?;
        validate_ancestors(target)?;
        require_absent(target)?;
        let before = metadata(original_source)?;
        if !before.is_file() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "clone source must be a regular file",
            ));
        }
        // User-tree capture rejects read-only inputs at its boundary. The
        // primitive also serves immutable cache files, which may be read-only.
        reject_named_streams(source_path)?;

        // A separate open, not try_clone(), preserves the caller's cursor:
        // Windows FileExt::seek_read changes its handle's shared file pointer.
        // Keep normal sharing for freshly hydrated cache sources whose writer
        // is still open. The existing trusted, quiescent tree contract applies.
        let source = open_node(source_path, GENERIC_READ)?;
        if !same_snapshot(&before, &metadata(&source)?) {
            return Err(io::Error::other(
                "source changed while opening clone handle",
            ));
        }
        let source_volume = volume_info(&source)?;
        if source_volume.name != "ReFS" || source_volume.flags & SUPPORTS_BLOCK_REFCOUNTING == 0 {
            return Ok(None);
        }
        let parent_path = target
            .parent()
            .filter(|path| !path.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        let parent = open_node(parent_path, READ_ATTRIBUTES)?;
        let parent_metadata = metadata(&parent)?;
        if !parent_metadata.is_dir() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "clone target parent must be a directory",
            ));
        }
        let target_volume = volume_info(&parent)?;
        if target_volume.name != "ReFS"
            || target_volume.flags & SUPPORTS_BLOCK_REFCOUNTING == 0
            || before.dev() != parent_metadata.dev()
        {
            return Ok(None);
        }
        let source_integrity = match integrity_info(&source) {
            Ok(info) => info,
            Err(error) if unsupported_clone(&error) => return Ok(None),
            Err(error) => return Err(error),
        };
        let cluster_size = u64::from(source_integrity.cluster_size);
        let length = before.len();
        let aligned_length = length - length % cluster_size;
        if length != 0 && aligned_length == 0 {
            return Ok(None);
        }
        // Every FSCTL range must fit LARGE_INTEGER, and each request is strictly
        // smaller than 4 GiB. No rounded-up request may pass the source EOF.
        if length > i64::MAX as u64 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "clone source exceeds signed Windows file offsets",
            ));
        }
        let max_extent = (u64::from(u32::MAX) / cluster_size) * cluster_size;
        if max_extent == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "ReFS cluster size exceeds the block-clone request limit",
            ));
        }

        // DELETE is solely for safe fallback cleanup by this exact handle.
        // Denying write/delete sharing pins this fresh target until returned;
        // no existing file, reparse point or stream can be overwritten.
        let output = OpenOptions::new()
            .read(true)
            .write(true)
            .access_mode(GENERIC_READ | GENERIC_WRITE | DELETE_ACCESS)
            .create_new(true)
            .share_mode(SHARE_READ)
            .custom_flags(OPEN_REPARSE_POINT)
            .open(target)?;
        let output_metadata = metadata(&output)?;
        if !output_metadata.is_file()
            || output_metadata.dev() != before.dev()
            || output_metadata.ino() == before.ino()
            || volume_info(&output)?.name != "ReFS"
        {
            return Err(io::Error::other("clone target changed during creation"));
        }
        let target_integrity = match integrity_info(&output) {
            Ok(info) => info,
            Err(error) if unsupported_clone(&error) => return discard_clone(output, target),
            Err(error) => return Err(error),
        };
        if source_integrity.checksum_algorithm != target_integrity.checksum_algorithm
            || source_integrity.flags != target_integrity.flags
            || source_integrity.checksum_chunk_size != target_integrity.checksum_chunk_size
            || source_integrity.cluster_size != target_integrity.cluster_size
        {
            // Do not change either file's integrity policy merely to clone it.
            return discard_clone(output, target);
        }
        if before.file_attributes() & SPARSE_FILE != 0 {
            let mut returned = 0;
            // SAFETY: FSCTL_SET_SPARSE with NULL input marks only this freshly
            // created destination sparse, as required for a sparse source.
            if unsafe {
                DeviceIoControl(
                    output.as_raw_handle(),
                    FSCTL_SET_SPARSE,
                    std::ptr::null(),
                    0,
                    std::ptr::null_mut(),
                    0,
                    &mut returned,
                    std::ptr::null_mut(),
                )
            } == 0
            {
                let error = io::Error::last_os_error();
                return if unsupported_clone(&error) {
                    discard_clone(output, target)
                } else {
                    Err(error)
                };
            }
        }
        output.set_len(length)?;
        let mut offset = 0;
        while offset < aligned_length {
            let byte_count = (aligned_length - offset).min(max_extent);
            let request = DuplicateExtents {
                source: source.as_raw_handle(),
                source_offset: offset as i64,
                target_offset: offset as i64,
                byte_count: byte_count as i64,
            };
            let mut returned = 0;
            // SAFETY: both handles and the documented repr(C) request stay
            // live throughout this synchronous call. Checked cluster-aligned
            // ranges fit both files' EOF and each request is less than 4 GiB.
            if unsafe {
                DeviceIoControl(
                    output.as_raw_handle(),
                    FSCTL_DUPLICATE_EXTENTS_TO_FILE,
                    (&request as *const DuplicateExtents).cast(),
                    std::mem::size_of::<DuplicateExtents>() as u32,
                    std::ptr::null_mut(),
                    0,
                    &mut returned,
                    std::ptr::null_mut(),
                )
            } == 0
            {
                let error = io::Error::last_os_error();
                return if offset == 0 && unsupported_clone(&error) {
                    // NOT_SUPPORTED can also report a range beyond a source
                    // that was concurrently truncated. That is not capability
                    // absence and must not be hidden by copy fallback.
                    if !same_snapshot(&before, &metadata(&source)?)
                        || !same_snapshot(&before, &metadata(original_source)?)
                        || !same_snapshot(&before, &super::symlink_metadata(source_path)?)
                    {
                        return Err(io::Error::other("source changed during block clone"));
                    }
                    discard_clone(output, target)
                } else {
                    // A failed later extent is never converted into a copy.
                    Err(error)
                };
            }
            offset += byte_count;
        }
        // Exact-byte tail handling also works with 64 KiB clusters and sparse
        // sources. Neither file is extended to a rounded-up cluster boundary.
        let mut buffer = [0u8; 64 * 1024];
        while offset < length {
            let count = (length - offset).min(buffer.len() as u64) as usize;
            let read = loop {
                match source.seek_read(&mut buffer[..count], offset) {
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                    result => break result?,
                }
            };
            if read == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "source shortened while copying clone tail",
                ));
            }
            let mut written = 0;
            while written < read {
                let count = match output.seek_write(&buffer[written..read], offset + written as u64)
                {
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                    result => result?,
                };
                if count == 0 {
                    return Err(io::Error::new(
                        io::ErrorKind::WriteZero,
                        "failed to write clone tail",
                    ));
                }
                written += count;
            }
            offset += read as u64;
        }
        if !same_snapshot(&before, &metadata(&source)?)
            || !same_snapshot(&before, &metadata(original_source)?)
            || !same_snapshot(&before, &super::symlink_metadata(source_path)?)
            || metadata(&output)?.len() != length
        {
            return Err(io::Error::other("source changed during block clone"));
        }
        reject_named_streams(source_path)?;
        Ok(Some(output))
    }

    pub(super) fn reject_named_streams(path: &Path) -> io::Result<()> {
        struct Search(*mut c_void);
        impl Drop for Search {
            fn drop(&mut self) {
                // SAFETY: this uniquely owned live enumeration handle came
                // from FindFirstStreamW and must be released with FindClose.
                unsafe {
                    FindClose(self.0);
                }
            }
        }
        let path = wide_path(path)?;
        let mut info = StreamInfo {
            size: 0,
            name: [0; 296],
        };
        // SAFETY: path is NUL terminated and output has the documented Win32
        // layout. The search handle is inspected before creating its owner.
        let handle = unsafe { FindFirstStreamW(path.as_ptr(), 0, &mut info, 0) };
        if handle == (-1isize) as *mut c_void {
            let error = io::Error::last_os_error();
            return if error.raw_os_error() == Some(38) {
                Ok(())
            } else {
                Err(error)
            };
        }
        let search = Search(handle);
        loop {
            let length = info
                .name
                .iter()
                .position(|&unit| unit == 0)
                .ok_or_else(|| {
                    io::Error::new(io::ErrorKind::InvalidData, "unterminated stream name")
                })?;
            if info.name[..length] != [58, 58, 36, 68, 65, 84, 65] {
                return Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    "named Windows data streams are not supported and will not be silently discarded",
                ));
            }
            // SAFETY: the live search handle owns this enumeration and output
            // remains a writable WIN32_FIND_STREAM_DATA-compatible structure.
            if unsafe { FindNextStreamW(search.0, &mut info) } == 0 {
                let error = io::Error::last_os_error();
                return if error.raw_os_error() == Some(38) {
                    Ok(())
                } else {
                    Err(error)
                };
            }
        }
    }

    pub(super) fn make_inheritable(file: &File) -> io::Result<()> {
        // SAFETY: File owns a valid duplicate HANDLE for the duration of the
        // call. Only its inheritance flag changes, not rights or ACLs.
        if unsafe { SetHandleInformation(file.as_raw_handle(), 1, 1) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    pub(super) fn open_node(path: &Path, access: u32) -> io::Result<File> {
        validate_ancestors(path)?;
        let file = OpenOptions::new()
            .access_mode(access)
            .custom_flags(OPEN_REPARSE_POINT | BACKUP_SEMANTICS)
            .open(path)?;
        metadata(&file)?;
        Ok(file)
    }

    /// Checks existing ancestors before any leaf open, creation, or rename.
    /// OPEN_REPARSE_POINT only protects the final component of an individual
    /// open, so each ancestor is opened separately before descending further.
    /// These checks do not guard against hostile concurrent replacement.
    pub(super) fn validate_ancestors(path: &Path) -> io::Result<()> {
        validate_path(path)?;
        let absolute = std::path::absolute(path)?;
        validate_path(&absolute)?;
        let Some(parent) = absolute.parent() else {
            return Ok(());
        };
        let mut current = std::path::PathBuf::new();
        for component in parent.components() {
            current.push(component.as_os_str());
            if matches!(component, Component::Prefix(_)) {
                continue;
            }
            // Do not call open_node here: that would recursively repeat every
            // ancestor check. Only directory/reparse attributes are needed.
            let directory = OpenOptions::new()
                .access_mode(READ_ATTRIBUTES)
                .custom_flags(OPEN_REPARSE_POINT | BACKUP_SEMANTICS)
                .open(&current)?;
            let metadata = directory.metadata()?;
            if !metadata.is_dir() || metadata.file_attributes() & REPARSE_POINT != 0 {
                return Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    "Windows path ancestors must be real directories, not reparse points or junctions",
                ));
            }
        }
        Ok(())
    }

    pub(super) fn validate_component(value: &str) -> io::Result<()> {
        let stem = value
            .split('.')
            .next()
            .unwrap_or("")
            .trim_end_matches(' ')
            .to_uppercase();
        let device = matches!(
            stem.as_str(),
            "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$"
        ) || ["COM", "LPT"].iter().any(|prefix| {
            stem.strip_prefix(prefix).is_some_and(|suffix| {
                matches!(
                    suffix,
                    "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "¹" | "²" | "³"
                )
            })
        });
        if value.is_empty()
            || value.ends_with(['.', ' '])
            || device
            || value
                .chars()
                .any(|character| character < ' ' || "<>:\\|?*\"".contains(character))
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "unsupported Windows path component or device name",
            ));
        }
        Ok(())
    }

    pub(super) fn validate_path(path: &Path) -> io::Result<()> {
        for component in path.components() {
            match component {
                Component::Prefix(prefix) => {
                    if !matches!(prefix.kind(), Prefix::Disk(_) | Prefix::VerbatimDisk(_))
                        || !path.has_root()
                    {
                        return Err(io::Error::new(
                            io::ErrorKind::Unsupported,
                            "Windows requires a local disk path, not a device, UNC or drive-relative path",
                        ));
                    }
                }
                Component::Normal(value) => {
                    validate_component(value.to_str().ok_or_else(|| {
                        io::Error::new(io::ErrorKind::InvalidInput, "non-UTF-8 path rejected")
                    })?)?
                }
                Component::ParentDir => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "parent-directory syntax is not allowed",
                    ));
                }
                _ => {}
            }
        }
        Ok(())
    }

    fn wide_path(path: &Path) -> io::Result<Vec<u16>> {
        let absolute = std::path::absolute(path)?;
        validate_path(&absolute)?;
        let absolute = if matches!(absolute.components().next(), Some(Component::Prefix(prefix))
            if matches!(prefix.kind(), Prefix::VerbatimDisk(_)))
        {
            absolute
        } else {
            let mut extended = std::ffi::OsString::from(r"\\?\");
            extended.push(absolute.as_os_str());
            extended.into()
        };
        Ok(absolute.as_os_str().encode_wide().chain(Some(0)).collect())
    }

    pub(super) fn rename(source: &Path, target: &Path) -> io::Result<()> {
        validate_ancestors(source)?;
        validate_ancestors(target)?;
        let source = wide_path(source)?;
        let target = wide_path(target)?;
        // SAFETY: both UTF-16 paths are NUL terminated and live for the call.
        // No COPY_ALLOWED flag: cross-volume non-atomic emulation is forbidden.
        if unsafe { MoveFileExW(source.as_ptr(), target.as_ptr(), 0x1 | 0x8) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
}
