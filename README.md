# Sprig

[简体中文](README.zh-CN.md)

**Real directories for parallel work. Content-addressed history for everything you keep.**

Sprig gives local experiments, build tools and coding agents independent native
workspaces with branches, three-way merges and recoverable edits. Work in an
ordinary directory with your usual tools, then record a revision. History shares
BLAKE3-addressed, FastCDC content chunks instead of storing another full native
tree for every commit. The default copy/reflink backend needs no database, daemon,
mount or network service. An optional Linux projection backend provides a mounted
workspace over the same history and chunk store.

Sprig is a local, single-user tool. A workspace is not a sandbox, and a checkpoint
is not a consistent snapshot of a live database.

## The model

- **Store:** a directory containing history, branch references, workspaces and
  recovery records.
- **Revision:** an immutable manifest with parent IDs, a logical clock and a
  message. Each entry records its logical identity, type, ordinary Unix mode and
  modification time; file content is an ordered list of chunk hashes.
- **Branch:** a movable name for a revision. Revisions form a directed acyclic
  graph; a normal merge can have two parents.
- **Workspace:** a real directory attached to a branch and a base revision.
  Multiple workspaces can share a branch. Files may differ from the recorded base.
- **Generation:** one working tree behind a workspace, either native or projected.
  Managed edits publish a new generation and retain the previous one for recovery.

Native cloning and content deduplication solve different problems. A supported
filesystem can clone a working file without copying its payload immediately.
Chunk storage shares identical historical content. Neither makes tree scanning,
hashing, metadata or retained native generations free.

## Build and try it

