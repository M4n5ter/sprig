mod callback;
mod file_system;
mod filetime;
mod flags;
mod info;
mod init;
mod security;

pub(crate) use callback::TrampolineInterface;

pub use callback::{FileContextKind, FileSystemInterface};
#[cfg(feature = "icon")]
pub use file_system::set_folder_icon;
pub use file_system::{
    pin_to_quick_access, unpin_to_quick_access, FileContextMode, FileSystem,
    OperationGuardStrategy, Params, VolumeParams,
};
pub use filetime::{filetime_from_utc, filetime_now};
pub use flags::{
    CleanupFlags, CreateOptions, FileAccessRights, FileAttributes, FileCreationDisposition,
    FileShareMode,
};
pub use info::{CreateFileInfo, DirInfo, FileInfo, VolumeInfo, VolumeLabelNameTooLong, WriteMode};
pub use init::{init, InitError};
pub use security::{PSecurityDescriptor, SecurityDescriptor};

// Reexport
pub use widestring::*;
pub use windows_sys::Win32::Foundation::*;

/// Whether the active Create request omitted a caller-supplied descriptor.
///
/// Returns `None` for a missing, non-Create, short, stream, or inconsistent
/// request. No request memory is returned or retained.
///
/// # Safety
/// WinFsp must be initialized, and this must be called synchronously on the
/// dispatch thread while servicing a live `FileSystemInterface` callback.
/// The operation context is thread-local and must not be queried from a worker,
/// controller, or a direct synthetic invocation of the Rust callback methods.
///
/// This is an original Sprig extension using the existing generated ABI types.
pub unsafe fn current_create_uses_default_security() -> Option<bool> {
    use winfsp_wrs_sys::{
        FspFileSystemGetOperationContext, FspFsctlTransactCreateKind, FSP_FSCTL_TRANSACT_REQ,
    };
    // SAFETY: caller supplies the live initialized WinFsp callback context.
    let context = unsafe { FspFileSystemGetOperationContext().as_ref() }?;
    // SAFETY: Request belongs to this live callback; no reference escapes.
    let request = unsafe { context.Request.as_ref() }?;
    if usize::from(request.Size) < std::mem::size_of::<FSP_FSCTL_TRANSACT_REQ>()
        || request.Kind != FspFsctlTransactCreateKind as u32
    {
        return None;
    }
    // SAFETY: the typed request is large enough and its kind selects Create.
    let create = unsafe { request.Req.Create };
    if create.NamedStream != 0 {
        return None;
    }
    match (
        create.SecurityDescriptor.Offset,
        create.SecurityDescriptor.Size,
    ) {
        (0, 0) => Some(true),
        (0, _) | (_, 0) => None,
        _ => Some(false),
    }
}
