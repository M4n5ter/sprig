# Isolated Windows storage-snapshot experiment

This is an experimental requester and synthetic acceptance test, not a new Sprig
checkpoint implementation. Sprig's existing strict checkpoint is unchanged.
All original source in this directory is MIT licensed, copyright M4n5ter.

## Contract

The live view is an ordinary NTFS directory, including open native file handles
and mappings. The Microsoft software VSS provider supplies a stable storage
snapshot that remains alive until the collector finishes. `FILE_SHARE_BACKUP`
uses no application writers and is nonpersistent. It is not a transaction across
application state, a snapshot of all unflushed memory, or a durable journal-prefix
API. Microsoft explicitly states that memory-mapped files and paging I/O are not
frozen during the provider's commit window.

This first test uses ordinary synthetic files only. It does not establish
production support for arbitrary trees, ACLs, streams, links, application writer
coordination, or interactive Windows desktop deployment. The management API used
for the bounded diff association has a Windows Server support boundary.

## Isolation and limits

The dispatch-only workflow compiles using the runner's existing official Visual
Studio/Windows SDK. The runtime input defaults to false. Enabling it requires the
explicitly approved disposable-runner test scope:

- A new, identity-checked dynamic VHDX, 1 GiB (within the approved 4 GiB maximum), containing only a new NTFS
  volume. No existing disk is initialized or formatted.
- Exactly the Microsoft system provider, with a maximum 512 MiB diff association
  from the new volume to itself. Existing associations are not resized. A larger
  system minimum or unsupported volume causes refusal, not a registry change or
  quota increase.
- CAS, temporary staging, restored trees and logs outside the snapshotted volume,
  with a 2 GiB synthetic-output limit. Fixed fixture sizes/capture counts and bounded
  child log sinks constrain normal and known-error-path writes; sampled checks
  stop further work before the reserved allowance is exhausted. This is not a
  filesystem quota or protection against arbitrary OS corruption. The containing host volume is never added
  to the snapshot set.
- Delete only the recorded snapshot ID without force; remove only the newly
  owned association after confirming snapshots are gone. Unknown cleanup is
  reported and never converted into forced volume removal. Nonpersistent snapshots
  can still auto-release when the requester object/process exits; that is not
  proof of cleanup, and this experiment does not promise an unknown child can
  continue reading such a snapshot. The live source is not rolled back or deleted.
- No driver install, test signing, certificate, boot/security setting change,
  application suspension, or service-wide VSS stop.

VSS and the Microsoft software provider may activate their existing Windows
services. This is not a promise of unprivileged snapshot creation; the experiment
uses the disposable runner's existing administrator context, without creating a
persistent privileged helper.

## Measurement and correctness

The bounded sequence distinguishes initial capture, capture while a bounded live
writer is running, settled changed content and an identical repeated capture.
A sealed file and flushed pre-token are checked. A post-snapshot live token and
repeated full snapshot hashes test independence from subsequent live mutations.
Mutable concurrent files need not match the live tree or each other at a single
application transaction boundary.

The ordinary Windows path validator still rejects device paths. The original
staged collector remains available as the baseline. The opt-in `vss-experiment`
feature adds a hidden, synthetic-only direct-import command: it accepts exactly
this requester-owned snapshot fixture, validates the volume marker, namespace,
read-only no-follow handles, metadata and exact EOF, and streams content into the
existing ObjectBatch. Store validation and durable batch completion precede
revision/head publication. It is not a general public VSS API, a lazy mount, or
permission to open arbitrary GLOBALROOT paths. The requester must keep its
immutable snapshot alive throughout the call.

The completed six-file direct-import acceptance used the same full-read,
post-token, CAS listing, native-restore and repeated-content oracles. Three
invalid identity/root calls must fail without changing state counts; unit tests
cover read/metadata/batch-publication failures. Direct capture omits the staging
copy, not its correctness checks, whose time remains reported separately.

