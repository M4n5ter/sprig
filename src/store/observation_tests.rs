//! Immutable observation references preserve publication and recovery evidence.
use super::*;
use std::cell::Cell;

thread_local! { static FAIL_AT: Cell<Option<&'static str>> = const { Cell::new(None) }; }
pub(super) fn inject(point: &'static str) -> Result<()> {
    if FAIL_AT.with(|value| value.get() == Some(point)) {
        return Err(failure(&format!(
            "injected observation publication fault: {point}"
        )));
    }
    Ok(())
}
struct Injection;
impl Drop for Injection {
    fn drop(&mut self) {
        FAIL_AT.with(|v| v.set(None));
    }
}
fn inject_at(point: &'static str) -> Injection {
    FAIL_AT.with(|v| v.set(Some(point)));
    Injection
}
fn fixture() -> Result<(PathBuf, PathBuf, Store)> {
    let root = std::env::temp_dir().join(format!("sprig-observation-{}", new_id()?));
    fs::create_dir(&root)?;
    let root = root.canonicalize()?;
    let source = root.join("source");
    fs::create_dir(&source)?;
    fs::write(source.join("file"), b"contents")?;
    let store = Store::init(&root.join("store"))?;
    Ok((root, source, store))
}

#[test]
fn observation_publication_failure_keeps_exact_authoritative_state() -> Result<()> {
    for point in [
        "observations-before-put",
        "observations-after-put",
        "save-before-backup",
        "save-after-backup",
        "save-before-rename",
        "save-after-rename",
        "save-after-root-sync",
        "save-after-staging-sync",
    ] {
        let (root, source, store) = fixture()?;
        let before = fs::read(store.root.join("state.json"))?;
        let injected = inject_at(point);
        let error = store
            .import(&source, "candidate", CopyMode::Copy)
            .unwrap_err();
        assert!(error.to_string().contains(point), "{point}: {error}");
        drop(injected);
        let state = store.state()?;
        let published = matches!(
            point,
            "save-after-rename" | "save-after-root-sync" | "save-after-staging-sync"
        );
        assert_eq!(state.branches.contains_key("candidate"), published);
        if published {
            assert_eq!(state.generation, 1);
            let lock = store.lock()?;
            store.verify_state_observations(&lock, &state)?;
            assert_eq!(
                store
                    .read_observed(&lock, &state.workspaces["candidate"].observed)?
                    .len(),
                2
            );
        } else {
            assert_eq!(fs::read(store.root.join("state.json"))?, before);
        }
        // No implicit replay or rollback removes either side's immutable evidence.
        assert_eq!(store.state()?.generation, u64::from(published));
        fs::remove_dir_all(root)?;
    }
    Ok(())
}

#[test]
fn observation_resolution_is_explicit_budgeted_and_missing_is_not_empty() -> Result<()> {
    let (root, source, store) = fixture()?;
    store.import(&source, "main", CopyMode::Copy)?;
    let reference = store.state()?.workspaces["main"].observed.clone();
    let path = store
        .observation_directory()
        .join(format!("{}.zst", reference.hash));
    let retained = root.join("retained-observation");
    fs::rename(&path, &retained)?;
    store.list()?;
    store.status()?;
    assert!(store.fork("main", "bad", CopyMode::Copy).is_err());
    assert!(store.checkpoint("main", "bad", None).is_err());
    assert!(!store.doctor()?.healthy);
    assert!(store.gc().is_err());
    fs::rename(&retained, &path)?;
    let low = store.clone().with_observation_budget(ObservationBudget {
        max_raw_bytes: 1,
        max_entries: 1,
        max_encoded_bytes: 1,
    });
    low.list()?;
    assert!(low.checkpoint("main", "budget", None).is_err());
    let bytes = fs::read(store.root.join("state.json"))?;
    assert!(low.gc().is_err());
    assert_eq!(fs::read(store.root.join("state.json"))?, bytes);
    let raised = store.clone().with_observation_budget(ObservationBudget {
        max_raw_bytes: 1024 * 1024,
        max_entries: 100,
        max_encoded_bytes: 1024 * 1024,
    });
    raised.checkpoint("main", "within explicit budget", None)?;
    assert!(raised.doctor()?.healthy);
    let other = Store::init(&root.join("other"))?;
    assert!(store.read_observed(&other.lock()?, &reference).is_err());
    fs::remove_dir_all(root)?;
    Ok(())
}

#[test]
fn observation_gc_protects_backups_staging_and_rejects_changed_evidence() -> Result<()> {
    let (root, source, store) = fixture()?;
    store.import(&source, "main", CopyMode::Copy)?;
    let mut state = store.state()?;
    let reference = state.workspaces["main"].observed.clone();
    let orphan = {
        let lock = store.lock()?;
        let mut map = store.read_observed(&lock, &reference)?;
        map.get_mut("file").unwrap().inode ^= 1_u128 << 100;
        store.put_observed(&lock, &map)?
    };
    let plan = store.gc()?;
    assert!(plan.unreachable_observations.contains(&orphan.hash));
    state.workspaces.get_mut("main").unwrap().observed = orphan.clone();
    let stage = store.root.join("staging/state-test.json");
    write_new(&stage, &serde_json::to_vec(&state)?)?;
    assert!(store.gc_apply(&plan.plan_id).is_err());
    let protected = store.gc()?;
    assert!(!protected.unreachable_observations.contains(&orphan.hash));
    store.gc_apply(&protected.plan_id)?;
    assert!(
        store
            .observation_directory()
            .join(format!("{}.zst", orphan.hash))
            .exists()
    );
    fs::remove_file(&stage)?;
    let opaque = store.root.join("staging/retained-state-unknown.json");
    write_new(&opaque, b"corrupt recovery evidence")?;
    assert!(store.gc().is_err());
    fs::remove_file(&opaque)?;
    let final_plan = store.gc()?;
    assert!(final_plan.unreachable_observations.contains(&orphan.hash));
    store.gc_apply(&final_plan.plan_id)?;
    assert!(
        !store
            .observation_directory()
            .join(format!("{}.zst", orphan.hash))
            .exists()
    );
    assert!(
        store
            .observation_directory()
            .join(format!("{}.zst", reference.hash))
            .exists()
    );
    assert!(store.doctor()?.healthy);
    fs::remove_dir_all(root)?;
    Ok(())
}

#[test]
fn observation_backup_only_root_and_recovery_preserve_missing_current_evidence() -> Result<()> {
    let (root, source, store) = fixture()?;
    store.import(&source, "main", CopyMode::Copy)?;
    let original = store.state()?.workspaces["main"].observed.clone();
    store.fork("main", "fork", CopyMode::Copy)?;
    let broken = store.state()?.workspaces["fork"].observed.clone();
    assert_ne!(original.hash, broken.hash);
    fs::remove_file(
        store
            .observation_directory()
            .join(format!("{}.zst", broken.hash)),
    )?;
    // Backup generation 1 has only main. Missing current dependencies must not
    // prevent explicitly restoring that independently valid recovery state.
    let recovered = Store::recover_state(&store.root, 1)?;
    assert_eq!(recovered.generation, 3);
    let state = store.state()?;
    assert!(!state.workspaces.contains_key("fork"));
    assert_eq!(state.workspaces["main"].observed, original);
    assert!(!names(&store.root.join("staging"))?.is_empty());
    assert!(store.gc().is_err()); // retained missing evidence is not empty
    fs::remove_dir_all(root)?;

    let (root, source, store) = fixture()?;
    store.import(&source, "main", CopyMode::Copy)?;
    let reference = store.state()?.workspaces["main"].observed.clone();
    {
        let lock = store.lock()?;
        let mut state = store.state()?;
        state.workspaces.clear();
        state.trash.clear();
        store.save(&lock, &mut state)?;
    }
    let plan = store.gc()?;
    assert!(!plan.unreachable_observations.contains(&reference.hash));
    store.gc_apply(&plan.plan_id)?;
    assert!(
        store
            .observation_directory()
            .join(format!("{}.zst", reference.hash))
            .exists()
    );
    fs::remove_dir_all(root)?;
    Ok(())
}

#[test]
fn explicit_writable_recovery_does_not_heal_broken_source_observations() -> Result<()> {
    for missing in [true, false] {
        let (root, source, store) = fixture()?;
        store.import_writable(&source, "main")?;
        let state = store.state()?;
        let workspace = &state.workspaces["main"];
        let blob = store
            .observation_directory()
            .join(format!("{}.zst", workspace.observed.hash));
        let before = fs::read(store.root.join("state.json"))?;
        if missing {
            fs::remove_file(&blob)?;
        } else {
            #[cfg(unix)]
            platform::set_path_mode(&blob, 0o600)?;
            fs::write(&blob, b"corrupt observation")?;
        }
        assert!(store.recover_writable("main", "recovered").is_err());
        assert_eq!(fs::read(store.root.join("state.json"))?, before);
        if missing {
            assert!(!blob.exists());
        } else {
            assert_eq!(fs::read(&blob)?, b"corrupt observation");
        }
        fs::remove_dir_all(root)?;
    }
    Ok(())
}

#[test]
fn observation_recovery_and_collection_faults_never_erase_published_roots() -> Result<()> {
    for point in ["recovery-before-rename", "recovery-after-rename"] {
        let (root, source, store) = fixture()?;
        store.import(&source, "main", CopyMode::Copy)?;
        store.fork("main", "fork", CopyMode::Copy)?;
        let before = fs::read(store.root.join("state.json"))?;
        let injected = inject_at(point);
        let error = Store::recover_state(&store.root, 1).unwrap_err();
        assert!(error.to_string().contains(point));
        drop(injected);
        let state = store.state()?;
        if point == "recovery-before-rename" {
            assert_eq!(fs::read(store.root.join("state.json"))?, before);
            assert!(state.workspaces.contains_key("fork"));
        } else {
            assert_eq!(state.generation, 3);
            assert!(!state.workspaces.contains_key("fork"));
        }
        assert!(fs::read_dir(store.root.join("staging"))?.any(|entry| {
            let entry = entry.expect("owned staging entry");
            entry
                .file_name()
                .to_string_lossy()
                .starts_with("retained-state-")
                && fs::read(entry.path()).is_ok_and(|bytes| bytes == before)
        }));
        store.verify_observation_roots(&store.lock()?, &state)?;
        fs::remove_dir_all(root)?;
    }
    for point in [
        "before_observation_unlink",
        "before_observation_gc_directory_sync",
    ] {
        let (root, source, store) = fixture()?;
        store.import(&source, "main", CopyMode::Copy)?;
        let state = store.state()?;
        let original = state.workspaces["main"].observed.clone();
        let orphan = {
            let lock = store.lock()?;
            let mut map = store.read_observed(&lock, &original)?;
            map.get_mut("file").unwrap().device ^= 1;
            store.put_observed(&lock, &map)?
        };
        let plan = store.gc()?;
        let before = fs::read(store.root.join("state.json"))?;
        let injected = inject_at(point);
        let error = store.gc_apply(&plan.plan_id).unwrap_err();
        assert!(error.to_string().contains(point));
        drop(injected);
        assert_eq!(fs::read(store.root.join("state.json"))?, before);
        store.verify_state_observations(&store.lock()?, &state)?;
        let orphan_exists = store
            .observation_directory()
            .join(format!("{}.zst", orphan.hash))
            .exists();
        assert_eq!(orphan_exists, point == "before_observation_unlink");
        // No replay: inspect/regenerate a new plan after an uncertain final sync.
        let _fresh = store.gc()?;
        fs::remove_dir_all(root)?;
    }
    Ok(())
}

#[test]
fn backup_retry_and_recovery_keep_exact_bytes_or_fail_closed_on_representation_conflict()
-> Result<()> {
    for rewrite_after_backup in [false, true] {
        let (root, source, store) = fixture()?;
        store.import(&source, "main", CopyMode::Copy)?;
        let generation = store.state()?.generation;
        let state_path = store.root.join("state.json");
        let canonical = fs::read(&state_path)?;
        let value: serde_json::Value = serde_json::from_slice(&canonical)?;
        let pretty = serde_json::to_vec_pretty(&value)?;
        if !rewrite_after_backup {
            fs::write(&state_path, &pretty)?;
        }
        let injected = inject_at("save-after-backup");
        assert!(store.branch("failed", "main").is_err());
        drop(injected);
        let backup_path = store
            .root
            .join("backups")
            .join(format!("{generation}.json.zst"));
        let backup = fs::read(&backup_path)?;
        if rewrite_after_backup {
            // Models a canonical backup from an earlier implementation/failure
            // alongside equivalent noncanonical current bytes. Never overwrite
            // either representation silently or claim recovery succeeded.
            fs::write(&state_path, &pretty)?;
            assert!(
                store
                    .branch("retry", "main")
                    .unwrap_err()
                    .to_string()
                    .contains("collision")
            );
            assert!(
                Store::recover_state(&store.root, generation)
                    .unwrap_err()
                    .to_string()
                    .contains("collision")
            );
            assert_eq!(fs::read(&state_path)?, pretty);
            assert_eq!(fs::read(&backup_path)?, backup);
            // Explicitly repair the test fixture, not production behavior.
            fs::write(&state_path, &canonical)?;
        } else {
            store.branch("retry", "main")?;
        }
        Store::recover_state(&store.root, generation)?;
        assert!(!store.state()?.branches.contains_key("failed"));
        assert!(!store.state()?.branches.contains_key("retry"));
        assert_eq!(store.read("main", "file")?, b"contents");
        fs::remove_dir_all(root)?;
    }
    Ok(())
}
