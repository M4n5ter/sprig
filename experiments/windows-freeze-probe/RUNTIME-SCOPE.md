# Disposable guest falsification experiment

The `kernel_probe` workflow input is opt-in and false by default. It rebuilds
pinned WinFsp `ddca7bd5481857a65ba552f643b8776fd070836f` with the test-only helper,
using the fixed Microsoft EWDK. The host-side build retains signing vetoes.
Only the separately opted-in, disposable guest may sign or load its driver.
This experiment is not part of the Sprig product or a released driver.

The guest is Windows Server 2025 Core evaluation, with no network adapter,
two virtual CPUs, 6 GiB RAM and a dynamic disk capped at 40 GiB. Only that guest
uses test signing and a short-lived, non-exportable signing key. Host boot,
trust, security configuration and existing VMs are not modified. Existing guest
VBS/HVCI protection must not be reduced; failure to verify the state stops the
experiment. The temporary guest and generated payload are destroyed afterward.

The only filesystem workload is a fresh RAM backend with two 64 KiB files.
A lock-only negative control must observe resident mapped stores. The coherency
trial then attempts to falsify the proposed per-file barrier using existing
writable views and continuously active background stores. An acknowledgment
inside the held interval is a counterexample. No acknowledgment is merely a
bounded observation, not proof that CPU stores, all faults or an entire volume
can be frozen safely. This does not implement or validate CAS publication,
epoch routing, rename/truncate concurrency or a production snapshot API.

Only a completed ordinary trial with valid controls, recovered writers and no
retention requirement may proceed to the single-driver, one-boot Driver
Verifier run (`winfsp-x64.sys`, mask `0x3B`). No all-driver, low-resource or
forced-pending-I/O validation is enabled. A stuck coherency call is unknown,
not a successful timeout; the filesystem dispatcher is not forcibly stopped.
Only the experiment VM may be stopped as part of bounded teardown.

Artifacts contain a fixed whitelist of build hashes, bounded build logs and
sanitized experiment status. They exclude binaries, private keys, credentials,
answer files, raw memory dumps, pagefiles and virtual disks. This is a laboratory
experiment under WinFsp's separate license, not a change to Sprig's MIT license.
