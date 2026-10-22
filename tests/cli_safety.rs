#![cfg(target_os = "linux")]

//! Black-box safety regressions. These tests exercise the shipped binary and
//! inspect real files, rather than treating a successful exit as proof of safety.
//! No test requires a reflink-capable filesystem or any external utility.

use std::ffi::{CString, OsStr, OsString};
use std::fs::{self, File, OpenOptions};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant};

static NEXT_ID: AtomicU64 = AtomicU64::new(0);

struct Fixture {
    root: PathBuf,
    store: PathBuf,
    source: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        // create_dir, rather than create_dir_all, prevents accidentally adopting
        // any preexisting test directory, including one containing a symlink.
        let root = std::env::temp_dir().join(format!(
            "sprig-test-{}-{}",
            std::process::id(),
            NEXT_ID.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&root).expect("test fixture operation should succeed");
        let store = root.join("store");
        let source = root.join("source");
        fs::create_dir(&source).expect("test fixture operation should succeed");
        Self {
            root,
            store,
            source,
        }
    }

    fn initialized() -> Self {
        let fixture = Self::new();
        fixture.ok(&["init"]);
        fixture
    }

    fn run(&self, args: &[&str]) -> Output {
        self.run_os(args.iter().map(OsStr::new))
    }

    fn run_os<I, S>(&self, args: I) -> Output
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let mut command = Command::new(env!("CARGO_BIN_EXE_sprig"));
        command.arg("--store").arg(&self.store).args(args);
        bounded_output(command)
    }

    fn ok(&self, args: &[&str]) -> String {
        success(self.run(args))
    }

    fn fail(&self, args: &[&str]) -> String {
        failure(self.run(args))
    }

    fn import(&self, name: &str, plain: bool) -> String {
        let mut args = Vec::<OsString>::new();
        if plain {
            args.push("--copy".into());
        }
        args.extend([
            "import".into(),
            self.source.as_os_str().to_owned(),
            name.into(),
        ]);
        success(self.run_os(args))
    }

    fn tree(&self, area: &str, name: &str) -> PathBuf {
        self.store.join(area).join(name).join("tree")
    }

    fn write_source(&self) {
        fs::create_dir(self.source.join("nested")).expect("test fixture operation should succeed");
        fs::create_dir(self.source.join("empty")).expect("test fixture operation should succeed");
        fs::write(self.source.join("hello.txt"), b"original\n")
            .expect("test fixture operation should succeed");
        fs::write(self.source.join("nested/binary"), [0, 255, 0, 42, 128])
            .expect("test fixture operation should succeed");
        fs::write(self.source.join("run.sh"), b"#!/bin/sh\nexit 0\n")
            .expect("test fixture operation should succeed");
        fs::set_permissions(
            self.source.join("run.sh"),
            fs::Permissions::from_mode(0o751),
        )
        .expect("test fixture operation should succeed");
        fs::set_permissions(
            self.source.join("nested"),
            fs::Permissions::from_mode(0o750),
        )
        .expect("test fixture operation should succeed");
        fs::set_permissions(&self.source, fs::Permissions::from_mode(0o750))
            .expect("test fixture operation should succeed");
    }

    fn dropped_token(&self, kind: &str, name: &str) -> String {
        self.ok(&["drop", kind, name])
            .trim()
            .strip_prefix("trash token: ")
            .expect("drop must expose a usable trash token")
            .to_owned()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn bounded_output(mut command: Command) -> Output {
    // In particular, a blocking lock or attempted read of a special file must
    // fail the test, not hang the entire suite indefinitely.
    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    let description = format!("{command:?}");
    let mut child = command.spawn().expect("Sprig test binary should start");
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        if child
            .try_wait()
            .expect("child process status should be available")
            .is_some()
        {
            return child
                .wait_with_output()
                .expect("child output should be available");
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let output = child
                .wait_with_output()
                .expect("child output should be available");
            panic!("command timed out: {description}\n{output:?}");
        }
        thread::sleep(Duration::from_millis(5));
    }
}

fn success(output: Output) -> String {
    assert!(output.status.success(), "expected success, got {output:?}");
    String::from_utf8(output.stdout).expect("CLI stdout should be UTF-8")
}

fn failure(output: Output) -> String {
    assert!(!output.status.success(), "expected failure, got {output:?}");
    let error = String::from_utf8(output.stderr).expect("CLI stderr should be UTF-8");
    assert!(
        error.contains("sprig:"),
        "missing actionable error: {error:?}"
    );
    error
}

fn contents(path: impl AsRef<Path>) -> Vec<u8> {
    fs::read(path).expect("test fixture operation should succeed")
}

