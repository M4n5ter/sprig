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
    let (captured, _) = store.capture_workspace(&store.lock()?, &owned)?;
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
    store.capture_workspace(&store.lock()?, &workspace)?;
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

#[test]
fn native_fork_copies_identity_without_publishing_uncommitted_content() -> Result<()> {
    fn files(root: &Path) -> Result<BTreeMap<PathBuf, Vec<u8>>> {
        fn visit(root: &Path, path: &Path, result: &mut BTreeMap<PathBuf, Vec<u8>>) -> Result<()> {
            for entry in fs::read_dir(path)? {
                let entry = entry?;
                if entry.file_type()?.is_dir() {
                    visit(root, &entry.path(), result)?;
                } else {
                    result.insert(
                        entry.path().strip_prefix(root)?.to_owned(),
                        fs::read(entry.path())?,
                    );
                }
            }
            Ok(())
        }
        let mut result = BTreeMap::new();
        visit(root, root, &mut result)?;
        Ok(result)
    }
    let root = std::env::temp_dir().join(format!("sprig-copy-identities-{}", new_id()?));
    fs::create_dir(&root)?;
    let root = root.canonicalize()?;
    let source = root.join("source");
    fs::create_dir(&source)?;
    fs::create_dir(source.join("old-dir"))?;
    fs::write(source.join("old-dir/file"), b"renamed file")?;
    fs::write(source.join("a"), b"old shared inode")?;
    fs::hard_link(source.join("a"), source.join("b"))?;
    let store = Store::init(&root.join("store"))?;
    let imported = store.import(&source, "main", CopyMode::Copy)?;
    let before = store.state()?;
    let original_ref = before.workspaces["main"].observed.clone();
    let original = store.read_observed(&store.lock()?, &original_ref)?;
    fs::rename(imported.path.join("old-dir"), imported.path.join("new-dir"))?;
    fs::write(
        imported.path.join("replacement"),
        b"independent replacement",
    )?;
    // Remove then rename is portable; the old inode is retained by alias b.
    fs::remove_file(imported.path.join("a"))?;
    fs::rename(imported.path.join("replacement"), imported.path.join("a"))?;
    fs::hard_link(imported.path.join("b"), imported.path.join("c"))?;
    fs::write(imported.path.join("new"), vec![b'n'; 1024 * 1024])?;
    let objects_before = files(&store.objects())?;
    let revisions_before = files(&store.root.join("revisions"))?;
    let fork = store.fork("main", "fork", CopyMode::Copy)?;
    assert_eq!(files(&store.objects())?, objects_before);
    assert_eq!(files(&store.root.join("revisions"))?, revisions_before);
    assert_eq!(fork.base_revision, imported.base_revision);
    let state = store.state()?;
    assert_eq!(state.workspaces["main"].observed, original_ref);
    let observed = store.read_observed(&store.lock()?, &state.workspaces["fork"].observed)?;
    assert_eq!(observed["new-dir"].id, original["old-dir"].id);
    assert_eq!(observed["new-dir/file"].id, original["old-dir/file"].id);
    assert_eq!(observed["a"].id, original["a"].id);
    assert_ne!(observed["a"].link_group, observed["b"].link_group);
    assert_eq!(observed["b"].link_group, original["b"].link_group);
    assert_eq!(observed["b"].link_group, observed["c"].link_group);
    assert_ne!(observed["b"].inode, original["b"].inode);
    assert_eq!(fs::read(fork.path.join("a"))?, b"independent replacement");
    fs::write(fork.path.join("c"), b"fork-only alias edit")?;
    assert_eq!(fs::read(fork.path.join("b"))?, b"fork-only alias edit");
    assert_eq!(fs::read(imported.path.join("b"))?, b"old shared inode");
    let committed = store.checkpoint("fork", "publish copied edits", None)?;
    assert!(committed.committed);
    assert_eq!(store.read("fork", "b")?, b"fork-only alias edit");
    assert_eq!(store.read("main", "a")?, b"old shared inode");
    let manifest = store.revision(&store.state()?.branches["fork"])?.manifest;
    for (path, entry) in &manifest {
        assert_eq!(entry.id, observed[path].id);
        assert_eq!(entry.link_group, observed[path].link_group);
    }
    assert_ne!(files(&store.objects())?, objects_before);
    // A failed copy must not publish a branch/workspace, even after it creates
    // its private generation. A FIFO is deliberately unsupported.
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        let fifo = imported.path.join("fifo");
        let c_path = std::ffi::CString::new(fifo.as_os_str().as_bytes())?;
        // SAFETY: a NUL-terminated path in this newly created synthetic tree.
        if unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) } != 0 {
            return Err(io::Error::last_os_error().into());
        }
        let state_bytes = fs::read(store.root.join("state.json"))?;
        assert!(store.fork("main", "rejected", CopyMode::Copy).is_err());
        assert_eq!(fs::read(store.root.join("state.json"))?, state_bytes);
        fs::remove_file(fifo)?;
    }
    assert!(store.doctor()?.healthy);
    fs::remove_dir_all(root)?;
    Ok(())
}

