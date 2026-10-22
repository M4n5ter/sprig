# Sprig

Small branches of a directory, using the filesystem you already have.

Sprig is a **Linux-first workspace snapshot CLI** for independent working copies
and named checkpoints. It uses Linux `FICLONE` to share
copy-on-write file extents when supported, and falls back to a normal byte copy
otherwise. There are no hardlinks between workspaces, database, daemon, FUSE
mount, async runtime, or Rust dependencies.

## Build

Use the current **stable Rust** toolchain and Linux x86-64 or AArch64.
The repository selects stable through `rust-toolchain.toml`, including rustfmt
and Clippy. Cargo's `rust-version = "1.89"` records the minimum language/API
requirement, not a pinned toolchain; CI runs stable.

```sh
cargo build --release
export PATH="$PWD/target/release:$PATH"
sprig --help
```

macOS/APFS and Windows backends are not implemented in this version. A filesystem
without reflink support works through the explicit ordinary-copy fallback.

## Quick start

Choose a store path that **does not exist** and is outside your source tree.
Stop programs writing the source/workspace before copying or checkpointing.

```sh
sprig --store "$HOME/sprig-store" init
sprig --store "$HOME/sprig-store" import ./my-project main
sprig --store "$HOME/sprig-store" fork main experiment
sprig --store "$HOME/sprig-store" show workspace experiment
# Edit files under the printed tree path with your usual tools.
sprig --store "$HOME/sprig-store" checkpoint experiment before-refactor
sprig --store "$HOME/sprig-store" restore before-refactor restored
sprig --store "$HOME/sprig-store" list
sprig --store "$HOME/sprig-store" status
```

`restore` always creates a **new** workspace. It never replaces an existing one.
`checkpoint` creates a named snapshot; Sprig has no command to edit one.
Snapshots are immutable **through the tool**, not protected against direct disk
writes by their owner. Never work directly in a snapshot tree.

To bypass reflink attempts, put `--copy` before the command:

```sh
sprig --store "$HOME/sprig-store" --copy fork main portable-copy
```

Every successful copy reports counts of reflinked files, ordinarily copied
files, and symbolic links. `show` includes these historical creation counts;
`status` reports backend policy, not a promise that every file supports reflink.
Reflinks save initial disk space but still require a tree walk and metadata work.
Large numbers of small files can therefore remain expensive.

## Drop and recover

```sh
sprig --store "$HOME/sprig-store" drop workspace experiment
# Copy the exact token printed by drop into the command below:
sprig --store "$HOME/sprig-store" recover TRASH_TOKEN recovered-experiment
```

`drop snapshot NAME` is also supported. Drop renames the entire entry into trash;
there is **no permanent purge command**. Recovery makes a new workspace and keeps
the trash entry. `list` includes trash tokens. Names are 1–96 ASCII letters,
digits, hyphens or underscores. Existing names, including dangling symlinks, are
never overwritten.

## Safety model and limitations

- **Quiesce all external writers.** A store-wide OS advisory lock serializes
  Sprig commands and is released automatically if the process exits. It does not
  freeze editors, databases, build tools, or other applications. A reflink is
  per-file, not an atomic whole-directory snapshot. Concurrent external mutation
  can produce inconsistent content or race the path walk. This is not a tool for
  snapshotting hostile live trees or live databases.
- The store is private to one trusted operating-system user. Do not change its
  metadata, lock file, entry layout, or parent paths while Sprig is running. This
  is not a security boundary against other processes with the same permissions.
- Symlinks are copied as symlinks and are **never traversed during copying**.
  Absolute links, dangling links, and links outside the workspace retain their
  original text. Opening these links later in your editor or shell may access
  files outside the workspace; a fork is not a sandbox.
- Only directories, regular files, and symbolic links are accepted. Sockets,
  FIFOs and device files fail the operation. Store/source containment and source
  root symlinks are rejected. Managed entry roots and metadata symlinks are
  rejected. Hardlinked source files become independent files in the copy.
- Ordinary Unix permission bits (including executable bits) are preserved;
  ownership, timestamps, ACLs, extended attributes, sparse layout and hardlink
  relationships are **not guaranteed**. Set-user-ID/set-group-ID bits are stripped.
  Reflink failure falls back to copying, including when source and store reside on
  different filesystems. Copy fallback may require the full logical file size.
- An entry is copied to `staging`, files and directory metadata are synced, then
  the complete entry directory is renamed into its published namespace. Failed
  or interrupted copies are not listed as usable entries and stay in staging for
  manual inspection. No user content is automatically purged on failure.
- Publication and drop use atomic rename within the store. This is not a
  multi-operation transaction or a promise against every filesystem, hardware,
  kernel or power-loss failure. A failure syncing directories *after* rename can
  mean the operation took effect despite an error; inspect `list` before retrying.
  A crash can leave staging entries; `status` reports their count. There is no
  automatic repair or stale-data cleanup. Keep independent backups.
- Import copies, never moves, the source. Restore never overwrites. The source
  itself and any data reached by its symlinks are not modified by Sprig.

## Layout

```text
STORE/
  format
  lock
  workspaces/NAME/{record,tree/}
  snapshots/NAME/{record,tree/}
  trash/TOKEN/{record,tree/}
  staging/UNPUBLISHED/{record,tree/}
```

The tiny line-oriented records identify the format and record creation backend
counts. Directory names are the index, so no separate database can drift out of
sync. An entry's `tree` directory is the only place to edit workspace content.

## Development

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```

Code follows the [Google Rust Style Guide](https://google.github.io/styleguide/rustguide.html)
in context: idiomatic errors, small focused modules, clear naming, rustfmt,
minimal abstraction, and one documented unsafe Linux ioctl call in production code. Chromium- and
Fuchsia-specific infrastructure is not adopted. CI runs the Linux checks above.
Tests exercise copy/fork independence, symlink handling, recoverability, failure
publication, locks and path validation. Filesystem reflink availability is not
assumed by the tests.

This first version deliberately does not implement diffs, merges, mountable
layers, background synchronization, quota management or distributed storage.
No project license has been selected yet.