fn create_fifo(path: &Path) {
    unsafe extern "C" {
        fn mkfifo(
            path: *const std::os::raw::c_char,
            mode: std::os::raw::c_uint,
        ) -> std::os::raw::c_int;
    }
    let path =
        CString::new(path.as_os_str().as_bytes()).expect("fixture path must not contain NUL");
    // SAFETY: the pathname is a live, NUL-terminated CString; mode_t is an
    // unsigned int on the Linux platforms supported by this binary.
    let result = unsafe { mkfifo(path.as_ptr(), 0o600) };
    assert_eq!(
        result,
        0,
        "cannot create FIFO fixture: {}",
        std::io::Error::last_os_error()
    );
}

fn assert_distinct_files(a: impl AsRef<Path>, b: impl AsRef<Path>) {
    let a = fs::metadata(a).expect("test fixture operation should succeed");
    let b = fs::metadata(b).expect("test fixture operation should succeed");
    assert_ne!(
        (a.dev(), a.ino()),
        (b.dev(), b.ino()),
        "copies must never share a writable inode"
    );
}

fn assert_fixture_contents(tree: &Path) {
    assert_eq!(contents(tree.join("hello.txt")), b"original\n");
    assert_eq!(contents(tree.join("nested/binary")), [0, 255, 0, 42, 128]);
    assert_eq!(contents(tree.join("run.sh")), b"#!/bin/sh\nexit 0\n");
    assert!(tree.join("empty").is_dir());
    assert_eq!(
        fs::metadata(tree.join("run.sh"))
            .expect("test fixture operation should succeed")
            .mode()
            & 0o777,
        0o751
    );
    assert_eq!(
        fs::metadata(tree.join("nested"))
            .expect("test fixture operation should succeed")
            .mode()
            & 0o777,
        0o750
    );
    assert_eq!(
        fs::metadata(tree)
            .expect("test fixture operation should succeed")
            .mode()
            & 0o777,
        0o750
    );
}

#[test]
fn forced_copy_preserves_bytes_permissions_empty_directories_and_independence() {
    let f = Fixture::initialized();
    f.write_source();
    fs::write(f.source.join("read_only"), b"read-only data")
        .expect("test fixture operation should succeed");
    fs::set_permissions(
        f.source.join("read_only"),
        fs::Permissions::from_mode(0o444),
    )
    .expect("test fixture operation should succeed");
    let output = f.import("main", true);
    assert!(output.contains("reflink=0 copy=4 symlink=0"), "{output}");
    let main = f.tree("workspaces", "main");
    assert_fixture_contents(&main);
    assert_eq!(contents(main.join("read_only")), b"read-only data");
    assert_eq!(
        fs::metadata(main.join("read_only"))
            .expect("test fixture operation should succeed")
            .mode()
            & 0o777,
        0o444
    );
    assert_distinct_files(f.source.join("hello.txt"), main.join("hello.txt"));
    fs::write(main.join("hello.txt"), b"workspace changed")
        .expect("test fixture operation should succeed");
    assert_eq!(contents(f.source.join("hello.txt")), b"original\n");
    fs::write(f.source.join("nested/binary"), b"source changed")
        .expect("test fixture operation should succeed");
    assert_eq!(contents(main.join("nested/binary")), [0, 255, 0, 42, 128]);
    assert_eq!(
        fs::read_dir(f.store.join("staging"))
            .expect("test fixture operation should succeed")
            .count(),
        0
    );
}

#[test]
fn default_backend_and_fork_checkpoint_restore_are_independent() {
    let f = Fixture::initialized();
    f.write_source();
    let report = f.import("main", false);
    assert!(report.starts_with("published: reflink="));
    let main = f.tree("workspaces", "main");
    assert_fixture_contents(&main);
    assert_distinct_files(f.source.join("hello.txt"), main.join("hello.txt"));
    f.ok(&["fork", "main", "branch"]);
    let branch = f.tree("workspaces", "branch");
    assert_fixture_contents(&branch);
    assert_distinct_files(main.join("hello.txt"), branch.join("hello.txt"));
    fs::write(branch.join("hello.txt"), b"branch checkpoint\n")
        .expect("test fixture operation should succeed");
    assert_eq!(contents(main.join("hello.txt")), b"original\n");
    f.ok(&["checkpoint", "branch", "saved"]);
    let saved = f.tree("snapshots", "saved");
    assert_distinct_files(branch.join("hello.txt"), saved.join("hello.txt"));
    fs::write(branch.join("hello.txt"), b"branch after checkpoint\n")
        .expect("test fixture operation should succeed");
    assert_eq!(contents(saved.join("hello.txt")), b"branch checkpoint\n");
    f.ok(&["restore", "saved", "restored"]);
    let restored = f.tree("workspaces", "restored");
    assert_distinct_files(saved.join("hello.txt"), restored.join("hello.txt"));
    assert_eq!(contents(restored.join("hello.txt")), b"branch checkpoint\n");
    fs::write(restored.join("hello.txt"), b"restored changed\n")
        .expect("test fixture operation should succeed");
    assert_eq!(contents(saved.join("hello.txt")), b"branch checkpoint\n");
    assert_eq!(
        contents(branch.join("hello.txt")),
        b"branch after checkpoint\n"
    );
    assert_eq!(contents(main.join("hello.txt")), b"original\n");
    assert_eq!(contents(f.source.join("hello.txt")), b"original\n");
    for tree in [&branch, &saved, &restored] {
        assert_eq!(
            fs::metadata(tree.join("run.sh"))
                .expect("test fixture operation should succeed")
                .mode()
                & 0o777,
            0o751
        );
    }
}

