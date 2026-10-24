//! Optional WinFsp lifecycle. Only the exact `generation/tree` junction is
//! followed, and only after binding it to a live session and volume identity.
//! All other paths retain platform.rs's no-reparse checks. Unknown startup or
//! shutdown preserves the journal and session markers; no PID killing, forced
//! detach, service installation, or destructor-based clean certification.
use crate::content::{Manifest, Result};
use crate::writable::WritableCas;
use crate::writable_windows::{WindowsCas, preflight_manifest};
use crate::{platform, writable_session as session};
use serde::{Deserialize, Serialize};
use std::ffi::c_void;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
use std::os::windows::io::{AsRawHandle, FromRawHandle};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};
use winfsp_wrs::{FileSystem, Params, SecurityDescriptor, U16CString};

const READY: &str = "writable-windows-ready.json";
const BUSY: &str = "writable-windows-busy.json";
const CONTROL: &str = "writable-windows-control.lock";
const START_HANDLE: &str = "SPRIG_WINDOWS_START_HANDLE";
const REPARSE: u32 = 0x400;
const TIMEOUT: Duration = Duration::from_secs(15);
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum MountState {
    Mounted,
    Unmounted,
    Partial,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReadyRecord {
    session: session::SessionRecord,
    nt_volume: String,
    serial: u32,
    filesystem: String,
}
fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}
fn uncertain(message: impl std::fmt::Display) -> io::Error {
    io::Error::other(format!(
        "{message}; Windows writable session outcome is uncertain; generation retained for recovery"
    ))
}
fn generation_name(path: &Path) -> io::Result<&str> {
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| invalid("invalid generation name"))?;
    if name.is_empty()
        || name.len() > 96
        || !name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
    {
        return Err(invalid("invalid generation name"));
    }
    Ok(name)
}
fn checked_directory(path: &Path) -> io::Result<()> {
    if !platform::symlink_metadata(path)?.is_dir() {
        return Err(invalid("expected a real owned directory"));
    }
    Ok(())
}
fn valid_revision(revision: &str) -> bool {
    revision.len() == 64
        && revision
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
fn read_json<T: serde::de::DeserializeOwned>(generation: &Path, name: &str) -> io::Result<T> {
    let mut bytes = Vec::new();
    platform::open_regular(&generation.join(name))?
        .take(8193)
        .read_to_end(&mut bytes)?;
    if bytes.len() > 8192 {
        return Err(invalid("oversized Windows control record"));
    }
    serde_json::from_slice(&bytes).map_err(io::Error::other)
}
fn write_json<T: Serialize>(generation: &Path, name: &str, value: &T) -> io::Result<()> {
    let bytes = serde_json::to_vec(value).map_err(io::Error::other)?;
    let mut file = private_new(&generation.join(name))?;
    file.write_all(&bytes)?;
    file.sync_all()?;
    drop(file);
    platform::sync_directory(generation)
}
fn absent(path: &Path) -> io::Result<bool> {
    match fs::symlink_metadata(path) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(true),
        Err(e) => Err(e),
        Ok(_) => Ok(false),
    }
}
fn control(generation: &Path) -> io::Result<File> {
    checked_directory(generation)?;
    let path = generation.join(CONTROL);
    match private_new(&path) {
        Ok(f) => drop(f),
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(e),
    }
    platform::open_lease(&path)
}

/// The inherited exclusive handle retains startup serialization on timeout.
/// Its identity is checked before the daemon opens the journal or mounts.
pub(crate) fn take_start_lock(generation: &Path) -> Result<File> {
    checked_directory(generation)?;
    let raw = std::env::var(START_HANDLE)
        .map_err(|_| invalid("missing inherited Windows startup lock"))?
        .parse::<usize>()
        .map_err(|_| invalid("invalid startup handle"))?;
    if raw == 0 || raw == usize::MAX {
        return Err(invalid("invalid startup handle").into());
    }
    // SAFETY: the internal launch protocol passes ownership of one inherited
    // handle. No pointer is dereferenced; Windows validates the handle below.
    let file = unsafe { File::from_raw_handle(raw as *mut c_void) };
    platform::file_metadata(&file)?;
    let expected = generation.canonicalize()?.join(CONTROL);
    let actual = final_name(&file, 0)?;
    if Path::new(&actual) != expected {
        return Err(invalid("startup lock identity mismatch").into());
    }
    // Stop inheriting this handle into any further children.
    if unsafe { SetHandleInformation(file.as_raw_handle(), 1, 0) } == 0 {
        return Err(io::Error::last_os_error().into());
    }
    Ok(file)
}

