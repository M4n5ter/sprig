#![cfg(windows)]
//! One grouped native Windows contract, plus an explicitly invoked child helper.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use sprig::content::{self, Observed};
use sprig::{CopyMode, MergeStrategy, Mutation, Result, Store};

struct Fixture(PathBuf);
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn wait_until(mut ready: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(30);
    while !ready() {
        assert!(Instant::now() < deadline, "Windows contract timed out");
        thread::sleep(Duration::from_millis(10));
    }
}

fn child_arguments() -> Vec<String> {
    ["--ignored", "--exact", "windows_exec_child", "--nocapture"]
        .into_iter()
        .map(String::from)
        .collect()
}

fn concurrent_exec(store: Store, name: &str, binary: &str) -> Result<i32> {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        match store.exec(name, binary, &child_arguments()) {
            Err(error)
                if error.to_string().contains("store_locked") && Instant::now() < deadline =>
            {
                // This failure precedes child creation; no uncertain mutation
                // or process launch is repeated.
                thread::sleep(Duration::from_millis(5));
            }
            result => return result,
        }
    }
}

/// Invoked only by this test through Sprig exec, in a disposable workspace.
#[test]
#[ignore]
fn windows_exec_child() -> Result<()> {
    fs::write("ready", b"ready")?;
    wait_until(|| Path::new("release").exists());
    fs::write("child-finished", b"finished")?;
    Ok(())
}

