#![cfg(unix)]
//! Three grouped end-to-end contracts: native storage, history, and recovery.
//! All data and subprocesses belong to disposable fixtures; no clone support is required.

use std::collections::BTreeSet;
use std::ffi::CString;
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use sprig::content::{Entry, EntryKind};
use sprig::{CopyMode, MergeStrategy, Mutation, Result, Store};

// POSIX fork can briefly retain another test thread's advisory-lock fds.
static TEST_RUN_LOCK: Mutex<()> = Mutex::new(());
static NEXT_ID: AtomicU64 = AtomicU64::new(0);

struct Fixture {
    root: PathBuf,
    source: PathBuf,
    store_path: PathBuf,
    store: Store,
}

impl Fixture {
    fn new() -> Result<Self> {
        let root = std::env::temp_dir().join(format!(
            "sprig-fixture-{}-{}-{}",
            std::process::id(),
            SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos(),
            NEXT_ID.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&root)?;
        let source = root.join("source");
        fs::create_dir(&source)?;
        let store_path = root.join("store");
        let store = Store::init(&store_path)?;
        Ok(Self {
            root,
            source,
            store_path,
            store,
        })
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_sprig"));
        command.arg("--store").arg(&self.store_path).args(args);
        command
    }

    fn cli(&self, args: &[&str]) -> Output {
        let mut child = self
            .command(args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("test CLI should start");
        wait_child(&mut child);
        child
            .wait_with_output()
            .expect("test CLI output should be readable")
    }

    fn state(&self) -> Vec<u8> {
        fs::read(self.store_path.join("state.json")).expect("state should be readable")
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn wait_child(child: &mut Child) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while child
        .try_wait()
        .expect("child status should be readable")
        .is_none()
    {
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("test command exceeded its deadline");
        }
        thread::sleep(Duration::from_millis(5));
    }
}

// A release gate makes lease checks deterministic; Drop also releases it on panic.
struct LeasedCommand {
    child: Child,
    release: PathBuf,
}
impl Drop for LeasedCommand {
    fn drop(&mut self) {
        let _ = fs::write(&self.release, b"release");
        wait_child(&mut self.child);
    }
}

fn chunks(entry: &Entry) -> &[String] {
    let EntryKind::File { chunks, .. } = &entry.kind else {
        panic!("expected regular file");
    };
    chunks
}

fn file_names(path: &Path) -> BTreeSet<String> {
    fs::read_dir(path)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .collect()
}

fn head(store: &Store, branch: &str) -> String {
    store.list().unwrap().branches[branch].clone()
}

#[test]
fn native_isolation_content_dedup_and_path_safety() -> Result<()> {
    let _serial = TEST_RUN_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let f = Fixture::new()?;
    let s = &f.store;
    fs::create_dir(f.source.join("empty"))?;
    fs::write(f.source.join("script"), b"#!/bin/sh\nexit 0\n")?;
    fs::set_permissions(f.source.join("script"), fs::Permissions::from_mode(0o751))?;
    let outside = f.root.join("outside");
    fs::create_dir(&outside)?;
    fs::write(outside.join("sentinel"), b"not imported")?;
    symlink(&outside, f.source.join("escape"))?;
    let mut random = 0x1234_5678_u64;
    let data: Vec<u8> = (0..2 * 1024 * 1024)
        .map(|_| {
            random ^= random << 13;
            random ^= random >> 7;
            random ^= random << 17;
            random as u8
        })
        .collect();
    fs::write(f.source.join("large"), &data)?;
    fs::write(f.source.join("duplicate"), &data)?;

    // A capture reuses its I/O allocation across files and directory boundaries.
    // Empty and short files must not inherit bytes or CDC boundaries from a predecessor.
    fs::create_dir_all(f.source.join("buffer/5"))?;
    let boundaries = [262_145, 0, 1, 262_143, 262_144, 262_145];
    let boundary_paths: Vec<_> = boundaries
        .iter()
        .enumerate()
        .map(|(index, _)| {
            if index == 5 {
                "buffer/5/nested".to_owned()
            } else {
                format!("buffer/{index}")
            }
        })
        .collect();
    for (path, &length) in boundary_paths.iter().zip(&boundaries) {
        fs::write(f.source.join(path), &data[..length])?;
    }

    let main = s.import(&f.source, "main", CopyMode::Copy)?;
    let base = main.base_revision.clone();
    // macOS system temporary paths can have symlinked ancestors.
    let objects = f.store_path.join("objects").canonicalize()?;
    for (path, &length) in boundary_paths.iter().zip(&boundaries) {
        let fresh_buffer = sprig::content::put_bytes(&data[..length], &objects)?;
        assert_eq!(s.stat("main", path)?.kind, fresh_buffer);
        assert_eq!(s.read("main", path)?, data[..length]);
    }
    assert!(main.path.is_dir() && main.path.join("empty").is_dir());
    assert_eq!(
        fs::metadata(main.path.join("script"))?.mode() & 0o777,
        0o751
    );
    assert_eq!(fs::read_link(main.path.join("escape"))?, outside);
    assert!(
        s.read("main", "escape").is_err(),
        "read must not dereference symbolic links"
    );
    assert!(
        s.glob("main", "escape/**")?.is_empty(),
        "symlink descendants must not be captured"
    );
    let original = s.stat("main", "large")?;
    assert!(
        chunks(&original).len() > 4,
        "large files must be content-defined chunks"
    );
    assert_eq!(chunks(&original), chunks(&s.stat("main", "duplicate")?));
    assert!(
        s.stats()?.object_bytes < 2 * data.len() as u64,
        "duplicate files share their chunk objects"
    );
    assert_eq!(
        s.checkpoint("main", "no content change", None)?.revision,
        base
    );

    let require_reflink = std::env::var_os("SPRIG_REQUIRE_REFLINK").is_some();
    let mode = if require_reflink {
        CopyMode::Clone
    } else {
        CopyMode::Auto
    };
    let fork = s.fork("main", "fork", mode)?;
    if require_reflink {
        let stats = fork.copy_stats.as_ref().expect("fork reports backend use");
        assert!(
            stats.cloned > 0 && stats.copied == 0,
            "required CoW backend must not silently fall back"
        );
    }
    for path in [&f.source, &main.path] {
        assert_ne!(
            fs::metadata(path.join("large"))?.ino(),
            fs::metadata(fork.path.join("large"))?.ino(),
            "copy-on-write may share extents but never mutable inodes across trees"
        );
    }
    let mut inserted = b"a small insertion at the start\n".to_vec();
    inserted.extend_from_slice(&data);
    fs::write(fork.path.join("large"), &inserted)?;
    let changed = s.checkpoint("fork", "insert prefix", None)?.revision;
    assert_eq!(fs::read(f.source.join("large"))?, data);
    assert_eq!(fs::read(main.path.join("large"))?, data);
    assert_eq!(s.read(&base, "large")?, data);
    let new_chunks = s.stat(&changed, "large")?;
    let retained = chunks(&original)
        .iter()
        .filter(|chunk| chunks(&new_chunks).contains(chunk))
        .count();
    assert!(
        retained * 4 >= chunks(&original).len() * 3,
        "a shifted file must reuse most CDC chunks"
    );
    let restored = s.restore(&changed, "restored")?;
    assert_eq!(fs::read(restored.path.join("large"))?, inserted);
    for (path, &length) in boundary_paths.iter().zip(&boundaries) {
        assert_eq!(fs::read(restored.path.join(path))?, data[..length]);
    }
    assert_eq!(fs::read_link(restored.path.join("escape"))?, outside);
    let mut streamed = Vec::new();
    s.read_to(&changed, "large", &mut streamed)?;
    assert_eq!(streamed, inserted);

    let state = f.state();
    for path in [
        "../outside/sentinel".to_owned(),
        outside.join("sentinel").to_string_lossy().into_owned(),
        "escape/sentinel".to_owned(),
        "escape".to_owned(),
    ] {
        assert!(
            s.apply(
                "main",
                &[Mutation::Write {
                    path,
                    content: b"unsafe".to_vec()
                }],
                "reject",
                None
            )
            .is_err()
        );
        assert_eq!(f.state(), state, "unsafe paths must publish nothing");
    }
    assert_eq!(fs::read(outside.join("sentinel"))?, b"not imported");
    let fifo = CString::new(f.source.join("special").as_os_str().as_bytes())?;
    // SAFETY: a valid NUL-terminated path owned by this fixture, ordinary mode bits.
    assert_eq!(
        unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) },
        0,
        "cannot create FIFO fixture: {}",
        std::io::Error::last_os_error()
    );
    assert!(
        s.import(&f.source, "special", CopyMode::Copy).is_err(),
        "special files are not snapshot data"
    );
    assert_eq!(f.state(), state);
    assert!(!s.list()?.branches.contains_key("special"));
    Ok(())
}

