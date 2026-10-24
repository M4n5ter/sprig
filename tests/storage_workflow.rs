//! One portable lifecycle for compressed history, cold refs, accounting and collection.
use sprig::{CopyMode, Mutation, Result, Store};
use std::fs;
use std::time::{SystemTime, UNIX_EPOCH};

#[test]
fn storage_lifecycle_keeps_live_data_and_rejects_stale_collection() -> Result<()> {
    let root = std::env::temp_dir().join(format!(
        "sprig-storage-{}-{}",
        std::process::id(),
        SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos()
    ));
    fs::create_dir(&root)?;
    let root = root.canonicalize()?;
    let source = root.join("source");
    fs::create_dir(&source)?;
    let bytes = vec![b'x'; 1024 * 1024];
    fs::write(source.join("data"), &bytes)?;
    fs::hard_link(source.join("data"), source.join("alias"))?;
    let path = root.join("store");
    let store = Store::init(&path)?;
    let initial = store.import_writable(&source, "main")?;
    assert_eq!(fs::read_dir(&initial.path)?.count(), 0);
    let generations = fs::read_dir(path.join("native"))?.count();
    for index in 0..8 {
        store.branch(&format!("cold-{index}"), "main")?;
    }
    assert_eq!(fs::read_dir(path.join("native"))?.count(), generations);
    let stats = store.stats()?;
    assert!(stats.storage.contains_key("backups"));
    assert!(stats.store_logical_bytes >= stats.object_storage_bytes);
    assert!(stats.object_storage_bytes < stats.object_bytes);
    if std::env::var_os("SPRIG_REQUIRE_REFLINK").is_some() {
        assert!(stats.store_allocated_bytes.is_some());
    }
    let warm = store.checkout("cold-0", "warm")?;
    let restored = store.restore(&initial.base_revision, "restored")?;
    if std::env::var_os("SPRIG_REQUIRE_REFLINK").is_some() {
        assert!(
            fs::read_dir(path.join("file-cache"))?.count() > 1,
            "actual native clone must populate the cache"
        );
    }
    let materialized_stats = store.stats()?;
    assert!(materialized_stats.store_allocated_bytes.is_some());
    assert!(
        materialized_stats.storage["native"].logical_bytes < 3 * bytes.len() as u64,
        "internal hardlink aliases must not double-count file bytes"
    );
    assert_eq!(fs::read(warm.path.join("data"))?, bytes);
    fs::write(warm.path.join("alias"), b"changed")?;
    assert_eq!(fs::read(warm.path.join("data"))?, b"changed");
    assert_eq!(fs::read(restored.path.join("data"))?, bytes);
    assert_eq!(store.read("main", "data")?, bytes);
    let changed = store.apply(
        "main",
        &[Mutation::Write {
            path: "note".into(),
            content: b"owned change".to_vec(),
        }],
        "edit",
        None,
    )?;
    assert!(changed.committed);
    // Deliberately unpublished synthetic content, never actual user data.
    sprig::content::put_bytes(b"orphan synthetic payload", &path.join("objects"))?;
    let stale = store.gc()?;
    store.branch("later", "main")?;
    assert!(store.gc_apply(&stale.plan_id).is_err());
    let plan = store.gc()?;
    assert!(!plan.unreachable_objects.is_empty());
    assert!(store.gc_apply_with_budget(&plan.plan_id, 1).is_err());
    let collected = store.gc_apply(&plan.plan_id)?;
    assert!(!collected.readers_busy);
    assert_eq!(store.read("main", "note")?, b"owned change");
    assert_eq!(store.read("cold-0", "data")?, bytes);
    assert!(store.doctor()?.healthy);
    let before_retention = store.gc()?;
    let retained = store.retention(2, &[], None)?;
    assert!(!retained.backups.is_empty());
    store.retention(2, &[], Some(&retained.plan_id))?;
    assert_eq!(fs::read_dir(path.join("backups"))?.count(), 2);
    assert!(store.gc_apply(&before_retention.plan_id).is_err());
    assert!(store.doctor()?.healthy);
    // Ordinary native import remains independent, without requiring a mount.
    let independent = store.import(&source, "native", CopyMode::Copy)?;
    assert_eq!(fs::read(independent.path.join("alias"))?, bytes);
    let raw_store = Store::init_with_compression(&root.join("raw-store"), false)?;
    raw_store.import_writable(&source, "raw")?;
    let raw_stats = raw_store.stats()?;
    assert_eq!(raw_stats.payload_compression, "never");
    assert!(raw_stats.object_storage_bytes > stats.object_storage_bytes);
    assert_eq!(raw_store.read("raw", "data")?, bytes);
    drop(raw_store);
    drop(store);
    assert_eq!(Store::open(&path)?.read("main", "note")?, b"owned change");
    fs::remove_dir_all(root)?;
    Ok(())
}