#[test]
fn windows_native_copy_history_and_leases() -> Result<()> {
    let temporary = std::env::temp_dir().join(format!(
        "sprig-windows-{}-{}",
        std::process::id(),
        SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos()
    ));
    fs::create_dir(&temporary)?;
    let fixture = Fixture(temporary.canonicalize()?);
    let source = fixture.0.join("source");
    fs::create_dir(&source)?;
    fs::create_dir(source.join("directory"))?;
    fs::write(
        source.join("directory/file.txt"),
        b"one\ntwo\nthree\nfour\nfive\n",
    )?;
    let store_path = fixture.0.join("store");
    let store = Store::init(&store_path)?;
    let main = store.import(&source, "main", CopyMode::Auto)?;
    let stats = main.copy_stats.as_ref().expect("native copy statistics");
    assert_eq!(stats.cloned, 0);
    assert_eq!(stats.copied, 1);
    let base = main.base_revision.clone();
    let topic = store.fork("main", "topic", CopyMode::Copy)?;
    fs::write(
        topic.path.join("directory/file.txt"),
        b"ONE\ntwo\nthree\nfour\nfive\n",
    )?;
    store.checkpoint("topic", "change first line", Some(&base))?;
    assert_eq!(
        fs::read(main.path.join("directory/file.txt"))?,
        b"one\ntwo\nthree\nfour\nfive\n"
    );
    fs::write(
        main.path.join("directory/file.txt"),
        b"one\ntwo\nthree\nfour\nFIVE\n",
    )?;
    store.checkpoint("main", "change last line", Some(&base))?;
    assert!(
        store
            .merge(
                "main",
                "topic",
                "combined",
                MergeStrategy::Conflict,
                &[],
                None
            )?
            .committed
    );
    let restored = store.checkout("combined", "restored")?;
    assert_eq!(
        fs::read(restored.path.join("directory/file.txt"))?,
        b"ONE\ntwo\nthree\nfour\nFIVE\n"
    );
    assert!(store.log(Some("combined"))?.len() >= 4);
    assert_eq!(
        store.read(&base, "directory/file.txt")?,
        b"one\ntwo\nthree\nfour\nfive\n"
    );
    let deleted = store.drop_entry("workspace", "restored")?;
    let recovered = store.recover(&deleted.token, "recovered", CopyMode::Copy)?;
    assert_eq!(
        fs::read(recovered.path.join("directory/file.txt"))?,
        b"ONE\ntwo\nthree\nfour\nFIVE\n"
    );

    let objects = fixture.0.join("objects");
    let (manifest, _) = content::capture(&source, &objects, &Observed::new())?;
    let materialized = fixture.0.join("materialized");
    content::materialize(&manifest, &objects, &materialized)?;
    let (round_trip, _) = content::capture(&materialized, &objects, &Observed::new())?;
    for (path, entry) in &manifest {
        assert_eq!(entry.mode, round_trip[path].mode);
        assert_eq!(entry.modified_ns, round_trip[path].modified_ns);
        assert_eq!(entry.kind, round_trip[path].kind);
    }
    let mut executable = manifest.clone();
    executable.get_mut("directory/file.txt").unwrap().mode = 0o755;
    assert!(content::materialize(&executable, &objects, &fixture.0.join("executable")).is_err());
    assert!(!fixture.0.join("executable").exists());
    assert!(store.fork("main", "clone", CopyMode::Clone).is_err());
    let state = fs::read(store_path.join("state.json"))?;
    for invalid in [
        "NUL",
        "file:stream",
        "trailing.",
        "..\\outside",
        "directory\\alias",
    ] {
        assert!(
            store
                .apply(
                    "main",
                    &[Mutation::Write {
                        path: invalid.into(),
                        content: b"bad".to_vec()
                    }],
                    "unsafe",
                    None
                )
                .is_err()
        );
        assert_eq!(fs::read(store_path.join("state.json"))?, state);
    }
    assert!(
        store
            .apply(
                "main",
                &[Mutation::Chmod {
                    path: "directory/file.txt".into(),
                    mode: 0o755
                }],
                "chmod",
                None
            )
            .is_err()
    );

    // Directory junctions require no Developer Mode or symlink privilege.
    let outside = fixture.0.join("outside");
    fs::create_dir(&outside)?;
    fs::write(outside.join("sentinel"), b"untouched")?;
    let junction = source.join("junction");
    let output = Command::new("cmd.exe")
        .args(["/d", "/c", "mklink", "/J"])
        .arg(&junction)
        .arg(&outside)
        .output()?;
    assert!(
        output.status.success(),
        "junction setup: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(content::capture(&source, &objects, &Observed::new()).is_err());
    assert!(
        sprig::copy_tree::copy_tree(&source, &fixture.0.join("junction-copy"), CopyMode::Copy)
            .is_err()
    );
    assert_eq!(fs::read(outside.join("sentinel"))?, b"untouched");
    // The supplied root itself can be a regular directory reached through an
    // intermediate junction. Reject it before canonicalization hides the link.
    fs::create_dir(outside.join("ordinary-child"))?;
    fs::write(outside.join("ordinary-child/file.txt"), b"outside")?;
    let aliased_source = junction.join("ordinary-child");
    assert!(content::capture(&aliased_source, &objects, &Observed::new()).is_err());
    assert!(
        sprig::copy_tree::copy_tree(
            &aliased_source,
            &fixture.0.join("aliased-copy"),
            CopyMode::Copy
        )
        .is_err()
    );
    let before_alias_import = fs::read(store_path.join("state.json"))?;
    assert!(
        store
            .import(&aliased_source, "aliased", CopyMode::Copy)
            .is_err()
    );
    assert_eq!(
        fs::read(store_path.join("state.json"))?,
        before_alias_import
    );
    assert!(Store::init(&junction.join("new-store")).is_err());
    assert!(!outside.join("new-store").exists());
    Store::init(&outside.join("existing-store"))?;
    assert!(Store::open(&junction.join("existing-store")).is_err());
    assert!(content::materialize(&manifest, &objects, &junction.join("new-tree")).is_err());
    assert!(!outside.join("new-tree").exists());
    fs::remove_dir(&junction)?;
    let readonly = source.join("readonly");
    fs::write(&readonly, b"readonly")?;
    let original_permissions = fs::metadata(&readonly)?.permissions();
    let mut permissions = original_permissions.clone();
    permissions.set_readonly(true);
    fs::set_permissions(&readonly, permissions)?;
    assert!(content::capture(&source, &objects, &Observed::new()).is_err());
    fs::set_permissions(&readonly, original_permissions)?;

    for entry in [source.join("directory/file.txt"), source.join("directory")] {
        let mut stream = entry.into_os_string();
        stream.push(":sprig-test-stream");
        fs::write(&stream, b"must not be discarded")?;
        assert!(content::capture(&source, &objects, &Observed::new()).is_err());
        fs::remove_file(stream)?;
    }

    // Killing Sprig must not release the lease while its child is still alive.
    let killed = store.checkout("main", "killed")?;
    let test_binary = std::env::current_exe()?;
    let mut supervisor = Command::new(env!("CARGO_BIN_EXE_sprig"))
        .arg("--store")
        .arg(&store_path)
        .args(["exec", "killed", "--"])
        .arg(&test_binary)
        .args(child_arguments())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()?;
    wait_until(|| killed.path.join("ready").exists() || supervisor.try_wait().unwrap().is_some());
    assert!(
        killed.path.join("ready").exists(),
        "managed child did not start"
    );
    supervisor.kill()?;
    supervisor.wait()?;
    let error = store
        .checkpoint("killed", "must remain busy", None)
        .unwrap_err();
    assert!(error.to_string().contains("workspace_busy"), "{error}");
    fs::write(killed.path.join("release"), b"release")?;
    wait_until(|| {
        store
            .status()
            .is_ok_and(|status| !status.active_leases.contains(&"killed".into()))
    });
    assert!(killed.path.join("child-finished").exists());
    store.checkpoint("killed", "after child exit", None)?;

    // Two simultaneous SDK execs must not accidentally retain one another's
    // lease after the short-lived child exits. Third-party host spawns are not
    // covered by Sprig's process-local spawn mutex and can prolong busy status.
    let first = store.restore(&base, "first")?;
    let second = store.restore(&base, "second")?;
    let first_store = store.clone();
    let second_store = store.clone();
    let binary = test_binary
        .to_str()
        .ok_or("non-UTF-8 test binary")?
        .to_owned();
    let second_binary = binary.clone();
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(3));
    let first_barrier = barrier.clone();
    let second_barrier = barrier.clone();
    let first_thread = thread::spawn(move || {
        first_barrier.wait();
        concurrent_exec(first_store, "first", &binary)
    });
    let second_thread = thread::spawn(move || {
        second_barrier.wait();
        concurrent_exec(second_store, "second", &second_binary)
    });
    barrier.wait();
    wait_until(|| first.path.join("ready").exists() && second.path.join("ready").exists());
    assert!(store.status()?.active_leases.contains(&"first".into()));
    fs::write(first.path.join("release"), b"release")?;
    assert_eq!(first_thread.join().expect("first exec thread")?, 0);
    let status = store.status()?;
    assert!(!status.active_leases.contains(&"first".into()));
    assert!(status.active_leases.contains(&"second".into()));
    fs::write(second.path.join("release"), b"release")?;
    assert_eq!(second_thread.join().expect("second exec thread")?, 0);
    assert!(store.doctor()?.healthy);
    Ok(())
}

