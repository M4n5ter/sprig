# Local experiment: reuse verified bytes during first span discovery

Base: `4eb0f4d7afdb19bf22d9667dc520ede8b162c737`.
Only `src/writable.rs` changes executable code and adds five regression tests.
This document records design, local evidence and limitations; the diagnostic
harness, binaries and raw measurements are not included in this commit.

## Mechanism and correctness boundary

The first read that reaches visible base content must decode and verify every
base chunk to establish trustworthy spans. Copy each requested intersection
from those already verified bytes into this read's output, rather than fetching
selected chunks again immediately. For N base chunks and K selected chunks,
source reasoning predicts N+K to N reader calls on discovery; later reads remain
K. These are structural counts, not observed I/O or hash counters.

All base chunks still validate on discovery, including unrequested, overwritten
and truncated-away chunks. Empty chunks, checked length accumulation, base-size
bounds and final size equality remain checked. Spans publish only after complete
validation; a partial copied output is never returned on failure. Requested-range
overflow is reported after base validation, preserving the previous error order.
The helper's start + output length is bounded by the caller's checked range end;
an overflowing request passes an empty buffer while discovery still validates.

No verified content survives the current read. Later reads still fetch, decode,
verify and length-check selected chunks. Truncate visibility, zero-filled growth,
hardlink inode sharing and journal application order remain unchanged. Every
historical journal extent overlapping the requested range is still read and
checksummed, even if a newer extent overwrites it. Locks, barriers, poison,
journal append, durable publication and recovery behavior are not changed.

Output allocation moves before discovery and may overlap validation of all
chunks. Its length is bounded by MAX_WRITE_BYTES (1 MiB), but this is NOT a bound
on total peak memory: decoded chunks, spans and other existing allocations also
remain live. Allocation/OOM timing may differ; no memory measurement or reduced
RSS claim is made. There is no persistent content cache.

## Correctness validation already performed by the parent task

Local log: `/tmp/sprig-verified-read-reuse-root-validation-20261027.log`.
The parent ran five focused verified_base_read tests, all 68 projection library
tests, and all-targets projection clippy with warnings denied; these passed.
Tests cover partial/cross-chunk/EOF reads, real packed/zstd backing, subsequent
read corruption detection, overlays, truncate/regrow, hardlinks, historical
extent checksums, corruption outside the requested range, failed-discovery
nonpublication, base-size errors and range-overflow error precedence.
These are local tests, not mounted qualification or a new CI result. No tests or
builds were rerun when adding this document.

## Actual microbenchmark observations and limits

Valid local attempt: `sprig-read-diagnostic/build-attempts/isolated-3X8porZm`.
Raw samples SHA256:
`b0f627018b2b245b65e14891c3b2ef2f0338dc49aac823202387bc748e2bcd28`.
Validated-build manifest SHA256:
`d087999123b4dd08589343ef933cc66231e1757126f4f214c064d045a819329b`.
The earlier shared-target build incorrectly reused the baseline executable and
is INVALID; it produced no benchmark results and is excluded. The valid attempt
used distinct existing target directories, distinct ELF hashes, matching harness
source hashes and test-list identity checks.

The valid experiment has 384 exact-content-checked records: 16 alternating AB/BA
pairs for raw/zstd x within/cross/full x first/later. It measures one read of a
four-chunk, 256 KiB fixture with warmed OS caches, on a microsecond timescale.
Engine creation, fixture setup, byte-oracle comparison and teardown are outside
timing. It is not whole-cargo, build throughput, mounted filesystem performance,
cold disk, end-to-end workspace performance or a LayerFS comparison.

Paired geometric candidate/baseline time ratios for first reads were:

| Backing | Within chunk | Cross boundary | Full file |
|---|---:|---:|---:|
| raw | 0.768 | 0.722 | 0.839 |
| zstd | 0.855 | 0.669 | 0.568 |

These observations support the targeted mechanism under this fixture only.
Later-read negative controls are not clean: paired median ratios were
1.001-1.029, with candidate median paired deltas +0.071 to +1.126 microseconds.
Small overhead cannot be separated from environmental noise here. A baseline
zstd-cross later outlier reached 1678.360 microseconds versus median 60.076;
its geometric ratio misleadingly looks favorable despite paired median near 1.
Candidate raw-full first reached 793.081 microseconds versus median 200.161.
Execution-order and early/late splits also changed some effect magnitudes.
Do not claim later-read non-regression, stable tail improvement or a broad speedup.

The local ANALYSIS.md, analysis.json and evidence-sha256.txt retain all twelve
scene medians, paired ratios, nearest-rank p90/max, order splits and raw identity.
No outliers were removed; sixteen pairs do not establish reliable population
tails. No trace/ptrace was used. Future benchmark preparation must pin this local
experiment commit, preserve exact content oracles and keep this evidence scoped.
Publication, CI and broader acceptance remain separate pending decisions.
