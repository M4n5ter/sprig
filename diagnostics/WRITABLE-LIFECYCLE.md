# Writable/Cargo lifecycle attribution (diagnostic only)

Base: product `071036f83691b89bd9e35bb162d34e305e1874b7`, schema 14.
No performance optimization or new performance result is claimed here.

## Existing evidence

Source: six original Sprig records in benchmark repository
`data/combined-20261025/cargo-full-corrected/**/cargo-transport-sprig-writable-*.json`.
These are pooled medians over both incremental settings, not paired speedups:

| Interval | Median ms |
|---|---:|
| Complete product checkpoint/readiness | 93.03 |
| Compiler quiesce + complete product checkpoint | 433.00 |
| Complete product restore | 24.68 |
| Restore to generation-bound compiler ready | 222.24 |
| Retained-input verification (two separate compiler sessions) | 1177.91 |
| Final cleanup | 502.50 |

Each record has 90–91 Docker CLI operations, including 16 command execs,
38–39 inspections, 12 inventories, and four separate create/start/marker/wait/log/remove
lifecycles. Median per-sample diagnostic totals: inspect 960.56 ms; inventory
196.60 ms; create 167.97 ms; start 549.67 ms; marker 312.00 ms; wait 63.70 ms;
logs 70.73 ms; removal 92.76 ms; exec 1583.39 ms. Exec includes real application
work; inspection includes Docker-side synchronization. These numbers do NOT
establish pure process-startup costs. All overlap the authoritative phase/task
wall timers; none may be subtracted to advertise a faster product.

Generation containers remain separate, with all starts, oracles, natural exits,
identity checks and normal removal charged. No persistent bind may cross checkpoint.

## Why diagnostics first

`wait_journal_release` still sleeps 25 ms after contention. Existing reports do
not expose whether it actually sleeps, nor how much of checkpoint time is lease
release, replay, persistence or remount. Faster polling is therefore only a
hypothesis, not an implemented optimization. Current schema 14 already retains
the replayed writable core through capture; that previous optimization is not new.

This executor is UID 1000 and has no `/dev/fuse`. It cannot validate actual normal
unmount, FUSE draining, journal contention or root-only binding behavior. Unit
checks must not be reported as kernel lifecycle acceptance.

## Opt-in trace contract

Set `SPRIG_LIFECYCLE_TRACE=1` in the actual product process environment. It is
inherited by the generation-scoped FUSE child. Each completed measured operation or scope exit writes one line to stderr:

```
SPRIG_LIFECYCLE_TRACE {"version":1,"pid":123,"phase":"writable.lease_poll","elapsed_ns":1234,"retries":0,"ok":true}
```

No trace is emitted for other values. Default-disabled calls still perform a
small environment check and closure dispatch; do not call the diagnostic binary
an identical uninstrumented baseline. Results/errors are returned unchanged.
`ok=null` denotes a scope-only span whose success/error is deliberately not inferred;
`ok=true/false` is emitted only when a Result was directly observed.
Failed stderr writes are ignored; diagnostics do not convert a completed mutation
into a failure. A panicking scope may emit `ok=null`; a killed process need not emit a final event: missing
records are unknown, not zero-time/success. No path or session identity from these
lines authorizes cleanup or substitutes for existing security/correctness evidence.

Record CLI stderr and **all generation `writable-*.log` files** on the measured
filesystem, including retained old generations. Child core-open/replay traces go
to those existing daemon logs, not CLI stderr. Do not move logging outside the
measured filesystem. Diagnostic writes add elapsed time, storage and possible
scheduling effects; retain whole-task timing and label this a diagnostic series.

### Phase meanings (nested, not additive)

- `store.checkpoint`: complete Store checkpoint call, including locks, capture,
  commit and authoritative remount; excludes CLI parse/process startup.
- `store.checkpoint_commit`: commit engine including revision persistence, new
  generation and publication; nested generation/publish spans overlap it.
- `store.checkpoint_state_publish`: final authoritative State save.
- `writable.restore_store`: Store restore call, excludes CLI-triggered mounting.
- `writable.restore_state_publish`: restore authoritative State save.
- `writable.generation_persist`: new generation/tree/pin/core initialization and
  synchronization, before State publication.
- `writable.new_generation_sync`: initialized core synchronization.
- `writable.stop_core`: mount inspection, normal stop, lease acquisition/replay
  and sync. `retries` counts the unchanged 10 ms retry sleeps after WouldBlock.
- `writable.server_stop`: stop certificate request, normal unmount, absent-mount
  verification, actual lease release, clean-certificate validation.
- `writable.normal_unmount`: exact `umount2(..., 0)` call only. Its completion
  alone is NOT journal release or a standalone kernel-drain proof.
- `writable.lease_wait`: checked lease-file open/metadata plus lock wait.
- `writable.lease_poll`: actual exclusive-flock observation loop; `retries`
  counts the unchanged 25 ms sleeps. Success still requires flock acquisition.
- `writable.stop_certificate`: existing clean-stop validation/publication.
- `writable.core_open_replay`: base-pin verification, revision/manifest load and
  complete WritableCas open/replay. Called in parent and child; use PID to separate.
- `writable.replayed_core_sync`: sync before returning the retained stopped core.
- `writable.capture_sync` / `writable.capture_snapshot`: capture's existing sync
  and snapshot, respectively. Neither barrier is removed as redundant.
- `writable.finish_commit_remount`: authoritative State read and remount/error
  handling; remount=false may be short.
- `writable.mount`: Store mount path, including revision load and checks.
- `writable.server_start`: validated generation checks, target sync, child spawn,
  readiness and final binding verification.
- `writable.server_readiness`: existing readiness handshake only; nested in start.

Each elapsed interval ends before its own log write. Enclosing intervals and
external whole-task timers include the child interval's log write. Nested spans,
parent/child process work and external timers must never be summed together.

## Required experiment before any polling change

Run separately labeled instrumented real Cargo and writable history stress with
unchanged oracles, all retained generations, production synchronization, ordinary
security and complete cleanup. Preserve per-sample trace, wall, retry count and
identity evidence. If lease sleeps materially contribute, compare a bounded
sleeping-backoff candidate to the same unmodified product on the same runner;
keep actual flock proof, timeout, identity validation and all fsync barriers.
Do not adopt polling changes based only on synthetic thread-lock timing.

Transport optimization is a separate harness series. A supported long-lived
Docker client could avoid CLI launches while retaining every explicit gate, but
must first prove identical exact-ID inspection, error/unknown-ack semantics,
generation lifetimes and accounting. No such transport change is implemented.

## Local validation (25 October 2026)

Fresh unique target directory, no reused Sprig binary: projection-feature complete
suite **60 tests passed** (54 library including three new trace tests; six
CLI/integration tests), and `cargo clippy --all-targets --features projection --
-D warnings` passed. Formatting and diff whitespace checks passed.

`python3 diagnostics/check_trace.py /absolute/path/to/sprig` passed against the
new binary. It parses actual stderr trace JSON, verifies unset/`0`/`true` silence,
checks byte-identical stdout and unchanged exit codes for writable status,
no-op checkpoint and missing-workspace failure, and exercises restore. The helper
uses only disposable unmounted owned stores; it is not a kernel/FUSE test.
Unit tests also verify exact-once execution, original success/error preservation,
record parsing and failed diagnostic output handling. No performance improvement,
real mounted lease contention or root-only lifecycle acceptance is asserted.
