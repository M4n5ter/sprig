//! Filesystem operations with explicit platform guarantees.
//!
//! Windows supports ordinary writable files and directories on local NTFS disks.
//! Existing reparse-point leaves and ancestors are rejected. Paths must stay
//! trusted and quiescent: this is not a sandbox against concurrent ancestor
//! replacement. Windows has no unprivileged
//! POSIX directory-fsync equivalent: file contents are flushed and publication
//! uses write-through rename, but equal power-loss namespace durability is not
//! claimed. Directory permissions inherit the user's existing ACLs.

use std::fs::{self, File, OpenOptions};
use std::io;
use std::path::Path;

#[cfg(unix)]
pub(crate) use std::fs::Metadata;
#[cfg(unix)]
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
#[cfg(windows)]
pub(crate) use windows::Metadata;

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

#[cfg(windows)]
mod windows {
    use super::*;
    use std::ffi::c_void;
    use std::ops::Deref;
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
    use std::os::windows::io::AsRawHandle;
    use std::path::{Component, Prefix};

    pub(super) const GENERIC_READ: u32 = 0x8000_0000;
    pub(super) const GENERIC_WRITE: u32 = 0x4000_0000;
    pub(super) const READ_ATTRIBUTES: u32 = 0x0080;
    pub(super) const WRITE_ATTRIBUTES: u32 = 0x0100;
    pub(super) const OPEN_REPARSE_POINT: u32 = 0x0020_0000;
    const BACKUP_SEMANTICS: u32 = 0x0200_0000;
    const REPARSE_POINT: u32 = 0x0400;
    const EPOCH_TICKS: i128 = 116_444_736_000_000_000;

    #[repr(C)]
    #[derive(Default)]
    struct FileTime {
        low: u32,
        high: u32,
    }
    #[repr(C)]
    #[derive(Default)]
    struct HandleInfo {
        attributes: u32,
        creation: FileTime,
        access: FileTime,
        write: FileTime,
        volume: u32,
        size_high: u32,
        size_low: u32,
        links: u32,
        index_high: u32,
        index_low: u32,
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
    struct StreamInfo {
        size: i64,
        name: [u16; 296], // MAX_PATH + 36, per WIN32_FIND_STREAM_DATA.
    }

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetFileInformationByHandle(handle: *mut c_void, info: *mut HandleInfo) -> i32;
        fn GetFileInformationByHandleEx(
            handle: *mut c_void,
            class: i32,
            info: *mut c_void,
            size: u32,
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
        inode: u64,
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
        pub(crate) fn ino(&self) -> u64 {
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
        require_ntfs(file)?;
        let mut info = HandleInfo::default();
        let mut basic = BasicInfo::default();
        // SAFETY: both outputs have the documented Win32 layout and remain
        // writable; the File keeps its HANDLE alive throughout both calls.
        if unsafe { GetFileInformationByHandle(file.as_raw_handle(), &mut info) } == 0
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
            device: info.volume as u64,
            inode: (info.index_high as u64) << 32 | info.index_low as u64,
        })
    }

    fn require_ntfs(file: &File) -> io::Result<()> {
        let mut filesystem = [0u16; 32];
        // SAFETY: File owns the live handle, the filesystem-name buffer is
        // writable, and all unused optional output pointers are null.
        if unsafe {
            GetVolumeInformationByHandleW(
                file.as_raw_handle(),
                std::ptr::null_mut(),
                0,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
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
        if filesystem[..length] != [78, 84, 70, 83] {
            // ReFS requires 128-bit file identities; truncating those to the
            // schema's 64-bit inode field could incorrectly identify renames.
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "the Windows native backend currently requires NTFS",
            ));
        }
        Ok(())
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
                    "named NTFS data streams are not supported and will not be silently discarded",
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
