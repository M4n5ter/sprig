# Live-mapping falsification harness

`freeze_probe.c` is original MIT-licensed x64 Win32 C source for a deliberately
narrow, disposable-guest experiment. Build-only CI has compiled this harness. Runtime outcomes are recorded in each
workflow artifact; this document is not a successful freezing test report.

## Authorization and environment boundary

This source-preparation task did not compile, run, install, load, or publish
anything. Driver loading, an experiment run, test signing, or boot/OS security
changes require separate explicit authorization. The source itself performs
none of those setup operations and does not stop an external memfs/driver
dispatcher. Any future experiment needs an independently supervised,
disposable Windows x64 guest.

A future authorized operator must already have a running, instrumented WinFsp
memfs RAM volume configured with its `-F -SPRIGPROBE` filesystem-name setting.
The harness requires an explicit command of this shape:

```
freeze_probe.exe --allow-experiment R:\
```

Here `R:\` must be replaced with the operator-owned RAM mount root. Passing this
argument is the operator's assertion of ownership and RAM-backend suitability;
the harness cannot prove either from a filesystem label. It validates the root through an opened directory handle, not the MountManager-dependent GetVolumePathName API. It validates that:

- The argument is the exact local DOS drive root, not a child
  directory, UNC share, device-namespace path, or ADS path.
- `GetVolumeInformationByHandleW` reports a name containing `SPRIGPROBE`, allowing a
  WinFsp-provided prefix. Each newly created handle is checked against the same
  volume serial and marker. The kernel independently requires its configured
  filesystem-name suffix to be exactly `-SPRIGPROBE`.
- The process has an elevated administrator token and already has
  `SeLoadDriverPrivilege`. It enables and verifies that existing privilege in
  its process token. It cannot grant a missing privilege and does not change
  account or local security policy. It never calls a driver-loading API.

The memfs process, its dispatcher, and the underlying filesystem driver must
remain running throughout. Do not stop them as a test step or cleanup shortcut.
The maximum requested kernel hold is 1,000 ms, not the ABI's 250 ms default.
The intended observations are approximately 40 ms with A held, then 40 ms with
both held, followed immediately by explicit release. The shorter observation
and larger ceiling reduce false timeouts on a two-vCPU guest; they do not make
an expired request valid.

## Building later, without installing or running

The only source dependency is the adjacent, unchanged `probe_protocol.h`. In an
already authorized Visual Studio x64 Native Tools environment with a Windows
SDK, this is the intended compile/link command:

```
cl /nologo /W4 /std:c11 /O2 /DUNICODE /D_UNICODE freeze_probe.c /link advapi32.lib
```

Do not execute that command merely because it is documented here. No compiler,
SDK, dependency, driver, or certificate was installed for this source task. The
source statically checks the fixed x64 ABI sizes: request 56 bytes, result 88
bytes. It deliberately rejects non-x64 targets. No C++ compiler or third-party
runtime is required.

## Fixtures and handle ownership

At the supplied mount root, the parent creates precisely:

- `sprig-freeze-A.bin`
- `sprig-freeze-B.bin`

Both use `CREATE_NEW`, are explicitly initialized as 65,536 zero bytes, and
remain open. Any existing file with either name causes a setup failure; the
harness never replaces, truncates, renames, or deletes a pre-existing file.
Only files successfully created by this invocation receive its ownership bit.
The parent retains the original file handle, section handle, and writable view
through both trials and until all its operations and children are quiescent.
No `FILE_SHARE_DELETE` permission is granted.

There are two independent writer processes, one per file. Each process inherits
exactly a three-handle whitelist using `STARTUPINFOEX` and
`PROC_THREAD_ATTRIBUTE_HANDLE_LIST`:

1. Its original file handle, referring to the same original `FILE_OBJECT`.
2. Its original file-backed section handle.
3. A private, unnamed pagefile-backed control-section handle.

There is no globally guessed object name, shared filesystem control file, or
cross-inheritance of the other writer's handles. The child validates the
control header and per-invocation nonce. Inherited numeric handles are internal
child-launch arguments and are not included in results.

Each child creates its one original `FILE_MAP_WRITE` view, pre-faults all of its
4 KiB pages before advertising readiness, and keeps that view alive across both
trials. Its main token writer uses an aligned 64-bit slot at offset zero. An
independent background writer continuously executes `InterlockedExchange64` on
an aligned slot at offset 4,096 of that same view, yielding between stores but
never pausing for Cc invocation or token commands. This covers invoking the
kernel operation while resident stores are already active, instead of testing
only an idle file. A third, independent heartbeat thread updates only the
pagefile control mapping approximately every millisecond; scheduling delays
can make that interval longer. Background completed-store counts and QPC pairs
are sampled with an even/odd sequence guard and must resume after release.

The parent also retains its own original writable view. Its post-release slot
check is explicitly a mapped-memory check, not an independent backing-store
read or a durability claim.

## Request protocol

The parent starts a separate synchronous `DeviceIoControl` thread per file.
Each request has its own manual-reset ready and release event. Its
`FileHandle` is the exact handle passed by that same thread to
`DeviceIoControl`, not a separately opened file or control-device handle. The
request contains fixed-width x64 handle values and the expected volume serial;
all reserved fields start at zero. Handles/events are not closed until the
request thread is confirmed finished.

`SPRIG_FSCTL_FREEZE_PROBE` requires file write access. An IOCTL transport success
is not a freeze success. In particular, a `DEVICE_BUSY` result from a failed
try-lock, a refused request, and a coherency failure remain unsuccessful even
when the diagnostic result was returned successfully.

A completed hold requires all of:

- The returned byte count, ABI version/size, mode, nonce, file size, and volume
  serial match the request.
- Ready was observed and the parent successfully signaled release.
- `Phase == SPRIG_PROBE_PHASE_RELEASED`.
- The unmodified numeric `ResultStatus == 0` and `WaitStatus == 0`.
- Coherency mode has numeric `FlushStatus == 0`. Lock-only mode has
  `FlushStatus == 0xC00000BB` (`STATUS_NOT_SUPPORTED`), never a fictitious
  successful coherency call.
- Kernel QPC frequency matches user QPC frequency and kernel timestamps are
  ordered around the parent's explicit release.

`EXPIRED`, `CANCELLED`, refusal, malformed results, early completion, and
nonzero result/wait status can never produce the successful experiment outcome,
regardless of observed file bytes or transport status.

## Experiment sequence and evidence

The harness runs one lock-only negative control and then, if operations remain
safely recoverable, one coherency trial. A failed but safely returned negative
control may still be followed by a diagnostic coherency trial, but the combined
outcome can never be successful without a valid negative control.

For each trial:

1. Both token writers complete a fresh baseline token and return to an
   acknowledged idle point; their background-store lanes remain active.
   No outstanding pre-ready token store is counted as a post-ready token test.
2. Start A's IOCTL and wait for A's ready event. Only then issue A's test token
   and start repeatedly issuing/acknowledging B tokens. B is not frozen yet.
   Observe approximately 40 ms, requiring at least two B acknowledgments.
3. Quiesce B at its last acknowledged token, start B's IOCTL while A remains
   held, and wait for B ready. If either hold has already completed, reject the
   observation instead of pretending the holds overlapped.
4. Issue B's overlap token. If A's first test is still blocked, retain that
   outstanding token. If it completed, issue A a fresh overlap token. Observe
   approximately another 40 ms with both IOCTLs still pending.
5. Signal both release events before waiting on either IOCTL. Join both calls.
   Require every outstanding token to acknowledge and preserve its final ack
   timestamp. A late ack strictly before kernel EndCounter still counts as a
   counterexample, even if it arrived after the last observation sample. For
   a no-in-hold-ack outcome, these recovered pending acks must be at or after
   EndCounter. Then require background-store progress to resume, issue and
   acknowledge one additional fresh token per writer, and check mapped slots.

For each requested token, the child records this ordered sequence:

```
request received
attempt QPC recorded
attempt_started = requested token
InterlockedExchange64(original mapped slot, token-derived value)
ack QPC recorded
ack = requested token
```

The parent records token publication and observation QPC values. Once the
kernel returns, issue/attempt/observation QPC must be strictly inside the
matching kernel `ReadyCounter` to `EndCounter` interval. An observed
acknowledgment must also fall strictly inside it. The kernel's `EndCounter` is
taken before unlock, so acknowledgments outside that conservative window are
not claimed as counterexamples. B's unfrozen progress must fit within A's hold,
and both final observations must fit within the common hold window.

A's outstanding token may have begun after A ready but **before B ready**. That
is intentional: a writer blocked in a mapped store cannot fetch a later token.
The result indicates whether A's pending attempt was reused. The harness does
not claim that both stores began simultaneously, that the two freezes share
one atomic cut, or that either cut covers an entire filesystem tree.

A valid negative control requires A to acknowledge while A is held, B to keep
progressing before its own hold, and both writers to acknowledge in the overlap
window. A coherency observation requires attempts from both writers, the same
unfrozen-B evidence, and both independent heartbeats to advance during **each**
observation window. Lack of acknowledgments by itself is never sufficient.
An unacknowledged token must have at least 30 ms between its recorded attempt
and observation; late scheduling otherwise makes the observation inconclusive.
Only after explicit successful kernel release, recovered/fresh token
acknowledgments, and resumed background progress may the harness report
`no_in_hold_ack_observed`. A sampled background store whose attempt and ack
both fall strictly inside a validated hold is also a counterexample. Background
sampling is not an exhaustive trace and can miss stores between samples.

## Results and interpretation

The parent emits one newline-terminated JSON object using a fixed field and
reason-string whitelist. It never prints mount paths, command lines, handle or
kernel-pointer values, credentials, raw buffers, or arbitrary system error
messages. Original NTSTATUS values are emitted as signed decimal 32-bit
numbers, preserving their underlying bit patterns; Win32 errors are emitted as
unsigned numbers. No result field silently substitutes transport status for
probe status. Unjoined IOCTL result buffers are not read, because their worker
threads may still be updating them.

The top-level outcomes are:

- `no_in_hold_ack_observed`: valid negative control, bounded no-ack observation,
  independent heartbeat progress, active background-store coverage, correct
  successful release, post-release recovery, and normal own-fixture cleanup.
  Exit code 0 means these mechanical checks passed, not that freezing is proved.
- `falsified`: a post-ready mapped store acknowledged inside a validated
  coherency hold, with the other validity checks and valid negative control.
  Exit code 1.
- `inconclusive`: any prerequisite/control/timing/recovery/cleanup failure,
  refused or expired hold, or uncertainty. Exit code 2 if it can exit safely.
- `not_run`: missing explicit argument or unavailable performance counter.
  Exit code 2.

The literal outcome is not a general theorem. A writer can be descheduled
between publishing attempt_started and executing its store, or between the
store and its acknowledgment. An independent heartbeat proves process
scheduling, not execution of that specific instruction. Thus a no-ack result
is consistent with blocking but does not prove it, even with minimum attempt
windows and eventual recovery. It establishes neither future timing behavior,
fairness, absence of alternate writers, nor whole-tree snapshot semantics.

The JSON always states:

```
"backing_store_snapshot_verified": false,
"whole_tree_snapshot_verified": false
```

This task does **not** modify memfs to capture an independent RAM-backend copy.
That coverage is missing. No cached read, shared mapping read, flush result,
lock acquisition, or lack of ack is substituted for that absent oracle. There
are no file-view CAS, rename, truncation, ADS, directory, reopen, or remapping
scenarios.
The source makes no claim about application-level transactions or filesystem
namespace consistency.

## Timeout, retention, and cleanup behavior

Readiness is bounded at 5 seconds; ordinary token/recovery waits at 2 seconds;
IOCTL-thread and child shutdown joins at 5 seconds each. These user-mode waits
do not make a kernel coherency call cancellable. No `TerminateThread`,
`TerminateProcess`, forced unmap, cancellation of a shared handle, external
process kill, driver unload, or dispatcher stop is used.

If a kernel call does not return, or a writer cannot recover, the parent emits
an inconclusive JSON diagnostic with `retention_required: true`, flushes stdout,
and deliberately remains alive holding its original handles, views, control
mappings, and children. If it has not begun ordinary shutdown, independent
heartbeats keep running. It does not exit and let Windows forcibly dismantle
live mappings, then mislabel that teardown as a successful release. A hang is
not a pass. A future separately authorized guest supervisor is responsible for
preserving diagnostics and resetting/disposing of the guest if necessary.

Ctrl+C/Break requests an orderly stop when the parent can respond, not forced
termination. Windows can still kill a process during console closure, OS
shutdown, or an external administrative action; the harness cannot prevent
that. Do not use any such forced teardown as evidence of a completed test.

During normal cleanup, only confirmed-finished IOCTL threads have their event
and thread handles closed. The parent cooperatively stops both child
processes and waits for them before unmapping or closing original resources.
It deletes only its own successful `CREATE_NEW` fixtures, by their retained
owning handles using `FileDispositionInfo`, not by pathname. Existing-name
collisions, unrelated files, external memfs processes, dispatchers, and drivers
are untouched. Cleanup failure changes the result to inconclusive.

## License

Copyright (c) 2026 M4n5ter

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell copies
of the Software, and to permit persons to whom the Software is furnished to
do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.