/// Runs only against an explicitly supplied, already existing ReFS volume.
/// It never creates or formats a disk and removes only its own new fixture.
#[test]
fn refs_block_clone_roundtrip_and_isolation() -> Result<()> {
    let Some(root) = std::env::var_os("SPRIG_REFS_TEST_ROOT") else {
        eprintln!("ReFS runtime test not exercised: SPRIG_REFS_TEST_ROOT is unset");
        return Ok(());
    };
    let root = PathBuf::from(root).canonicalize()?;
    assert_eq!(
        sprig::copy_tree::native_backend_info(&root)?.filesystem,
        "ReFS"
    );
    let temporary = root.join(format!(
        "sprig-refs-{}-{}",
        std::process::id(),
        SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos()
    ));
    fs::create_dir(&temporary)?;
    let fixture = Fixture(temporary.canonicalize()?);
    let source = fixture.0.join("source");
    fs::create_dir(&source)?;
    let payload: Vec<u8> = (0..1_048_593)
        .map(|i| ((i * 17 + i / 4096) % 251) as u8)
        .collect();
    fs::write(source.join("data"), &payload)?;
    let other_root = std::env::temp_dir().canonicalize()?;
    if sprig::copy_tree::native_backend_info(&other_root)?.filesystem == "NTFS" {
        let other = other_root.join(format!(
            "sprig-refs-cross-{}-{}",
            std::process::id(),
            SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos()
        ));
        fs::create_dir(&other)?;
        let other = Fixture(other.canonicalize()?);
        fs::write(other.0.join("data"), &payload)?;
        let cross = fixture.0.join("cross-volume");
        let stats = sprig::copy_tree::copy_tree(&other.0, &cross, CopyMode::Auto)?;
        assert_eq!(stats.cloned, 0);
        assert_eq!(stats.copied, 1);
        assert_eq!(fs::read(cross.join("data"))?, payload);
        assert_eq!(fs::read(other.0.join("data"))?, payload);
    }
    let target = fixture.0.join("clone");
    let copied = sprig::copy_tree::copy_tree(&source, &target, CopyMode::Clone)?;
    assert_eq!(copied.cloned, 1);
    assert_eq!(copied.copied, 0);
    assert_eq!(fs::read(target.join("data"))?, payload);
    assert!(sprig::copy_tree::copy_tree(&source, &target, CopyMode::Clone).is_err());
    assert_eq!(fs::read(target.join("data"))?, payload);
    let objects = fixture.0.join("objects");
    let (_, original_ids) = content::capture(&source, &objects, &Observed::new())?;
    let (_, clone_ids) = content::capture(&target, &objects, &Observed::new())?;
    assert_ne!(original_ids["data"].inode, clone_ids["data"].inode);
    assert_eq!(original_ids["data"].device, clone_ids["data"].device);
    fs::write(target.join("data"), b"private edit")?;
    assert_eq!(fs::read(source.join("data"))?, payload);
    if sprig::copy_tree::native_backend_info(&source)?.hardlink_support == Some(true) {
        fs::hard_link(source.join("data"), source.join("alias"))?;
        let linked = fixture.0.join("linked-clone");
        let stats = sprig::copy_tree::copy_tree(&source, &linked, CopyMode::Clone)?;
        assert_eq!(stats.cloned, 1);
        assert_eq!(stats.hardlinks, 1);
        fs::write(linked.join("alias"), b"shared inside this copy")?;
        assert_eq!(fs::read(linked.join("data"))?, b"shared inside this copy");
        assert_eq!(fs::read(source.join("data"))?, payload);
    }
    let store = Store::init(&fixture.0.join("store"))?;
    let imported = store.import(&source, "main", CopyMode::Clone)?;
    assert_eq!(imported.copy_stats.as_ref().unwrap().cloned, 1);
    let forked = store.fork("main", "fork", CopyMode::Clone)?;
    fs::write(forked.path.join("data"), b"fork edit")?;
    store.checkpoint("fork", "edit", None)?;
    assert_eq!(fs::read(imported.path.join("data"))?, payload);
    let restored = store.restore(&imported.base_revision, "restored")?;
    assert_eq!(fs::read(restored.path.join("data"))?, payload);
    assert!(store.doctor()?.healthy);
    println!(
        "ReFS actual strict block clone, nonaligned EOF, capture identity, checkpoint, restore and write isolation passed"
    );
    Ok(())
}
