# Bounded journal-release observation experiment

Baseline: uninstrumented main `071036f83691b89bd9e35bb162d34e305e1874b7`.
This candidate changes only writable lease observation cadence, not durability,
mount/session identity, compiler generations, or checkpoint contents.

## Measured reason

Real mounted diagnostic canary [40181092961](https://github.com/M4n5ter/sprig-benchmarks/actions/runs/40181092961),
product `f09120e3138596c4983371aaccf6f019f395e9fd`, preserved 54 complete log
files and 227 valid trace records. Independent byte-count/SHA-256 verification
passed. Every one of 13 lease waits slept exactly once at the old 25 ms cadence:
median 25.075 ms, range 25.033–25.080 ms, sum 325.943 ms. Normal unmount median
was 0.327 ms; complete server-stop median 31.287 ms. Four checkpoint Store scopes
had median 56.884 ms and each contained one lease sleep. Complete external
checkpoint/readiness median was 62.018 ms. These are dependent operations in one
instrumented canary, **not** a performance comparison or a predicted speedup.

## Change and safety boundary

After an unsuccessful actual nonblocking exclusive `flock`, sleep for 1, 2, 4,
8, 16, then 25 ms, remaining capped at 25 ms. This gives quick ordinary daemon
teardown several early opportunities without busy-spinning; prolonged contention
returns to the old cadence. For never-released fake-clock contention there are
604 sleeps, versus the old nominal 600 over 15 seconds, not an unbounded fast poll.

The same 15-second observation budget starts immediately before the lock loop.
Each final sleep is clipped to the remaining budget; the deadline is never reset.
As before, every iteration attempts the actual lock before classifying timeout:
only lock acquisition succeeds, and an absent mount or elapsed time never proves
journal release. Scheduler delays can exceed the budget, as before; after the
next failed acquisition there is no further sleep or deadline extension.

Only WouldBlock and Interrupted are observed again. Other original OS errors
return unchanged. The exact timeout error and retained-generation behavior remain.
No signals, forced/lazy unmounts, lock deletion, open-core bypass, skipped sync,
clean-stop certificate changes, or compiler/container lifecycle changes occur.

## Tests and required acceptance

Fake-clock tests cover immediate release without sleeping; prompt and late
release with actual reacquisition; never-release timeout with clipped last sleep;
interrupted acquisition with sleeping; unchanged fatal errno; and scheduler delay
without timeout reset. They do not stand in for kernel lifecycle acceptance.

Before promotion, require ordinary kernel CI plus same-runner alternating
baseline/candidate writable pressure with full history, bytes/types/modes/exact
mtimes/hardlinks/symlinks oracles, normal cleanup and complete wall accounting.
Neither side should enable diagnostic traces. Compare complete checkpoint and
restore/read/cleanup/task intervals, not arithmetic subtraction of traced time.
A single lease wait is nested in each affected lifecycle; the 13 observed waits
must not be added again to checkpoint and total wall. Restore startup itself has
no journal-release wait, while normal cleanup of a restored generation does.