pub(crate) fn inspect(generation: &Path) -> Result<MountState> {
    checked_directory(generation)?;
    generation_name(generation)?;
    let target = generation.join("tree");
    let metadata = match fs::symlink_metadata(&target) {
        Ok(m) => m,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return unmounted_state(generation),
        Err(e) => return Err(e.into()),
    };
    if metadata.file_attributes() & REPARSE == 0 {
        checked_directory(&target)?;
        return unmounted_state(generation);
    }
    let ready: ReadyRecord = read_json(generation, READY)?;
    verify_record(generation, &ready, None)?;
    Ok(MountState::Mounted)
}
fn unmounted_state(generation: &Path) -> Result<MountState> {
    Ok(
        if session::has_session(generation)?
            || !absent(&generation.join(READY))?
            || !absent(&generation.join(BUSY))?
        {
            MountState::Partial
        } else {
            MountState::Unmounted
        },
    )
}
pub(crate) fn verify_binding(generation: &Path, revision: &str) -> Result<()> {
    checked_directory(generation)?;
    let ready: ReadyRecord = read_json(generation, READY)?;
    verify_record(generation, &ready, Some(revision))?;
    Ok(())
}
fn verify_record(
    generation: &Path,
    record: &ReadyRecord,
    revision: Option<&str>,
) -> io::Result<()> {
    if session::active_record(generation)? != record.session
        || record.session.generation != generation_name(generation)?
        || revision.is_some_and(|r| r != record.session.revision)
        || record.serial != serial(&record.session)
        || record.filesystem != "SprigCAS"
        || !valid_nt_volume(&record.nt_volume)
    {
        return Err(invalid(
            "Windows ready record does not match the exact session",
        ));
    }
    let target = generation.join("tree");
    if fs::symlink_metadata(&target)?.file_attributes() & REPARSE == 0 {
        return Err(invalid("owned WinFsp mount junction is absent"));
    }
    let root = open_mount_root(&target)?;
    let (volume, serial, filesystem) = identity(&root)?;
    if volume != record.nt_volume || serial != record.serial || filesystem != record.filesystem {
        return Err(invalid("Windows writable mount volume identity mismatch"));
    }
    Ok(())
}
fn serial(record: &session::SessionRecord) -> u32 {
    let hash = blake3::hash(
        format!("{}:{}:{}", record.generation, record.revision, record.nonce).as_bytes(),
    );
    u32::from_le_bytes(hash.as_bytes()[..4].try_into().expect("four bytes"))
}
fn prepare_target(generation: &Path) -> io::Result<()> {
    let target = generation.join("tree");
    if absent(&target)? {
        return Ok(());
    }
    // WinFsp directory mount points MUST NOT exist. Never delete an arbitrary
    // junction or populated directory merely because it occupies this name.
    checked_directory(&target)?;
    if fs::read_dir(&target)?.next().is_some() {
        return Err(invalid("owned Windows mount target is not empty"));
    }
    fs::remove_dir(&target)?;
    platform::sync_directory(generation)
}

