# Independent small-file cache reuse experiment

Experimental only: no performance improvement or main-branch promotion is
claimed. The prior 1 KiB threshold and capacity-only admission experiments
regressed cold, unique small-file restores. Removing clean reservation barriers
did not demonstrate a material improvement. This experiment changes admission,
not durability, clone ordering, or content validation.

## Isolated changes

1. Start at main `b300693e01c652c1a9631c61b7a439892d8f75cd`.
2. Replay inventory-only `8558b130323d904ae8eaec822d502f52459c5b79` as
   `c392d45`: one bounded inventory per exclusive cache lease instead of
   rescanning on every population. Its scan, reservation and publication
   directory barriers remain; this prerequisite needs its own comparison.
3. Admit a 1–64 KiB (upper bound exclusive) content key only when the current
   manifest requests at least two independent materializations of that key.
   A hardlink group counts once; its aliases do not incur another CAS restore.

Planning runs once per tree. It holds at most 4096 distinct small content keys
and one borrowed first-link-group identity per key, not a second unbounded
path/group set. A 4097th distinct small key disables all small admission. A sum
of admitted logical sizes plus 4096 bytes per key exceeding 256 MiB also
disables it. Both decisions are conservative and independent of path order.
Actual allocated charges and the cache's existing 256 MiB / 4096-entry bounds
still govern every fill; admission does not reserve space or guarantee a hit.

Single-use small keys, zero/tiny files, absent planned keys and overflowed plans
do not open, lock, probe or populate the cache. Existing large-file policy is
unchanged. A valid plan can still miss due to eviction, an unsupported clone
backend, contention, or an optional cache failure; authoritative CAS reads and
full-file length checks remain the fallback. Cached hits still verify every
chunk before an independent clone/copy. No mutable destination hardlinks to a
cache entry. All per-file mode/mtime/sync and tree-publication ordering remain.

This deliberately gives up potential **cross-command** reuse of a singleton
small key. Main already bypasses such keys; the intended benefit is amortizing
first-fill cost over independently restored duplicate content in the same
tree, not accelerating every history workload. Large entries can still evict
small entries; no new cache-protection or persistent policy state is introduced.

## Deterministic correctness coverage

- Independent duplicates, one-group aliases, different groups, mixed grouped
  and ungrouped files, and deletion of the only independent copy.
- Unique 4096 / 4097 / 10000-key plans; exact key and logical-byte boundaries;
  reversed traversal after overflow; zero/tiny/large files.
- Rejected and absent keys bypass even an existing cache; large files retain
  their original behavior in the same session.
- Cold miss, verified hit, same-length corrupt hit and CAS repair, independent
  physical file identities and writable isolation.
- Public native materialization preserves internal hardlinks, independent
  duplicates, mode-000 finalization on Unix, and source/other-tree invariance.
- Existing inventory/contention/budget/fill-failure tests remain. The injected
  optional-fill error is test-only and makes the inherited failure test work
  even when a platform declines cloning before touching its pending path.

## Pressure gate before promotion

Do not pool this with the ordinary Cargo/rename experiment. Compare exact
main → inventory and inventory → admission pins separately, then confirm the
combined candidate against main. Use balanced repeated order and fresh product
caches, retain all images/trees/history, and record source and binary hashes.
Do not describe an empty product cache as a cold OS page cache.

Required negative controls: unique 4096 and 10000 files, existing unique-file
history workload, alias-only duplicate paths, and >4096 distinct small keys
mixed with duplicated content. Required positive/crossover probes: independent
1 KiB and 4 KiB duplicate content with two and many independent copies per key;
include interleaved and separated-copy traversal, near-capacity key counts,
mixed large files and nonempty-cache conditions. Do not tune a cutoff to a
particular fixture or claim that two copies necessarily amortize first fill.

Predeclared screening gate: repeated median restore gain of at least 10% on
intended duplicate workloads, no more than 5% median whole-run or restore
regression on any negative control, and no unaccounted repeated >10% tail
regression. Report all cells and order sensitivity, not just passing subsets;
small sample counts are not statistical confidence. If the duplication
crossover still loses materially, reject or redesign rather than promote.

Every sample must pass full-content/metadata/topology comparisons, source and
retained-view invariance, historical readback/recovery checks and cleanup.
Track full-filesystem physical Used including cache and retained state, not
only logical/referenced bytes. Stay inside the existing runner time, image,
disk and concurrency budgets; coordinate the exact matrix before dispatch.

## Local validation, 2026-10-26

The formatted candidate passed all 9 focused cache unit tests, then all 68
projection-enabled library unit tests in the same exact compiled executable
(0 failed, 0 ignored). Root-owned validation reused the existing debug target
and stable Rust 1.99.1; compilation added about 23 MiB, and protected benchmark
executables and the compiled source hashes were unchanged. The full-library
test run took 2.87 seconds. Small local logs/manifests are retained outside the
source tree in `sprig-cache-validation`.

These are ordinary unmounted local checks, not a native Btrfs clone result or
performance evidence. Strict lint, broader CLI/integration checks, cross-platform
CI and the separate repeated-pressure matrix still need validation before any
promotion decision.
