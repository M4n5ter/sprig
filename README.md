# Sprig

Native filesystem workspaces with one content-addressed revision history.

Sprig combines ordinary directories you can edit and execute with immutable
revision manifests, branches, merges and recoverable workspace transactions.
History stores BLAKE3-addressed FastCDC chunks, not a full native tree per commit.
Working copies use per-file native cloning where available, with ordinary copy
fallback. There is no database, daemon, FUSE mount or network service.

## Build

```sh
cargo build --release
export PATH="$PWD/target/release:$PATH"
sprig --help
```

The repository selects current stable Rust, including rustfmt and Clippy.
Cargo's minimum Rust version is 1.89, not a pinned development toolchain.
CI targets stable Rust on Linux and macOS. The native backend suite passed on
both platforms at `f631f4c` ([verified run](https://github.com/M4n5ter/sprig/actions/runs/39740305505)).
The macOS job enforced `SPRIG_REQUIRE_REFLINK=1`, verified actual clones with no
copy fallback, and checked write isolation. Linux's passing checks do not
establish successful reflinking. Linux uses `FICLONE`; macOS uses `fclonefileat`.
Both clone individual files, not whole trees or Btrfs subvolumes. Cloning requires
filesystem support. Windows is unsupported.

## Quick start

Choose a new store path outside your source tree. Stop external writers while
importing, forking or capturing changes.

```sh
STORE="$HOME/sprig-store"
sprig --store "$STORE" init
sprig --store "$STORE" import ./my-project main
sprig --store "$STORE" fork main experiment
sprig --store "$STORE" show experiment
# Edit files at the returned path with your usual tools.
sprig --store "$STORE" checkpoint experiment -m "Try a refactor"
sprig --store "$STORE" diff main experiment
sprig --store "$STORE" merge main experiment combined
sprig --store "$STORE" checkout combined merged-work
sprig --store "$STORE" exec merged-work -- sh -c 'pwd; ls'
```

Import creates a workspace and same-named branch with an initial revision.
Fork copies the current working tree, including uncommitted changes, and creates
a new branch at the source workspace's base revision. Checkpoint the new
workspace to add those changes to its history. `commit` aliases `checkpoint`.

A branch is a movable reference; a workspace is a native directory plus its
branch and base revision. `checkout BRANCH NAME` creates a new workspace attached
to an existing branch. `restore REVISION NAME` creates a new workspace and a new
same-named branch. Neither replaces existing content. References accepted by
history commands can be full revision IDs, branch names or `workspace:NAME`.
The explicit workspace form resolves its committed base, not uncommitted files.

Merge creates a new output branch and leaves both input references unchanged.
If one input already contains the other's history, the output reuses the newer
revision without creating a merge commit. A non-fast-forward merge creates a
two-parent revision. Conflicts create neither a branch nor a revision.
Rollback changes the named branch, **not existing workspace files**. Use a new
checkout to work on that head. `status` lists stale workspaces.

## Commands

All commands take `--store PATH` before the command. Use `COMMAND --help` for
individual arguments.

```text
init
import SOURCE NAME
fork WORKSPACE NEW_NAME
checkout BRANCH NAME
restore REVISION NAME
checkpoint WORKSPACE [-m MESSAGE] [--expected-head REVISION]
resolve WORKSPACE PATH...
branch NAME REVISION
merge OURS THEIRS OUTPUT [--strategy conflict|lww] [--unit PATH,PATH]...
                        [--expected-head REVISION]
rollback BRANCH REVISION [-m MESSAGE] [--expected-head REVISION]
log [REFERENCE]
diff FROM TO
diff-files FROM TO
blame REFERENCE PATH
read REFERENCE PATH
ls REFERENCE [PATH]
stat REFERENCE PATH
grep REFERENCE PATTERN
glob REFERENCE PATTERN
list
status
show WORKSPACE
exec WORKSPACE -- PROGRAM [ARGS...]
drop workspace|branch NAME
recover TOKEN NAME
doctor
recover-state GENERATION
gc [--dry-run]
stats
backend
write WORKSPACE PATH (--file INPUT | --text TEXT) [-m MESSAGE]
edit WORKSPACE PATH OLD NEW [-m MESSAGE]
cp WORKSPACE FROM TO [-m MESSAGE]
mkdir WORKSPACE PATH [-m MESSAGE]
rm WORKSPACE PATH [-m MESSAGE]
mv WORKSPACE FROM TO [-m MESSAGE]
chmod WORKSPACE OCTAL_MODE PATH [-m MESSAGE]
```

All seven managed mutation commands also accept `--expected-head REVISION`.
Names use 1–96 ASCII letters, digits, underscores or hyphens. Managed paths are
relative to the workspace, not absolute paths or parent-directory escapes.
`write --file` accepts arbitrary bytes; `--text` adds no trailing newline.
`edit` requires exactly one match of OLD in a UTF-8 file. `cp` and `mv` use an
exact destination path that must not exist. `chmod` accepts ordinary octal
permission bits, such as `644` or `0755`.
`diff` reports entry changes and moves. `diff-files` adds unified patches for
text files within the 16-MiB read limit; binary/large changes have no patch and
are explicitly marked. `log` without a reference walks the history of current
branch heads.

`blame` attributes individual text lines along first-parent history, following
logical file identity across renames. Unchanged lines retain earlier attribution;
lines introduced or changed by a merge belong to that merge, not its second
parent. Each result line includes `line`, `text` (retaining line endings),
`revision`, `original_line`, `original_path` and `message`. It accepts NUL-free
UTF-8 within 16 MiB and 100,000 lines, and traverses at most 10,000 revisions.
The same text bounds apply to compared historical files. Unsupported historical
content or exceeding these bounds fails explicitly rather than returning partial blame.

Read operations resolve a reference once and inspect its immutable revision.
`read` streams raw bytes without an added newline; `--json` instead buffers the
file and returns a byte array.
`ls` lists immediate children, defaulting to the root. `stat` returns recorded
entry metadata. `grep` searches literal text and skips binary/NUL-containing or
larger-than-16-MiB files. `glob` uses standard globset patterns; quote patterns
such as `'**/*.rs'` so your shell does not expand them first.

Copy policy applies to import, fork and recovery of native workspace trees:

- `--copy-mode auto` (default): try native cloning; copy if unsupported.
- `--copy-mode copy`, or `--copy`: never try native cloning.
- `--copy-mode clone`: fail if a regular file cannot be natively cloned.

Checkout, revision restore and recovery of a dropped branch reconstruct files
from chunks; they do not use the copy policy. Managed transactions use automatic
clone/copy. `backend` describes the platform and filesystem, not a guarantee
that every source/destination pair can clone.

## History, concurrency and merge

Revisions form a DAG with parent IDs, a logical clock, a message and an entry
manifest. Identical chunks are shared across revisions. Native workspaces stay
independent: there are no hardlinks between them. Stable logical entry IDs let
managed moves and recognized native renames participate in history and merge.
Native rename recognition is best effort, not a guarantee for arbitrary editor
or filesystem operations.

`--expected-head` adds an explicit caller-side revision check before publishing.
A store-wide advisory lock coordinates metadata publication; it does not freeze
other apps. Workspace commits use the same three-way engine as branch merges
when their branch has advanced, rather than silently overwriting newer history.
A checkpoint with no manifest change reuses the current revision instead of
creating history solely for a different message.

If a workspace commit conflicts, the branch stays unchanged and the command
returns an error, but a new recoverable conflict generation becomes active.
`show WORKSPACE` gives its path and pending conflicts. Resolve the content with
your normal tools or managed `write`/`edit` operations, then acknowledge each
path with `resolve WORKSPACE PATH...` and run `checkpoint` again. Marker checks
and explicit acknowledgments still apply; fork and recovery preserve these
pending guards. The original generation is retained. A further head change can
require another merge. Workspace merges take `.lww` rules from the current
branch head.

The default three-way merge handles independent changes, identity-aware moves
and non-overlapping UTF-8 text edits. It reports incompatible content, deletion,
location or structural changes without publishing an output branch. Binary or
oversized files are not text-merged. Content, mode and modification time are
merged as separate fields so independent changes can coexist. A one-sided time
change is retained; bilateral time changes use the greater timestamp.

The left input's `.lww` rules opt selected paths into last-write-wins handling.
For `merge`, OURS must be a branch and `--expected-head` checks that input head.
Rules support gitignore-like globs, `!` negation and last-match precedence.
`%include` paths are relative to the including file; patterns remain relative
to the tree root. Includes are limited to 16 levels, 4,096 patterns and 1 MiB of
aggregate UTF-8 rules; cycles, unsafe paths and missing/nonregular includes fail.
For affected conflicts, the newer recorded modification time wins; ties choose
the left input. Deletion uses the nearest surviving ancestor's time.
`--strategy lww` applies this policy to all conflicts, selecting a conflicting
structural component by its maximum timestamp. These are source filesystem
times, not commit chronology; competing changes can be discarded.

Repeated `--unit a,b` options group paths and their subtrees. If both sides
change a unit, the side with the latest member modification time wins as a
whole, with ties choosing the left input. Units bypass text and `.lww` decisions;
when units overlap, the first has priority.

Rollback appends a new commit with the selected revision's content and the
current head as its parent. It never erases intervening history.

## Managed writes and execution

`write`, `edit`, `cp`, `mkdir`, `rm`, `mv` and `chmod` share the Rust library's
`apply` transaction path. They edit and capture a private native generation,
including existing uncommitted changes, and retain the previous generation in
recoverable trash. Normally the new workspace and branch head publish together,
returning `committed: true`.

While a merge is pending, these operations instead save a new working generation
with `committed: false` and leave the branch unchanged. This permits managed
conflict-resolution edits without publishing unresolved content. Explicit
`resolve`, then `checkpoint`, is still required to publish the result.

**Run `show WORKSPACE` again after a managed mutation.** Its native path can
change. Quiesce external writers and readers while changing generations, then
reopen readers at the new path. Previously opened paths or editor file handles
can still refer to the retained old generation. Direct edits at the current path
are ordinary filesystem writes; only a later checkpoint records them in history.

`exec` runs in the current workspace directory and inherits standard input,
output and error. It holds a lease excluding Sprig's managed mutations of that
workspace while the child runs. It does not checkpoint afterward, stop unrelated
external writers, restrict network/filesystem access or create a sandbox. Run a
checkpoint after the child exits to capture changes. `exec` returns the child's
exit status and cannot be combined with `--json`.

Library callers can apply several mutations in one transaction:

```rust
use std::path::Path;
use sprig::{Mutation, Store};

fn main() -> sprig::Result<()> {
    let store = Store::open(Path::new("/path/to/store"))?;
    let result = store.apply(
        "main",
        &[
            Mutation::Mkdir { path: "notes".into() },
            Mutation::Write {
                path: "notes/idea.txt".into(),
                content: b"A small experiment\n".to_vec(),
            },
        ],
        "Add experiment notes",
        None,
    )?;
    if !result.committed {
        eprintln!("Edits saved; resolve pending conflicts before checkpointing.");
    }
    Ok(())
}
```

## Recovery and inspection

`drop workspace NAME` retains the native tree, including uncommitted files, in
recoverable trash. It does not delete the branch. Drop attached workspaces before
`drop branch NAME`. A branch's revision remains recoverable. `recover TOKEN NAME`
creates a new workspace and branch without consuming the trash entry. `list`
shows the exact recovery tokens.

`doctor` checks metadata and referenced content hashes, and reports orphaned
revisions, native generations and staging entries. It does not repair or delete
them automatically. `gc` is **dry-run only**, including without `--dry-run`.
Its conservative reachability roots include branch heads, workspace bases,
pending conflict candidates, trash, revision parents and state backups. There is no permanent purge command.
Backups and retained generations intentionally consume space.

`recover-state GENERATION` explicitly restores a validated metadata backup,
including when the current state cannot be opened. It preserves the current
state bytes for inspection rather than discarding them. Choose a generation
from the store's `backups` directory carefully: restoring older metadata can
move references back and leave later data unreferenced. This is recovery, not
normal content rollback. Run `doctor` afterward; keep a separate store backup
before manual recovery work.

The store layout is:

```text
STORE/
  state.json          # Versioned branch/workspace/trash metadata
  lock                # Advisory metadata lock
  objects/HASH        # Immutable content chunks
  revisions/HASH.json # Immutable revision manifests and parents
  native/GENERATION/  # Native working generations and lease files
  staging/            # Unpublished metadata and retained recovery records
  backups/            # Previous state generations
```

Schema v2 does not silently reinterpret the original v1 snapshot format.
Opening a legacy store fails without modifying it. To migrate, create a separate
v2 store and import the old workspace tree paths; keep the old store as a backup.
This imports current contents, not the old snapshot names or history.

## JSON and exit codes

`--json` produces one pretty-printed JSON object on stdout for results and errors:

```json
{"schema_version": 1, "result": {"initialized": true, "store": "/path/to/store"}}
```

```json
{"schema_version": 1, "error": {"code": "operation_failed", "message": "..."}}
```

The output schema version is separate from the on-disk store format version.
Object ordering is deterministic; generated IDs and changing store state are
not fixed fixtures. Without `--json`, results are readable JSON without the
wrapper and errors go to stderr. `read` is the raw-byte exception. Help and
version output remain text.

Exit codes: `0` success, `1` operation failure or an unhealthy doctor report,
`2` invalid CLI arguments, `3` uncommitted merge conflicts. Merge conflicts return
the full result with `committed: false` and conflict details, not an error wrapper.
An unhealthy doctor likewise returns its full report. A workspace checkpoint
conflict returns an error with exit `1`; inspect `show` for its pending state.
A managed edit to an already-pending workspace succeeds with exit `0` and
`committed: false`: inspect that field rather than assuming success published a
revision. Normal commit results use `committed: true`, including unchanged
checkpoints that reuse a revision. `exec` uses the child exit code instead of
these meanings. Inspect state before retrying a failed publish.

## Safety boundaries

- Use trusted, quiescent trees. Source-change checks detect some races, not every
  external write or hostile ancestor replacement. This is not a live-database
  snapshot tool or a security boundary against your own operating-system user.
- Keep store metadata and immutable objects private and unmodified. Immutability
  is a tool contract, not protection from an owner editing raw storage files.
- Regular files, directories and symlinks are supported. Symlinks are retained
  without following them while copying or capturing; opening them later can
  access paths outside the workspace. Special files and non-UTF-8 entry names
  are rejected. Source roots cannot be symlinks or contain the store.
- Ordinary Unix modes, including executable bits, and recorded modification
  times are preserved, subject to filesystem timestamp precision. Special mode
  bits are stripped. Ownership, access/change times, ACLs, extended attributes
  and sparse layout are not preserved. Source hardlinks become independent
  files with distinct logical identities; equal bytes still deduplicate in CAS.
- Publication uses synced data and atomic metadata replacement within the
  store. It is not a guarantee against every filesystem, kernel, hardware or
  power-loss failure. A failure after publication may mean the action took
  effect despite an error. Check `status`, `list` and `doctor` before retrying.
- Failed attempts may leave unpublished objects or native generations for
  inspection. Nothing is automatically purged. Keep independent backups.
- Fork and checkpoint scan and hash the source tree; reflinks avoid copying
  payload bytes but not these scans or metadata work. Ordinary copying may need
  the complete logical file size. Chunk history saves repeated content, but
  native working trees and retained generations have their own storage cost.

## Development

Follow the [Google Rust Style Guide](https://google.github.io/styleguide/rustguide.html)
where applicable to this standalone Cargo project.

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```

The CLI is a thin adapter over the `sprig` library. Default tests accommodate
filesystems without clone support; `SPRIG_REQUIRE_REFLINK=1 cargo test` requires
real cloning and verifies isolation. Tests cover history, concurrency,
transactions, recovery, storage integrity and the parser. The linked CI run
records native-backend verification, not a substitute for testing later changes.
No project license has been selected.
