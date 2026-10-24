#![cfg(all(windows, feature = "windows-mount"))]
//! One explicit, disposable-runner mount contract; never installs a runtime.
use sprig::{Result, Store};
use std::ffi::c_void;
use std::fs::{self, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
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
        assert_eq!(fs::read(view.path.join("original.txt"))?, b"original");
        let new = view.path.join("created.txt");
        fs::write(&new, b"abcdefgh")?;
        let mut file = OpenOptions::new().read(true).write(true).open(&new)?;
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
        assert_eq!(fs::read(now.path.join("renamed.txt"))?, b"abc\0\0\0\0Z");
        assert_eq!(fs::read(now.path.join("mapped.bin"))?[17], 91);
        assert_eq!(
            store.read(&commit.revision, "renamed.txt")?,
            b"abc\0\0\0\0Z"
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
    let cleanup: Vec<_> = ["restored", "main"]
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
    fs::write(
        parent.join("mount-result.json"),
        serde_json::to_vec_pretty(
            &serde_json::json!({"ok":result.is_ok(),"checks":completed,"error":result.as_ref().err().map(ToString::to_string),"cleanup":cleanup,"fixture":fixture}),
        )?,
    )?;
    result
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
