# Experimental writable-tail measurement

Base: `b300693e01c652c1a9631c61b7a439892d8f75cd`. This branch measures only;
it changes no required sync, ordering, lock, mutation, or recovery decision.
It is not a production latency fix.

Enable explicitly with `SPRIG_WRITABLE_DIAGNOSTICS=1` in the Sprig server's
inherited environment. Other values leave it disabled. Linux only: the events
use `CLOCK_MONOTONIC`, the same clock as Linux Python `perf_counter_ns()`.
No paths, file contents, environment values, request payloads or credentials
are logged. A 65,536-event recent ring is allocated only when enabled. The
`overwritten_events` field discloses truncation; totals cover earlier events.
The in-memory events have fixed-size fields. There is no per-operation I/O.

Directory events cover the existing whole-node `nlink` scan only, not lookup
queue time or the entire lookup callback. Sync events cover each unchanged
`journal.sync_all()` call; `sequence` is the generation's next journal sequence.
`repeats_successful_sync` indicates that the prior successful sync observed the
same sequence. It is diagnostic only: even repeated syncs still execute fully.
Clock failures are counted and their event is omitted, never propagated to the
filesystem. No callback mutex-wait framework is added: fuser 0.16's `Session::run`
is a non-concurrent read-dispatch loop and the adapter spawns no callback tasks.

Only clean, requested normal unmount emits one JSON line to the existing server
stderr log, after the session acknowledgement. Unclean/unrequested exits do not
emit a misleading normal result. Log emission errors do not alter durability
or publication. This branch must not be used to justify failure-path claims.
The final dump and enabled timing overhead affect overall task measurements.

## Required diagnostic comparison

Use a separate, bounded ordinary positive-path pressure profile with a longer
steady state; keep the agreed filesystem/container/job/wall caps. Capture
worker operation start/end monotonic integers, worker id, iteration and class,
not only duration arrays. Preserve normal unmount, immutable raw logs, complete
content/metadata/hardlink/replay/checkpoint/restore oracles and their accounting.
The baseline workload currently has only ~0.4–0.6 s of timed worker activity,
within a ~3–4 s phase including setup and validation.

Run identical traced and untraced controls with balanced order. Compare
operation intervals to sync and directory scan intervals; separately report
unattributed scheduler/kernel/request-queue time. Report count, absolute
latencies, pair scatter and trace coverage. Do not call overlapping operations
independent trials, or their p99 an end-to-end task p99. A shared slow interval
is not by itself proof of fsync causality. Reject an attribution when trace
coverage is missing or overhead materially changes the workload.

Before publication: run format, focused diagnostic aggregation test, existing
writable tests, projection compile/tests and clippy under root's serialized
validation ownership. No builds or mounted runs have been performed by the
preparing agent.