#[test]
fn copied_pending_rename_merges_with_original_path_edit() -> Result<()> {
    let root = std::env::temp_dir().join(format!("sprig-fork-merge-{}", new_id()?));
    fs::create_dir(&root)?;
    let root = root.canonicalize()?;
    let source = root.join("source");
    fs::create_dir(&source)?;
    fs::write(source.join("old"), b"base\n")?;
    let store = Store::init(&root.join("store"))?;
    let main = store.import(&source, "main", CopyMode::Copy)?;
    let old_id = store
        .read_observed(&store.lock()?, &store.state()?.workspaces["main"].observed)?["old"]
        .id
        .clone();
    fs::rename(main.path.join("old"), main.path.join("renamed"))?;
    let fork = store.fork("main", "fork", CopyMode::Auto)?;
    assert_eq!(
        store.read_observed(&store.lock()?, &store.state()?.workspaces["fork"].observed)?["renamed"].id,
        old_id
    );
    store.checkpoint("fork", "rename", None)?;
    fs::rename(main.path.join("renamed"), main.path.join("old"))?;
    fs::write(main.path.join("old"), b"edited original\n")?;
    store.checkpoint("main", "edit original", None)?;
    let merged = store.merge("main", "fork", "merged", MergeStrategy::Conflict, &[], None)?;
    assert!(merged.committed, "{merged:?}");
    assert_eq!(store.read("merged", "renamed")?, b"edited original\n");
    assert_eq!(fs::read(fork.path.join("renamed"))?, b"base\n");
    assert_eq!(
        store.revision(&store.state()?.branches["merged"])?.manifest["renamed"].id,
        old_id
    );
    fs::remove_dir_all(root)?;
    Ok(())
}

#[test]
fn query_operations_decode_each_reference_once_without_cross_operation_cache() -> Result<()> {
    let root = std::env::temp_dir().join(format!("sprig-query-reuse-{}", new_id()?));
    fs::create_dir(&root)?;
    let source = root.join("source");
    fs::create_dir(&source)?;
    fs::write(source.join("file"), b"first")?;
    let store = Store::init(&root.join("store"))?;
    let workspace = store.import(&source, "main", CopyMode::Auto)?;
    macro_rules! once {
        ($operation:expr, $count:expr) => {{
            REVISION_READS.with(|count| count.set(0));
            $operation?;
            assert_eq!(REVISION_READS.with(Cell::get), $count);
        }};
    }
    once!(store.read("main", "file"), 1);
    once!(store.ls("main", ""), 1);
    once!(store.stat("main", "file"), 1);
    once!(store.glob("main", "*"), 1);
    once!(store.grep("main", "first"), 1);
    once!(store.diff("main", "main"), 2);
    once!(store.diff_files("main", "main"), 2);
    fs::write(workspace.path.join("file"), b"second")?;
    store.checkpoint("main", "new head", None)?;
    assert_eq!(store.read("main", "file")?, b"second");
    let revision = store.state()?.branches["main"].clone();
    let path = store
        .root
        .join("revisions")
        .join(format!("{revision}.json.zst"));
    #[cfg(unix)]
    platform::set_path_mode(&path, 0o600)?;
    fs::write(path, b"damaged revision")?;
    assert!(store.read("main", "file").is_err());
    assert!(store.stat("main", "file").is_err());
    fs::remove_dir_all(root)?;
    Ok(())
}

