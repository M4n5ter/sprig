# Regular-file read metadata TTL experiment

This isolated candidate starts from Sprig
`4eb0f4d7afdb19bf22d9667dc520ede8b162c737`. It does not include the
dirty-journal-sync or native restore candidates. All existing write, flush,
fsync, snapshot and session durability barriers are unchanged.

## Exact scope

Only successful **LOOKUP and GETATTR of regular files** return a fixed 60-second
TTL. With the pinned fuser 0.16.0 API, LOOKUP's `ReplyEntry::entry` uses the same
TTL for the positive dentry and its attributes; GETATTR caches attributes only.

CREATE, SETATTR, LINK, MKDIR and SYMLINK replies still use zero TTL, even when a
mutation returns regular-file attributes. Directory and symlink LOOKUP/GETATTR
remain zero. Missing names still return an error, not a cacheable negative-entry
reply. No notification mechanism, dependency, mount option or I/O flag changes.

The comparison target, LayerFS
`caebd268dc376865e964f33f78047f7918763190`, uses 60 seconds for positive entries
and attributes and for negative entries in
[`request/reply.rs`](https://github.com/Ephemeral-AI-Lab/layerfs/blob/caebd268dc376865e964f33f78047f7918763190/engine/crates/layerfs-fuse/src/request/reply.rs).
This experiment deliberately does **not** copy its whole cache policy.

## Source-based coherence rationale, not runtime proof

* `src/writable.rs` holds an exclusive `writable.lease` for the live generation;
  inode identities are not reused during a mounted session. Supported writes by
  other processes go through the same FUSE connection. There is no supported
  second writer modifying that generation behind the mount.
* The writable lifecycle normally unmounts and reacquires the actual core lease
  before snapshotting or replacing a generation. See
  `src/store/writable.rs::stop_writable_core`, `src/writable_lifecycle.rs::stop`,
  `src/store.rs::capture_workspace`, and the checkpoint/apply/fork/recovery
  callers. Remounting establishes a new kernel connection. Branch-history
  changes do not install a different base into an already mounted generation.
  Immutable object/pack publication does not change the mounted namespace.
* In upstream Linux v6.17, successful namespace changes invalidate/update the
  affected dentries and inode attributes; successful SETATTR installs returned
  attributes and handles truncation. Hardlink aliases share an inode. See
  [`fs/fuse/dir.c`](https://github.com/torvalds/linux/blob/v6.17/fs/fuse/dir.c),
  especially `fuse_dir_changed`, `fuse_entry_unlinked`, `fuse_rename_common`,
  `fuse_link`, and `fuse_do_setattr`.
* The adapter's O_SYNC/O_DSYNC path can commit a logical write and then return an
  error from `core.sync()`. This is not an assumed all-or-nothing error path.
  Upstream v6.17 `fuse_perform_write` calls `fuse_write_update_attr` even when the
  write reply failed; the latter invalidates size/mtime independently of the
  positive-byte-count size update. The direct-write path also calls it on error,
  and the non-writeback-cache page-write completion invalidates modification
  attributes. See
  [`fs/fuse/file.c`](https://github.com/torvalds/linux/blob/v6.17/fs/fuse/file.c).
  Sprig does not negotiate WRITEBACK_CACHE. These source paths support this
  narrow experiment, but do not replace an actual failed-barrier mounted test.

There are concrete reasons not to extend this change to every reply. CREATE
publishes a file before `open_handle`; an explicit O_TRUNC can then fail during
a second journal append. The generic kernel CREATE error path does not
necessarily invalidate cached parent attributes or negative entries. Also,
CREATE computes its reply attributes before a successful defensive O_TRUNC can
change the timestamp. Keeping CREATE, directory and negative TTLs zero avoids
expanding those cases. `open_handle` allocates before truncation; there is no
claimed post-truncation allocation failure here.

This review used upstream v6.17, not a proof about every downstream kernel. The
existing comparison ran on `6.17.0-1022-azure`. Sprig currently uses no fuser
invalidation notifier. A future in-place external mutation/base installation
would invalidate this rationale and needs its own coherent invalidation design.

## Required mounted regressions before promotion

The pure policy tests only establish the TTL selection and unchanged zero-TTL
constant. They do not establish kernel coherence. On an isolated supported
Linux test host, prewarm positive LOOKUP/GETATTR caches, then check immediately
(well inside 60 seconds) from another process, including path and held-descriptor
views:

1. O_SYNC and O_DSYNC writes whose logical append succeeds but following sync
   fails. Establish that exact ordering using a diagnostic-only test seam if
   needed; a generic ENOSPC failure is not evidence of a failed post-write
   barrier. Assert the error and promptly refreshed size/mtime/content, retaining
   the existing failed-session/poison behavior. No production fault flag is
   proposed or added here.
2. Truncate to zero and to a shorter size, then regrow; verify size, nanosecond
   mtime and zero-filled regrowth through paths and already-open descriptors.
3. Rename over a cached existing destination, retaining old source/destination
   descriptors; verify the new path identity/content and old unlinked inode's
   descriptor contents and link count.
4. Create/remove hardlink aliases and write through either alias or a retained
   descriptor, including after unlink. Verify shared identity, size, content,
   nanosecond mtime and link count without waiting for TTL expiry.
5. Cross-process mode and timestamp changes and descriptor writes; verify stat,
   fstat and permission behavior immediately. Exercise both shrinking and
   extending writes, ordinary cached I/O and supported direct/mapped I/O paths.
6. Busy lifecycle transitions must refuse rather than silently replace a live
   view. After a successful normal unmount, generation change and remount,
   verify fresh namespace, identities, attributes and content with no stale view
   from the former connection.

Retain controls for directory/symlink lookups, missing-name then create, CREATE
with O_TRUNC, and mutation reply TTLs. Run the existing mounted writable workflow
as well as these focused cases. Do not infer these pass from non-mount unit tests.

Only after coherence qualification should an isolated baseline-versus-candidate
mounted comparison assess the Cargo Fresh/no-op and restored-Fresh gap. Existing
rounded Cargo wall times motivate the experiment; they neither identify cache
misses as the cause nor demonstrate a speedup. Keep barriers and other candidate
optimizations identical, and retain correctness checks and individual receipts.