#[test]
fn source_hardlinks_become_independent_regular_files() {
    let f = Fixture::initialized();
    fs::write(f.source.join("first"), b"linked source")
        .expect("test fixture operation should succeed");
    fs::hard_link(f.source.join("first"), f.source.join("second"))
        .expect("test fixture operation should succeed");
    f.import("main", false);
    let tree = f.tree("workspaces", "main");
    assert_distinct_files(tree.join("first"), tree.join("second"));
    assert_distinct_files(f.source.join("first"), tree.join("first"));
    fs::write(tree.join("first"), b"only one copy changed")
        .expect("test fixture operation should succeed");
    assert_eq!(contents(tree.join("second")), b"linked source");
    assert_eq!(contents(f.source.join("first")), b"linked source");
}

#[test]
fn copies_preserve_ordinary_permissions_but_strip_special_mode_bits() {
    let f = Fixture::initialized();
    fs::write(
        f.source.join("data"),
        b"inert permission fixture; never executed",
    )
    .expect("test fixture operation should succeed");
    fs::create_dir(f.source.join("directory")).expect("test fixture operation should succeed");
    fs::set_permissions(f.source.join("data"), fs::Permissions::from_mode(0o6751))
        .expect("test fixture operation should succeed");
    fs::set_permissions(
        f.source.join("directory"),
        fs::Permissions::from_mode(0o3750),
    )
    .expect("test fixture operation should succeed");
    assert_eq!(
        fs::metadata(f.source.join("data"))
            .expect("test fixture operation should succeed")
            .mode()
            & 0o7777,
        0o6751
    );
    assert_eq!(
        fs::metadata(f.source.join("directory"))
            .expect("test fixture operation should succeed")
            .mode()
            & 0o7777,
        0o3750
    );
    for (name, plain) in [("copy", true), ("default", false)] {
        f.import(name, plain);
        let tree = f.tree("workspaces", name);
        assert_eq!(
            fs::metadata(tree.join("data"))
                .expect("test fixture operation should succeed")
                .mode()
                & 0o7777,
            0o751
        );
        assert_eq!(
            fs::metadata(tree.join("directory"))
                .expect("test fixture operation should succeed")
                .mode()
                & 0o7777,
            0o750
        );
    }
}

#[test]
fn symbolic_links_are_preserved_verbatim_and_never_traversed() {
    let f = Fixture::initialized();
    f.write_source();
    let outside = f.root.join("outside");
    fs::create_dir(&outside).expect("test fixture operation should succeed");
    fs::write(outside.join("private"), b"outside sentinel")
        .expect("test fixture operation should succeed");
    create_fifo(&outside.join("must-not-traverse.fifo"));
    let links = [
        ("relative", PathBuf::from("nested/binary")),
        ("dangling", PathBuf::from("missing")),
        ("directory", outside.clone()),
        ("external_file", outside.join("private")),
        ("parent", PathBuf::from("..")),
        ("loop", PathBuf::from("loop")),
        ("store_link", f.store.clone()),
    ];
    for (name, target) in &links {
        symlink(target, f.source.join(name)).expect("test fixture operation should succeed");
    }
    assert!(
        f.import("main", true)
            .contains("reflink=0 copy=3 symlink=7")
    );
    f.ok(&["fork", "main", "branch"]);
    f.ok(&["checkpoint", "main", "saved"]);
    f.ok(&["restore", "saved", "restored"]);
    let token = f.dropped_token("workspace", "branch");
    f.ok(&["recover", &token, "recovered"]);
    for (area, name) in [
        ("workspaces", "main"),
        ("snapshots", "saved"),
        ("workspaces", "restored"),
        ("workspaces", "recovered"),
    ] {
        let tree = f.tree(area, name);
        for (link, target) in &links {
            assert!(
                fs::symlink_metadata(tree.join(link))
                    .expect("test fixture operation should succeed")
                    .file_type()
                    .is_symlink()
            );
            assert_eq!(
                fs::read_link(tree.join(link)).expect("test fixture operation should succeed"),
                *target
            );
        }
    }
    assert_eq!(contents(outside.join("private")), b"outside sentinel");
}

#[test]
fn non_utf8_file_names_and_link_targets_are_preserved() {
    let f = Fixture::initialized();
    let file_name = OsString::from_vec(vec![b'f', 0xff]);
    let link_name = OsString::from_vec(vec![b'l', 0xfe]);
    fs::write(f.source.join(&file_name), b"non-UTF8 name")
        .expect("test fixture operation should succeed");
    symlink(&file_name, f.source.join(&link_name)).expect("test fixture operation should succeed");
    f.import("main", true);
    let tree = f.tree("workspaces", "main");
    assert_eq!(contents(tree.join(&file_name)), b"non-UTF8 name");
    assert_eq!(
        fs::read_link(tree.join(&link_name)).expect("test fixture operation should succeed"),
        PathBuf::from(file_name)
    );
}