Baseline run [40100028865](https://github.com/M4n5ter/sprig/actions/runs/40100028865)
passed all four captures, including active mmap/ordinary writes and exact cleanup.
Its file-allocation fields used GetCompressedFileSizeW, which returns logical
length for ordinary uncompressed nonsparse files. Preserve that raw evidence,
but do not call its 428.675 MiB host file-length sample physical allocation or a
peak. Current measurement uses FILE_STANDARD_INFO.AllocationSize instead.

Report snapshot creation, snapshot reads/staging, CAS import, native restore and
cleanup separately. Report live file logical/allocated bytes, VSS diff used and
allocated bytes, complete CAS/staging/restore allocation, and actual VHDX file
allocation. This experiment has no file-clone template cache; its template allocation is zero. Diff storage is inside the VHDX: do not add it again when computing
host physical totals. Sampling is not a continuous peak measurement. No speed or
space improvement is claimed before actual measurements.

## Official contracts

- https://learn.microsoft.com/en-us/windows/win32/vss/shadow-copy-context-configurations
- https://learn.microsoft.com/en-us/windows/win32/api/vsmgmt/nf-vsmgmt-ivssdifferentialsoftwaresnapshotmgmt-adddiffarea
- https://learn.microsoft.com/en-us/windows/win32/api/vsprov/nf-vsprov-ivssprovidercreatesnapshotset-commitsnapshots

## 中文摘要

这是独立的在线“存储时点快照”试验，不修改 Sprig 原有严格 checkpoint。
原生 NTFS 文件和映射继续存在；VSS 不保证所有未同步内存写入或多文件应用事务的原子性。
首次仅验合成普通文件，使用固定微软 provider、无应用 writer 的非持久快照。
测试仅格式化本次新建且已核身份的1 GiB 虚拟盘（不超过已批 4 GiB 上限），差分区只在该卷内部、上限 512 MiB。
CAS、暂存和恢复目录在卷外，额外合成输出上限 2 GiB。不改变已有卷、注册表、启动设置或信任配置。
仅显式清理本次精确快照及新关联；不确定状态拒绝强制卸载。非持久快照仍可能随 requester 退出自动释放，不能把这种释放当成已核清理或承诺异常子进程可继续读取。
原暂存收录保留作基线。当前显式试验使用 vss-experiment feature 下的受限直读入口，
仅接受 requester 保持存活的固定快照、marker 与六文件白名单，逐只读 handle 校验身份、元数据与精确 EOF，
复用 ObjectBatch 和 Store 持久发布次序；不是通用 GLOBALROOT 开放或正式 VSS API。
全部恢复/重复内容/快照稳定性检查仍保留，新增错误身份与根路径拒绝测试，不改变严格 checkpoint。
基线四轮已通过，但其 GetCompressedFileSizeW 字段对普通文件实际是逻辑长度口径，428.675 MiB 不代表物理峰值。
新测量改用 FILE_STANDARD_INFO.AllocationSize，仍不等于卷级独占占用或连续峰值；不冒称全场景加速、省空间。

## Bounded Cargo acceptance (candidate, not yet runtime verified)

The next explicit runtime dispatch uses a dependency-free Rust library and its
real Cargo target directory, not the six-file benchmark. Development and test
profiles retain incremental compilation, disable debug information, and use one
codegen unit. Clean, no-op and fixed-edit tests precede one storage snapshot.
Source plus target must fit 32 MiB summed across paths (including aliases), 512
entries and 16 MiB per file; exceeding a limit fails rather than dropping target
files or changing the profile. A native restore must match every supported byte,
mtime, mode and internal hardlink group, then actually report Cargo `Fresh`.
Another source edit and test must work in that restored directory.

The requester owns the VSS Session until its entire child job has drained. It
opens the snapshot's fixed fixture directory read-only and explicitly inherits
only that handle and NUL standard streams. The Python collector forwards only
that one capability to the hidden import command. A directory handle does not
itself keep a VSS snapshot alive: the requester/child lifetime protocol remains
mandatory. The importer checks granted access, handle type and bound snapshot
identity, then opens children relative to retained directory handles. It does
not grant ordinary Sprig APIs permission to traverse arbitrary device paths.
This remains a restricted experiment, not an unprivileged production broker.

The same 1 GiB new NTFS VHDX, same-volume 512 MiB diff cap, fixed Microsoft provider
and 2 GiB total synthetic-output allowance apply. One Cargo capture replaces the
four former captures; the larger external reserve is retained conservatively.
No registry, ACL, service startup, driver or host-security setting is changed.
Neither a successful build nor a Fresh restore establishes application-atomic
snapshot semantics or Windows desktop compatibility.

The Cargo oracle obtains Windows metadata through no-follow read-only handles,
using one query family for identity, size, links, mtime and the complete attribute
word. Attribute changes still fail capture; no unknown bits are masked away.
The known `NOT_CONTENT_INDEXED` bit is accepted as a content-indexing hint but is
not persisted, like the archive hint. This is not a full Windows metadata backup:
ACLs, ownership and indexing policy are not part of the canonical restore contract.

Cargo 校验通过只读、不跟随重解析点的句柄统一读取 Windows 元数据，仍完整比较
属性位，变化即拒绝，不屏蔽未知位。已知的 `NOT_CONTENT_INDEXED` 是内容索引提示，
允许读取但不持久化，与归档提示相同；此能力不是完整 Windows 元数据备份，
不承诺恢复 ACL、所有者或索引策略。