pub(crate) fn start(
    store_root: &Path,
    revision: &str,
    generation: &Path,
    executable: &Path,
    manifest: &Manifest,
    _initialize: bool,
) -> Result<()> {
    preflight_manifest(manifest)?;
    if !valid_revision(revision) {
        return Err(invalid("invalid writable base revision").into());
    }
    checked_directory(store_root)?;
    checked_directory(generation)?;
    if generation != store_root.join("native").join(generation_name(generation)?) {
        return Err(
            invalid("generation must be a direct child of this store's native directory").into(),
        );
    }
    let lock = control(generation)?;
    if inspect(generation)? == MountState::Mounted {
        return verify_binding(generation, revision);
    }
    session::ensure_clean(generation)?;
    if !absent(&generation.join(READY))? || !absent(&generation.join(BUSY))? {
        return Err(
            invalid("stale Windows control evidence; recover into a new generation").into(),
        );
    }
    // Resolve runtime before removing the empty target or starting a daemon.
    winfsp_wrs::init().map_err(|e| io::Error::new(io::ErrorKind::Unsupported, e.to_string()))?;
    prepare_target(generation)?;
    let attempt = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_nanos();
    let log = private_new(&generation.join(format!("writable-windows-{attempt}.log")))?;
    let mut command = Command::new(executable);
    command
        .current_dir(store_root)
        .arg("--store")
        .arg(store_root)
        .arg("__writable-serve")
        .arg("--revision")
        .arg(revision)
        .arg("--generation")
        .arg(generation_name(generation)?)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(log);
    static SPAWN: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let mut child = {
        let _guard = SPAWN
            .lock()
            .map_err(|_| io::Error::other("Windows spawn lock poisoned"))?;
        let inherited = lock.try_clone()?;
        if unsafe { SetHandleInformation(inherited.as_raw_handle(), 1, 1) } == 0 {
            return Err(io::Error::last_os_error().into());
        }
        command.env(
            START_HANDLE,
            (inherited.as_raw_handle() as usize).to_string(),
        );
        let spawned = command.spawn();
        drop(inherited);
        spawned?
    };
    let mut stdout = child
        .stdout
        .take()
        .ok_or_else(|| uncertain("missing readiness pipe"))?;
    let (tx, rx) = mpsc::sync_channel(1);
    std::thread::spawn(move || {
        let mut byte = [0];
        let result = stdout.read_exact(&mut byte).and_then(|()| {
            if byte == *b"R" {
                Ok(())
            } else {
                Err(invalid("invalid readiness byte"))
            }
        });
        let _ = tx.send(result);
    });
    rx.recv_timeout(TIMEOUT)
        .map_err(uncertain)?
        .map_err(uncertain)?;
    verify_binding(generation, revision)?;
    // Child ownership is intentionally not a kill-on-drop process wrapper.
    drop(child);
    Ok(())
}