#[test]
fn drop_and_recover_retain_trash_and_never_reuse_its_writable_inode() {
    let f = Fixture::initialized();
    f.write_source();
    f.import("main", true);
    f.ok(&["checkpoint", "main", "saved"]);
    for (kind, area, name, recovered) in [
        ("workspace", "workspaces", "main", "recovered_workspace"),
        ("snapshot", "snapshots", "saved", "recovered_snapshot"),
    ] {
        let old_inode = fs::metadata(f.tree(area, name).join("hello.txt"))
            .expect("test fixture operation should succeed")
            .ino();
        let token = f.dropped_token(kind, name);
        assert!(!f.store.join(area).join(name).exists());
        let trash = f.tree("trash", &token);
        assert_fixture_contents(&trash);
        assert_eq!(
            fs::metadata(trash.join("hello.txt"))
                .expect("test fixture operation should succeed")
                .ino(),
            old_inode
        );
        assert!(f.ok(&["show", "trash", &token]).contains("sprig-entry-v1"));
        f.ok(&["recover", &token, recovered]);
        let restored = f.tree("workspaces", recovered);
        assert_fixture_contents(&restored);
        assert_distinct_files(trash.join("hello.txt"), restored.join("hello.txt"));
        fs::write(restored.join("hello.txt"), b"recovered changed")
            .expect("test fixture operation should succeed");
        assert_eq!(contents(trash.join("hello.txt")), b"original\n");
        let again = format!("{recovered}_again");
        f.ok(&["recover", &token, &again]);
        assert_fixture_contents(&f.tree("workspaces", &again));
        assert!(f.ok(&["list"]).contains(&format!("trash\t{token}")));
        f.fail(&["drop", "trash", &token]);
        assert!(trash.exists());
    }
}

#[test]
fn existing_destinations_are_never_overwritten() {
    let f = Fixture::initialized();
    f.write_source();
    f.import("main", true);
    f.ok(&["fork", "main", "occupied"]);
    f.ok(&["checkpoint", "main", "saved"]);
    fs::write(
        f.tree("workspaces", "occupied").join("hello.txt"),
        b"keep workspace",
    )
    .expect("test fixture operation should succeed");
    fs::write(
        f.tree("snapshots", "saved").join("hello.txt"),
        b"keep snapshot",
    )
    .expect("test fixture operation should succeed");
    failure(f.run_os([
        OsStr::new("import"),
        f.source.as_os_str(),
        OsStr::new("occupied"),
    ]));
    f.fail(&["fork", "main", "occupied"]);
    f.fail(&["fork", "main", "main"]);
    f.fail(&["checkpoint", "main", "saved"]);
    f.fail(&["restore", "saved", "occupied"]);
    let token = f.dropped_token("workspace", "main");
    f.fail(&["recover", &token, "occupied"]);
    assert_eq!(
        contents(f.tree("workspaces", "occupied").join("hello.txt")),
        b"keep workspace"
    );
    assert_eq!(
        contents(f.tree("snapshots", "saved").join("hello.txt")),
        b"keep snapshot"
    );
    assert!(f.tree("trash", &token).exists());
    assert_eq!(
        fs::read_dir(f.store.join("staging"))
            .expect("test fixture operation should succeed")
            .count(),
        0
    );
}

#[test]
fn dangling_symlink_destinations_are_collisions_not_free_names() {
    let f = Fixture::initialized();
    fs::write(f.source.join("data"), b"safe").expect("test fixture operation should succeed");
    f.import("main", true);
    for (area, command) in [("workspaces", "fork"), ("snapshots", "checkpoint")] {
        let path = f.store.join(area).join("occupied");
        let outside = f.root.join(format!("must-not-create-{area}"));
        symlink(&outside, &path).expect("test fixture operation should succeed");
        f.fail(&[command, "main", "occupied"]);
        assert_eq!(
            fs::read_link(path).expect("test fixture operation should succeed"),
            outside
        );
        assert!(!outside.exists());
    }
    assert_eq!(
        fs::read_dir(f.store.join("staging"))
            .expect("test fixture operation should succeed")
            .count(),
        0
    );
}

