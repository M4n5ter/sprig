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

- A new, identity-checked dynamic VHDX, maximum 4 GiB, containing only a new NTFS
  volume. No existing disk is initialized or formatted.
- Exactly the Microsoft system provider, with a maximum 512 MiB diff association
  from the new volume to itself. Existing associations are not resized. A larger
  system minimum or unsupported volume causes refusal, not a registry change or
  quota increase.
- CAS, temporary staging, restored trees and logs outside the snapshotted volume,
  with a 2 GiB synthetic-output limit. The containing host volume is never added
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

The current production Windows path validator intentionally rejects device
paths. The collector therefore stages the known snapshot files outside the live
volume, preserving their supported metadata, then uses the actual Sprig writable
import and native restore commands. This extra staging is explicitly measured;
it is not a direct/lazy production VSS ingestion fast path. It does not relax the
production no-reparse or device-path restrictions.

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
测试仅格式化本次新建且已核身份的最大 4 GiB 虚拟盘，差分区只在该卷内部、上限 512 MiB。
CAS、暂存和恢复目录在卷外，额外合成输出上限 2 GiB。不改变已有卷、注册表、启动设置或信任配置。
仅显式清理本次精确快照及新关联；不确定状态拒绝强制卸载。非持久快照仍可能随 requester 退出自动释放，不能把这种释放当成已核清理或承诺异常子进程可继续读取。
当前收录需要显式暂存，成本全部统计，不冒称生产直读快路径或已实现全场景加速、省空间。
