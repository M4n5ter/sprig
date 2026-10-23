//! One portable end-to-end scenario for owned journals and the shared Store.
use sprig::{Mutation, Store};
use std::fs;

#[test]
fn owned_workspaces_share_publication_merge_and_recovery() {
    let root = std::env::temp_dir().join(format!(
        "sprig-owned-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::create_dir(&root).unwrap();
    let source = root.join("source");
    fs::create_dir(&source).unwrap();
    fs::write(source.join("text"), b"one\ntwo\nthree\n").unwrap();
    let store_path = root.join("store");
    Store::init(&store_path).unwrap();
    let store = Store::open(&store_path).unwrap();
    let initial = store.import_writable(&source, "main").unwrap();
    let a = store.checkout_writable("main", "a").unwrap();
    let b = store.checkout_writable("main", "b").unwrap();
    assert_eq!(a.base_revision, b.base_revision);
    let first = store
        .apply(
            "a",
            &[Mutation::Edit {
                path: "text".into(),
                old: "one".into(),
                new: "ONE".into(),
            }],
            "first",
            Some(&initial.base_revision),
        )
        .unwrap();
    let merged = store
        .apply(
            "b",
            &[Mutation::Edit {
                path: "text".into(),
                old: "three".into(),
                new: "THREE".into(),
            }],
            "second",
            None,
        )
        .unwrap();
    assert_ne!(first.revision, merged.revision);
    assert_eq!(store.read("main", "text").unwrap(), b"ONE\ntwo\nTHREE\n");
    assert!(
        store
            .apply(
                "b",
                &[Mutation::Write {
                    path: "bad".into(),
                    content: b"bad".to_vec()
                }],
                "stale",
                Some(&initial.base_revision)
            )
            .is_err()
    );
    assert!(store.stat("main", "bad").is_err());
    let c = store.checkout_writable("main", "c").unwrap();
    store
        .apply(
            "b",
            &[Mutation::Edit {
                path: "text".into(),
                old: "two".into(),
                new: "left".into(),
            }],
            "left",
            None,
        )
        .unwrap();
    assert!(
        store
            .apply(
                "c",
                &[Mutation::Edit {
                    path: "text".into(),
                    old: "two".into(),
                    new: "right".into()
                }],
                "right",
                None
            )
            .is_err()
    );
    let conflict = store.show("c").unwrap();
    assert!(!conflict.pending_conflicts.is_empty());
    assert!(store.checkpoint("c", "unresolved", None).is_err());
    let fixed = store
        .apply(
            "c",
            &[Mutation::Write {
                path: "text".into(),
                content: b"ONE\nresolved\nTHREE\n".to_vec(),
            }],
            "resolve bytes",
            None,
        )
        .unwrap();
    assert!(!fixed.committed);
    store.resolve_conflicts("c", &["text".into()]).unwrap();
    let resolved = store.checkpoint("c", "resolve", None).unwrap();
    assert_eq!(
        store.read(&resolved.revision, "text").unwrap(),
        b"ONE\nresolved\nTHREE\n"
    );
    let no_op = store.checkpoint("c", "no change", None).unwrap();
    assert_eq!(no_op.revision, resolved.revision);
    let restored = store
        .restore_writable(&c.base_revision, "restored")
        .unwrap();
    assert_eq!(
        store.read(&restored.base_revision, "text").unwrap(),
        b"ONE\ntwo\nTHREE\n"
    );
    assert!(store.doctor().unwrap().healthy);
    assert!(store.gc().unwrap().dry_run);
    let status = store.writable_status("c").unwrap();
    assert!(!status.read_only && status.dirty_paths.is_empty());
    // Invalid transactions never replace the active generation or head.
    let before = store.show("c").unwrap();
    assert!(
        store
            .apply(
                "c",
                &[
                    Mutation::Write {
                        path: "temporary".into(),
                        content: b"not published".to_vec()
                    },
                    Mutation::Move {
                        from: "temporary".into(),
                        to: "text".into()
                    },
                ],
                "invalid batch",
                None
            )
            .is_err()
    );
    assert_eq!(store.show("c").unwrap().path, before.path);
    assert_eq!(store.show("c").unwrap().base_revision, before.base_revision);
    drop(store);
    let reopened = Store::open(&store_path).unwrap();
    assert_eq!(
        reopened.read("main", "text").unwrap(),
        b"ONE\nresolved\nTHREE\n"
    );
    assert!(reopened.doctor().unwrap().healthy);
    let dropped = reopened.drop_entry("workspace", "restored").unwrap();
    let recovered = reopened
        .recover(&dropped.token, "recovered", sprig::CopyMode::Auto)
        .unwrap();
    assert!(matches!(
        recovered.backend,
        sprig::WorkspaceBackend::Writable { .. }
    ));
    // Ordinary checkpoint cannot silently commit a truncated journal prefix.
    let view = reopened.show("c").unwrap();
    let journal = view.path.parent().unwrap().join("writable.journal");
    {
        use std::io::Write;
        fs::OpenOptions::new()
            .append(true)
            .open(&journal)
            .unwrap()
            .write_all(b"SPR")
            .unwrap();
    }
    let retained = fs::read(&journal).unwrap();
    assert!(reopened.writable_status("c").unwrap().incomplete_tail);
    assert!(
        reopened
            .checkpoint("c", "must refuse prefix", None)
            .is_err()
    );
    assert_eq!(
        reopened.show("c").unwrap().base_revision,
        view.base_revision
    );
    let prefix = reopened.recover_writable("c", "prefix").unwrap();
    assert_eq!(fs::read(&journal).unwrap(), retained);
    let prefix_commit = reopened.checkpoint("prefix", "recovered", None).unwrap();
    assert_eq!(
        reopened.read(&prefix_commit.revision, "text").unwrap(),
        b"ONE\nresolved\nTHREE\n"
    );
    assert_ne!(prefix.path, view.path);
    // Both portable owned mutations and native materialization preserve aliases
    // across publication while forks never share mutable host inodes.
    for owned in [false, true] {
        let name = if owned { "links-owned" } else { "links-native" };
        if owned {
            reopened.import_writable(&source, name).unwrap();
        } else {
            reopened
                .import(&source, name, sprig::CopyMode::Copy)
                .unwrap();
        }
        let linked = reopened
            .apply(
                name,
                &[Mutation::Link {
                    from: "text".into(),
                    to: "alias".into(),
                }],
                "link",
                None,
            )
            .unwrap();
        let a = reopened.stat(&linked.revision, "text").unwrap();
        let b = reopened.stat(&linked.revision, "alias").unwrap();
        assert_ne!(a.id, b.id);
        assert!(a.link_group.is_some());
        assert_eq!(a.link_group, b.link_group);
        reopened
            .apply(
                name,
                &[Mutation::Write {
                    path: "alias".into(),
                    content: b"linked content".to_vec(),
                }],
                "write alias",
                None,
            )
            .unwrap();
        assert_eq!(reopened.read(name, "text").unwrap(), b"linked content");
        let fork = format!("{name}-fork");
        reopened.fork(name, &fork, sprig::CopyMode::Auto).unwrap();
        reopened
            .apply(
                &fork,
                &[Mutation::Write {
                    path: "text".into(),
                    content: b"fork only".to_vec(),
                }],
                "isolated",
                None,
            )
            .unwrap();
        assert_eq!(reopened.read(&fork, "alias").unwrap(), b"fork only");
        assert_eq!(reopened.read(name, "alias").unwrap(), b"linked content");
        let restored = format!("{name}-restored");
        let view = reopened.restore(&linked.revision, &restored).unwrap();
        fs::write(view.path.join("alias"), b"restored alias").unwrap();
        assert_eq!(fs::read(view.path.join("text")).unwrap(), b"restored alias");
        assert_eq!(
            reopened.read(&linked.revision, "text").unwrap(),
            b"one\ntwo\nthree\n"
        );
        reopened
            .apply(
                name,
                &[
                    Mutation::Mkdir { path: "dir".into() },
                    Mutation::Copy {
                        from: "text".into(),
                        to: "dir/first".into(),
                    },
                    Mutation::Link {
                        from: "dir/first".into(),
                        to: "dir/second".into(),
                    },
                    Mutation::Copy {
                        from: "dir".into(),
                        to: "copied".into(),
                    },
                    Mutation::Write {
                        path: "copied/first".into(),
                        content: b"copy only".to_vec(),
                    },
                ],
                "independent copied link group",
                None,
            )
            .unwrap();
        assert_eq!(reopened.read(name, "copied/second").unwrap(), b"copy only");
        assert_eq!(reopened.read(name, "dir/first").unwrap(), b"linked content");
        let before = reopened.show(name).unwrap().base_revision;
        assert!(
            reopened
                .apply(
                    name,
                    &[Mutation::Link {
                        from: "".into(),
                        to: "bad-dir-link".into()
                    }],
                    "reject",
                    None
                )
                .is_err()
        );
        assert_eq!(reopened.show(name).unwrap().base_revision, before);
    }
    let report = reopened.doctor().unwrap();
    assert_eq!(
        report.errors,
        ["workspace c: incomplete writable journal; explicit prefix recovery required"]
    );
    assert!(!report.healthy);
    for (name, marker) in [
        ("partial-session", "writable-session.json"),
        ("stray-session", "writable-clean.json"),
    ] {
        let abandoned = reopened.import_writable(&source, name).unwrap();
        let marker = abandoned.path.parent().unwrap().join(marker);
        fs::write(&marker, b"{incomplete").unwrap();
        assert!(reopened.writable_status(name).unwrap().read_only);
        assert!(reopened.checkpoint(name, "must refuse", None).is_err());
        assert!(
            reopened
                .apply(
                    name,
                    &[Mutation::Write {
                        path: "text".into(),
                        content: b"must not write".to_vec()
                    }],
                    "must refuse",
                    None
                )
                .is_err()
        );
        let recovered = reopened
            .recover_writable(name, &format!("{name}-recovered"))
            .unwrap();
        assert_eq!(
            reopened.read(&recovered.base_revision, "text").unwrap(),
            b"one\ntwo\nthree\n"
        );
        assert_eq!(fs::read(&marker).unwrap(), b"{incomplete");
        assert_eq!(
            reopened.show(name).unwrap().base_revision,
            abandoned.base_revision
        );
    }
    assert_eq!(reopened.doctor().unwrap().errors.len(), 3);
    assert!(reopened.gc().unwrap().dry_run);
    drop(reopened);
    fs::remove_dir_all(root).unwrap();
}