#[test]
fn unsafe_names_are_rejected_in_every_source_and_destination_position() {
    let f = Fixture::initialized();
    fs::write(f.source.join("data"), b"safe").expect("test fixture operation should succeed");
    f.import("main", true);
    f.ok(&["checkpoint", "main", "saved"]);
    let token = f.dropped_token("workspace", "main");
    f.ok(&["recover", &token, "main"]);
    let escaped = f.root.join("escape").to_string_lossy().into_owned();
    let too_long = "a".repeat(97);
    let bad_names = [
        "",
        ".",
        "..",
        "../escape",
        "a/b",
        "a\\b",
        "a.b",
        "with space",
        "é",
        "a\nb",
        &escaped,
        &too_long,
    ];
    for bad in bad_names {
        failure(f.run_os([OsStr::new("import"), f.source.as_os_str(), OsStr::new(bad)]));
        for command in ["fork", "checkpoint"] {
            f.fail(&[command, "main", bad]);
            f.fail(&[command, bad, "new"]);
        }
        f.fail(&["restore", "saved", bad]);
        f.fail(&["restore", bad, "new"]);
        f.fail(&["recover", &token, bad]);
        f.fail(&["recover", bad, "new"]);
        for kind in ["workspace", "snapshot"] {
            f.fail(&["drop", kind, bad]);
            f.fail(&["show", kind, bad]);
        }
        f.fail(&["show", "trash", bad]);
    }
    assert!(!f.root.join("escape").exists());
    assert!(!f.store.join("workspaces/new").exists());
    assert!(!f.store.join("snapshots/new").exists());
    assert_eq!(
        fs::read_dir(f.store.join("staging"))
            .expect("test fixture operation should succeed")
            .count(),
        0
    );
    assert_eq!(contents(f.tree("workspaces", "main").join("data")), b"safe");
}

#[test]
fn valid_name_boundaries_and_separate_namespaces_work() {
    let f = Fixture::initialized();
    let longest = "a".repeat(96);
    f.import(&longest, true);
    f.ok(&["fork", &longest, "AZaz09_-job"]);
    f.ok(&["checkpoint", "AZaz09_-job", "AZaz09_-job"]);
    assert!(f.tree("workspaces", "AZaz09_-job").is_dir());
    assert!(f.tree("snapshots", "AZaz09_-job").is_dir());
}

#[test]
fn non_utf8_entry_names_are_rejected_without_publication() {
    let f = Fixture::initialized();
    let invalid = OsString::from_vec(vec![b'n', 0xff]);
    failure(f.run_os([OsStr::new("import"), f.source.as_os_str(), &invalid]));
    assert_eq!(f.ok(&["list"]), "");
    assert_eq!(
        fs::read_dir(f.store.join("staging"))
            .expect("test fixture operation should succeed")
            .count(),
        0
    );
}

#[test]
fn store_source_overlap_is_rejected_even_through_canonical_aliases() {
    let f = Fixture::initialized();
    fs::create_dir(f.store.join("nested-source")).expect("test fixture operation should succeed");
    let alias = f.root.join("alias");
    symlink(&f.root, &alias).expect("test fixture operation should succeed");
    for source in [
        f.store.clone(),
        f.root.clone(),
        f.store.join("nested-source"),
        alias.join("store/nested-source"),
        f.source.join(".."),
    ] {
        let error =
            failure(f.run_os([OsStr::new("import"), source.as_os_str(), OsStr::new("bad")]));
        assert!(error.contains("contain one another"), "{source:?}: {error}");
    }
    assert_eq!(f.ok(&["list"]), "");
    assert_eq!(
        fs::read_dir(f.store.join("staging"))
            .expect("test fixture operation should succeed")
            .count(),
        0
    );
}

#[test]
fn import_requires_a_real_directory_not_a_root_symlink_or_regular_file() {
    let f = Fixture::initialized();
    let link = f.root.join("source-link");
    let file = f.root.join("file");
    symlink(&f.source, &link).expect("test fixture operation should succeed");
    fs::write(&file, b"not a directory").expect("test fixture operation should succeed");
    let trailing_slash = PathBuf::from(format!("{}/", link.display()));
    let trailing_dot = link.join(".");
    let repeated_slash_dot = PathBuf::from(format!("{}//./", link.display()));
    for source in [
        link,
        trailing_slash,
        trailing_dot,
        repeated_slash_dot,
        file,
        f.root.join("missing"),
    ] {
        failure(f.run_os([OsStr::new("import"), source.as_os_str(), OsStr::new("bad")]));
    }
    assert_eq!(f.ok(&["list"]), "");
}

