//! Restricted, optional WinFsp view of the owned journal, not another store.
//! The dispatcher must remain alive through final Close and the volume flush.
//! No destructor flushes or certifies a clean session.
//! Regular files expose a fixed ARCHIVE bit, and creation/change times are
//! derived from mtime. They are not independent persistent metadata. Explicit
//! requests to clear ARCHIVE or set creation/change times are unsupported.
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::c_void;
use std::io;
use std::sync::{Arc, Mutex, MutexGuard};

use crate::content::{Entry, EntryKind, Manifest};
use crate::writable::{MAX_WRITE_BYTES, NodeAttr, ROOT_INODE, SetAttr, WritableCas};
use winfsp_wrs::*;

const EPOCH_TICKS: i128 = 116_444_736_000_000_000;
type Result<T> = std::result::Result<T, NTSTATUS>;

/// Numeric descriptor tokens deliberately support NULL (volume Flush). Using
/// Arc directly with winfsp_wrs 0.4's trampoline would dereference NULL there.
#[derive(Clone, Copy)]
pub(crate) struct ContextToken(usize);
impl FileContextKind for ContextToken {
    const MODE: FileContextMode = FileContextMode::Descriptor;
    unsafe fn write(self, out: *mut *mut c_void) {
        // SAFETY: WinFsp supplies writable context output storage.
        unsafe { out.write(self.0 as *mut c_void) };
    }
    unsafe fn access(raw: *mut c_void) -> Self {
        Self(raw as usize)
    }
    unsafe fn access_for_close(raw: *mut c_void) -> Self {
        Self(raw as usize)
    }
}

struct OpenContext {
    inode: u64,
    handle: u64,
    access: FileAccessRights,
    write_through: bool,
    enumeration: Mutex<Option<Vec<(String, FileInfo)>>>,
}
struct State {
    accepting: bool,
    next: usize,
    contexts: BTreeMap<usize, Arc<OpenContext>>,
    sticky: Option<String>,
    dispatcher_stopped: Option<bool>,
    synced_for_stop: bool,
}
struct Shared {
    core: Arc<WritableCas>,
    file_security: SecurityDescriptor,
    directory_security: SecurityDescriptor,
    // Covers callbacks and the controller's gate/busy/sync operations together.
    operation: Mutex<()>,
    state: Mutex<State>,
}
#[derive(Clone)]
pub(crate) struct WindowsCas(Arc<Shared>);

fn unsupported(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::Unsupported, message)
}
fn status(error: io::Error) -> NTSTATUS {
    match error.kind() {
        io::ErrorKind::NotFound => STATUS_OBJECT_NAME_NOT_FOUND,
        io::ErrorKind::AlreadyExists => STATUS_OBJECT_NAME_COLLISION,
        io::ErrorKind::PermissionDenied => STATUS_ACCESS_DENIED,
        io::ErrorKind::InvalidInput => STATUS_INVALID_PARAMETER,
        io::ErrorKind::Unsupported => STATUS_NOT_SUPPORTED,
        io::ErrorKind::DirectoryNotEmpty => STATUS_DIRECTORY_NOT_EMPTY,
        io::ErrorKind::NotADirectory => STATUS_NOT_A_DIRECTORY,
        io::ErrorKind::IsADirectory => STATUS_FILE_IS_A_DIRECTORY,
        io::ErrorKind::StorageFull => STATUS_DISK_FULL,
        _ => STATUS_IO_DEVICE_ERROR,
    }
}
fn ticks(ns: i128) -> io::Result<u64> {
    let value = ns
        .checked_div(100)
        .and_then(|n| n.checked_add(EPOCH_TICKS))
        .ok_or_else(|| unsupported("timestamp outside Windows FILETIME range"))?;
    // Windows APIs use signed 64-bit FILETIME arithmetic.
    if !(0..=i64::MAX as i128).contains(&value) {
        return Err(unsupported("timestamp outside Windows FILETIME range"));
    }
    Ok(value as u64)
}
fn nanoseconds(value: u64) -> Result<i128> {
    if value > i64::MAX as u64 {
        return Err(STATUS_INVALID_PARAMETER);
    }
    Ok((value as i128 - EPOCH_TICKS) * 100)
}
fn valid_name(name: &str) -> bool {
    if name.is_empty()
        || name == "."
        || name == ".."
        || name.ends_with([' ', '.'])
        || name.len() > 255
        || name.encode_utf16().count() > 255
        || name.chars().any(|c| c < ' ' || "<>:\"/\\|?*".contains(c))
    {
        return false;
    }
    let stem = name.split('.').next().unwrap_or("").to_uppercase();
    if matches!(
        stem.as_str(),
        "CON" | "PRN" | "AUX" | "NUL" | "CLOCK$" | "CONIN$" | "CONOUT$"
    ) {
        return false;
    }
    if let Some(suffix) = stem
        .strip_prefix("COM")
        .or_else(|| stem.strip_prefix("LPT"))
        && matches!(
            suffix,
            "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "¹" | "²" | "³"
        )
    {
        return false;
    }
    true
}
fn path(name: &U16CStr) -> Result<String> {
    let name = name.to_string().map_err(|_| STATUS_OBJECT_NAME_INVALID)?;
    let name = name.strip_prefix('\\').ok_or(STATUS_OBJECT_NAME_INVALID)?;
    if name.is_empty() {
        return Ok(String::new());
    }
    if !name.split('\\').all(valid_name) {
        return Err(STATUS_OBJECT_NAME_INVALID);
    }
    Ok(name.replace('\\', "/"))
}
fn supported_entry(entry: &Entry) -> io::Result<()> {
    let modes: &[u32] = match entry.kind {
        EntryKind::Directory => &[0o755, 0o555],
        EntryKind::File { .. } => &[0o644, 0o444],
        EntryKind::Symlink { .. } => {
            return Err(unsupported("Windows owned mount does not support symlinks"));
        }
    };
    if !modes.contains(&entry.mode) {
        return Err(unsupported(
            "Windows owned mount cannot represent this Unix mode",
        ));
    }
    ticks(entry.modified_ns)?;
    Ok(())
}
/// Call before ANY session/mount mutation. Native/SDK paths do not use this.
pub(crate) fn preflight_manifest(manifest: &Manifest) -> io::Result<()> {
    let mut groups = BTreeSet::new();
    for (name, entry) in manifest {
        if !name.is_empty() && !name.split('/').all(valid_name) {
            return Err(unsupported(
                "manifest contains a Windows-unrepresentable name",
            ));
        }
        if name.len() > 4096 {
            return Err(unsupported("manifest path too long"));
        }
        supported_entry(entry)?;
        if let Some(group) = &entry.link_group
            && !groups.insert(group)
        {
            return Err(unsupported(
                "Windows owned mount cannot expose hardlink aliases",
            ));
        }
    }
    Ok(())
}
fn info(attr: &NodeAttr) -> Result<FileInfo> {
    let directory = matches!(attr.entry.kind, EntryKind::Directory);
    let size = match attr.entry.kind {
        EntryKind::File { size, .. } => size,
        _ => 0,
    };
    let mut attributes = if directory {
        FileAttributes::DIRECTORY
    } else {
        FileAttributes::ARCHIVE
    };
    if attr.entry.mode & 0o222 == 0 {
        attributes =
            FileAttributes((attributes.0 & !FileAttributes::NORMAL.0) | FileAttributes::READONLY.0);
    }
    let modified = ticks(attr.entry.modified_ns).map_err(status)?;
    let mut out = FileInfo::default();
    out.set_file_attributes(attributes)
        .set_file_size(size)
        .set_allocation_size(size)
        .set_time(modified)
        .set_last_access_time(ticks(attr.accessed_ns).map_err(status)?)
        .set_index_number(attr.inode)
        .set_hard_links(attr.nlink);
    Ok(out)
}
fn check_options(options: CreateOptions) -> Result<()> {
    // WinFsp v2.1 forwards the NT create disposition in the upper byte
    // (src/dll/fsop.c FspFileSystemOpCreate). It is not an option flag;
    // WinFsp has already selected Create/Open/Overwrite from this value.
    if options.0 >> 24 > 5 {
        return Err(STATUS_INVALID_PARAMETER);
    }
    let option_bits = options.0 & 0x00ff_ffff;
    let allowed = CreateOptions::FILE_DIRECTORY_FILE.0
        | CreateOptions::FILE_NON_DIRECTORY_FILE.0
        | CreateOptions::FILE_WRITE_THROUGH.0
        | CreateOptions::FILE_SEQUENTIAL_ONLY.0
        | CreateOptions::FILE_RANDOM_ACCESS.0
        | CreateOptions::FILE_NO_INTERMEDIATE_BUFFERING.0
        | CreateOptions::FILE_SYNCHRONOUS_IO_ALERT.0
        | CreateOptions::FILE_SYNCHRONOUS_IO_NONALERT.0
        | CreateOptions::FILE_NO_EA_KNOWLEDGE.0
        | CreateOptions::FILE_DELETE_ON_CLOSE.0
        | CreateOptions::FILE_OPEN_FOR_BACKUP_INTENT.0;
    if option_bits & !allowed != 0 {
        eprintln!(
            "Windows owned open rejected unsupported create options {:#x}",
            options.0
        );
        return Err(STATUS_NOT_SUPPORTED);
    }
    Ok(())
}
fn mode_for(attributes: FileAttributes, directory: bool) -> Result<u32> {
    let allowed = FileAttributes::NORMAL.0
        | FileAttributes::READONLY.0
        | FileAttributes::DIRECTORY.0
        | FileAttributes::ARCHIVE.0;
    if attributes.0 & !allowed != 0 {
        return Err(STATUS_NOT_SUPPORTED);
    }
    if directory && attributes.is(FileAttributes::ARCHIVE) {
        return Err(STATUS_NOT_SUPPORTED);
    }
    if !directory && attributes.is(FileAttributes::DIRECTORY) {
        return Err(STATUS_INVALID_PARAMETER);
    }
    Ok(match (directory, attributes.is(FileAttributes::READONLY)) {
        (true, false) => 0o755,
        (true, true) => 0o555,
        (false, false) => 0o644,
        (false, true) => 0o444,
    })
}