Build with stable Rust and a C linker. The following examples use a POSIX shell
on Linux or macOS; Windows has a [native-copy subset](#windows-native-workspaces)
with its own requirements below. Run this block from the repository. It creates its own tiny source
project and a fresh store in a temporary directory; no existing project is needed.

```sh
cargo build --locked --release
export PATH="$PWD/target/release:$PATH"

DEMO="$(mktemp -d "${TMPDIR:-/tmp}/sprig-demo.XXXXXX")"
DEMO="$(cd "$DEMO" && pwd -P)"
STORE="$DEMO/store"
mkdir "$DEMO/source"
printf 'title=demo\ntheme=light\ncache=on\nworkers=2\nlog=info\n' \
  > "$DEMO/source/settings.conf"
printf 'Initial note\n' > "$DEMO/source/notes.txt"
s() { sprig --store "$STORE" "$@"; }

s init
s import "$DEMO/source" main
s branch baseline main
s fork main experiment

# Managed edits capture and commit automatically.
s edit main settings.conf 'theme=light' 'theme=dark' -m 'Use a dark theme'
s edit experiment settings.conf 'workers=2' 'workers=4' -m 'Use four workers'

# Ordinary programs can edit the native directory. Capture those edits explicitly.
s exec experiment -- sh -c 'printf "Try parallel work\n" >> notes.txt'
s checkpoint experiment -m 'Record experiment notes'
s diff-files baseline experiment

# Merge into a NEW branch; neither input branch moves.
s merge main experiment combined
s checkout combined merged-work
s exec merged-work -- cat settings.conf notes.txt
s log combined
s doctor
printf '\nDemo retained at: %s\n' "$DEMO"
```

The merged configuration contains both `theme=dark` and `workers=4`, and the notes
contain the extra line. The imported source directory is unchanged. The demo is
left on disk so you can inspect it. The `s` helper below uses this same store.

### Choose what to copy or restore

- `import SOURCE NAME` creates a workspace, a same-named branch and an initial revision.
- `fork WORKSPACE NAME` copies the **current working files**, including uncommitted
  edits, and creates a branch at the source workspace's base. Checkpoint to record
  those edits on the new branch.
- `checkout BRANCH NAME` reconstructs the committed head into a new workspace
  attached to that branch.
- `restore REFERENCE NAME` reconstructs a committed revision into a new workspace
  and a new same-named branch.

These commands do not replace an existing workspace. History references accept
full revision IDs, branch names or `workspace:NAME`. The explicit workspace form
means its **committed base**, never its uncommitted files. `merge OURS THEIRS OUTPUT`
requires OURS to be a branch; THEIRS may be any history reference.

## Optional Linux projection

Build with `cargo build --locked --release --features projection`, then add
`--projected` to `import`, `fork`, `checkout` or `restore`. Without this option,
creation uses the existing native backend. Forking or recovering a projected
workspace preserves its backend even without the option. Projection is not
available on macOS or Windows. Native workspaces remain the default.

A projected generation combines a read-only FUSE view of an immutable revision
with an OverlayFS writable upper directory. File contents are read from the
existing BLAKE3 chunk store on demand, verified before entering a 16 MiB cache per
view. There is no separate history format. Checkpoint reuses lower content only
when its provenance and absence of upper changes are proven; changed or uncertain
files are hashed. An ordinary checkpoint advances the branch without changing
that generation's immutable lower pin or discarding its upper data.

### Requirements and example

Projection currently requires **explicit root execution**, accessible `/dev/fuse`,
kernel FUSE and OverlayFS support, and permission to mount both filesystems.
The upper filesystem must support OverlayFS. Root inside a restricted container
is not enough. Keep the store root-owned and private, and use trusted programs:
`exec` is not a sandbox and does not drop privileges. Sprig installs no privileged
helper, system service or persistent permission changes. One Sprig FUSE worker
runs for each mounted view until it is unmounted.

Run the following example only on an isolated, disposable Linux test host. It
creates a separate root-owned store, leaves its data in place and normally unmounts
both views at the end:

```sh
cargo build --locked --release --features projection
sudo env SPRIG="$PWD/target/release/sprig" sh <<'SH'
set -eu
DEMO="$(mktemp -d /var/tmp/sprig-projection.XXXXXX)"
STORE="$DEMO/store"
mkdir "$DEMO/source"
printf 'hello\n' > "$DEMO/source/note.txt"
s() { "$SPRIG" --store "$STORE" "$@"; }
s init
s --projected import "$DEMO/source" main
s fork main experiment
s exec experiment -- sh -c 'printf "experiment\n" >> note.txt'
s checkpoint experiment -m 'Record projected edits'
s diff-files main experiment
s show experiment
s unmount experiment
s mount experiment
s exec experiment -- cat note.txt
s unmount experiment
s unmount main
s doctor
printf '\nDemo retained at: %s\n' "$DEMO"
SH
```

### Mounts, busy views and recovery

- `show`, `list` and `status` report the backend and mount state. `mount NAME`
  reopens the retained generation; operations that need its files also mount an
  unmounted projection. `show` itself does not mount: run `mount` before opening
  an unmounted path directly. A partial or invalid mount is an error, not an empty tree.
- Stop external writers before checkpointing. Close open files and leave working
  directories before unmounting or replacing a generation. `exec` leases coordinate
  Sprig operations only; they cannot stop unrelated applications.
- `unmount NAME` uses normal unmounts, never forced or lazy detach. A busy mount
  causes failure rather than forced cleanup. If only part of the view unmounted,
  inspect `show` and `doctor`, close remaining users, then retry normal unmount
  before remounting. Retained upper data is not deleted.
- Managed edits and stale-workspace merges create replacement generations and
  retain the old one. They must normally unmount the old view before publication;
  a busy view can therefore prevent publication. Re-query the path afterward.
- `drop workspace NAME` normally unmounts before moving the workspace to recoverable
  trash. `recover TOKEN NAME` retains the original, restores its previous mount
  state, and creates a new projected workspace. Use `unmount-generation GENERATION`
  for a retained generation listed in trash or an orphan reported by `doctor`; it
  removes mounts normally without deleting data. Failed startup can leave retained generations and diagnostic logs;
  inspect them before retrying rather than removing directories under a mount.

Projection rejects pre-1970 modification times and symlink targets longer than
4,095 bytes. It shares the trusted, quiescent-tree and metadata limits described
below. Mounting, crash recovery and performance depend on the host; unit tests
alone do not establish their behavior on a particular kernel or filesystem.

## Everyday workflows

### Edit with your usual tools

```sh
s show experiment
s exec experiment -- sh -c 'printf "One more idea\n" >> notes.txt'
s checkpoint experiment -m 'Keep the next idea'
s read experiment notes.txt
s blame experiment notes.txt
```

`show` returns the workspace directory in `path`. Open it in an editor, build there,
or use `exec`. External edits become history only when you checkpoint; `commit`
is an alias. A checkpoint with an unchanged manifest reuses the current revision,
even if you supply a different message.

**Look up the path again after a managed edit or a checkpoint that merges a stale
workspace.** The active generation can change. Pause external readers and writers
while changing generations, then reopen the current path. For native workspaces,
old paths and open handles can still point to a retained generation. Projected
replacements require normal unmount of the old view.

`exec` inherits standard input, output and error, runs in the workspace directory
and returns the child's exit status. Its lease excludes Sprig's managed mutations
of that workspace while the child runs. It does not checkpoint automatically,
stop other applications or restrict filesystem and network access.

### Restore content without erasing history

Continue from the quick start:

```sh
s branch saved-result combined
s rollback combined baseline -m 'Return to the initial configuration'
s checkout combined rolled-back
s restore saved-result restored-result
s diff combined saved-result
s status
```

`rollback` appends a new revision with the selected content and the current head
as its parent. Intervening history remains. It moves the branch, **not existing
workspace files**: `merged-work` still has its earlier tree, while `rolled-back`
contains the restored branch head. `status` identifies stale workspaces.

### Resolve concurrent work explicitly

A checkpoint on a stale workspace three-way merges its changes with the current
branch head. Independent changes can combine; conflicting changes do not silently
overwrite newer history. `--expected-head REVISION` additionally requires an exact
branch head before publication.

If a workspace checkpoint conflicts:

1. The command fails and the branch stays unchanged. A new conflict generation
   becomes active; the original tree remains recoverable.
2. Run `show WORKSPACE` to obtain the current path and `pending_conflicts`.
3. Edit the affected files and remove conflict markers. Normal editor writes or
   managed `write` / `edit` operations are both supported.
4. Run `resolve WORKSPACE PATH...` for every listed conflict, then
   `checkpoint WORKSPACE -m 'Resolve concurrent edits'`.

While resolution is pending, managed edits save a generation with
`committed: false`; they do not advance the branch. A successful `resolve` only
acknowledges paths, not commits them. Marker checks and explicit acknowledgment
are required; fork and recovery preserve these guards. Another branch-head change
can require another merge.

### Measure the current behavior

The [reproducible example](examples/compare.py) uses Python 3's standard library
and creates its own deterministic fixture, roughly 12 MiB: two identical 4 MiB
blobs, one distinct 4 MiB blob and a small text file.

```sh
cargo build --locked --release
python3 examples/compare.py --binary target/release/sprig

# Require a successful native clone as well, on a supporting filesystem.
python3 examples/compare.py --binary target/release/sprig --require-clone
```

It measures an ordinary byte-copy baseline, Sprig's automatic fork policy and
forced-copy fork policy. The optional strict-clone case must clone or fail; failure is expected on an
unsupported filesystem. It also
checks write isolation, chunk reuse after a prefix insertion, automatic merging
of non-overlapping edits in shared-branch workspaces, and recoverable conflicts
for overlapping edits. The JSON report contains the environment, fixture sizes,
measured operations, assertions and command trace.

One measured run on **2026-10-22**, Linux/x86_64, **tmpfs**, release build:

| Operation | Wall time | Files cloned / copied | New CAS bytes |
| --- | ---: | ---: | ---: |
| Ordinary byte copy | 6.211 ms | 0 / 4 | Not applicable |
| Sprig fork, automatic | 41.251 ms | 0 / 4 | 0 |
| Sprig fork, forced copy | 36.626 ms | 0 / 4 | 0 |

The fixture held **12,582,984 logical bytes**; initial import stored **8,388,680
chunk bytes** because the duplicate blob reused content. Inserting a **50-byte
prefix** into a 4,194,304-byte file then checkpointing added **157,420 chunk
bytes**, reusing **51 of the original 52 chunks** (4,036,934 bytes). Isolation,
non-overlapping merge, conflict preservation, recovery and integrity assertions
all passed. This run used **no native clones** and measured no exclusive physical
allocation. The ordinary copy was faster here; it also did less work.

Interpret the numbers narrowly:

- `wall_seconds` measures whole operations. Sprig also scans, hashes, syncs and
  publishes metadata; the byte-copy baseline does not implement those semantics.
  These are observable costs, not an apples-to-apples speedup benchmark.
- Logical bytes describe file contents, not exclusively allocated disk space.
  Clone statistics report what the backend did, not physical bytes saved.
- CAS object-byte growth measures stored chunks only. Native workspaces, retained
  generations, manifests and backups also take space; exclusive physical storage
  is not measured.
- Results depend on the filesystem, cache, hardware and workload. Run the script
  where you plan to use Sprig rather than extrapolating one machine's timings.

## Merge rules

The default three-way engine combines independent changes, identity-aware moves
and non-overlapping UTF-8 text edits. Incompatible content, deletion, location or
structure produces conflicts. Binary files and files over 16 MiB are not
text-merged. Content, mode and modification time merge separately: a one-sided
time change is retained; two-sided time changes choose the greater timestamp.
Managed moves preserve identity; recognition of arbitrary native renames is best
effort.

A branch merge always targets a new output branch and leaves both inputs alone.
When one input contains the other's history, it reuses the descendant revision.
Otherwise, a successful merge creates a two-parent revision. An unresolved branch
merge creates neither an output branch nor a revision; it does not create a
conflict workspace. Merge inputs must share an ancestor.

### Opt-in last-write-wins

The left input's `.lww` file selects paths for timestamp-based last-write-wins
handling. It supports gitignore-like patterns, `!` negation and last-match
precedence. `%include` paths are relative to the containing rules file; patterns
remain relative to the tree root. Includes are limited to 16 levels, 4,096
patterns and 1 MiB of aggregate UTF-8 rules. Cycles, unsafe paths and missing or
nonregular includes fail explicitly. Stale-workspace merges read rules from the
current branch head.

For affected changes, the greater recorded modification time wins, with ties
choosing the left input. Deletions use the nearest surviving ancestor's time.
`--strategy lww` applies timestamp selection to all conflicts, choosing structural
conflicts as whole components using their maximum timestamps.

Repeated `--unit a,b` options group paths and their subtrees. If both sides change
a unit, the side with the latest member modification time wins **as a whole**;
ties choose the left input. Units bypass text and `.lww` decisions. The first unit
has priority when units overlap.

These policies use source filesystem times, not commit order. They can discard
competing changes. Use the default conflict strategy when that would be unsafe.

## Command reference

Put `--store PATH` before the command. `--json`, `--copy-mode` and `--projected`
are global options; `--projected` is accepted only for the four creation commands
listed above. Use `sprig --help` or `sprig --store PATH COMMAND --help` for syntax.
Names contain 1–96 ASCII letters, digits, underscores or hyphens. Managed paths
are workspace-relative; absolute paths and parent-directory escapes are rejected.

```text
# Workspaces and branches
init
import SOURCE NAME
fork WORKSPACE NEW_NAME
checkout BRANCH NAME
restore REFERENCE NAME
branch NAME REFERENCE
checkpoint WORKSPACE [-m MESSAGE] [--expected-head REVISION]
resolve WORKSPACE PATH...
merge OURS THEIRS OUTPUT [--strategy conflict|lww] [--unit PATH,PATH]...
                        [--expected-head REVISION]
rollback BRANCH REFERENCE [-m MESSAGE] [--expected-head REVISION]
show WORKSPACE
list
status
exec WORKSPACE -- PROGRAM [ARGS...]
mount WORKSPACE
unmount WORKSPACE
unmount-generation GENERATION

# Immutable history and contents
log [REFERENCE]
diff FROM TO
diff-files FROM TO
blame REFERENCE PATH
read REFERENCE PATH
ls REFERENCE [PATH]
stat REFERENCE PATH
grep REFERENCE PATTERN
glob REFERENCE PATTERN

# Managed edits: each also accepts [-m MESSAGE] [--expected-head REVISION]
write WORKSPACE PATH (--file INPUT | --text TEXT)
edit WORKSPACE PATH OLD NEW
cp WORKSPACE FROM TO
mkdir WORKSPACE PATH
rm WORKSPACE PATH
mv WORKSPACE FROM TO
chmod WORKSPACE OCTAL_MODE PATH

# Recovery and diagnostics
drop workspace|branch NAME
recover TOKEN NAME
doctor
recover-state GENERATION
gc [--dry-run]
stats
backend
```

### Copy policy

For native import, fork and recovery of a native workspace tree:

- `--copy-mode auto` (default): try native cloning; fall back to ordinary copying.
- `--copy-mode copy`, or `--copy`: use ordinary copying only.
- `--copy-mode clone`: fail if any regular file cannot be natively cloned.

Checkout, revision restore and recovery of a dropped branch reconstruct files
from chunks, so the policy does not apply. Projected creation and projected
workspace recovery do not use this policy. Managed edits prepare changes with
automatic clone/copy before publishing the workspace backend.
`backend` describes capabilities, not proof that a particular pair of paths can
clone. Source and destination filesystem support still matters. Windows supports
ordinary copying only: `auto` and `copy` work; strict `clone` fails.

### Reading and inspecting history

Read commands resolve a reference once and inspect that immutable revision.
`read` streams exact bytes, without an added newline; `--json` buffers the file
and returns a byte array. `ls` lists immediate children, defaulting to the root.
`stat` reports recorded metadata. `grep` searches literal text and skips binary,
NUL-containing or larger-than-16-MiB files. `glob` uses globset patterns; quote
patterns such as `'**/*.rs'` to prevent shell expansion.

`diff` reports entry changes and moves. `diff-files` adds unified patches for text
within the 16-MiB read limit and explicitly marks binary/large changes without a
patch. `log` without a reference walks history from current branch heads.

`blame` attributes lines along first-parent history and follows logical identity
across renames. Unchanged lines retain their earlier attribution; lines introduced
or changed by a merge belong to that merge, not its second parent. Each line
includes `line`, `text` (with line endings), `revision`, `original_line`,
`original_path` and `message`. Inputs and compared historical files must be
NUL-free UTF-8, at most 16 MiB and 100,000 lines. Traversal is limited to 10,000
revisions. Unsupported content or exceeded limits fail rather than return partial
attribution.

### Managed transactions and the Rust API

The seven managed edit commands share `Store::apply`. They prepare edits in a
private native tree, include existing uncommitted edits in the capture, and retain
the previous generation in recoverable trash. Projected workspaces publish a new
projected generation after preparation; this path still materializes working files.
Normally the workspace and branch head publish together with `committed: true`.
Pending conflict resolution is the exception described above.

`write --file` accepts arbitrary bytes; `--text` adds no newline. `edit` requires
exactly one match of OLD in UTF-8 text. `cp` and `mv` require a new exact destination
path. `chmod` accepts ordinary octal permission bits, such as `644` or `0755`.
Library callers can combine mutations into one transaction:

```rust
use sprig::{Mutation, Store};

fn add_notes(store: &Store) -> sprig::Result<()> {
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
        None, // Optional expected branch-head revision.
    )?;
    if !result.committed {
        eprintln!("Edits saved; resolve pending conflicts before checkpointing.");
    }
    Ok(())
}
```

Library callers using projection must also configure the Sprig executable through
`Store::with_projection_executable` and satisfy the same mount requirements. The
CLI configures its own executable automatically.

### JSON and exit status

`--json` emits one pretty-printed object on stdout for results or errors:

```json
{"schema_version": 1, "result": {"initialized": true, "store": "/path/to/store"}}
```

```json
{"schema_version": 1, "error": {"code": "operation_failed", "message": "..."}}
```

The output schema marker is separate from the storage schema. Ordering is
deterministic; generated IDs and changing state are not fixed fixtures. Without
`--json`, results are unwrapped JSON and errors go to stderr. Raw `read`, help
and version output are exceptions. `exec` cannot be combined with `--json`.

- `0`: success. Inspect `committed`: an edit to a pending workspace can succeed
  without publishing a revision.
- `1`: operation failure, workspace checkpoint conflict, or unhealthy `doctor`.
- `2`: invalid CLI arguments.
- `3`: unresolved branch merge; the result contains `committed: false` and
  conflict details, not an error wrapper.

An unhealthy `doctor` returns its complete report. `exec` uses the child's exit
status instead. Check store state before retrying a failed publication.

## Recovery and storage

`drop workspace NAME` retains its generation, including uncommitted files, and
leaves its branch in place. Drop attached workspaces before `drop branch NAME`.
`list` gives exact trash tokens. `recover TOKEN NAME` creates a new workspace and
branch without consuming the recovery entry. Dropped branch recovery materializes
its committed revision; dropped workspace recovery preserves its retained tree.

`doctor` checks metadata and referenced content hashes and reports orphaned
revisions, workspace generations and staging entries. It does not repair or delete.
`gc` is **dry-run only**, even without `--dry-run`. Its conservative roots include
branch heads, workspace bases, immutable projection lowers, pending candidates,
trash, revision ancestors and metadata backups. There is no permanent purge command.

`recover-state GENERATION` restores a validated metadata backup, even if current
metadata cannot be opened, while retaining the current state bytes for inspection.
This can move references backward and leave later data unreferenced. It is not
normal content rollback: keep an independent backup, choose a generation from
`backups` carefully and run `doctor` afterward.

```text
STORE/
  state.json          # Branches, workspaces, trash and a schema marker
  lock                # Advisory metadata lock
  objects/            # Immutable payload packs, address index and object-writer lock
  revisions/HASH.json.zst # Compressed immutable manifests and parents
  native/GENERATION/  # Native trees or projection lower/upper/work/tree, and leases
  staging/            # Unpublished data and retained recovery records
  backups/            # Compressed previous metadata generations
```

Revision manifests and metadata backups use lossless, checksummed Zstandard
compression. Revision IDs still hash the original JSON bytes; reads validate the
frame and revision hash. No history or recovery roots are discarded to save space.
The current `state.json` stays uncompressed. A batch with exactly one new chunk
publishes a loose object; two or more new chunks share an immutable payload pack.
Content addresses and CDC boundaries do not change. Publication syncs the pack
and its directory before an atomic, checksummed index replacement, then completes
the index publication barriers before any revision/head can reference it. These
namespace barriers have the weaker Windows guarantees described below. Reads still verify every chunk hash. A separate object-writer lock
also serializes low-level `ObjectBatch` users; concurrent writers receive an error.
An interrupted batch can leave an unreferenced pack, never a valid partial chunk.
No published pack is deleted automatically.

The current index is rewritten when a pack is added and is limited to 64 MiB
on disk. Ordinary file reads pin one index generation and binary-search bounded
records, then verify the requested chunk hashes and whole-file size. They do not
validate the global index checksum or all records: unrelated corruption can be
detected later, and damaged ordering can produce a missing-object error. A loose
fallback must independently verify the requested hash; it is not proof that an
unused packed alias is healthy. Writers, doctor, GC, bulk readers and projection
metadata planning still validate the full index. Bulk operations share a validated
snapshot; catalog metadata is expanded only when needed. These are explicit
capacity and growth costs, not constant-time update claims. Single-new-chunk
batches do not rewrite the index.
Loose chunks remain readable within this layout, but that does not make older
store schemas compatible. GC counts packed bytes as reclaimable only when the
whole pack is unreachable; it does not pretend dead slices can be freed in place.
Native capture reuses one 256 KiB I/O buffer across files, not their contents:
every file is still read and every existing chunk verified, even when size and
mtime match. Projected capture uses the lower-provenance checks described above.

`stats.object_bytes` totals unique indexed/loose payload bytes, including
unreferenced objects. `object_storage_bytes` includes all object-directory files,
including index, lock, orphan packs and temporary files. On Unix,
`object_allocated_bytes` sums their reported allocated blocks; it is not exclusive
physical usage on shared/CoW storage. Windows reports that field as null.
`reachable_logical_bytes` sums file lengths across protected revisions; repeated
content counts in each revision. None is whole-store physical usage: retained
trees, revision records and backups also consume space. `doctor` lists all object
store entries so retained orphan/temporary files are visible.

The current storage schema is **5**. Earlier or otherwise unsupported schemas are
rejected without migration or modification, including schema 4 stores created by
the previous binary. Use a fresh store for this version;
there is no storage-format or API compatibility guarantee. Keep independent backups.

## Guarantees and boundaries

- Workspaces do not share hardlinks. Native clones remain independently writable.
  Import and fork turn source hardlinks into independent files with distinct
  logical identities. Identical content can still share historical chunks.
- A store-wide advisory lock coordinates Sprig's metadata publication. It does
  not freeze other programs. Use trusted, quiescent trees for import, fork and
  checkpoint; source-change checks detect some races, not every external write
  or hostile ancestor replacement.
- On Unix, regular files, directories and symlinks are supported. Copying and
  capture keep symlinks without following them; opening those links later can leave the
  workspace. Special files and non-UTF-8 entry names are rejected. Source roots
  cannot be symlinks, and source and store must not contain one another. Windows
  rejects reparse points, including symlinks and junctions.
- On Unix, ordinary permissions, including executable bits, and recorded modification
  times are preserved subject to timestamp precision. Special mode bits are
  stripped. Ownership, access/change times, ACLs, extended attributes and sparse
  layout are not preserved.
- Publication syncs data and atomically replaces store metadata. This does not
  guarantee survival of every filesystem, kernel, hardware or power-loss failure.
  An error after publication can mean the change already took effect. Inspect
  `status`, `list` and `doctor` before retrying.
- Failed attempts may retain unpublished objects or workspace generations. Nothing
  is automatically purged. Keep storage private and unmodified: immutability is
  a tool contract, not protection against the operating-system owner editing it.

## Platforms and development

The repository selects **stable Rust**, with rustfmt and Clippy. Cargo declares a
minimum Rust version of **1.89**, not a pinned development toolchain. Linux uses
`FICLONE`; macOS uses `fclonefileat`. Both clone individual files, not whole trees
or Btrfs subvolumes. Clone support depends on the filesystem. Windows uses ordinary
copying, with the restrictions below.

[CI](.github/workflows/ci.yml) runs stable Rust on Linux and macOS. The macOS job
sets `SPRIG_REQUIRE_REFLINK=1` to require real APFS clones and test write isolation.
Linux tests ordinary and fallback paths; passing there does not prove reflink
support on the runner. The example runs in CI too, with strict cloning on macOS.

### Windows native workspaces

The Windows backend supports **writable regular files and directories on local
NTFS disks**, using ordinary copies. Other Windows filesystems are unsupported.
Build with stable Rust's MSVC toolchain and the Visual Studio C++ build tools,
including the Windows SDK; the Zstandard dependency
needs a C compiler. Running native workspaces needs no filesystem driver or
administrator access.

PowerShell example from the repository root:

```powershell
cargo build --locked --release
$Sprig = Join-Path $PWD 'target\release\sprig.exe'
$Demo = Join-Path ([IO.Path]::GetTempPath()) ('sprig-demo-' + [guid]::NewGuid().ToString('N'))
$Source = Join-Path $Demo 'source'
$Store = Join-Path $Demo 'store'
New-Item -ItemType Directory -Path $Source -Force | Out-Null
[IO.File]::WriteAllText((Join-Path $Source 'note.txt'), "hello`n")
function s { & $Sprig --store $Store @args; if ($LASTEXITCODE) { throw 'Sprig failed' } }
s init
s import $Source main
s fork main experiment
s write experiment note.txt --text 'hello from Windows' -m 'Edit copy'
s exec experiment -- cmd /c type note.txt
s diff-files main experiment
s doctor
Write-Output "Demo retained at: $Demo"
```

Windows limitations are explicit rather than silent conversions:

- Native cloning, Linux projection and POSIX `chmod` are unsupported. Ordinary
  `exec` is supported, with the same requirement to checkpoint external edits.
- Reparse points, including symlinks and junctions, are rejected without traversal.
  UNC, device and drive-relative paths, reserved names, alternate-stream path syntax
  and unsupported path aliases are rejected. Use ordinary local NTFS paths and
  UTF-8 names.
- Files and directories with named NTFS data streams, including `Zone.Identifier`,
  are rejected before capture or copying rather than silently dropping those streams.
- Captures record `0755` for directories and `0644` for writable files. Read-only
  files are rejected. Materialization refuses other modes or timestamps finer than
  100 ns instead of silently losing recorded metadata. This limits cross-platform
  history materialization.
- New directories inherit Windows ACLs; Unix private-mode bits are not an ACL
  guarantee. Keep the store in a location already private to your account.
- Regular files are flushed and metadata replacement first uses write-through
  rename. For an immutable pack index held by existing readers, an access-denied
  result can fall back to atomic POSIX-style replacement with checked file flushes
  before and after the rename. This requires Windows 10 version 1607 or later on
  local NTFS; unsupported systems fail rather than emulate replacement. Existing
  readers retain their old index generation. This fallback is not a write-through
  rename or a directory fsync. Equal power-loss namespace durability with the Unix
  backend is not claimed.

Windows `exec` keeps its lease through an inherited file handle opened without sharing.
In a multithreaded SDK host, children spawned outside Sprig during the brief handle-
inheritance window can also retain that lease and prolong `workspace_busy`. Wait
for those children to finish; Sprig does not force the lease open or sandbox them.

The [Windows CI contract](https://github.com/M4n5ter/sprig/actions/runs/39894835808)
has passed on a hosted Windows runner: native copying, history and merge, recovery,
path/stream rejection, and inherited execution leases. The same run passed Linux
and macOS native checks and the rootful Linux projection workflow, including
busy-handle refusal and normal cleanup. These synthetic checks do not establish
full platform parity or power-loss durability.

### Development checks

Follow the [Google Rust Style Guide](https://google.github.io/styleguide/rustguide.html)
where applicable to this standalone Cargo project.

```sh
cargo fmt --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked

# Linux projection code and non-mount tests; this does not prove mount support.
cargo test --locked --features projection

# Use only on a filesystem supporting the native clone backend.
SPRIG_REQUIRE_REFLINK=1 cargo test --locked
```

The opt-in [mounted workflow](tests/projection_workflow.py) requires Python 3 and
an isolated, disposable Linux runner with the mount privileges described above. It creates a
synthetic store, retains it on failure, and never forces or lazily detaches mounts:

```sh
cargo build --locked --features projection
sudo python3 tests/projection_workflow.py --binary target/debug/sprig
```

The CLI is a thin adapter over the Rust library. Tests cover history, concurrent
workspaces, merges, line attribution, transactions, recovery, storage integrity,
native backends and CLI parsing. No project license has been selected.