#[test]
fn special_file_failure_leaves_only_unpublished_staging() {
    let f = Fixture::initialized();
    fs::write(f.source.join("normal"), b"normal file")
        .expect("test fixture operation should succeed");
    fs::create_dir(f.source.join("nested")).expect("test fixture operation should succeed");
    create_fifo(&f.source.join("nested/fifo"));
    let error = failure(f.run_os([
        OsStr::new("import"),
        f.source.as_os_str(),
        OsStr::new("failed"),
    ]));
    assert!(error.contains("special file rejected"), "{error}");
    assert!(!f.store.join("workspaces/failed").exists());
    assert_eq!(f.ok(&["list"]), "");
    let staged: Vec<_> = fs::read_dir(f.store.join("staging"))
        .expect("test fixture operation should succeed")
        .map(|entry| entry.expect("test fixture operation should succeed").path())
        .collect();
    assert_eq!(staged.len(), 1);
    assert!(staged[0].join("tree").is_dir());
    assert!(!staged[0].join("record").exists());
    assert!(f.ok(&["status"]).contains("staging entries: 1"));
    f.fail(&["fork", "failed", "forked"]);
    f.fail(&["checkpoint", "failed", "saved"]);
    f.fail(&["restore", "failed", "restored"]);
    assert_eq!(contents(f.source.join("normal")), b"normal file");
    fs::remove_file(f.source.join("nested/fifo")).expect("test fixture operation should succeed");
    f.import("failed", true);
    assert_eq!(
        contents(f.tree("workspaces", "failed").join("normal")),
        b"normal file"
    );
    assert!(
        staged[0].exists(),
        "retry must not silently discard diagnostic staging"
    );
}

#[test]
fn special_files_in_existing_entries_cannot_publish_forks_or_restores() {
    let f = Fixture::initialized();
    fs::write(f.source.join("data"), b"original").expect("test fixture operation should succeed");
    f.import("main", true);
    f.ok(&["checkpoint", "main", "saved"]);
    f.ok(&["fork", "main", "dropped"]);
    let token = f.dropped_token("workspace", "dropped");
    for (area, name) in [
        ("workspaces", "main"),
        ("snapshots", "saved"),
        ("trash", token.as_str()),
    ] {
        create_fifo(&f.tree(area, name).join("special"));
    }
    for (command, from, to, area) in [
        ("fork", "main", "failed_fork", "workspaces"),
        ("checkpoint", "main", "failed_checkpoint", "snapshots"),
        ("restore", "saved", "failed_restore", "workspaces"),
        ("recover", token.as_str(), "failed_recover", "workspaces"),
    ] {
        let error = f.fail(&[command, from, to]);
        assert!(error.contains("special file rejected"), "{error}");
        assert!(!f.store.join(area).join(to).exists());
    }
    assert_eq!(
        fs::read_dir(f.store.join("staging"))
            .expect("test fixture operation should succeed")
            .count(),
        4
    );
    assert!(!f.ok(&["list"]).contains("failed_"));
    for (area, name) in [
        ("workspaces", "main"),
        ("snapshots", "saved"),
        ("trash", token.as_str()),
    ] {
        assert_eq!(contents(f.tree(area, name).join("data")), b"original");
    }
}

#[test]
fn staging_entries_are_never_listed_or_used_as_published_sources() {
    let f = Fixture::initialized();
    let stage = f.store.join("staging/interrupted");
    fs::create_dir_all(stage.join("tree")).expect("test fixture operation should succeed");
    fs::write(stage.join("tree/partial"), b"unfinished")
        .expect("test fixture operation should succeed");
    fs::write(
        stage.join("record"),
        b"sprig-entry-v1\ncloned=0\ncopied=1\nsymlinks=0\n",
    )
    .expect("test fixture operation should succeed");
    assert_eq!(f.ok(&["list"]), "");
    assert!(f.ok(&["status"]).contains("staging entries: 1"));
    f.fail(&["fork", "interrupted", "new"]);
    f.fail(&["restore", "interrupted", "new"]);
    f.fail(&["recover", "interrupted", "new"]);
    f.fail(&["show", "workspace", "interrupted"]);
    assert_eq!(contents(stage.join("tree/partial")), b"unfinished");
}

#[test]
fn init_refuses_existing_directories_files_symlinks_and_stores() {
    for kind in ["directory", "file", "symlink"] {
        let f = Fixture::new();
        match kind {
            "directory" => {
                fs::create_dir(&f.store).expect("test fixture operation should succeed");
                fs::write(f.store.join("sentinel"), b"do not adopt")
                    .expect("test fixture operation should succeed");
            }
            "file" => fs::write(&f.store, b"do not replace")
                .expect("test fixture operation should succeed"),
            _ => symlink(f.root.join("missing"), &f.store)
                .expect("test fixture operation should succeed"),
        }
        f.fail(&["init"]);
        match kind {
            "directory" => {
                assert_eq!(contents(f.store.join("sentinel")), b"do not adopt");
                assert!(!f.store.join("format").exists());
            }
            "file" => assert_eq!(contents(&f.store), b"do not replace"),
            _ => assert_eq!(
                fs::read_link(&f.store).expect("test fixture operation should succeed"),
                f.root.join("missing")
            ),
        }
    }
    let f = Fixture::initialized();
    f.import("main", true);
    f.fail(&["init"]);
    assert_eq!(f.ok(&["list"]), "workspaces\tmain\n");
}