impl WindowsCas {
    pub(crate) fn new(
        core: Arc<WritableCas>,
        file_security: SecurityDescriptor,
        directory_security: SecurityDescriptor,
    ) -> io::Result<Self> {
        if file_security.is_empty() || directory_security.is_empty() {
            return Err(unsupported(
                "restricted current-user security descriptor required",
            ));
        }
        let mut inodes = BTreeSet::new();
        for (name, attr) in core.paths()? {
            supported_entry(&attr.entry)?;
            if (!name.is_empty() && !name.split('/').all(valid_name)) || !inodes.insert(attr.inode)
            {
                return Err(unsupported(
                    "Windows owned mount has unsupported names or aliases",
                ));
            }
        }
        Ok(Self(Arc::new(Shared {
            core,
            file_security,
            directory_security,
            operation: Mutex::new(()),
            state: Mutex::new(State {
                accepting: true,
                next: 1,
                contexts: BTreeMap::new(),
                sticky: None,
                dispatcher_stopped: None,
                synced_for_stop: false,
            }),
        })))
    }
    fn lock(&self) -> Result<MutexGuard<'_, ()>> {
        self.0.operation.lock().map_err(|_| STATUS_IO_DEVICE_ERROR)
    }
    fn state(&self) -> Result<MutexGuard<'_, State>> {
        self.0.state.lock().map_err(|_| STATUS_IO_DEVICE_ERROR)
    }
    fn context(&self, token: ContextToken) -> Result<Arc<OpenContext>> {
        self.state()?
            .contexts
            .get(&token.0)
            .cloned()
            .ok_or(STATUS_INVALID_HANDLE)
    }
    fn failure(&self, message: String) {
        if let Ok(mut state) = self.0.state.lock() {
            state.sticky.get_or_insert(message);
            state.synced_for_stop = false;
        }
    }
    fn mutation<T>(&self, result: io::Result<T>) -> Result<T> {
        self.state()?.synced_for_stop = false;
        result.map_err(|error| {
            // Ordinary invalid/unsupported namespace requests are not I/O failures.
            if !matches!(
                error.kind(),
                io::ErrorKind::NotFound
                    | io::ErrorKind::AlreadyExists
                    | io::ErrorKind::PermissionDenied
                    | io::ErrorKind::InvalidInput
                    | io::ErrorKind::Unsupported
                    | io::ErrorKind::DirectoryNotEmpty
                    | io::ErrorKind::NotADirectory
                    | io::ErrorKind::IsADirectory
            ) {
                self.failure(error.to_string());
            }
            status(error)
        })
    }
    fn sync(&self) -> Result<()> {
        self.0.core.sync().map_err(|error| {
            self.failure(error.to_string());
            status(error)
        })
    }
    fn security(&self, directory: bool) -> &SecurityDescriptor {
        if directory {
            &self.0.directory_security
        } else {
            &self.0.file_security
        }
    }
    fn same_security(&self, proposed: &SecurityDescriptor, directory: bool) -> Result<bool> {
        equivalent_security(self.security(directory).as_bytes(), proposed.as_bytes())
    }
    fn parent(&self, name: &str) -> Result<(u64, String)> {
        if name.is_empty() {
            return Err(STATUS_ACCESS_DENIED);
        }
        let (parent, leaf) = name.rsplit_once('/').unwrap_or(("", name));
        Ok((
            self.0.core.lookup_path(parent).map_err(status)?.inode,
            leaf.to_owned(),
        ))
    }
    fn open_attr(
        &self,
        attr: NodeAttr,
        options: CreateOptions,
        access: FileAccessRights,
    ) -> Result<(ContextToken, FileInfo)> {
        let directory = matches!(attr.entry.kind, EntryKind::Directory);
        if directory && options.is(CreateOptions::FILE_NON_DIRECTORY_FILE) {
            return Err(STATUS_FILE_IS_A_DIRECTORY);
        }
        if !directory && options.is(CreateOptions::FILE_DIRECTORY_FILE) {
            return Err(STATUS_NOT_A_DIRECTORY);
        }
        let file_info = info(&attr)?;
        let write = access.0
            & (FileAccessRights::FILE_WRITE_DATA.0 | FileAccessRights::FILE_APPEND_DATA.0)
            != 0;
        if !directory && write && attr.entry.mode & 0o222 == 0 {
            return Err(STATUS_ACCESS_DENIED);
        }
        let mut state = self.state()?;
        if !state.accepting {
            return Err(STATUS_DEVICE_NOT_READY);
        }
        let next = state
            .next
            .checked_add(1)
            .ok_or(STATUS_INSUFFICIENT_RESOURCES)?;
        let handle = self
            .0
            .core
            .open_handle(attr.inode, write && !directory)
            .map_err(status)?;
        let token = state.next;
        state.next = next;
        state.contexts.insert(
            token,
            Arc::new(OpenContext {
                inode: attr.inode,
                handle,
                access,
                write_through: options.is(CreateOptions::FILE_WRITE_THROUGH),
                enumeration: Mutex::new(None),
            }),
        );
        Ok((ContextToken(token), file_info))
    }
    pub(crate) fn gate_new_opens(&self) -> io::Result<()> {
        let _op = self.lock().map_err(nt_error)?;
        self.state().map_err(nt_error)?.accepting = false;
        Ok(())
    }
    pub(crate) fn ungate_new_opens(&self) -> io::Result<()> {
        let _op = self.lock().map_err(nt_error)?;
        let mut state = self.state().map_err(nt_error)?;
        if state.dispatcher_stopped.is_some() || state.sticky.is_some() {
            return Err(io::Error::other(
                "cannot reopen a stopped or failed Windows owned mount",
            ));
        }
        state.accepting = true;
        state.synced_for_stop = false;
        Ok(())
    }
    pub(crate) fn dispatcher_finished(&self) -> io::Result<bool> {
        Ok(self.state().map_err(nt_error)?.dispatcher_stopped.is_some())
    }
    pub(crate) fn active_handles(&self) -> io::Result<usize> {
        Ok(self.state().map_err(nt_error)?.contexts.len())
    }
    /// Call only AFTER the OS volume flush has succeeded, with dispatcher alive.
    pub(crate) fn sync_for_stop(&self) -> io::Result<()> {
        let _op = self.lock().map_err(nt_error)?;
        {
            let state = self.state().map_err(nt_error)?;
            if state.accepting || !state.contexts.is_empty() {
                return Err(io::Error::new(
                    io::ErrorKind::WouldBlock,
                    "Windows owned mount is still busy or accepting opens",
                ));
            }
            if let Some(error) = &state.sticky {
                return Err(io::Error::other(error.clone()));
            }
        }
        self.sync().map_err(nt_error)?;
        self.state().map_err(nt_error)?.synced_for_stop = true;
        Ok(())
    }
    /// Must be checked after an explicit dispatcher stop, never from Drop.
    pub(crate) fn clean_stop_ready(&self) -> io::Result<()> {
        let _op = self.lock().map_err(nt_error)?;
        let state = self.state().map_err(nt_error)?;
        if state.accepting
            || !state.contexts.is_empty()
            || !state.synced_for_stop
            || state.dispatcher_stopped != Some(true)
            || state.sticky.is_some()
        {
            return Err(io::Error::other(
                "Windows owned mount lacks a clean final-close/flush/normal-stop barrier",
            ));
        }
        Ok(())
    }
}
fn nt_error(code: NTSTATUS) -> io::Error {
    io::Error::other(format!(
        "Windows owned callback failed: NTSTATUS 0x{:08x}",
        code as u32
    ))
}

