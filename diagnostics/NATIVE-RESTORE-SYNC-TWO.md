# Experimental bounded native file sync

Baseline: `4eb0f4d7afdb19bf22d9667dc520ede8b162c737`. Local experimental
candidate, not a main-branch promotion or universal performance claim.

## Scope and ordering

On Linux, the native `restore_locked` materialization path (checkout, restore,
and branch-trash recovery) uses two operation-local workers for regular-file
`sync_all` only. Public `content::materialize`, projection, import, fork and
checkpoint stay serial; non-Linux native materialization also stays serial.
No format, dependency, global sync, permission or persistent-thread changes.

Creation, verified cache/CAS reads, writes, mode and mtime remain serial and in
original order. At most two worker jobs are outstanding; the producer can hold
a third prepared file handle while waiting to submit. Handles are moved, not
reopened by pathname. Hardlink groups, including singleton groups, first drain
pending independent files and retain synchronous representative finalization.
Aliases therefore cannot precede their representative's completed sync.

All outstanding work is drained and workers joined before directory metadata,
directory/parent sync or publication. Sync failure prevents those barriers and
state publication; traversal failure also drains and joins. The earliest
submitted sync error takes precedence over a later traversal error. Partial
thread startup and unwinding join existing workers. Failed materialization can
leave a private partial tree, as before; it does not publish a new generation,
branch or workspace. This preserves durability calls, not crash-atomic staging
cleanup or identical error precedence when multiple operations fail.

## Separate local measurements

Each dataset has 32 timed samples / 128 successful CLI commands: raw and
compressed 512-file fixtures, 48 KiB per file, four paired trials in AB, BA, BA,
AB order. Values below are median paired candidate/baseline elapsed-time ratios,
not ratios of aggregate medians. Checkpoint is an unchanged negative control.

| Dataset | Raw restore | Compressed restore | Raw checkpoint | Compressed checkpoint |
| --- | ---: | ---: | ---: | ---: |
| Initial `native-compare-20261027-0705` | 0.6326 (-36.7%) | 0.6534 (-34.7%) | 1.0271 (+2.7%) | 0.9715 (-2.9%) |
| Final `native-final-20261027-0720` | 0.6142 (-38.6%) | 0.5752 (-42.5%) | 1.0452 (+4.5%) | 0.9559 (-4.4%) |

These datasets are **not pooled**: final source adds pipeline failure and mixed
hardlink coverage/test seams. Final restore improved in all eight pairs and
both orders. Controls remain noisy: one final raw checkpoint pair was 1.5722;
one initial raw restore pair was 0.9847. No checkpoint speedup is claimed.

Evidence is retained separately under `sprig-restore-sync-two-evidence`, including
per-command stdout/stderr, timing samples, full manifests, independent audits
and cleanup receipts. Each dataset passed full file bytes/mode/mtime readback
and doctor checks; exactly its 16 generated stores and two source fixtures were
removed after verification and no-use checks. Neither reports nor binaries were
removed. Final executable SHA-256:
`0055828f67f422a8a64112059bc701af1907c0de7040e71aae495417f47ef7b8`.
Baseline executable SHA-256:
`395a2220f7603da0ca14a8118daac77f5dfea4a712fdca7470cf3cb04e2b211e`.

## Validation and limits

Final source passed formatting, 71 projection-enabled library tests and strict
all-target Clippy. Tests cover bounded workers, draining on failure/early return,
panic conversion, actual Store pipeline full-batch/final-drain sync failures
(skipped directory barriers and unchanged persisted state), and nonempty drains
between independent files, hardlink representatives/aliases and a singleton.

Measurements are one Linux overlayfs executor and a warm-cache pipeline, not
cold-cache, Btrfs, macOS/Windows runtime, power-loss or whole-Cargo validation.
Concurrency may regress on other storage. Fallible startup is handled but no
resource-exhaustion startup test was run. This experiment needs broader native
filesystem and portability validation before considering main-branch promotion.