#[cfg(target_os = "linux")]
fn assert_restore_sync_failure_is_unpublished(file_count: usize) -> Result<()> {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct Cleanup(PathBuf);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    let root = std::env::temp_dir().join(format!("sprig-restore-sync-failure-{}", new_id()?));
    fs::create_dir(&root)?;
    let root = root.canonicalize()?;
    let _cleanup = Cleanup(root.clone());
    let source = root.join("source");
    fs::create_dir(&source)?;
    for index in 1..=file_count {
        fs::write(source.join(format!("file-{index}")), vec![b'x'; index])?;
    }
    let store = Store::init(&root.join("store"))?;
    let imported = store.import(&source, "main", CopyMode::Copy)?;
    let lock = store.lock()?;
    let mut state = store.state()?;
    let before_state = serde_json::to_vec(&state)?;
    let before_persisted = fs::read(store.root.join("state.json"))?;
    let before_generation = state.generation;
    let revision = imported.base_revision;
    let data = store.revision(&revision)?;
    let finished = Arc::new(AtomicUsize::new(0));
    let finished_worker = finished.clone();
    let sync = crate::file_sync::FileSync::for_test(move |file| {
        let result = if file.metadata()?.len() == 1 {
            Err(io::Error::other("injected first-file sync failure"))
        } else {
            file.sync_all()
        };
        finished_worker.fetch_add(1, Ordering::SeqCst);
        result
    })?;
    let barriers = Cell::new(0);
    let target = std::cell::RefCell::new(None);
    let result = store.restore_locked_with_materializer(
        &lock,
        &mut state,
        &revision,
        &data,
        "failed-branch",
        "failed-workspace",
        true,
        |manifest, objects, tree| {
            target.replace(Some(tree.to_owned()));
            content::materialize_with_test_sync(manifest, objects, tree, sync, || {
                barriers.set(barriers.get() + 1);
            })
        },
    );
    assert_eq!(
        result.unwrap_err().to_string(),
        "injected first-file sync failure"
    );
    assert_eq!(finished.load(Ordering::SeqCst), 2);
    assert_eq!(
        Arc::strong_count(&finished),
        1,
        "all workers must be joined"
    );
    assert_eq!(
        barriers.get(),
        0,
        "no directory/parent sync after file failure"
    );
    assert_eq!(state.generation, before_generation);
    assert_eq!(serde_json::to_vec(&state)?, before_state);
    assert_eq!(fs::read(store.root.join("state.json"))?, before_persisted);
    assert_eq!(serde_json::to_vec(&store.state()?)?, before_state);
    assert!(!state.branches.contains_key("failed-branch"));
    assert!(!state.workspaces.contains_key("failed-workspace"));
    let target = target.into_inner().expect("materialization started");
    for index in 1..=file_count {
        assert_eq!(
            fs::read(target.join(format!("file-{index}")))?,
            vec![b'x'; index]
        );
    }
    // Three files reaches submit's full-batch drain after writing file three;
    // two files reaches finish's final drain. Only the first two were submitted.
    assert_eq!(names(&target)?.len(), file_count);
    assert_eq!(fs::read(imported.path.join("file-1"))?, b"x");
    Ok(())
}

#[cfg(target_os = "linux")]
#[test]
fn native_restore_full_batch_sync_failure_skips_barriers_and_publication() -> Result<()> {
    assert_restore_sync_failure_is_unpublished(3)
}

#[cfg(target_os = "linux")]
#[test]
fn native_restore_final_drain_sync_failure_skips_barriers_and_publication() -> Result<()> {
    assert_restore_sync_failure_is_unpublished(2)
}
