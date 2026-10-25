//! Collector-free native checkpoint pressure harness, not a public CLI option.
#[cfg(unix)]
mod unix {
    use sprig::{CopyMode, Result, Store};
    use std::fs::{self, File};
    use std::io::Write;
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    use std::path::{Path, PathBuf};
    use std::time::Instant;

    struct Group {
        paths: Vec<PathBuf>,
        bytes: Vec<u8>,
        mode: u32,
    }
    fn usage(path: &Path) -> Result<u64> {
        let m = fs::symlink_metadata(path)?;
        let mut bytes = m.len();
        if m.is_dir() {
            for e in fs::read_dir(path)? {
                bytes += usage(&e?.path())?;
            }
        }
        Ok(bytes)
    }
    fn budget(root: &Path) -> Result<u64> {
        let bytes = usage(root)?;
        assert!(
            bytes < 512 * 1024 * 1024,
            "512 MiB live fixture budget exceeded: {bytes}"
        );
        Ok(bytes)
    }
    pub(super) fn run() -> Result<()> {
        let args: Vec<_> = std::env::args().collect();
        assert_eq!(args.len(), 5, "ROOT CASE SAMPLES LABEL");
        let root = PathBuf::from(&args[1]);
        assert!(!root.exists());
        fs::create_dir(&root)?;
        let case = &args[2];
        let samples: usize = args[3].parse()?;
        let label = &args[4];
        assert!(samples > 0 && samples <= 16);
        let source = root.join("source");
        fs::create_dir(&source)?;
        let (unique, aliases, payload) = match case.as_str() {
            "flat10k" => (10_000, 1, 256),
            "deep128" => (128, 1, 4096),
            "hardlinks8k" => (2048, 4, 4096),
            _ => panic!("unknown case"),
        };
        let mut groups = Vec::new();
        let mut prefix = PathBuf::new();
        for i in 0..unique {
            if case == "deep128" {
                prefix.push("d");
                fs::create_dir(source.join(&prefix))?;
            }
            let bytes = (0..payload)
                .map(|j| ((i * 17 + j * 31) % 251) as u8)
                .collect::<Vec<_>>();
            let mut paths = vec![prefix.join(format!("f{i:05}"))];
            fs::write(source.join(&paths[0]), &bytes)?;
            fs::set_permissions(source.join(&paths[0]), fs::Permissions::from_mode(0o644))?;
            for alias in 1..aliases {
                let p = prefix.join(format!("f{i:05}-alias{alias}"));
                fs::hard_link(source.join(&paths[0]), source.join(&p))?;
                paths.push(p);
            }
            groups.push(Group {
                paths,
                bytes,
                mode: 0o644,
            });
        }
        let store = Store::init(&root.join("store"))?;
        let work = store.import(&source, "work", CopyMode::Copy)?;
        let mut records = Vec::new();
        let mut revision = work.base_revision.clone();
        let mut ordinal = 0;
        for sample in 0..samples {
            for kind in ["changed", "unchanged", "metadata"] {
                if kind == "changed" {
                    for j in 0..64.min(unique) {
                        let index = (sample * 97 + j * 13) % unique;
                        let group = &mut groups[index];
                        group.bytes[0] = group.bytes[0].wrapping_add(1);
                        // Deliberately do NOT fsync fixture edits: checkpoint bears durability cost.
                        let mut file = File::options()
                            .write(true)
                            .truncate(true)
                            .open(work.path.join(&group.paths[0]))?;
                        file.write_all(&group.bytes)?;
                    }
                } else if kind == "metadata" {
                    let group = &mut groups[sample % unique];
                    group.mode = if group.mode == 0o644 { 0o600 } else { 0o644 };
                    fs::set_permissions(
                        work.path.join(&group.paths[0]),
                        fs::Permissions::from_mode(group.mode),
                    )?;
                }
                let before = Instant::now();
                let result = store.checkpoint("work", "pressure", None)?;
                let ns = before.elapsed().as_nanos();
                assert!(result.committed);
                assert_eq!(result.previous_head.as_ref(), Some(&revision));
                assert_eq!(result.revision == revision, kind == "unchanged");
                revision = result.revision.clone();
                let check = &groups[(sample * 97) % unique];
                assert_eq!(
                    store.read("work", check.paths[0].to_str().unwrap())?,
                    check.bytes
                );
                records.push(serde_json::json!({"ordinal":ordinal,"sample":sample,"kind":kind,"checkpoint_ns":ns,"revision":result.revision}));
                ordinal += 1;
                budget(&root)?;
            }
        }
        // Verify every committed payload, mode and hardlink identity via a fresh restore.
        let restored = store.restore(&revision, "oracle")?;
        for group in &groups {
            let mut identity = None;
            for path in &group.paths {
                let path = restored.path.join(path);
                assert_eq!(fs::read(&path)?, group.bytes);
                let m = fs::metadata(&path)?;
                assert_eq!(m.mode() & 0o777, group.mode);
                let current = (m.dev(), m.ino());
                if let Some(previous) = identity {
                    assert_eq!(current, previous);
                } else {
                    identity = Some(current);
                }
            }
        }
        let doctor = store.doctor()?;
        assert!(doctor.healthy, "{doctor:?}");
        let report = serde_json::json!({"complete":true,"label":label,"case":case,"unique_files":unique,"file_paths":unique*aliases,"samples_per_kind":samples,"records":records,"full_committed_payload_mode_hardlink_oracle":true,"doctor":doctor,"logical_bytes":budget(&root)?,"collector":false,"fixture_edits_fsynced_before_checkpoint":false});
        let mut out = File::create(root.join("results.json"))?;
        out.write_all(&serde_json::to_vec_pretty(&report)?)?;
        out.sync_all()?;
        eprintln!("PRESSURE complete case={case} label={label} checkpoints={ordinal}");
        Ok(())
    }
}

#[cfg(unix)]
fn main() -> sprig::Result<()> {
    unix::run()
}

#[cfg(not(unix))]
fn main() -> sprig::Result<()> {
    Err("checkpoint pressure harness requires Unix metadata semantics".into())
}