// Win32 self-relative descriptor wire fields are documented OS layout, not the
// private Rust wrapper layout. Bounds are checked before any Win32 dereference.
// The aligned copy remains owned through all descriptor queries.
type CheckedAcl = Option<(u8, Vec<Vec<u8>>)>;
struct CheckedSecurity {
    storage: Vec<u32>,
    control: u16,
    revision: u32,
    resource_manager_control: u8,
    acls: [CheckedAcl; 2],
}
impl CheckedSecurity {
    fn new(bytes: &[u8]) -> Result<Self> {
        use windows_sys::Win32::Security::{
            GetSecurityDescriptorControl, IsValidSecurityDescriptor,
        };
        if bytes.len() < 20 || bytes[0] != 1 {
            return Err(STATUS_INVALID_SECURITY_DESCR);
        }
        let control = u16::from_le_bytes([bytes[2], bytes[3]]);
        if control & 0x8000 == 0 {
            return Err(STATUS_INVALID_SECURITY_DESCR);
        }
        let offset = |at: usize| -> Result<usize> {
            let value = u32::from_le_bytes(
                bytes[at..at + 4]
                    .try_into()
                    .map_err(|_| STATUS_INVALID_SECURITY_DESCR)?,
            ) as usize;
            if value != 0 && (value < 20 || !value.is_multiple_of(4) || value >= bytes.len()) {
                return Err(STATUS_INVALID_SECURITY_DESCR);
            }
            Ok(value)
        };
        // Owner and group SID have an eight-byte header and four-byte subauthorities.
        for at in [4, 8] {
            let start = offset(at)?;
            if start != 0 {
                let head = bytes
                    .get(start..start + 8)
                    .ok_or(STATUS_INVALID_SECURITY_DESCR)?;
                if head[0] != 1 || head[1] > 15 || start + 8 + head[1] as usize * 4 > bytes.len() {
                    return Err(STATUS_INVALID_SECURITY_DESCR);
                }
            }
        }
        let mut acls = [None, None];
        for (index, at) in [12, 16].into_iter().enumerate() {
            let start = offset(at)?;
            if start == 0 {
                continue;
            }
            let present = if index == 0 { 0x0010 } else { 0x0004 };
            if control & present == 0 {
                return Err(STATUS_INVALID_SECURITY_DESCR);
            }
            let head = bytes
                .get(start..start + 8)
                .ok_or(STATUS_INVALID_SECURITY_DESCR)?;
            let size = u16::from_le_bytes([head[2], head[3]]) as usize;
            let count = u16::from_le_bytes([head[4], head[5]]) as usize;
            if size < 8 || !size.is_multiple_of(4) {
                return Err(STATUS_INVALID_SECURITY_DESCR);
            }
            let acl = bytes
                .get(start..start + size)
                .ok_or(STATUS_INVALID_SECURITY_DESCR)?;
            let mut cursor = 8;
            let mut entries = Vec::new();
            for _ in 0..count {
                let header = acl
                    .get(cursor..cursor + 4)
                    .ok_or(STATUS_INVALID_SECURITY_DESCR)?;
                let size = u16::from_le_bytes([header[2], header[3]]) as usize;
                if size < 4 || !size.is_multiple_of(4) {
                    return Err(STATUS_INVALID_SECURITY_DESCR);
                }
                let mut ace = acl
                    .get(cursor..cursor + size)
                    .ok_or(STATUS_INVALID_SECURITY_DESCR)?
                    .to_vec();
                // Only the ordinary allow/deny/audit/alarm ACE wire formats are
                // supported. Reject object/callback/conditional ACEs rather than
                // guess at embedded SID offsets or discard authorization data.
                if ace[0] > 3 {
                    return Err(STATUS_NOT_SUPPORTED);
                }
                if ace.len() < 16
                    || ace[8] != 1
                    || ace[9] > 15
                    || 16 + ace[9] as usize * 4 != ace.len()
                {
                    return Err(STATUS_INVALID_SECURITY_DESCR);
                }
                // Provenance only: retain OI/CI/INHERIT_ONLY/NO_PROPAGATE and
                // audit flags, every mask, and the complete validated SID.
                if index == 1 {
                    ace[1] &= !0x10;
                } // DACL INHERITED_ACE only
                entries.push(ace);
                cursor += size;
            }
            // Ignore ACL spare allocation and reserved bytes, but retain revision,
            // ACE order, ACE type, inheritance behavior, masks and SIDs.
            acls[index] = Some((head[0], entries));
        }
        let mut storage = vec![0_u32; bytes.len().div_ceil(4)];
        for (word, chunk) in storage.iter_mut().zip(bytes.chunks(4)) {
            let mut value = [0; 4];
            value[..chunk.len()].copy_from_slice(chunk);
            *word = u32::from_ne_bytes(value);
        }
        let ptr = storage.as_mut_ptr().cast();
        let mut checked_control = 0;
        let mut revision = 0;
        // SAFETY: self-relative header and every referenced SID/ACL extent were
        // bounded above; storage is DWORD-aligned and lives through these calls.
        if unsafe { IsValidSecurityDescriptor(ptr) } == 0
            || unsafe { GetSecurityDescriptorControl(ptr, &mut checked_control, &mut revision) }
                == 0
        {
            return Err(STATUS_INVALID_SECURITY_DESCR);
        }
        Ok(Self {
            storage,
            control: checked_control,
            revision,
            resource_manager_control: if control & 0x4000 != 0 { bytes[1] } else { 0 },
            acls,
        })
    }
    fn ptr(&self) -> *mut c_void {
        self.storage.as_ptr().cast_mut().cast()
    }
}
fn equivalent_security(left: &[u8], right: &[u8]) -> Result<bool> {
    use windows_sys::Win32::Security::{
        EqualSid, GetSecurityDescriptorGroup, GetSecurityDescriptorOwner,
    };
    let left = CheckedSecurity::new(left)?;
    let right = CheckedSecurity::new(right)?;
    // Creation canonicalizes onto our immutable fixed ACL. Owner/group origin
    // and DACL protection/auto-inheritance provenance do not alter the matched
    // access ACEs. No future parent ACL change can occur: SetSecurity is rejected.
    // OI/CI/INHERIT_ONLY/NO_PROPAGATE remain exact in each DACL ACE. SACL presence,
    // contents and control flags remain strict, as do all other control bits.
    const ENCODING_AND_DEFAULTED: u16 =
        0x8000 | 0x0001 | 0x0002 | 0x0008 | 0x0100 | 0x0400 | 0x1000;
    if left.control & !ENCODING_AND_DEFAULTED != right.control & !ENCODING_AND_DEFAULTED
        || left.revision != right.revision
        || left.resource_manager_control != right.resource_manager_control
        || left.acls != right.acls
    {
        return Ok(false);
    }
    for owner in [true, false] {
        let mut a = std::ptr::null_mut();
        let mut b = std::ptr::null_mut();
        let mut defaulted = 0;
        // SAFETY: owned aligned descriptors were bounded and validated above;
        // these accessors return borrowed pointers kept alive by left/right.
        let (a_ok, b_ok) = unsafe {
            if owner {
                (
                    GetSecurityDescriptorOwner(left.ptr(), &mut a, &mut defaulted),
                    GetSecurityDescriptorOwner(right.ptr(), &mut b, &mut defaulted),
                )
            } else {
                (
                    GetSecurityDescriptorGroup(left.ptr(), &mut a, &mut defaulted),
                    GetSecurityDescriptorGroup(right.ptr(), &mut b, &mut defaulted),
                )
            }
        };
        if a_ok == 0 || b_ok == 0 {
            return Err(STATUS_INVALID_SECURITY_DESCR);
        }
        if a.is_null() || b.is_null() {
            if a != b {
                return Ok(false);
            }
        // SAFETY: non-null SIDs belong to the validated, still-live descriptors.
        } else if unsafe { EqualSid(a, b) } == 0 {
            return Ok(false);
        }
    }
    Ok(true)
}