#[test]
fn shared_branch_merges_conflicts_and_immutable_branch_outputs() -> Result<()> {
    let _serial = TEST_RUN_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let f = Fixture::new()?;
    let s = &f.store;
    let original_text = b"first\nkeep one\nkeep two\nkeep three\nlast\n";
    fs::write(f.source.join("text"), original_text)?;
    fs::write(f.source.join("script"), b"old script\n")?;
    fs::set_permissions(f.source.join("script"), fs::Permissions::from_mode(0o644))?;
    fs::create_dir(f.source.join("directory"))?;
    fs::write(f.source.join("directory/child"), b"before\n")?;
    let first = s.import(&f.source, "main", CopyMode::Copy)?;
    let base = first.base_revision.clone();
    let initial_blame = s.blame(&base, "text")?;
    assert_eq!(initial_blame.lines.len(), 5);
    for (index, line) in initial_blame.lines.iter().enumerate() {
        assert_eq!(line.line, index + 1);
        assert_eq!(line.original_line, index + 1);
        assert_eq!(line.revision, base);
        assert_eq!(line.original_path, "text");
    }
    assert_eq!(
        initial_blame
            .lines
            .iter()
            .map(|line| line.text.as_str())
            .collect::<String>()
            .as_bytes(),
        original_text
    );
    let child_id = s.stat(&base, "directory/child")?.id;
    let stale = s.checkout("main", "stale")?;
    fs::write(
        first.path.join("text"),
        b"FIRST\nkeep one\nkeep two\nkeep three\nlast\n",
    )?;
    fs::rename(first.path.join("directory"), first.path.join("renamed"))?;
    fs::set_permissions(first.path.join("script"), fs::Permissions::from_mode(0o755))?;
    let advanced = s.checkpoint("main", "first writer", None)?.revision;
    fs::write(
        stale.path.join("text"),
        b"first\nkeep one\nkeep two\nkeep three\nLAST\n",
    )?;
    fs::write(
        stale.path.join("directory/child"),
        b"edited through original path\n",
    )?;
    fs::write(stale.path.join("script"), b"new script\n")?;
    assert!(s.status()?.stale_workspaces.contains(&"stale".to_owned()));
    let state = f.state();
    assert!(
        s.checkpoint("stale", "wrong expected head", Some(&base))
            .is_err()
    );
    assert_eq!(
        f.state(),
        state,
        "explicit CAS failures must not publish a generation"
    );
    let merged = s.checkpoint("stale", "second writer", None)?.revision;
    let log = s.log(Some(&merged))?;
    assert_eq!(
        log[0].parents.len(),
        2,
        "automatic merges retain both histories"
    );
    assert!(log[0].parents.contains(&advanced));
    assert_eq!(
        s.read("main", "text")?,
        b"FIRST\nkeep one\nkeep two\nkeep three\nLAST\n"
    );
    assert_eq!(
        s.read("main", "renamed/child")?,
        b"edited through original path\n"
    );
    assert_eq!(s.stat("main", "renamed/child")?.id, child_id);
    assert_eq!(
        s.stat("main", "script")?.mode,
        0o755,
        "mode and contents merge independently"
    );
    assert_eq!(s.read("main", "script")?, b"new script\n");
    let merged_blame = s.blame(&merged, "text")?;
    assert_eq!(merged_blame.lines[0].revision, advanced);
    assert_eq!(merged_blame.lines[1].revision, base);
    assert_eq!(merged_blame.lines[4].text, "LAST\n");
    assert_eq!(
        merged_blame.lines[4].revision, merged,
        "first-parent blame attributes a second-parent change to the merge"
    );
    assert!(
        s.diff(&base, &merged)?
            .iter()
            .any(|change| change.path == "renamed/child"
                && change.previous_path.as_deref() == Some("directory/child"))
    );

    let writer = s.checkout("main", "writer")?;
    let rival = s.checkout("main", "rival")?;
    fs::write(writer.path.join("text"), b"writer choice\n")?;
    let winning = s.checkpoint("writer", "winner", None)?.revision;
    fs::write(rival.path.join("text"), b"rival choice\n")?;
    let error = s
        .checkpoint("rival", "conflict", None)
        .unwrap_err()
        .to_string();
    assert!(error.contains("merge_conflict"), "{error}");
    assert_eq!(
        head(s, "main"),
        winning,
        "a conflict cannot move the branch"
    );
    let conflict = s.show("rival")?;
    assert_ne!(
        conflict.path, rival.path,
        "conflicts use a new recoverable generation"
    );
    assert_eq!(
        fs::read(rival.path.join("text"))?,
        b"rival choice\n",
        "original dirty bytes remain intact"
    );
    assert!(!conflict.pending_conflicts.is_empty());
    assert!(fs::read_to_string(conflict.path.join("text"))?.contains("<<<<<<<"));
    assert!(
        s.checkpoint("rival", "cannot bypass resolve", None)
            .is_err()
    );
    let paths: Vec<_> = conflict
        .pending_conflicts
        .iter()
        .map(|item| item.path.clone())
        .collect();
    assert!(
        s.resolve_conflicts("rival", &paths)
            .unwrap_err()
            .to_string()
            .contains("unresolved_markers")
    );
    assert!(
        s.gc()?.unreachable_revisions.is_empty(),
        "pending candidates remain GC roots"
    );
    let pending_fork = s.fork("rival", "pending-fork", CopyMode::Copy)?;
    assert_eq!(pending_fork.pending_conflicts.len(), paths.len());
    assert!(
        s.checkpoint("pending-fork", "must not escape conflict", None)
            .is_err()
    );
    let pending_trash = s.drop_entry("workspace", "pending-fork")?;
    let pending_recovered = s.recover(&pending_trash.token, "pending-recovered", CopyMode::Copy)?;
    assert_eq!(pending_recovered.pending_conflicts.len(), paths.len());
    assert!(
        s.checkpoint("pending-recovered", "recovery must retain conflict", None)
            .is_err()
    );

    let staged_edit = s.apply(
        "rival",
        &[Mutation::Edit {
            path: "text".into(),
            old: "rival choice".into(),
            new: "staged choice".into(),
        }],
        "edit unresolved workspace",
        None,
    )?;
    assert!(!staged_edit.committed);
    assert_eq!(head(s, "main"), winning);
    assert!(fs::read_to_string(s.show("rival")?.path.join("text"))?.contains("staged choice"));
    let staged_write = s.apply(
        "rival",
        &[Mutation::Write {
            path: "text".into(),
            content: b"resolved explicitly\n".to_vec(),
        }],
        "prepare resolution",
        None,
    )?;
    assert!(!staged_write.committed);
    assert_eq!(
        head(s, "main"),
        winning,
        "pending SDK writes must not publish a revision"
    );
    assert_eq!(s.read("main", "text")?, b"writer choice\n");
    assert_eq!(s.show("rival")?.pending_conflicts.len(), paths.len());
    assert!(
        s.checkpoint("rival", "clean bytes still require explicit resolve", None)
            .is_err()
    );
    assert!(fs::read_to_string(conflict.path.join("text"))?.contains("<<<<<<<"));
    s.resolve_conflicts("rival", &paths)?;
    let resolution = s.checkpoint("rival", "resolved", None)?;
    assert!(resolution.committed);
    let resolved = resolution.revision;
    assert_eq!(s.log(Some(&resolved))?[0].parents.len(), 2);
    assert_eq!(s.read("main", "text")?, b"resolved explicitly\n");

    let left = s.restore(&base, "left")?;
    let right = s.restore(&base, "right")?;
    fs::write(left.path.join("left-only"), b"left")?;
    fs::write(right.path.join("right-only"), b"right")?;
    let left_head = s.checkpoint("left", "left", None)?.revision;
    let right_head = s.checkpoint("right", "right", None)?.revision;
    let output = s.merge(
        "left",
        "right",
        "combined",
        MergeStrategy::Conflict,
        &[],
        Some(&left_head),
    )?;
    assert!(output.committed && output.conflicts.is_empty());
    let combined = output.revision.unwrap();
    assert_eq!(
        s.log(Some(&combined))?[0].parents,
        [left_head.clone(), right_head.clone()]
    );
    assert_eq!(head(s, "left"), left_head);
    assert_eq!(head(s, "right"), right_head);
    assert!(!left.path.join("right-only").exists() && !right.path.join("left-only").exists());
    assert_eq!(s.read("combined", "left-only")?, b"left");
    assert_eq!(s.read("combined", "right-only")?, b"right");
    let revisions = s.stats()?.revisions;
    let ff = s.merge(
        "left",
        "combined",
        "fast-forward",
        MergeStrategy::Conflict,
        &[],
        None,
    )?;
    assert_eq!(ff.revision.as_deref(), Some(combined.as_str()));
    assert_eq!(
        s.stats()?.revisions,
        revisions,
        "fast-forward creates only a new reference"
    );
    assert!(
        s.merge("left", "right", "left", MergeStrategy::Conflict, &[], None)
            .is_err()
    );
    assert_eq!(head(s, "left"), left_head);

    s.checkout("combined", "blame-work")?;
    let edited = s
        .apply(
            "blame-work",
            &[
                Mutation::Edit {
                    path: "text".into(),
                    old: "keep two\n".into(),
                    new: "inserted\nchanged two\n".into(),
                },
                Mutation::Write {
                    path: "binary".into(),
                    content: vec![0, 255, 128],
                },
            ],
            "blame edits",
            None,
        )?
        .revision;
    let edited_blame = s.blame(&edited, "text")?;
    assert_eq!(edited_blame.identity, initial_blame.identity);
    assert_eq!(edited_blame.lines.len(), 6);
    for (index, (origin, original_line, text)) in [
        (&base, 1, "first\n"),
        (&base, 2, "keep one\n"),
        (&edited, 3, "inserted\n"),
        (&edited, 4, "changed two\n"),
        (&base, 4, "keep three\n"),
        (&base, 5, "last\n"),
    ]
    .iter()
    .enumerate()
    {
        let line = &edited_blame.lines[index];
        assert_eq!(line.line, index + 1);
        assert_eq!(&line.revision, *origin);
        assert_eq!(line.original_line, *original_line);
        assert_eq!(line.original_path, "text");
        assert_eq!(line.text, *text);
    }
    assert_eq!(edited_blame.lines[2].message, "blame edits");
    assert!(
        s.blame(&edited, "binary").is_err(),
        "binary data has no text-line blame"
    );
    let moved = s
        .apply(
            "blame-work",
            &[Mutation::Move {
                from: "text".into(),
                to: "moved-text".into(),
            }],
            "move without changing lines",
            None,
        )?
        .revision;
    let moved_blame = s.blame(&moved, "moved-text")?;
    assert_eq!(moved_blame.path, "moved-text");
    assert_eq!(moved_blame.identity, edited_blame.identity);
    assert_eq!(moved_blame.lines.len(), edited_blame.lines.len());
    for (before, after) in edited_blame.lines.iter().zip(&moved_blame.lines) {
        assert_eq!(after.text, before.text);
        assert_eq!(
            after.revision, before.revision,
            "moves must not steal line provenance"
        );
        assert_eq!(after.original_line, before.original_line);
        assert_eq!(after.original_path, "text");
    }
    Ok(())
}

