# Experiment license boundary

This directory is an isolated, source-only experiment, not a Sprig filesystem
feature. It does not change the MIT license of the Sprig product.

- `sprig-freeze-probe.h` and the resulting modified WinFsp driver are
  GPL-3.0-only. See `LICENSE-GPL-3.0.txt`.
- The original protocol, user-mode harness, preparation script, and documentation
  are MIT. See `LICENSE-MIT.txt` and the individual SPDX notices.
- The preparation script inserts only the explicitly documented experiment hooks
  into separately obtained upstream source. It does not relicense upstream code.

The exact upstream source is WinFsp commit
`ddca7bd5481857a65ba552f643b8776fd070836f`:
https://github.com/winfsp/winfsp/tree/ddca7bd5481857a65ba552f643b8776fd070836f

Upstream's own license, notices, and applicable exceptions remain authoritative:
https://github.com/winfsp/winfsp/blob/ddca7bd5481857a65ba552f643b8776fd070836f/License.txt

The build-only workflow does not distribute Microsoft tools or generated driver
binaries. Building does not authorize installation, execution, certificate trust,
or any host or guest security-setting change.
