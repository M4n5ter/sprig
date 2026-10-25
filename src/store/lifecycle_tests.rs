//! Operation-local reuse must never become a stale cross-operation cache.
use super::*;
use std::cell::Cell;

thread_local! {
    pub(super) static REVISION_READS: Cell<usize> = const { Cell::new(0) };
    pub(super) static CORE_OPENS: Cell<usize> = const { Cell::new(0) };
}

#[test]
fn restore_reuses_one_verified_revision_and_rechecks_next_operation() -> Result<()> {
    let root = std::env::temp_dir().join(format!("sprig-lifecycle-{}", new_id()?));
    fs::create_dir(&root)?;
    let root = root.canonicalize()?;
    let source = root.join("source");
    fs::create_dir(&source)?;
    fs::write(source.join("file"), b"first")?;
    let store = Store::init(&root.join("store"))?;
    let imported = store.import(&source, "main", CopyMode::Auto)?;
    REVISION_READS.with(|c| c.set(0));
    let restored = store.restore("main", "native")?;
    assert_eq!(REVISION_READS.with(Cell::get), 1);
    assert_eq!(fs::read(restored.path.join("file"))?, b"first");
    REVISION_READS.with(|c| c.set(0));
    store.restore_writable("main", "owned")?;
    assert_eq!(REVISION_READS.with(Cell::get), 1);
    let owned = store.state()?.workspaces["owned"].clone();
    let core = store.writable_core(&owned)?;
    let inode = core.lookup_path("file")?.inode;
    assert_eq!(core.read_inode(inode, 0, 64)?, b"first");
    let handle = core.open_handle(inode, true)?;
    assert_eq!(core.write(handle, 0, b"THIRD")?, 5);
    core.release(handle)?;
    core.sync()?;
    drop(core);
    CORE_OPENS.with(|c| c.set(0));
    let (captured, _) = store.capture_workspace(&owned)?;
    assert_eq!(CORE_OPENS.with(Cell::get), 1);
    assert_eq!(
        content::read_file(&captured["file"], &store.objects())?,
        b"THIRD"
    );
    assert_eq!(store.read("main", "file")?, b"first");
    fs::write(imported.path.join("file"), b"second")?;
    store.checkpoint("main", "changed", None)?;
    let second = store.restore("main", "second")?;
    assert_eq!(fs::read(second.path.join("file"))?, b"second");
    let state = fs::read(store.root.join("state.json"))?;
    let revision = store.state()?.branches["main"].clone();
    let path = store
        .root
        .join("revisions")
        .join(format!("{revision}.json.zst"));
    #[cfg(unix)]
    platform::set_path_mode(&path, 0o600)?;
    fs::write(path, b"damaged revision")?;
    assert!(store.restore("main", "bad").is_err());
    assert_eq!(fs::read(store.root.join("state.json"))?, state);
    fs::remove_dir_all(root)?;
    Ok(())
}

#[test]
fn stopped_owned_core_is_reused_with_lease_and_failure_checks_intact() -> Result<()> {
    let root = std::env::temp_dir().join(format!("sprig-owned-reuse-{}", new_id()?));
    fs::create_dir(&root)?;
    let root = root.canonicalize()?;
    let source = root.join("source");
    fs::create_dir(&source)?;
    fs::write(source.join("file"), b"first")?;
    let store = Store::init(&root.join("store"))?;
    store.import_writable(&source, "main")?;
    let workspace = store.state()?.workspaces["main"].clone();
    let core = store.stop_writable_core(&workspace)?;
    let error = store
        .writable_core(&workspace)
        .err()
        .expect("generation lease must be exclusive");
    let error = error.downcast_ref::<io::Error>().unwrap();
    #[cfg(unix)]
    assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
    // Windows' share-deny lease rejects the second open before try_lock.
    // Require the exact sharing conflict, not any open or metadata failure.
    #[cfg(windows)]
    assert_eq!(error.raw_os_error(), Some(32), "{error:?}"); // ERROR_SHARING_VIOLATION

    drop(core);
    drop(store.writable_core(&workspace)?);
    CORE_OPENS.with(|c| c.set(0));
    store.capture_workspace(&workspace)?;
    assert_eq!(CORE_OPENS.with(Cell::get), 1);
    CORE_OPENS.with(|c| c.set(0));
    store.sync_workspace(&workspace)?;
    assert_eq!(CORE_OPENS.with(Cell::get), 1);
    CORE_OPENS.with(|c| c.set(0));
    REVISION_READS.with(|c| c.set(0));
    store.checkpoint("main", "unchanged", None)?;
    assert_eq!(CORE_OPENS.with(Cell::get), 2);
    assert_eq!(REVISION_READS.with(Cell::get), 3);
    let state = fs::read(store.root.join("state.json"))?;
    let workspace = store.state()?.workspaces["main"].clone();
    let journal = store
        .root
        .join("native")
        .join(&workspace.directory)
        .join("writable.journal");
    let original = fs::read(&journal)?;
    let mut file = platform::open_read_write(&journal)?;
    use std::io::{Seek, SeekFrom};
    file.seek(SeekFrom::End(0))?;
    file.write_all(&original[..4])?;
    file.sync_all()?;
    drop(file);
    assert!(store.checkpoint("main", "refuse torn", None).is_err());
    assert_eq!(fs::read(store.root.join("state.json"))?, state);
    // Failed capture released its retained lease but preserves recovery-only state.
    assert!(store.writable_core(&workspace)?.recovery_info()?.read_only);
    fs::remove_dir_all(root)?;
    Ok(())
}