#[test]
fn transactional_sdk_exec_leases_recovery_and_non_destructive_inspection() -> Result<()> {
    let _serial = TEST_RUN_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let f = Fixture::new()?;
    let s = &f.store;
    fs::write(f.source.join("document"), b"one two\n")?;
    let original = s.import(&f.source, "main", CopyMode::Copy)?;
    let old_id = s.stat("main", "document")?.id;
    let restore_generation = s.status()?.generation;
    let state = f.state();
    assert!(
        s.apply(
            "main",
            &[
                Mutation::Write {
                    path: "document".into(),
                    content: b"partial".to_vec()
                },
                Mutation::Edit {
                    path: "document".into(),
                    old: "absent".into(),
                    new: "never".into()
                },
            ],
            "must roll back all operations",
            None
        )
        .is_err()
    );
    assert_eq!(f.state(), state);
    assert_eq!(fs::read(original.path.join("document"))?, b"one two\n");
    assert_eq!(s.read("main", "document")?, b"one two\n");
    let committed = s
        .apply(
            "main",
            &[
                Mutation::Copy {
                    from: "document".into(),
                    to: "copy".into(),
                },
                Mutation::Move {
                    from: "document".into(),
                    to: "moved".into(),
                },
                Mutation::Edit {
                    path: "moved".into(),
                    old: "one".into(),
                    new: "three".into(),
                },
                Mutation::Chmod {
                    path: "moved".into(),
                    mode: 0o750,
                },
                Mutation::Mkdir {
                    path: "directory".into(),
                },
                Mutation::Write {
                    path: "directory/binary".into(),
                    content: vec![0, 255, 128, 42],
                },
            ],
            "atomic batch",
            Some(&original.base_revision),
        )?
        .revision;
    let current = s.show("main")?;
    assert_ne!(current.path, original.path);
    assert_eq!(
        fs::read(original.path.join("document"))?,
        b"one two\n",
        "old generations stay recoverable"
    );
    assert!(!current.path.join("document").exists());
    assert_eq!(s.read("main", "moved")?, b"three two\n");
    assert_eq!(
        s.stat("main", "moved")?.id,
        old_id,
        "move/edit preserves identity"
    );
    assert_ne!(
        s.stat("main", "copy")?.id,
        old_id,
        "copy creates a new identity"
    );
    assert_eq!(s.stat("main", "moved")?.mode, 0o750);
    let raw = f.cli(&["read", "main", "directory/binary"]);
    assert!(raw.status.success(), "{raw:?}");
    assert_eq!(
        raw.stdout,
        [0, 255, 128, 42],
        "read stdout is exact bytes, not formatted text"
    );
    let json = f.cli(&["--json", "read", "main", "missing"]);
    assert!(!json.status.success());
    let error: serde_json::Value = serde_json::from_slice(&json.stdout)?;
    assert_eq!(error["schema_version"], 1);
    assert!(
        error["error"]["message"]
            .as_str()
            .is_some_and(|message| message.contains("path not found"))
    );

    fs::write(current.path.join("uncommitted"), b"preserve dirty work")?;
    let dropped = s.drop_entry("workspace", "main")?;
    let branch = s.drop_entry("branch", "main")?;
    let recovered = s.recover(&dropped.token, "recovered", CopyMode::Copy)?;
    let recovered_branch = s.recover(&branch.token, "recovered-branch", CopyMode::Copy)?;
    assert_eq!(
        fs::read(recovered.path.join("uncommitted"))?,
        b"preserve dirty work"
    );
    assert!(
        !recovered_branch.path.join("uncommitted").exists(),
        "branch recovery restores committed content only"
    );
    assert_eq!(
        fs::read(current.path.join("uncommitted"))?,
        b"preserve dirty work"
    );
    let trash = s.list()?.trash;
    assert!(
        trash.iter().any(|item| item.token == dropped.token)
            && trash.iter().any(|item| item.token == branch.token),
        "recovery copies rather than consumes trash"
    );

    let release = recovered.path.join("lease-release");
    let mut running = LeasedCommand {
        child: f.command(&["exec", "recovered", "--", "sh", "-c",
            "printf started > lease-started; while test ! -e lease-release; do sleep 0.01; done; exit 7"])
            .stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).spawn()?,
        release: release.clone(),
    };
    let deadline = Instant::now() + Duration::from_secs(5);
    while !recovered.path.join("lease-started").exists() {
        assert!(Instant::now() < deadline, "exec must enter its workspace");
        assert!(
            running.child.try_wait()?.is_none(),
            "exec must stay alive until released"
        );
        thread::sleep(Duration::from_millis(5));
    }
    assert!(
        s.checkpoint("recovered", "blocked by lease", None)
            .unwrap_err()
            .to_string()
            .contains("workspace_busy")
    );
    assert!(s.status()?.active_leases.contains(&"recovered".to_owned()));
    s.checkpoint("recovered-branch", "other workspaces remain usable", None)?;
    fs::write(&release, b"release")?;
    wait_child(&mut running.child);
    assert_eq!(
        running.child.wait()?.code(),
        Some(7),
        "CLI must preserve child exit status"
    );
    drop(running);
    assert!(s.status()?.active_leases.is_empty());
    s.checkpoint("recovered", "lease released", None)?;
    assert!(
        s.exec("recovered", "/definitely-not-a-sprig-program", &[])
            .is_err()
    );
    s.checkpoint("recovered", "failed spawn also releases lease", None)?;

    // Revert metadata, not history, including after an interrupted generation and
    // parseable but structurally invalid state. Keep the invalid bytes for diagnosis.
    let incomplete = f.store_path.join("native/interrupted-generation");
    fs::create_dir(&incomplete)?;
    fs::write(incomplete.join("retained"), b"unfinished generation")?;
    let mut invalid: serde_json::Value = serde_json::from_slice(&f.state())?;
    invalid["branches"]["invalid/name"] = serde_json::Value::String(committed.clone());
    let invalid_bytes = serde_json::to_vec(&invalid)?;
    fs::write(f.store_path.join("state.json"), &invalid_bytes)?;
    assert!(
        Store::open(&f.store_path).is_err(),
        "the fixture must actually be invalid"
    );
    let recovered_state = Store::recover_state(&f.store_path, restore_generation)?;
    assert!(recovered_state.generation > invalid["generation"].as_u64().unwrap());
    assert_eq!(
        fs::read(incomplete.join("retained"))?,
        b"unfinished generation"
    );
    let retained_invalid = fs::read_dir(f.store_path.join("staging"))?.any(|entry| {
        let entry = entry.unwrap();
        entry
            .file_name()
            .to_string_lossy()
            .starts_with("retained-state-")
            && fs::read(entry.path()).is_ok_and(|bytes| bytes == invalid_bytes)
    });
    assert!(
        retained_invalid,
        "recovery must retain the invalid metadata bytes"
    );
    assert_eq!(s.read("main", "document")?, b"one two\n");
    assert!(recovered.path.exists() && current.path.exists());
    assert!(s.doctor()?.healthy);
    let loose = b"unreachable test object";
    let loose_hash = blake3::hash(loose).to_hex().to_string();
    fs::write(f.store_path.join("objects").join(&loose_hash), loose)?;
    let objects_before = file_names(&f.store_path.join("objects"));
    let revisions_before = file_names(&f.store_path.join("revisions"));
    let plan = s.gc()?;
    assert!(plan.dry_run && plan.unreachable_objects.contains(&loose_hash));
    assert!(
        !plan.unreachable_revisions.contains(&committed),
        "state backups protect former heads"
    );
    for retained in [&original.path, &current.path, &recovered.path] {
        let directory = retained
            .parent()
            .unwrap()
            .file_name()
            .unwrap()
            .to_str()
            .unwrap();
        assert!(
            !plan.orphan_workspaces.iter().any(|name| name == directory),
            "workspace/trash/backup roots are protected"
        );
    }
    assert_eq!(
        file_names(&f.store_path.join("objects")),
        objects_before,
        "GC must not delete objects"
    );
    assert_eq!(
        file_names(&f.store_path.join("revisions")),
        revisions_before
    );
    let entry = s.stat("main", "document")?;
    let object = f.store_path.join("objects").join(&chunks(&entry)[0]);
    let saved = fs::read(&object)?;
    fs::set_permissions(&object, fs::Permissions::from_mode(0o644))?;
    fs::write(&object, b"corrupt")?;
    let report = s.doctor()?;
    assert!(
        !report.healthy
            && report
                .errors
                .iter()
                .any(|message| message.contains("hash mismatch"))
    );
    assert!(
        s.read("main", "document").is_err(),
        "reads verify chunk hashes"
    );
    fs::write(&object, saved)?;
    fs::set_permissions(&object, fs::Permissions::from_mode(0o444))?;
    assert!(s.doctor()?.healthy);
    // Compression is a representation change, never permission to skip integrity.
    let revision_path = f
        .store_path
        .join("revisions")
        .join(format!("{committed}.json.zst"));
    let encoded = fs::read(&revision_path)?;
    assert!(encoded.starts_with(&[0x28, 0xb5, 0x2f, 0xfd]));
    for invalid in [encoded[..encoded.len() - 1].to_vec(), {
        let mut bytes = encoded.clone();
        bytes.extend_from_slice(&encoded);
        bytes
    }] {
        fs::write(&revision_path, invalid)?;
        assert!(s.log(Some(&committed)).is_err());
    }
    fs::write(&revision_path, &encoded)?;
    assert!(s.log(Some(&committed)).is_ok());
    let backup_path = f
        .store_path
        .join("backups")
        .join(format!("{restore_generation}.json.zst"));
    let backup = fs::read(&backup_path)?;
    let mut corrupt = backup.clone();
    *corrupt.last_mut().unwrap() ^= 1;
    let before_recovery = f.state();
    fs::write(&backup_path, corrupt)?;
    assert!(Store::recover_state(&f.store_path, restore_generation).is_err());
    assert_eq!(f.state(), before_recovery);
    fs::write(backup_path, backup)?;
    assert!(s.doctor()?.healthy);
    let unknown = f.root.join("unknown-store");
    fs::create_dir(&unknown)?;
    fs::write(unknown.join("user-file"), b"keep this data\n")?;
    assert!(Store::open(&unknown).is_err());
    assert!(Store::init(&unknown).is_err());
    assert_eq!(
        file_names(&unknown),
        BTreeSet::from(["user-file".to_owned()])
    );
    assert_eq!(fs::read(unknown.join("user-file"))?, b"keep this data\n");
    let state_path = f.store_path.join("state.json");
    let current_bytes = fs::read(&state_path)?;
    let mut unsupported: serde_json::Value = serde_json::from_slice(&current_bytes)?;
    unsupported["version"] = serde_json::json!(u32::MAX);
    let unsupported_bytes = serde_json::to_vec(&unsupported)?;
    fs::write(&state_path, &unsupported_bytes)?;
    let error = Store::open(&f.store_path).unwrap_err().to_string();
    assert!(error.contains("unsupported store schema"), "{error}");
    assert_eq!(fs::read(&state_path)?, unsupported_bytes);
    assert!(Store::recover_state(&f.store_path, restore_generation).is_err());
    assert_eq!(fs::read(&state_path)?, unsupported_bytes);
    unsupported["future_field"] = serde_json::json!(true);
    let future_bytes = serde_json::to_vec(&unsupported)?;
    fs::write(&state_path, &future_bytes)?;
    assert!(Store::open(&f.store_path).is_err());
    assert!(Store::recover_state(&f.store_path, restore_generation).is_err());
    assert_eq!(fs::read(&state_path)?, future_bytes);
    fs::write(state_path, current_bytes)?;
    Ok(())
}
