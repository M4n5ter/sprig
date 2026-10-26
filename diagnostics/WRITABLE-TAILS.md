# Experimental writable-tail measurement

Algorithm base: `b300693e01c652c1a9631c61b7a439892d8f75cd`. This branch measures
only; it changes no required sync, ordering, lock, mutation, or recovery decision.
It is not a production latency fix. Buffered wall-only predecessor:
`0f00ac4afbfc42b76c63a31833eabb07b82152f2`; its source remains separately pinned.

Enable explicitly with `SPRIG_WRITABLE_DIAGNOSTICS=1` in the Sprig server's
inherited environment. Other values return before any clock read or allocation.
Linux only; non-Linux clock helpers explicitly return unavailable, never zero.
No paths, file contents, environment values, request payloads or credentials
are logged. A 65,536-event recent ring is allocated only when enabled. The
`overwritten_events` field discloses truncation; wall totals cover earlier events.
There is no per-operation I/O and no CPU lifetime aggregate inferred from a
truncated ring.

## Two clock scopes, not an I/O attribution

Schema `sprig-writable-diagnostics-v2` adds one nullable `cpu_ns` per event and
headers `cpu_clock`, `cpu_clock_failures`, `cpu_resolution_ns`.
Each measurement reads CLOCK_MONOTONIC, then CLOCK_THREAD_CPUTIME_ID; after the
unchanged operation it reads the CPU clock, then the wall clock. CPU endpoints
therefore lie inside the wall endpoints. Only the elapsed CPU delta is retained,
not a cross-process CPU timestamp. This adds two CPU-clock reads per event;
their cost is not assumed equivalent to a cheap wall-clock read.

Linux defines CLOCK_THREAD_CPUTIME_ID as CPU time consumed by the calling thread;
clock resolution is implementation-dependent and clock calls can fail. See the
[Linux clock_gettime manual](https://man7.org/linux/man-pages/man3/clock_gettime.3.html).
The positive resolution is queried once when enabled. Failure of that probe
increments `cpu_clock_failures`; failed CPU endpoints or a backwards delta add
one failure per attempted scope. Such events retain wall measurements with
`cpu_ns: null`. A genuine zero CPU delta remains numeric zero. Any CPU clock
failure or missing resolution disqualifies CPU analysis, without altering
filesystem results. Wall clock failures keep their separate existing counter.

CPU elapsed includes instrumented-thread user/kernel execution and some sampling
cost, not all work performed elsewhere on behalf of an operation. A large wall
scope with little charged thread CPU is compatible with blocking or descheduling;
it does not distinguish storage I/O, run-queue delay or another wait. Resolution
is not a guarantee of accuracy. Preserve anomalous CPU/wall ratios; do not clamp
or silently subtract a presumed clock overhead. CPU costs cannot be prorated
by wall-overlap fractions or assigned to a worker without request identity.

Directory events cover the original whole-node `nlink` scan only, not lookup
queue time or the entire callback. Sync events cover each unchanged
`journal.sync_all()` call; `sequence` is the next journal sequence.
`repeats_successful_sync` indicates a preceding successful sync at the same
sequence. It is diagnostic only: repeated syncs still execute fully. No callback
mutex-wait framework is added: fuser's `Session::run` dispatches synchronously.

## Coverage and exact integer transport

Worker endpoints must use integer CLOCK_MONOTONIC timestamps. Verify host/server
and container time-namespace identities/offsets and a host-before/container/host-
after clock sandwich outside timed work. Parse absolute trace u64 values from the
complete raw file as Python integers; never from JavaScript-number copies or log
previews. Add the helper's decimal-string origin to its integer relative times.

Map the trace to the exact pressure generation and clean normal-unmount record.
For coverage of the two instrumented classes, require zero wall-clock failures,
non-saturated totals, `calls = attr.calls + sync.calls`,
`len(events) = min(calls,65536)`, `overwritten_events = calls - len(events)`,
ordered/nonoverlapping events and nondecreasing sequence numbers. First retained
start must be no later than pressure start; the final destroy sync must finish
after pressure end. Startup overwrite is acceptable only under this complete-
window proof. Post-pressure full-oracle activity can overwrite pressure events:
do not substitute a covered suffix or lifetime totals when coverage fails.

## Normal shutdown, bounded dump and accounting

Only clean, requested normal unmount emits one complete JSON line after session
acknowledgement. Existing server stderr is a regular create-new writable log,
not a pipe. A 64 KiB BufWriter and explicit flush complete while the original
journal lease is held. The existing 15-second release wait is unchanged; a slow
log can delay teardown. Output errors do not alter durability/publication, and
successful stop alone does not prove a usable trace. Missing, truncated or
invalid JSON is unavailable evidence, never zero cost. The log is not fsynced.

The exact conservative v2 serialization bound is **16,188,232 bytes including
newline**, 588,984 bytes below 16 MiB: 65,536 widest events (246 bytes plus comma),
maximum-width u64 fields, fixed schema/clock strings and header/totals. A counting-
writer test verifies this without allocating a second full dump. Null CPU values
are shorter. Other unexpected log output is additional and fails a 16 MiB parser
cap; do not discard it. The expected 64-bit Event payload is 72 bytes (4.5 MiB
per ring); Rust layout is not portable, so a test bounds each event to 80 bytes.

The existing workload produces **five server log files across four generation
directories**, with exactly one pressure-generation log. Maximum trace source
bytes are therefore 80,941,160 (~77.2 MiB) per sample inside the 4 GiB filesystem.
Full archive copies are external and separately budgeted. Runtime log previews
contain only the final 65,536 bytes and cannot validate a trace. Late cleanup
dumps follow the original active-space measurement: retain that metric and add
a separate post-normal-cleanup, pre-outer-unmount space observation. Charge dump
and archive work to whole-task time; no deadline/resource enlargement or forced
cleanup is allowed.

## Proposed finite protocol canary, not a matrix

After root's source/test review, use **one identical release ELF** derived from
b300, with diagnostics absent versus exactly enabled. Dedicated diagnostic
manifest/profile: do not relax the primary untraced workflow guard or claim this
ELF is the unmodified b300 binary. Proposed finite canary is two inc0 pairs,
off/on then on/off, four fresh symmetric samples total. Keep the exact current
1024-cycle two-worker helper, all witnesses/oracles, 4 GiB images, jobs2 and the
existing admission/job deadlines. Stop on missing coverage, clock failure,
bounded-log/space violation, or normal-cleanup failure; retain all evidence.
No automatic matrix or extra mode follows a passing canary.

Require complete pressure-window coverage in both enabled samples and complete
absence of enabled traces in controls. Report every absolute off/on loop,
operation, shutdown and whole-task timing plus space difference. Two pairs are
protocol/overhead observations, not a reliable noise or equivalence estimate.
Any conspicuous perturbation blocks attributing traced behavior to the untraced
workload; small observed changes do not prove zero overhead. Attribute CPU only
to fully contained trace scopes; report boundary-crossing scopes separately.

Before publication: root performs formatting, deterministic aggregation/failure/
integer/bounds tests, existing writable tests, projection tests and strict
all-target clippy under the protected-artifact/disk guard. Preparing/reviewing
agents perform no builds, mounted runs or pushes. Root must separately review
and authorize source publication, standard CI and the finite diagnostic canary.