/// The core and its journal lease remain owned by this daemon until the exact
/// session has completed a verified volume flush and normal dispatcher stop.
pub(crate) fn serve(
    core: Arc<WritableCas>,
    generation: &Path,
    revision: &str,
    startup: File,
) -> Result<()> {
    checked_directory(generation)?;
    if !valid_revision(revision) || !absent(&generation.join("tree"))? {
        return Err(
            invalid("Windows server requires an absent mount target and valid revision").into(),
        );
    }
    winfsp_wrs::init().map_err(|e| io::Error::new(io::ErrorKind::Unsupported, e.to_string()))?;
    let security = SecurityDescriptor::from_wstr(
        &U16CString::from_str(current_user_sddl()?).map_err(io::Error::other)?,
    )
    .map_err(io::Error::other)?;
    let adapter = WindowsCas::new(core, security)?;
    let record = session::begin(generation, revision)?;
    let mut params = Params::default();
    params
        .volume_params
        .set_sector_size(4096)
        .set_sectors_per_allocation_unit(1)
        .set_max_component_length(255)
        .set_case_sensitive_search(true)
        .set_case_preserved_names(true)
        .set_unicode_on_disk(true)
        .set_persistent_acls(false)
        .set_hard_links(false)
        .set_reparse_point(false)
        .set_named_streams(false)
        .set_extended_attributes(false)
        .set_volume_serial_number(serial(&record))
        .set_flush_and_purge_on_cleanup(true);
    let fs_name = U16CString::from_str("SprigCAS").map_err(io::Error::other)?;
    if params.volume_params.set_file_system_name(&fs_name).is_err() {
        return Err(invalid("invalid WinFsp filesystem name").into());
    }
    let target = U16CString::from_os_str(generation.join("tree")).map_err(io::Error::other)?;
    let host = FileSystem::start(params, Some(&target), adapter.clone())
        .map_err(|code| uncertain(format!("WinFsp start failed with NTSTATUS {code:#x}")))?;
    // No FileSystem layout assumptions or private FSP_FILE_SYSTEM pointers.
    let ready = {
        let root = open_mount_root(&generation.join("tree"))?;
        let (nt_volume, volume_serial, filesystem) = identity(&root)?;
        ReadyRecord {
            session: record.clone(),
            nt_volume,
            serial: volume_serial,
            filesystem,
        }
    };
    if ready.serial != serial(&record) || ready.filesystem != "SprigCAS" {
        return Err(uncertain("unexpected WinFsp volume identity").into());
    }
    // This raw-volume handle does not create a filesystem file context. Keep
    // it alive for FlushFileBuffers while dispatch remains operational.
    let volume = open_volume(&ready.nt_volume)?;
    write_json(generation, READY, &ready)?;
    verify_record(generation, &ready, Some(revision))?;
    {
        let mut out = io::stdout().lock();
        out.write_all(b"R")?;
        out.flush()?;
    }
    drop(startup);
    loop {
        if adapter.dispatcher_finished()? {
            return Err(uncertain("dispatcher stopped without a certified stop").into());
        }
        if session::read_stop(generation, &record)? {
            adapter.gate_new_opens()?;
            if adapter.active_handles()? != 0 {
                session::cancel_stop(generation, &record)?;
                write_json(generation, BUSY, &record)?;
                adapter.ungate_new_opens()?;
                std::thread::sleep(Duration::from_millis(25));
                continue;
            }
            // Kernel cache flush comes FIRST. StopDispatcher is not a flush.
            if unsafe { FlushFileBuffers(volume.as_raw_handle()) } == 0 {
                return Err(uncertain(io::Error::last_os_error()).into());
            }
            adapter.sync_for_stop()?;
            drop(volume);
            host.stop();
            adapter.clean_stop_ready()?;
            session::acknowledge(generation, &record)?;
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

pub(crate) fn stop(generation: &Path) -> Result<()> {
    let _control = control(generation)?;
    match inspect(generation)? {
        MountState::Unmounted => return Ok(()),
        MountState::Partial => {
            return Err(
                uncertain("unmounted generation still has session or control evidence").into(),
            );
        }
        MountState::Mounted => {}
    }
    let active = session::active_record(generation)?;
    if !absent(&generation.join(BUSY))? {
        let previous: session::SessionRecord = read_json(generation, BUSY)?;
        if previous != active {
            return Err(invalid("stale Windows busy response").into());
        }
        fs::remove_file(generation.join(BUSY))?;
        platform::sync_directory(generation)?;
    }
    let record = session::request_stop(generation)?;
    let deadline = Instant::now() + TIMEOUT;
    loop {
        if !absent(&generation.join(BUSY))? {
            let busy: session::SessionRecord = read_json(generation, BUSY)?;
            if busy != record {
                return Err(invalid("mismatched Windows busy response").into());
            }
            return Err(io::Error::new(io::ErrorKind::WouldBlock, "Windows workspace is busy; close files, directories and memory mappings before retrying").into());
        }
        // Absence alone is NEVER clean evidence. Also require lease release
        // and the exact ACTIVE/STOP/CLEAN certificate below.
        let target = generation.join("tree");
        let unmounted = match fs::symlink_metadata(&target) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => true,
            Err(e) => return Err(e.into()),
            Ok(metadata) if metadata.file_attributes() & REPARSE != 0 => false,
            Ok(_) => {
                checked_directory(&target)?;
                true
            }
        };
        // Do not open the root after requesting STOP: the daemon has gated
        // opens, and a controller root context would itself make stop busy.
        if unmounted {
            match platform::open_lease(&generation.join("writable.lease")) {
                Ok(lease) => {
                    fs::remove_file(generation.join(READY))?;
                    platform::sync_directory(generation)?;
                    session::finish_stop(generation, &record)?;
                    drop(lease);
                    return Ok(());
                }
                Err(e) if matches!(e.raw_os_error(), Some(32 | 33)) => {}
                Err(e) => return Err(e.into()),
            }
        }
        if Instant::now() >= deadline {
            return Err(uncertain(
                "timed out waiting for clean Windows stop and journal lease release",
            )
            .into());
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

// Minimal, documented Win32 calls, separate from WinFsp's private structures.
fn wide(path: impl AsRef<std::ffi::OsStr>) -> io::Result<Vec<u16>> {
    use std::os::windows::ffi::OsStrExt;
    let mut value: Vec<u16> = path.as_ref().encode_wide().collect();
    if value.contains(&0) {
        return Err(invalid("embedded NUL in Windows path"));
    }
    value.push(0);
    Ok(value)
}
fn open_mount_root(path: &Path) -> io::Result<File> {
    // Sole deliberate reparse traversal: caller checks trusted generation and
    // then attests the resulting handle's volume before trusting any contents.
    OpenOptions::new()
        .access_mode(0x80)
        .share_mode(7)
        .custom_flags(0x02000000)
        .open(path)
}
fn final_name(file: &File, flags: u32) -> io::Result<String> {
    let mut buffer = vec![0u16; 32768];
    let count = unsafe {
        GetFinalPathNameByHandleW(
            file.as_raw_handle(),
            buffer.as_mut_ptr(),
            buffer.len() as u32,
            flags,
        )
    };
    if count == 0 {
        return Err(io::Error::last_os_error());
    }
    if count as usize >= buffer.len() {
        return Err(invalid("oversized final Windows path"));
    }
    String::from_utf16(&buffer[..count as usize]).map_err(|_| invalid("invalid final Windows path"))
}
fn valid_nt_volume(value: &str) -> bool {
    value
        .strip_prefix("\\Device\\Volume{")
        .and_then(|v| v.strip_suffix('}'))
        .is_some_and(|v| v.len() == 36 && v.bytes().all(|b| b.is_ascii_hexdigit() || b == b'-'))
}
fn identity(root: &File) -> io::Result<(String, u32, String)> {
    let path = final_name(root, 2)?; // VOLUME_NAME_NT, normalized file name.
    let volume = path
        .strip_suffix('\\')
        .ok_or_else(|| invalid("mount handle is not a volume root"))?;
    if !valid_nt_volume(volume) {
        return Err(invalid("unexpected WinFsp NT volume path"));
    }
    let mut serial = 0;
    let mut filesystem = [0u16; 64];
    if unsafe {
        GetVolumeInformationByHandleW(
            root.as_raw_handle(),
            std::ptr::null_mut(),
            0,
            &mut serial,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            filesystem.as_mut_ptr(),
            filesystem.len() as u32,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    let end = filesystem
        .iter()
        .position(|v| *v == 0)
        .ok_or_else(|| invalid("unterminated filesystem name"))?;
    Ok((
        volume.to_owned(),
        serial,
        String::from_utf16(&filesystem[..end]).map_err(|_| invalid("invalid filesystem name"))?,
    ))
}
fn open_volume(nt_volume: &str) -> io::Result<File> {
    if !valid_nt_volume(nt_volume) {
        return Err(invalid("invalid raw WinFsp volume path"));
    }
    OpenOptions::new()
        .read(true)
        .write(true)
        .share_mode(7)
        .open(format!("\\\\?\\GLOBALROOT{nt_volume}"))
}
#[repr(C)]
struct SecurityAttributes {
    length: u32,
    descriptor: *mut c_void,
    inherit: i32,
}
fn private_new(path: &Path) -> io::Result<File> {
    // The parent is validated before creation; a protected DACL prevents
    // inherited world/group write access to lifecycle control and log files.
    checked_directory(
        path.parent()
            .ok_or_else(|| invalid("control path has no parent"))?,
    )?;
    let sddl = wide(current_user_sddl()?)?;
    let mut descriptor = std::ptr::null_mut();
    if unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            sddl.as_ptr(),
            1,
            &mut descriptor,
            std::ptr::null_mut(),
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    let name = wide(path)?;
    let attributes = SecurityAttributes {
        length: std::mem::size_of::<SecurityAttributes>() as u32,
        descriptor,
        inherit: 0,
    };
    let handle = unsafe {
        CreateFileW(
            name.as_ptr(),
            0xc0000000,
            0,
            &attributes,
            1,
            0x00200000,
            std::ptr::null_mut(),
        )
    };
    let error = io::Error::last_os_error();
    unsafe {
        LocalFree(descriptor);
    }
    if handle as isize == -1 {
        return Err(error);
    }
    Ok(unsafe { File::from_raw_handle(handle) })
}
fn current_user_sddl() -> io::Result<String> {
    let mut token = std::ptr::null_mut();
    if unsafe { OpenProcessToken(GetCurrentProcess(), 8, &mut token) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let token = unsafe { File::from_raw_handle(token) };
    let mut needed = 0;
    unsafe {
        GetTokenInformation(
            token.as_raw_handle(),
            1,
            std::ptr::null_mut(),
            0,
            &mut needed,
        );
    }
    if needed == 0 || needed > 65536 {
        return Err(invalid("invalid token user size"));
    }
    // Pointer-aligned allocation for TOKEN_USER and embedded SID.
    let mut data = vec![0usize; (needed as usize).div_ceil(std::mem::size_of::<usize>())];
    if unsafe {
        GetTokenInformation(
            token.as_raw_handle(),
            1,
            data.as_mut_ptr().cast(),
            needed,
            &mut needed,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    let sid = data[0] as *mut c_void; // TOKEN_USER begins with SID_AND_ATTRIBUTES.Sid.
    let mut sid_text = std::ptr::null_mut();
    if unsafe { ConvertSidToStringSidW(sid, &mut sid_text) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let mut len = 0;
    while len < 256 && unsafe { *sid_text.add(len) } != 0 {
        len += 1;
    }
    let value = if len == 256 {
        Err(invalid("oversized current-user SID"))
    } else {
        String::from_utf16(unsafe { std::slice::from_raw_parts(sid_text, len) })
            .map_err(|_| invalid("invalid current-user SID"))
    };
    unsafe {
        LocalFree(sid_text.cast());
    }
    let sid = value?;
    Ok(format!("O:{sid}G:{sid}D:P(A;;FA;;;{sid})"))
}
#[link(name = "kernel32")]
unsafe extern "system" {
    fn GetCurrentProcess() -> *mut c_void;
    fn LocalFree(memory: *mut c_void) -> *mut c_void;
    fn SetHandleInformation(handle: *mut c_void, mask: u32, flags: u32) -> i32;
    fn FlushFileBuffers(handle: *mut c_void) -> i32;
    fn GetFinalPathNameByHandleW(handle: *mut c_void, path: *mut u16, len: u32, flags: u32) -> u32;
    fn GetVolumeInformationByHandleW(
        handle: *mut c_void,
        volume: *mut u16,
        len: u32,
        serial: *mut u32,
        max_component: *mut u32,
        flags: *mut u32,
        filesystem: *mut u16,
        filesystem_len: u32,
    ) -> i32;
    fn CreateFileW(
        name: *const u16,
        access: u32,
        share: u32,
        security: *const SecurityAttributes,
        creation: u32,
        flags: u32,
        template: *mut c_void,
    ) -> *mut c_void;
}
#[link(name = "advapi32")]
unsafe extern "system" {
    fn OpenProcessToken(process: *mut c_void, access: u32, token: *mut *mut c_void) -> i32;
    fn GetTokenInformation(
        token: *mut c_void,
        class: u32,
        buffer: *mut c_void,
        length: u32,
        needed: *mut u32,
    ) -> i32;
    fn ConvertSidToStringSidW(sid: *mut c_void, value: *mut *mut u16) -> i32;
    fn ConvertStringSecurityDescriptorToSecurityDescriptorW(
        value: *const u16,
        revision: u32,
        descriptor: *mut *mut c_void,
        size: *mut u32,
    ) -> i32;
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn raw_volume_names_cannot_escape_to_other_devices_or_files() {
        assert!(valid_nt_volume(
            r"\Device\Volume{01234567-89ab-cdef-0123-456789abcdef}"
        ));
        for path in [
            r"\Device\HarddiskVolume1",
            r"\Device\Volume{01234567-89ab-cdef-0123-456789abcdef}\file",
            r"\Device\Volume{..}",
            r"\Device\Volume{01234567-89ab-cdef-0123-456789abcdef}\",
        ] {
            assert!(!valid_nt_volume(path));
        }
    }
    #[test]
    fn volume_serial_is_bound_to_full_session_not_only_revision() {
        let first = session::SessionRecord {
            version: 1,
            generation: "g".into(),
            revision: "a".repeat(64),
            nonce: "abc-123".into(),
        };
        let mut next = first.clone();
        next.nonce = "abc-124".into();
        assert_ne!(serial(&first), serial(&next));
        next = first.clone();
        next.generation = "h".into();
        assert_ne!(serial(&first), serial(&next));
        assert!(valid_revision(&first.revision));
        assert!(!valid_revision(&"A".repeat(64)));
    }
}
