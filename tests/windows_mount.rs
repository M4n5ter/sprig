#![cfg(all(windows, feature = "windows-mount"))]
//! One explicit, disposable-runner mount contract; never installs a runtime.
use sprig::{Result, Store};
use std::ffi::c_void;
use std::fs::{self, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::io::AsRawHandle;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

#[test]
fn windows_owned_mount_commit_and_lifetime() -> Result<()> {
    if std::env::var("SPRIG_TEST_WINDOWS_MOUNT").as_deref() != Ok("1") {
        eprintln!("WinFsp runtime coverage unavailable: explicit synthetic test opt-in is absent.");
        return Ok(());
    }
    let parent = std::path::PathBuf::from(std::env::var("SPRIG_WINDOWS_MOUNT_RESULTS")?);
    let fixture = parent.join(format!(
        "fixture-{}",
        SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos()
    ));
    fs::create_dir(&fixture)?;
    let fixture = fixture.canonicalize()?;
    let source = fixture.join("source");
    fs::create_dir(&source)?;
    fs::write(source.join("mapped.bin"), vec![0u8; 4096])?;
    fs::write(source.join("original.txt"), b"original")?;
    let store = Store::init(&fixture.join("store"))?
        .with_projection_executable(env!("CARGO_BIN_EXE_sprig").into());
    store.import_writable(&source, "main")?;
    let mut completed = Vec::<&str>::new();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| -> Result<()> {
        let view = store.mount_workspace("main")?;
        completed.push("mounted");
        assert_eq!(fs::read(view.path.join("original.txt"))?, b"original");
        completed.push("read_original");
        let new = view.path.join("created.txt");
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&new)?;
        file.write_all(b"abcdefgh")?;
        completed.push("created_regular_file");
        assert_current_user_owner(&file)?;
        completed.push("created_owner_is_current_user");
        file.set_len(3)?;
        file.set_len(8)?;
        file.seek(SeekFrom::Start(0))?;
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)?;
        assert_eq!(bytes, b"abc\0\0\0\0\0");
        file.seek(SeekFrom::Start(7))?;
        file.write_all(b"Z")?;
        file.sync_all()?;
        assert!(store.checkpoint("main", "must stay busy", None).is_err());
        drop(file);
        fs::rename(&new, view.path.join("renamed.txt"))?;
        fs::create_dir(view.path.join("directory"))?;
        fs::write(view.path.join("directory/child"), b"child")?;
        assert!(
            fs::hard_link(view.path.join("original.txt"), view.path.join("alias.txt")).is_err()
        );
        completed.push("namespace_holes_flush_busy_hardlink_rejection");

        let exclusive = OpenOptions::new()
            .read(true)
            .write(true)
            .share_mode(0)
            .open(view.path.join("renamed.txt"))?;
        let denied = OpenOptions::new()
            .read(true)
            .open(view.path.join("renamed.txt"))
            .unwrap_err();
        assert_eq!(denied.raw_os_error(), Some(32));
        drop(exclusive);
        let mut renamed_handle = OpenOptions::new()
            .read(true)
            .write(true)
            .open(view.path.join("renamed.txt"))?;
        fs::rename(view.path.join("renamed.txt"), view.path.join("moved.txt"))?;
        renamed_handle.seek(SeekFrom::Start(0))?;
        let mut still_open = Vec::new();
        renamed_handle.read_to_end(&mut still_open)?;
        assert_eq!(still_open, b"abc\0\0\0\0Z");
        renamed_handle.seek(SeekFrom::Start(0))?;
        renamed_handle.write_all(b"A")?;
        renamed_handle.sync_all()?;
        drop(renamed_handle);
        fs::rename(view.path.join("moved.txt"), view.path.join("renamed.txt"))?;
        fs::write(view.path.join("delete-open.txt"), b"detached")?;
        let mut detached = OpenOptions::new()
            .read(true)
            .write(true)
            .open(view.path.join("delete-open.txt"))?;
        fs::remove_file(view.path.join("delete-open.txt"))?;
        detached.seek(SeekFrom::Start(0))?;
        let mut old = Vec::new();
        detached.read_to_end(&mut old)?;
        assert_eq!(old, b"detached");
        detached.seek(SeekFrom::Start(0))?;
        detached.write_all(b"retained")?;
        detached.sync_all()?;
        drop(detached);
        assert!(!view.path.join("delete-open.txt").exists());
        let directory = OpenOptions::new()
            .read(true)
            .custom_flags(0x02000000)
            .open(view.path.join("directory"))?;
        assert!(
            store
                .checkpoint("main", "directory remains busy", None)
                .is_err()
        );
        drop(directory);
        completed.push("share_modes_rename_open_delete_open_directory_busy");

        // Establish the ordinary closed-handle lifecycle independently of mmap.
        let plain = store.checkpoint("main", "plain Windows changes", None)?;
        store.restore_writable(&plain.revision, "plain-restored")?;
        let plain_restored = store.mount_workspace("plain-restored")?;
        assert_eq!(
            fs::read(plain_restored.path.join("renamed.txt"))?,
            b"Abc\0\0\0\0Z"
        );
        fs::write(plain_restored.path.join("original.txt"), b"plain-isolated")?;
        let view = store.mount_workspace("main")?;
        assert_eq!(fs::read(view.path.join("original.txt"))?, b"original");
        store.unmount_workspace("plain-restored")?;
        completed.push("plain_checkpoint_restore_isolation_unmount");
        completed.push(check_deleted_mapping_lifetime(&store, &view.path)?);

        let mapped = OpenOptions::new()
            .read(true)
            .write(true)
            .open(view.path.join("mapped.bin"))?;
        // SAFETY: synthetic regular file; handles and view remain alive until
        // the explicit unmap/close below. The mapped offset is inside 4096 bytes.
        unsafe {
            let mapping = CreateFileMappingW(
                mapped.as_raw_handle(),
                std::ptr::null(),
                4,
                0,
                0,
                std::ptr::null(),
            );
            if mapping.is_null() {
                return Err(std::io::Error::last_os_error().into());
            }
            let address = MapViewOfFile(mapping, 6, 0, 0, 4096);
            if address.is_null() {
                CloseHandle(mapping);
                return Err(std::io::Error::last_os_error().into());
            }
            (address as *mut u8).add(17).write(91);
            let flushed = FlushViewOfFile(address, 4096);
            if flushed == 0 {
                UnmapViewOfFile(address);
                CloseHandle(mapping);
                return Err(std::io::Error::last_os_error().into());
            }
            mapped.sync_all()?;
            drop(mapped);
            assert!(
                store
                    .checkpoint("main", "mapped section remains busy", None)
                    .is_err()
            );
            assert_ne!(UnmapViewOfFile(address), 0);
            assert_ne!(CloseHandle(mapping), 0);
        }
        std::thread::sleep(Duration::from_millis(200));
        completed.push("mapped_flush_final_close_barrier");
        let commit = store.checkpoint("main", "Windows owned changes", None)?;
        let now = store.mount_workspace("main")?;
        assert_eq!(fs::read(now.path.join("renamed.txt"))?, b"Abc\0\0\0\0Z");
        assert_eq!(fs::read(now.path.join("mapped.bin"))?[17], 91);
        assert_eq!(
            store.read(&commit.revision, "renamed.txt")?,
            b"Abc\0\0\0\0Z"
        );
        store.restore_writable(&commit.revision, "restored")?;
        let restored = store.mount_workspace("restored")?;
        assert_eq!(fs::read(restored.path.join("mapped.bin"))?[17], 91);
        fs::write(restored.path.join("original.txt"), b"isolated")?;
        assert_eq!(fs::read(now.path.join("original.txt"))?, b"original");
        store.unmount_workspace("restored")?;
        store.unmount_workspace("main")?;
        completed.push("commit_restore_cross_workspace_isolation_normal_unmount");
        Ok(())
    }))
    .unwrap_or_else(|_| {
        Err(
            std::io::Error::other("Windows mount contract assertion failed; retained fixture")
                .into(),
        )
    });
    // Retain all evidence. Normal unmount only; a busy/unknown mount is never
    // removed or recursively deleted by fixture cleanup.
    let cleanup: Vec<_> = ["restored", "plain-restored", "main"]
        .iter()
        .map(|name| {
            (
                name,
                store
                    .unmount_workspace(name)
                    .map(|_| "unmounted".to_owned())
                    .unwrap_or_else(|e| e.to_string()),
            )
        })
        .collect();
    eprintln!(
        "Windows mounted checks completed: {completed:?}; result: {result:?}; cleanup: {cleanup:?}"
    );
    fs::write(
        parent.join("mount-result.json"),
        serde_json::to_vec_pretty(
            &serde_json::json!({"ok":result.is_ok(),"checks":completed,"error":result.as_ref().err().map(ToString::to_string),"cleanup":cleanup,"fixture":fixture}),
        )?,
    )?;
    result
}
/// Query the actual open WinFsp handle, not adapter-local metadata. This catches
/// a kernel-side cached default owner differing from the fixed current-user view.
fn assert_current_user_owner(file: &fs::File) -> std::io::Result<()> {
    use windows_sys::Win32::Foundation::{CloseHandle as CloseToken, HANDLE, LocalFree};
    use windows_sys::Win32::Security::Authorization::{GetSecurityInfo, SE_FILE_OBJECT};
    use windows_sys::Win32::Security::{
        EqualSid, GetTokenInformation, OWNER_SECURITY_INFORMATION, TOKEN_QUERY, TOKEN_USER,
        TokenUser,
    };
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    struct Token(HANDLE);
    impl Drop for Token {
        fn drop(&mut self) {
            // SAFETY: this guard exclusively owns a successful token handle.
            unsafe {
                CloseToken(self.0);
            }
        }
    }
    struct Descriptor(*mut c_void);
    impl Drop for Descriptor {
        fn drop(&mut self) {
            // SAFETY: GetSecurityInfo allocates this descriptor with LocalAlloc;
            // borrowed owner SID pointers are not freed independently.
            unsafe {
                LocalFree(self.0);
            }
        }
    }
    let mut token = 0;
    // SAFETY: valid pseudo-process handle and writable token output storage.
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
        return Err(std::io::Error::last_os_error());
    }
    let token = Token(token);
    let mut needed = 0;
    // SAFETY: the zero-length query requests only the required buffer length.
    unsafe {
        GetTokenInformation(token.0, TokenUser, std::ptr::null_mut(), 0, &mut needed);
    }
    if needed < std::mem::size_of::<TOKEN_USER>() as u32 {
        return Err(std::io::Error::last_os_error());
    }
    // usize alignment covers TOKEN_USER and the embedded SID returned by Win32.
    let mut storage = vec![0_usize; (needed as usize).div_ceil(std::mem::size_of::<usize>())];
    // SAFETY: buffer is aligned and has at least the requested capacity.
    if unsafe {
        GetTokenInformation(
            token.0,
            TokenUser,
            storage.as_mut_ptr().cast(),
            needed,
            &mut needed,
        )
    } == 0
    {
        return Err(std::io::Error::last_os_error());
    }
    let mut descriptor = std::ptr::null_mut();
    let mut owner = std::ptr::null_mut();
    // SAFETY: file remains open throughout the query and comparison. Win32 owns
    // allocation of the returned descriptor; owner borrows from that allocation.
    let error = unsafe {
        GetSecurityInfo(
            file.as_raw_handle() as HANDLE,
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION,
            &mut owner,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut descriptor,
        )
    };
    if error != 0 {
        return Err(std::io::Error::from_raw_os_error(error as i32));
    }
    let _descriptor = Descriptor(descriptor);
    if owner.is_null() {
        return Err(std::io::Error::other("created file returned no owner SID"));
    }
    // SAFETY: successful TokenUser query initialized the aligned TOKEN_USER;
    // both SID pointers remain live until after EqualSid returns.
    let user = unsafe { &*storage.as_ptr().cast::<TOKEN_USER>() };
    assert!(
        !user.User.Sid.is_null(),
        "process token returned no user SID"
    );
    let same = unsafe { EqualSid(owner, user.User.Sid) } != 0;
    assert!(same, "created file owner is not the process TokenUser");
    Ok(())
}
/// A deleted inode must remain usable through its last mapped section, without
/// becoming visible again or allowing a clean checkpoint before final release.
fn check_deleted_mapping_lifetime(
    store: &Store,
    mounted_root: &std::path::Path,
) -> Result<&'static str> {
    struct Mapping(*mut c_void);
    impl Mapping {
        fn close(&mut self) -> std::io::Result<()> {
            if !self.0.is_null() {
                // SAFETY: this guard exclusively owns a valid mapping handle.
                if unsafe { CloseHandle(self.0) } == 0 {
                    return Err(std::io::Error::last_os_error());
                }
                self.0 = std::ptr::null_mut();
            }
            Ok(())
        }
    }
    impl Drop for Mapping {
        fn drop(&mut self) {
            let _ = self.close();
        }
    }
    struct View(*mut c_void);
    impl View {
        fn unmap(&mut self) -> std::io::Result<()> {
            if !self.0.is_null() {
                // SAFETY: this guard exclusively owns this mapped view.
                if unsafe { UnmapViewOfFile(self.0) } == 0 {
                    return Err(std::io::Error::last_os_error());
                }
                self.0 = std::ptr::null_mut();
            }
            Ok(())
        }
    }
    impl Drop for View {
        fn drop(&mut self) {
            let _ = self.unmap();
        }
    }
    fn assert_absent(path: &std::path::Path) -> std::io::Result<()> {
        match fs::symlink_metadata(path) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error),
            Ok(_) => Err(std::io::Error::other(
                "deleted mapped file reappeared in the namespace",
            )),
        }
    }

    let path = mounted_root.join("deleted-mapped.bin");
    let original = vec![0x4d_u8; 4096];
    let mut file = OpenOptions::new()
        .create_new(true)
        .read(true)
        .write(true)
        .open(&path)?;
    file.write_all(&original)?;
    file.sync_all()?;
    // SAFETY: the synthetic file has exactly 4096 initialized bytes. Mapping
    // and view guards remain live for every pointer access and unwind safely.
    let raw_mapping = unsafe {
        CreateFileMappingW(
            file.as_raw_handle(),
            std::ptr::null(),
            4,
            0,
            0,
            std::ptr::null(),
        )
    };
    if raw_mapping.is_null() {
        return Err(std::io::Error::last_os_error().into());
    }
    let mut mapping = Mapping(raw_mapping);
    // SAFETY: mapping is live and requests the initialized file extent only.
    let raw_view = unsafe { MapViewOfFile(mapping.0, 6, 0, 0, 4096) };
    if raw_view.is_null() {
        return Err(std::io::Error::last_os_error().into());
    }
    // Declared after mapping so unwinding unmaps the view before closing it.
    let mut view = View(raw_view);
    match fs::remove_file(&path) {
        Ok(()) => {}
        Err(error) if error.raw_os_error() == Some(5) => {
            // Windows may explicitly reject deleting a live data section. This
            // narrower contract is safe only if neither namespace nor bytes changed.
            assert_eq!(fs::read(&path)?, original);
            // SAFETY: the unchanged file still backs this live 4096-byte view.
            assert_eq!(
                unsafe { std::slice::from_raw_parts(view.0.cast::<u8>(), 4096) },
                original
            );
            view.unmap()?;
            mapping.close()?;
            drop(file);
            fs::remove_file(&path)?;
            assert_absent(&path)?;
            return Ok("mapped_delete_rejected_without_mutation");
        }
        Err(error) => return Err(error.into()),
    }
    drop(file);
    assert_absent(&path)?;
    // SAFETY: deletion and closing the file handle do not invalidate a live
    // mapping; this assertion is the specific Windows lifetime contract tested.
    let mapped_bytes = unsafe { std::slice::from_raw_parts(view.0.cast::<u8>(), 4096) };
    assert_eq!(mapped_bytes, original.as_slice());
    // SAFETY: offset 17 lies inside the live writable view. The preceding shared
    // slice is no longer used, so no Rust reference overlaps this mutation.
    unsafe { view.0.cast::<u8>().add(17).write(0x5a) };
    // SAFETY: the same live view covers the full requested flush extent.
    if unsafe { FlushViewOfFile(view.0, 4096) } == 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    // SAFETY: read back within the still-live view after the mapped write.
    assert_eq!(unsafe { view.0.cast::<u8>().add(17).read() }, 0x5a);
    assert!(
        store
            .checkpoint("main", "deleted mapped section remains busy", None)
            .is_err(),
        "checkpoint must reject a deleted inode with a live mapped section"
    );
    view.unmap()?;
    mapping.close()?;
    assert_absent(&path)?;
    Ok("deleted_mapping_survives_final_handle_close")
}

#[link(name = "kernel32")]
unsafe extern "system" {
    fn CreateFileMappingW(
        file: *mut c_void,
        attributes: *const c_void,
        protection: u32,
        high: u32,
        low: u32,
        name: *const u16,
    ) -> *mut c_void;
    fn MapViewOfFile(
        mapping: *mut c_void,
        access: u32,
        high: u32,
        low: u32,
        bytes: usize,
    ) -> *mut c_void;
    fn FlushViewOfFile(address: *const c_void, bytes: usize) -> i32;
    fn UnmapViewOfFile(address: *const c_void) -> i32;
    fn CloseHandle(handle: *mut c_void) -> i32;
}