impl FileSystemInterface for WindowsCas {
    type FileContext = ContextToken;
    const GET_VOLUME_INFO_DEFINED: bool = true;
    const GET_SECURITY_BY_NAME_DEFINED: bool = true;
    const CREATE_EX_DEFINED: bool = true;
    const OPEN_DEFINED: bool = true;
    const OVERWRITE_EX_DEFINED: bool = true;
    const CLEANUP_DEFINED: bool = true;
    const CLOSE_DEFINED: bool = true;
    const READ_DEFINED: bool = true;
    const WRITE_DEFINED: bool = true;
    const FLUSH_DEFINED: bool = true;
    const GET_FILE_INFO_DEFINED: bool = true;
    const SET_BASIC_INFO_DEFINED: bool = true;
    const SET_FILE_SIZE_DEFINED: bool = true;
    const CAN_DELETE_DEFINED: bool = true;
    const RENAME_DEFINED: bool = true;
    const GET_SECURITY_DEFINED: bool = true;
    const SET_SECURITY_DEFINED: bool = true;
    const READ_DIRECTORY_DEFINED: bool = true;
    const DISPATCHER_STOPPED_DEFINED: bool = true;

    fn get_volume_info(&self) -> Result<VolumeInfo> {
        let _op = self.lock()?;
        let stats = self.0.core.stats().map_err(status)?;
        VolumeInfo::new(
            stats.logical_limit,
            stats.logical_limit.saturating_sub(stats.logical_bytes),
            U16String::from_str("Sprig owned CAS").as_ustr(),
        )
        .map_err(|_| STATUS_INVALID_PARAMETER)
    }
    fn get_security_by_name(
        &self,
        name: &U16CStr,
        _: impl Fn() -> Option<FileAttributes>,
    ) -> Result<(FileAttributes, PSecurityDescriptor, bool)> {
        let _op = self.lock()?;
        let attr = self.0.core.lookup_path(&path(name)?).map_err(status)?;
        Ok((
            info(&attr)?.file_attributes(),
            self.security(matches!(attr.entry.kind, EntryKind::Directory))
                .as_ptr(),
            false,
        ))
    }
    fn create_ex(
        &self,
        name: &U16CStr,
        request: CreateFileInfo,
        security: SecurityDescriptor,
        buffer: &[u8],
        reparse: bool,
    ) -> Result<(ContextToken, FileInfo)> {
        let _op = self.lock()?;
        check_options(request.create_options)?;
        let directory = request
            .create_options
            .is(CreateOptions::FILE_DIRECTORY_FILE);
        if reparse || !buffer.is_empty() || !self.same_security(&security, directory)? {
            return Err(STATUS_NOT_SUPPORTED);
        }
        if !self.state()?.accepting {
            return Err(STATUS_DEVICE_NOT_READY);
        }
        // Allocation is only a reservation hint; it never sets logical EOF.
        if request.allocation_size > i64::MAX as u64 {
            return Err(STATUS_INVALID_PARAMETER);
        }
        let mode = mode_for(request.file_attributes, directory)?;
        let name = path(name)?;
        let (parent, leaf) = self.parent(&name)?;
        let attr = if directory {
            self.mutation(self.0.core.mkdir(parent, &leaf, mode))?
        } else {
            self.mutation(self.0.core.create_file(parent, &leaf, mode))?
        };
        // Metadata is already journaled. If context allocation fails retain the
        // journal and prevent certification rather than pretend no create occurred.
        self.open_attr(attr, request.create_options, request.granted_access)
            .inspect_err(|e| {
                self.failure(format!("created file but failed opening descriptor: {e}"));
            })
    }
    fn open(
        &self,
        name: &U16CStr,
        options: CreateOptions,
        access: FileAccessRights,
    ) -> Result<(ContextToken, FileInfo)> {
        let _op = self.lock()?;
        check_options(options)?;
        let attr = self.0.core.lookup_path(&path(name)?).map_err(status)?;
        self.open_attr(attr, options, access)
    }
    fn overwrite_ex(
        &self,
        token: ContextToken,
        attributes: FileAttributes,
        replace: bool,
        allocation: u64,
        buffer: &[u8],
    ) -> Result<FileInfo> {
        let _op = self.lock()?;
        if !buffer.is_empty() {
            return Err(STATUS_NOT_SUPPORTED);
        }
        if allocation > i64::MAX as u64 {
            return Err(STATUS_INVALID_PARAMETER);
        }
        let ctx = self.context(token)?;
        if !ctx.access.is(FileAccessRights::FILE_WRITE_DATA) {
            return Err(STATUS_ACCESS_DENIED);
        }
        let old = self.0.core.getattr(ctx.inode).map_err(status)?;
        if !matches!(old.entry.kind, EntryKind::File { .. }) {
            return Err(STATUS_FILE_IS_A_DIRECTORY);
        }
        let mode = mode_for(attributes, false)?;
        let mode = if replace {
            mode
        } else if attributes.is(FileAttributes::READONLY) {
            0o444
        } else {
            old.entry.mode
        };
        let attr = self.mutation(self.0.core.setattr(
            ctx.inode,
            SetAttr {
                size: Some(0),
                mode: Some(mode),
                ..Default::default()
            },
        ))?;
        if ctx.write_through {
            self.sync()?;
        }
        info(&attr)
    }
    fn cleanup(&self, token: ContextToken, _: Option<&U16CStr>, flags: CleanupFlags) {
        let result = (|| -> Result<()> {
            let _op = self.lock()?;
            let ctx = self.context(token)?;
            if flags.is(CleanupFlags::DELETE) {
                // Find by stable inode, not a stale pathname after rename/replacement.
                if let Some((name, attr)) = self
                    .0
                    .core
                    .paths()
                    .map_err(status)?
                    .into_iter()
                    .find(|(_, a)| a.inode == ctx.inode)
                {
                    let (parent, leaf) = self.parent(&name)?;
                    if matches!(attr.entry.kind, EntryKind::Directory) {
                        self.mutation(self.0.core.rmdir(parent, &leaf))?;
                    } else {
                        self.mutation(self.0.core.unlink(parent, &leaf))?;
                    }
                }
            }
            // Allocation is a hint. ARCHIVE is a fixed regular-file view;
            // change time derives from mtime. Automatic cleanup notifications
            // for these fields need no write and must not override user mtime.
            if flags.is(CleanupFlags::SET_LAST_ACCESS_TIME)
                || flags.is(CleanupFlags::SET_LAST_WRITE_TIME)
            {
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_err(|_| STATUS_INVALID_PARAMETER)?
                    .as_nanos() as i128;
                self.mutation(self.0.core.setattr(
                    ctx.inode,
                    SetAttr {
                        accessed_ns: flags.is(CleanupFlags::SET_LAST_ACCESS_TIME).then_some(now),
                        modified_ns: flags.is(CleanupFlags::SET_LAST_WRITE_TIME).then_some(now),
                        ..Default::default()
                    },
                ))?;
            }
            Ok(())
        })();
        if let Err(e) = result {
            self.failure(format!("Cleanup cannot report failure: NTSTATUS {e:#x}"));
        }
        // DO NOT release the core handle here: paging IO can follow Cleanup.
    }
    fn close(&self, token: ContextToken) {
        let result = (|| -> Result<()> {
            let _op = self.lock()?;
            let ctx = self
                .state()?
                .contexts
                .remove(&token.0)
                .ok_or(STATUS_INVALID_HANDLE)?;
            self.0.core.release(ctx.handle).map_err(status)
        })();
        if let Err(e) = result {
            self.failure(format!("final Close failed: NTSTATUS {e:#x}"));
        }
    }
    fn read(&self, token: ContextToken, buffer: &mut [u8], offset: u64) -> Result<usize> {
        let _op = self.lock()?;
        let ctx = self.context(token)?;
        if !ctx.access.is(FileAccessRights::FILE_READ_DATA) {
            return Err(STATUS_ACCESS_DENIED);
        }
        let bytes = self
            .0
            .core
            .read(ctx.handle, offset, buffer.len())
            .map_err(status)?;
        if bytes.is_empty() && !buffer.is_empty() {
            return Err(STATUS_END_OF_FILE);
        }
        buffer[..bytes.len()].copy_from_slice(&bytes);
        Ok(bytes.len())
    }
    fn write(
        &self,
        token: ContextToken,
        buffer: &[u8],
        mode: WriteMode,
    ) -> Result<(usize, FileInfo)> {
        let _op = self.lock()?;
        let ctx = self.context(token)?;
        let mut buffer = &buffer[..buffer.len().min(MAX_WRITE_BYTES)];
        let count = match mode {
            WriteMode::WriteToEOF => {
                if ctx.access.0
                    & (FileAccessRights::FILE_WRITE_DATA.0 | FileAccessRights::FILE_APPEND_DATA.0)
                    == 0
                {
                    return Err(STATUS_ACCESS_DENIED);
                }
                self.mutation(self.0.core.write_append(ctx.handle, buffer))?
            }
            WriteMode::Normal { offset } | WriteMode::ConstrainedIO { offset } => {
                if !ctx.access.is(FileAccessRights::FILE_WRITE_DATA) {
                    return Err(STATUS_ACCESS_DENIED);
                }
                if matches!(mode, WriteMode::ConstrainedIO { .. }) {
                    let size = info(&self.0.core.getattr(ctx.inode).map_err(status)?)?.file_size();
                    buffer = &buffer[..buffer
                        .len()
                        .min(size.saturating_sub(offset).min(usize::MAX as u64) as usize)];
                }
                if buffer.is_empty() {
                    0
                } else {
                    self.mutation(self.0.core.as_ref().write(ctx.handle, offset, buffer))?
                }
            }
        };
        if ctx.write_through {
            self.sync()?;
        }
        Ok((
            count,
            info(&self.0.core.getattr(ctx.inode).map_err(status)?)?,
        ))
    }
    fn flush(&self, token: ContextToken) -> Result<FileInfo> {
        let _op = self.lock()?;
        let inode = if token.0 == 0 {
            ROOT_INODE
        } else {
            self.context(token)?.inode
        };
        self.sync()?;
        info(&self.0.core.getattr(inode).map_err(status)?)
    }
    fn get_file_info(&self, token: ContextToken) -> Result<FileInfo> {
        let _op = self.lock()?;
        info(
            &self
                .0
                .core
                .getattr(self.context(token)?.inode)
                .map_err(status)?,
        )
    }
    fn set_basic_info(
        &self,
        token: ContextToken,
        attributes: FileAttributes,
        creation: u64,
        access: u64,
        write: u64,
        change: u64,
    ) -> Result<FileInfo> {
        let _op = self.lock()?;
        let ctx = self.context(token)?;
        if !ctx.access.is(FileAccessRights::FILE_WRITE_ATTRIBUTES) {
            return Err(STATUS_ACCESS_DENIED);
        }
        // The revision model has no independent creation/change timestamps.
        if creation != 0 || change != 0 {
            return Err(STATUS_NOT_SUPPORTED);
        }
        let old = self.0.core.getattr(ctx.inode).map_err(status)?;
        if attributes != FileAttributes::INVALID
            && matches!(old.entry.kind, EntryKind::File { .. })
            && !attributes.is(FileAttributes::ARCHIVE)
        {
            return Err(STATUS_NOT_SUPPORTED);
        }
        let mode = if attributes == FileAttributes::INVALID {
            None
        } else {
            Some(mode_for(
                attributes,
                matches!(old.entry.kind, EntryKind::Directory),
            )?)
        };
        let attrs = SetAttr {
            mode,
            accessed_ns: if access == 0 {
                None
            } else {
                Some(nanoseconds(access)?)
            },
            modified_ns: if write == 0 {
                None
            } else {
                Some(nanoseconds(write)?)
            },
            ..Default::default()
        };
        info(&self.mutation(self.0.core.setattr(ctx.inode, attrs))?)
    }
    fn set_file_size(
        &self,
        token: ContextToken,
        size: u64,
        allocation_only: bool,
    ) -> Result<FileInfo> {
        let _op = self.lock()?;
        let ctx = self.context(token)?;
        if !ctx.access.is(FileAccessRights::FILE_WRITE_DATA) {
            return Err(STATUS_ACCESS_DENIED);
        }
        if size > i64::MAX as u64 {
            return Err(STATUS_INVALID_PARAMETER);
        }
        let attr = self.0.core.getattr(ctx.inode).map_err(status)?;
        if !matches!(attr.entry.kind, EntryKind::File { .. }) {
            return Err(STATUS_FILE_IS_A_DIRECTORY);
        }
        if allocation_only {
            return info(&attr);
        }
        let attr = self.mutation(self.0.core.setattr(
            ctx.inode,
            SetAttr {
                size: Some(size),
                ..Default::default()
            },
        ))?;
        if ctx.write_through {
            self.sync()?;
        }
        info(&attr)
    }
    fn can_delete(&self, token: ContextToken, _: &U16CStr) -> Result<()> {
        let _op = self.lock()?;
        let ctx = self.context(token)?;
        if ctx.inode == ROOT_INODE || !ctx.access.is(FileAccessRights::DELETE) {
            return Err(STATUS_ACCESS_DENIED);
        }
        let attr = self.0.core.getattr(ctx.inode).map_err(status)?;
        if attr.entry.mode & 0o222 == 0 {
            return Err(STATUS_CANNOT_DELETE);
        }
        if matches!(attr.entry.kind, EntryKind::Directory)
            && !self.0.core.readdir(ctx.inode).map_err(status)?.is_empty()
        {
            return Err(STATUS_DIRECTORY_NOT_EMPTY);
        }
        Ok(())
    }
    fn rename(
        &self,
        token: ContextToken,
        old: &U16CStr,
        new: &U16CStr,
        replace: bool,
    ) -> Result<()> {
        let _op = self.lock()?;
        let ctx = self.context(token)?;
        if !ctx.access.is(FileAccessRights::DELETE) {
            return Err(STATUS_ACCESS_DENIED);
        }
        let old = path(old)?;
        let new = path(new)?;
        if self.0.core.lookup_path(&old).map_err(status)?.inode != ctx.inode {
            return Err(STATUS_OBJECT_NAME_NOT_FOUND);
        }
        if old == new {
            return Ok(());
        }
        match self.0.core.lookup_path(&new) {
            Ok(target) => {
                if !replace {
                    return Err(STATUS_OBJECT_NAME_COLLISION);
                }
                if target.entry.mode & 0o222 == 0 {
                    return Err(STATUS_ACCESS_DENIED);
                }
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => (),
            Err(e) => return Err(status(e)),
        }
        let (parent, leaf) = self.parent(&old)?;
        let (new_parent, new_leaf) = self.parent(&new)?;
        self.mutation(self.0.core.rename(parent, &leaf, new_parent, &new_leaf))
    }
    fn get_security(&self, token: ContextToken) -> Result<PSecurityDescriptor> {
        let _op = self.lock()?;
        let ctx = self.context(token)?;
        let attr = self.0.core.getattr(ctx.inode).map_err(status)?;
        Ok(self
            .security(matches!(attr.entry.kind, EntryKind::Directory))
            .as_ptr())
    }
    fn set_security(&self, _: ContextToken, _: u32, _: PSecurityDescriptor) -> Result<()> {
        Err(STATUS_NOT_SUPPORTED)
    }
    fn read_directory(
        &self,
        token: ContextToken,
        marker: Option<&U16CStr>,
        mut add: impl FnMut(DirInfo) -> bool,
    ) -> Result<()> {
        let _op = self.lock()?;
        let ctx = self.context(token)?;
        let marker = marker
            .map(|m| m.to_string().map_err(|_| STATUS_OBJECT_NAME_INVALID))
            .transpose()?;
        let mut snapshot = ctx.enumeration.lock().map_err(|_| STATUS_IO_DEVICE_ERROR)?;
        if marker.is_none() || snapshot.is_none() {
            let mut entries = Vec::new();
            if ctx.inode != ROOT_INODE {
                entries.push((
                    ".".into(),
                    info(&self.0.core.getattr(ctx.inode).map_err(status)?)?,
                ));
                entries.push((
                    "..".into(),
                    info(&self.0.core.lookup(ctx.inode, "..").map_err(status)?)?,
                ));
            }
            for (name, attr) in self.0.core.readdir(ctx.inode).map_err(status)? {
                entries.push((name, info(&attr)?));
            }
            entries.sort_by(|a, b| a.0.encode_utf16().cmp(b.0.encode_utf16()));
            *snapshot = Some(entries);
        }
        for (name, info) in snapshot.as_ref().ok_or(STATUS_IO_DEVICE_ERROR)? {
            if marker.as_ref().is_some_and(|m| {
                name.encode_utf16().cmp(m.encode_utf16()) != std::cmp::Ordering::Greater
            }) {
                continue;
            }
            if !add(DirInfo::from_str(*info, name)) {
                break;
            }
        }
        Ok(())
    }
    fn dispatcher_stopped(&self, normally: bool) {
        if let Ok(mut state) = self.0.state.lock() {
            state.dispatcher_stopped = Some(normally);
            if !normally {
                state
                    .sticky
                    .get_or_insert_with(|| "WinFsp dispatcher stopped abnormally".into());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;

    fn entry(kind: EntryKind) -> Entry {
        Entry {
            id: "entry".into(),
            link_group: None,
            mode: if matches!(kind, EntryKind::Directory) {
                0o755
            } else {
                0o644
            },
            modified_ns: 0,
            kind,
        }
    }
    fn fixture() -> (PathBuf, WindowsCas) {
        winfsp_wrs::init().expect("Windows callback tests require the approved WinFsp runtime");
        let root = std::env::temp_dir().join(format!(
            "sprig-win-callbacks-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir(&root).unwrap();
        let root = fs::canonicalize(root).unwrap();
        let objects = root.join("objects");
        let generation = root.join("generation");
        fs::create_dir(&objects).unwrap();
        fs::create_dir(&generation).unwrap();
        let mut base = Manifest::new();
        base.insert("".into(), entry(EntryKind::Directory));
        let core = Arc::new(WritableCas::create(&objects, &generation, &base, "r", "g").unwrap());
        // Synthetic callbacks only; this fixture is never mounted.
        let security = SecurityDescriptor::from_wstr(u16cstr!("O:SYG:SYD:P(A;;FA;;;SY)")).unwrap();
        let directory_security =
            SecurityDescriptor::from_wstr(u16cstr!("O:SYG:SYD:P(A;OICI;FA;;;SY)")).unwrap();
        (
            root,
            WindowsCas::new(core, security, directory_security).unwrap(),
        )
    }
    #[test]
    fn preflight_and_filetime_are_checked() {
        for disposition in 0..=5 {
            assert!(check_options(CreateOptions((disposition << 24) | 0x21)).is_ok());
        }
        assert!(check_options(CreateOptions(6 << 24)).is_err());
        assert!(
            check_options(CreateOptions(
                (1 << 24) | CreateOptions::FILE_OPEN_BY_FILE_ID.0
            ))
            .is_err()
        );
        for name in [
            "CON", "com1.txt", "LPT²", "a:b", "a\\b", "trail.", "trail ", "..", "a\0b",
        ] {
            assert!(!valid_name(name), "{name}");
        }
        for name in ["src", "Cargo.toml", "界", "com10"] {
            assert!(valid_name(name), "{name}");
        }
        assert_eq!(ticks(0).unwrap(), EPOCH_TICKS as u64);
        assert_eq!(nanoseconds(EPOCH_TICKS as u64).unwrap(), 0);
        assert!(ticks(i128::MAX).is_err());
        assert!(nanoseconds(u64::MAX).is_err());
        let mut base = Manifest::new();
        let mut file = entry(EntryKind::File {
            size: 0,
            chunks: vec![],
        });
        file.link_group = Some("group".into());
        base.insert("a".into(), file.clone());
        assert!(preflight_manifest(&base).is_ok());
        base.insert("b".into(), file);
        assert!(preflight_manifest(&base).is_err());
    }
    #[test]
    fn cleanup_keeps_context_until_close_and_volume_flush_accepts_zero() {
        let (root, adapter) = fixture();
        let core = adapter.0.core.clone();
        let attr = core.create_file(ROOT_INODE, "a", 0o644).unwrap();
        let access = FileAccessRights::FILE_GENERIC_READ
            | FileAccessRights::FILE_GENERIC_WRITE
            | FileAccessRights::DELETE;
        let (token, _) = adapter
            .open(u16cstr!("\\a"), CreateOptions(0), access)
            .unwrap();
        adapter.write(token, b"abc", WriteMode::WriteToEOF).unwrap();
        adapter.cleanup(token, None, CleanupFlags(0));
        assert_eq!(adapter.active_handles().unwrap(), 1);
        adapter.write(token, b"d", WriteMode::WriteToEOF).unwrap();
        assert_eq!(core.read_inode(attr.inode, 0, 10).unwrap(), b"abcd");
        assert_eq!(
            adapter
                .write(token, b"XX", WriteMode::ConstrainedIO { offset: 3 })
                .unwrap()
                .0,
            1
        );
        assert_eq!(
            adapter
                .write(token, b"XX", WriteMode::ConstrainedIO { offset: 9 })
                .unwrap()
                .0,
            0
        );
        assert_eq!(core.read_inode(attr.inode, 0, 10).unwrap(), b"abcX");
        adapter.set_file_size(token, 1, true).unwrap();
        assert_eq!(adapter.get_file_info(token).unwrap().file_size(), 4);
        adapter.flush(ContextToken(0)).unwrap();
        adapter.gate_new_opens().unwrap();
        assert!(adapter.sync_for_stop().is_err());
        assert!(
            adapter
                .open(u16cstr!("\\a"), CreateOptions(0), access)
                .is_err()
        );
        adapter.close(token);
        assert_eq!(adapter.active_handles().unwrap(), 0);
        adapter.sync_for_stop().unwrap();
        assert!(adapter.clean_stop_ready().is_err());
        adapter.dispatcher_stopped(true);
        adapter.clean_stop_ready().unwrap();
        drop(core);
        drop(adapter);
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn directory_context_is_busy_and_ordinary_rejections_are_not_sticky() {
        let (root, adapter) = fixture();
        let (token, _) = adapter
            .open(
                u16cstr!("\\"),
                CreateOptions::FILE_DIRECTORY_FILE,
                FileAccessRights::FILE_GENERIC_READ,
            )
            .unwrap();
        assert_eq!(adapter.active_handles().unwrap(), 1);
        assert_eq!(
            adapter.set_security(token, 0, PSecurityDescriptor::default()),
            Err(STATUS_NOT_SUPPORTED)
        );
        adapter.gate_new_opens().unwrap();
        assert!(adapter.sync_for_stop().is_err());
        adapter.ungate_new_opens().unwrap();
        adapter.close(token);
        adapter.gate_new_opens().unwrap();
        adapter.sync_for_stop().unwrap();
        adapter.dispatcher_stopped(true);
        adapter.clean_stop_ready().unwrap();
        drop(adapter);
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn security_comparison_rejects_other_subjects_and_rights() {
        let (root, adapter) = fixture();
        assert!(
            adapter
                .same_security(&adapter.0.file_security, false)
                .unwrap()
        );
        let other = SecurityDescriptor::from_wstr(u16cstr!("O:SYG:SYD:P(A;;FA;;;WD)")).unwrap();
        assert!(!adapter.same_security(&other, false).unwrap());
        let less = SecurityDescriptor::from_wstr(u16cstr!("O:SYG:SYD:P(A;;FR;;;SY)")).unwrap();
        assert!(!adapter.same_security(&less, false).unwrap());
        let owner = SecurityDescriptor::from_wstr(u16cstr!("O:BAG:SYD:P(A;;FA;;;SY)")).unwrap();
        assert!(!adapter.same_security(&owner, false).unwrap());
        let inheritance =
            SecurityDescriptor::from_wstr(u16cstr!("O:SYG:SYD:P(A;OICI;FA;;;SY)")).unwrap();
        assert!(!adapter.same_security(&inheritance, false).unwrap());
        let unprotected =
            SecurityDescriptor::from_wstr(u16cstr!("O:SYG:SYD:(A;;FA;;;SY)")).unwrap();
        assert!(adapter.same_security(&unprotected, false).unwrap());
        let extra =
            SecurityDescriptor::from_wstr(u16cstr!("O:SYG:SYD:P(A;;FA;;;SY)(A;;FA;;;BA)")).unwrap();
        assert!(!adapter.same_security(&extra, false).unwrap());
        let inherit_only =
            SecurityDescriptor::from_wstr(u16cstr!("O:SYG:SYD:P(A;OIIO;FA;;;SY)")).unwrap();
        assert!(!adapter.same_security(&inherit_only, false).unwrap());
        assert!(
            adapter
                .same_security(&adapter.0.directory_security, true)
                .unwrap()
        );
        let group = SecurityDescriptor::from_wstr(u16cstr!("O:SYG:BAD:P(A;;FA;;;SY)")).unwrap();
        assert!(!adapter.same_security(&group, false).unwrap());
        let audit =
            SecurityDescriptor::from_wstr(u16cstr!("O:SYG:SYD:P(A;;FA;;;SY)S:(AU;SA;FA;;;SY)"))
                .unwrap();
        assert!(!adapter.same_security(&audit, false).unwrap());
        let bytes = adapter.0.file_security.as_bytes();
        let mut null_dacl = bytes.to_vec();
        null_dacl[16..20].fill(0);
        assert!(!equivalent_security(bytes, &null_dacl).unwrap());
        let mut absent_dacl = null_dacl.clone();
        let control = u16::from_le_bytes([absent_dacl[2], absent_dacl[3]]) & !0x0004;
        absent_dacl[2..4].copy_from_slice(&control.to_le_bytes());
        assert!(!equivalent_security(bytes, &absent_dacl).unwrap());
        let mut rearranged = bytes[..20].to_vec();
        // Repack referenced components in reverse order, with different offsets
        // and padding. This is the documented self-relative Windows wire format.
        for at in [16, 12, 8, 4] {
            let old = u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap()) as usize;
            if old == 0 {
                continue;
            }
            let length = if at < 12 {
                8 + bytes[old + 1] as usize * 4
            } else {
                u16::from_le_bytes([bytes[old + 2], bytes[old + 3]]) as usize
            };
            rearranged.extend_from_slice(&[0; 4]);
            let new = rearranged.len() as u32;
            rearranged[at..at + 4].copy_from_slice(&new.to_le_bytes());
            rearranged.extend_from_slice(&bytes[old..old + length]);
        }
        let control = u16::from_le_bytes([rearranged[2], rearranged[3]]) | 0x0003;
        rearranged[2..4].copy_from_slice(&control.to_le_bytes());
        assert!(equivalent_security(bytes, &rearranged).unwrap());
        let dacl = u32::from_le_bytes(rearranged[16..20].try_into().unwrap()) as usize;
        rearranged[dacl + 8 + 1] |= 0x10; // inherited DACL ACE provenance
        let control = u16::from_le_bytes([rearranged[2], rearranged[3]]);
        rearranged[2..4].copy_from_slice(&((control & !0x1000) | 0x0400).to_le_bytes());
        assert!(equivalent_security(bytes, &rearranged).unwrap());
        let mut malformed = rearranged.clone();
        malformed[4..8].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(equivalent_security(bytes, &malformed).is_err());
        drop(adapter);
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn sticky_io_failure_cannot_be_erased_by_later_success() {
        let (root, adapter) = fixture();
        adapter.failure("synthetic write IO failure".into());
        adapter.flush(ContextToken(0)).unwrap();
        adapter.gate_new_opens().unwrap();
        assert!(adapter.sync_for_stop().is_err());
        adapter.dispatcher_stopped(true);
        assert!(adapter.clean_stop_ready().is_err());
        drop(adapter);
        fs::remove_dir_all(root).unwrap();
    }
}