#[test]
fn invalid_store_layout_and_format_are_rejected() {
    for component in [
        "format",
        "lock",
        "workspaces",
        "snapshots",
        "trash",
        "staging",
    ] {
        let f = Fixture::initialized();
        let path = f.store.join(component);
        if path.is_dir() {
            fs::remove_dir(&path).expect("test fixture operation should succeed");
            fs::write(&path, b"not a directory").expect("test fixture operation should succeed");
        } else {
            fs::remove_file(&path).expect("test fixture operation should succeed");
            fs::create_dir(&path).expect("test fixture operation should succeed");
        }
        f.fail(&["list"]);
    }
    for bad_format in [
        b"".as_slice(),
        b"sprig-store-v2\n",
        b"sprig-store-v1\nextra\n",
        &[0xff],
    ] {
        let f = Fixture::initialized();
        fs::write(f.store.join("format"), bad_format)
            .expect("test fixture operation should succeed");
        f.fail(&["status"]);
        assert_eq!(
            fs::read_dir(f.store.join("staging"))
                .expect("test fixture operation should succeed")
                .count(),
            0
        );
    }
}

#[test]
fn store_components_and_root_cannot_be_symlinks() {
    for component in [
        "format",
        "lock",
        "workspaces",
        "snapshots",
        "trash",
        "staging",
    ] {
        let f = Fixture::initialized();
        let original = f.store.join(component);
        let moved = f.root.join(format!("outside-{component}"));
        fs::rename(&original, &moved).expect("test fixture operation should succeed");
        symlink(&moved, &original).expect("test fixture operation should succeed");
        f.fail(&["list"]);
        assert!(
            fs::symlink_metadata(original)
                .expect("test fixture operation should succeed")
                .file_type()
                .is_symlink()
        );
    }
    let f = Fixture::initialized();
    let moved = f.root.join("real-store");
    fs::rename(&f.store, &moved).expect("test fixture operation should succeed");
    symlink(&moved, &f.store).expect("test fixture operation should succeed");
    f.fail(&["list"]);
    for suffix in ["/", "/.", "//./"] {
        let mut command = Command::new(env!("CARGO_BIN_EXE_sprig"));
        command
            .arg("--store")
            .arg(format!("{}{suffix}", f.store.display()))
            .arg("list");
        failure(bounded_output(command));
    }
}

#[test]
fn malformed_entry_metadata_is_rejected_before_read_or_mutation() {
    for bad_record in [
        b"".as_slice(),
        b"sprig-entry-v2\ncloned=0\ncopied=1\nsymlinks=0\n",
        b"sprig-entry-v1\n",
        b"sprig-entry-v1\ncloned=no\ncopied=1\nsymlinks=0\n",
        b"sprig-entry-v1\ncloned=-1\ncopied=1\nsymlinks=0\n",
        b"sprig-entry-v1\ncloned=0\ncopied=18446744073709551616\nsymlinks=0\n",
        b"sprig-entry-v1\ncloned=0\ncopied=1\n",
        b"sprig-entry-v1\ncloned=0\ncloned=1\nsymlinks=0\n",
        b"sprig-entry-v1\ncloned=0\ncopied=1\nsymlinks=0",
        b"sprig-entry-v1\ncloned=0\ncopied=1\nsymlinks=0\nextra=1\n",
        &[0xff],
    ] {
        let f = Fixture::initialized();
        fs::write(f.source.join("data"), b"original")
            .expect("test fixture operation should succeed");
        f.import("main", true);
        fs::write(f.store.join("workspaces/main/record"), bad_record)
            .expect("test fixture operation should succeed");
        f.fail(&["show", "workspace", "main"]);
        f.fail(&["list"]);
        f.fail(&["fork", "main", "new"]);
        f.fail(&["checkpoint", "main", "saved"]);
        f.fail(&["drop", "workspace", "main"]);
        assert_eq!(
            contents(f.tree("workspaces", "main").join("data")),
            b"original"
        );
        assert!(!f.store.join("workspaces/new").exists());
        assert!(!f.store.join("snapshots/saved").exists());
        assert_eq!(
            fs::read_dir(f.store.join("trash"))
                .expect("test fixture operation should succeed")
                .count(),
            0
        );
        assert_eq!(
            fs::read_dir(f.store.join("staging"))
                .expect("test fixture operation should succeed")
                .count(),
            0
        );
    }
}

#[test]
fn missing_or_symlinked_entry_components_cannot_escape_validation() {
    for component in ["entry", "tree", "record"] {
        for replacement in ["missing", "symlink"] {
            let f = Fixture::initialized();
            fs::write(f.source.join("data"), b"original")
                .expect("test fixture operation should succeed");
            f.import("main", true);
            let original = match component {
                "entry" => f.store.join("workspaces/main"),
                "tree" => f.tree("workspaces", "main"),
                _ => f.store.join("workspaces/main/record"),
            };
            let moved = f.root.join("outside-entry-component");
            fs::rename(&original, &moved).expect("test fixture operation should succeed");
            if replacement == "symlink" {
                symlink(&moved, &original).expect("test fixture operation should succeed");
            }
            f.fail(&["show", "workspace", "main"]);
            f.fail(&["fork", "main", "new"]);
            f.fail(&["drop", "workspace", "main"]);
            assert!(moved.exists());
            assert!(!f.store.join("workspaces/new").exists());
            assert_eq!(
                fs::read_dir(f.store.join("trash"))
                    .expect("test fixture operation should succeed")
                    .count(),
                0
            );
        }
    }
}

