# Experimental per-file coherency probe

Source-only preparation against WinFsp commit
`ddca7bd5481857a65ba552f643b8776fd070836f`. Nothing in this directory authorizes
loading the modified driver, changing test-signing, creating certificate trust,
turning off Secure Boot, enabling Verifier, or executing the harness. Those guest
operations require their separate, explicit approval.

## Build integration and licensing

The sole existing WinFsp file changed is `src/sys/fsctl.c`. It conditionally
includes `src/sys/sprig-freeze-probe.h` and dispatches one private FSCTL only when
`FSP_TEST_FREEZE_PROBE` is defined. An ordinary build has neither this entrypoint
nor helper. No project, installer, DLL, service-registration, or CI file changes
are made here. The integrator owns all compilation.

Keep the layout `probe_protocol.h` next to the pinned WinFsp source directory:
the kernel helper includes it using `../../../probe_protocol.h`. Build x64 with
the real PagingIoResource; the helper rejects a build with
`FSP_FILE_NODE_NO_PGIO`. ABI sizes are statically checked: 56-byte input, 88-byte
output. Kernel changes remain GPL-3.0; the original protocol and user-mode test
are MIT. These files are not a Sprig feature or a production filesystem API.

## Guards and object lifetime

The request is METHOD_BUFFERED and requires FILE_WRITE_ACCESS. It is accepted
only at PASSIVE_LEVEL in the original user requestor's process, with this exact
IRP as TopLevelIrp and no preexisting WinFsp resource flags. WOW64 is refused.
The current effective token must already possess and enable
SeLoadDriverPrivilege, matching WinFsp's existing privileged unload boundary.
The user-mode controller also checks its administrative token.

Only the explicitly named `-SPRIGPROBE` volume parameters with no UNC prefix,
matching volume serial, and the two fixed root filenames are accepted. Each file
must be a regular non-stream file of exactly 64 KiB. These are mistake-prevention
guards, not proof that a malicious administrator supplied a RAM backend.

The request carries three user handles, never kernel pointers. The file handle
is referenced in UserMode as an IoFileObjectType with read/write data access and
must equal the IRP's exact FILE_OBJECT. Ready and release handles are referenced
as ExEventObjectType in UserMode with EVENT_MODIFY_STATE and SYNCHRONIZE rights,
respectively. Aliased or initially signalled events are refused. All successful
references are released in finally. The pending IRP additionally retains its
file object.

## Lock and coherency experiment

One owner/request thread acquires one FileNode, Main then Paging, with a
nonblocking TryFull. Busy is a refusal, not a hidden indefinite resource wait.
Both exclusive acquisitions are checked. The existing per-file top-level IRP
flags let recursive paging I/O use WinFsp's existing same-stream path; the test
does not loop over two streams under one IRP.

LOCK_ONLY is the negative control. It makes no coherency call and reports
STATUS_NOT_SUPPORTED in FlushStatus, rather than pretending it flushed.
COHERENCY calls CcCoherencyFlushAndPurgeCache directly over the entire stream,
offset NULL, length zero, flags zero. The original status is retained. Only
exact STATUS_SUCCESS reaches ready; notably STATUS_CACHE_PAGE_LOCKED is refused
although NT_SUCCESS would accept it. No reserved flags, false "no writable
views" assertion, split flush/purge fallback, or paging/dispatcher shutdown is
used.

The observation hold is bounded to at most 1000 ms (ABI default 250 ms). A
10-ms poll checks cancellation. A release signalled before ready is refused;
expiration or cancellation never counts as successful completion. Only the
same original thread releases the ERESOURCEs, in finally. The Cc call itself is
synchronous and has no safe cancellation mechanism here: a hang in Cc is
unknown, not a completed bounded freeze. The external isolated-guest supervisor
is responsible for diagnosing such a hang after obtaining the required scope.

## Result contract

Validation failures before the diagnostic body return an ordinary failing
NTSTATUS. Once the diagnostic body starts, the FSCTL transport returns success
so the entire result structure is available even when the experiment refused.
`DeviceIoControl == TRUE` is therefore **not** evidence of freezing.

A completed observation requires all of:

- Exact output size, version, nonce and expected mode.
- Phase RELEASED, ResultStatus exactly zero and WaitStatus exactly zero.
- For COHERENCY, FlushStatus exactly zero.
- Independent writer attempts and heartbeat observations, not merely no writes.

Every output byte is zero-initialized. There are no kernel pointers, addresses,
file contents or credentials in the result. QPC ReadyCounter precedes ready
signalling; EndCounter is sampled **before** the owner unlocks, conservatively
bounding the measured hold window. A post-ready token's acknowledgement strictly
inside that window can falsify the candidate. A lack of acknowledgement cannot
prove all CPU/MM interleavings are safe.

## Explicit first-cut limitations

This patch adds no paging-IRP PRELOCK/POSTLOCK instrumentation, because its scope
is limited to fsctl.c and the test helper. It does not independently copy the RAM
backend or prove that a blocked store stopped specifically at a refault lock.
The harness must disclose these missing observations; cached ReadFile is not a
substitute for an independent backing-store snapshot. A completed sample is at
most an unfalsified observation under that run's conditions.

No namespace gate, section-admission protocol, rename/truncate support,
multi-file application-consistency, storage epoch, persistence, CAS publication
or GC protocol is implemented. Existing mappings and their handles stay live
during the observations; the harness does not close them to manufacture success.

## Primary references

- https://learn.microsoft.com/en-us/windows-hardware/drivers/ddi/ntifs/nf-ntifs-cccoherencyflushandpurgecache
- https://learn.microsoft.com/en-us/windows-hardware/drivers/ddi/wdm/nf-wdm-obreferenceobjectbyhandle
- https://learn.microsoft.com/en-us/windows-hardware/drivers/ddi/ntddk/nf-ntddk-sesingleprivilegecheck
- https://github.com/winfsp/winfsp/blob/ddca7bd5481857a65ba552f643b8776fd070836f/src/sys/file.c#L456-L512
- https://github.com/winfsp/winfsp/blob/ddca7bd5481857a65ba552f643b8776fd070836f/src/sys/callbacks.c#L303-L340