#[test]
fn invalid_snapshot_and_trash_records_cannot_be_restored_or_recovered() {
    let f = Fixture::initialized();
    fs::write(f.source.join("data"), b"original").expect("test fixture operation should succeed");
    f.import("main", true);
    f.ok(&["checkpoint", "main", "saved"]);
    let token = f.dropped_token("workspace", "main");
    for (area, name, command) in [
        ("snapshots", "saved", "restore"),
        ("trash", token.as_str(), "recover"),
    ] {
        fs::write(f.store.join(area).join(name).join("record"), b"invalid\n")
            .expect("test fixture operation should succeed");
        f.fail(&[command, name, "new"]);
        assert_eq!(contents(f.tree(area, name).join("data")), b"original");
        assert!(!f.store.join("workspaces/new").exists());
    }
}

#[test]
fn operating_system_store_lock_blocks_readers_and_mutators_then_releases() {
    let f = Fixture::initialized();
    fs::write(f.source.join("data"), b"original").expect("test fixture operation should succeed");
    f.import("main", true);
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .open(f.store.join("lock"))
        .expect("test fixture operation should succeed");
    lock.try_lock()
        .expect("test fixture operation should succeed");
    for args in [
        vec!["list"],
        vec!["status"],
        vec!["show", "workspace", "main"],
        vec!["fork", "main", "new"],
        vec!["drop", "workspace", "main"],
    ] {
        let error = f.fail(&args);
        assert!(error.contains("locked"), "{error}");
    }
    assert_eq!(
        contents(f.tree("workspaces", "main").join("data")),
        b"original"
    );
    assert_eq!(
        fs::read_dir(f.store.join("staging"))
            .expect("test fixture operation should succeed")
            .count(),
        0
    );
    assert_eq!(
        fs::read_dir(f.store.join("trash"))
            .expect("test fixture operation should succeed")
            .count(),
        0
    );
    drop(lock);
    f.ok(&["fork", "main", "new"]);
    // A completed CLI process must not leave a stale userspace lock behind.
    File::open(f.store.join("lock"))
        .expect("test fixture operation should succeed")
        .try_lock()
        .expect("test fixture operation should succeed");
}

#[test]
fn list_status_and_show_report_actual_entries_and_copy_counts() {
    let f = Fixture::initialized();
    f.write_source();
    symlink("hello.txt", f.source.join("link")).expect("test fixture operation should succeed");
    f.import("zeta", true);
    f.ok(&["--copy", "fork", "zeta", "alpha"]);
    f.ok(&["--copy", "checkpoint", "alpha", "saved"]);
    assert_eq!(
        f.ok(&["list"]),
        "workspaces\talpha\nworkspaces\tzeta\nsnapshots\tsaved\n"
    );
    let shown = f.ok(&["show", "snapshot", "saved"]);
    assert_eq!(
        shown
            .lines()
            .next()
            .expect("test fixture operation should succeed"),
        f.tree("snapshots", "saved")
            .to_str()
            .expect("test fixture operation should succeed")
    );
    assert!(
        shown.contains("sprig-entry-v1\ncloned=0\ncopied=3\nsymlinks=1\n"),
        "{shown}"
    );
    let status = f.ok(&["status"]);
    assert!(status.contains("FICLONE"));
    assert!(status.contains("staging entries: 0"));
    assert!(status.contains("external writers"));
    assert!(status.contains("snapshots\tsaved"));
}

#[test]
fn command_grammar_errors_do_not_mutate_store() {
    let f = Fixture::initialized();
    for args in [
        vec![],
        vec!["--copy"],
        vec!["unknown"],
        vec!["init", "extra"],
        vec!["import"],
        vec!["fork", "one"],
        vec!["restore", "one"],
        vec!["checkpoint", "one", "two", "three"],
        vec!["list", "extra"],
        vec!["status", "extra"],
        vec!["show", "unknown", "one"],
        vec!["drop", "trash", "one"],
        vec!["recover", "one"],
    ] {
        f.fail(&args);
    }
    assert_eq!(f.ok(&["list"]), "");
    assert_eq!(
        fs::read_dir(f.store.join("staging"))
            .expect("test fixture operation should succeed")
            .count(),
        0
    );
    for args in [vec![], vec!["--help"], vec!["-h"]] {
        let mut command = Command::new(env!("CARGO_BIN_EXE_sprig"));
        command.args(args);
        assert!(success(bounded_output(command)).contains("Usage: sprig"));
    }
    let mut command = Command::new(env!("CARGO_BIN_EXE_sprig"));
    command.arg("--version");
    assert_eq!(
        success(bounded_output(command)).trim(),
        format!("sprig {}", env!("CARGO_PKG_VERSION"))
    );
}
