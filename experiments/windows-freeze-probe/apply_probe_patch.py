#!/usr/bin/env python3
"""MIT-licensed source preparation only; never builds, signs or runs software.

The integrator supplies the separately downloaded, hash-verified official source.
This directory must contain the original protocol and GPL experimental helper.
"""

from __future__ import annotations

import argparse
import hashlib
import os
from pathlib import Path
import stat
import tempfile

COMMIT = "ddca7bd5481857a65ba552f643b8776fd070836f"
BASELINE_FSCTL_SHA256 = "a249e32a77f760f1ff80a6f4bb1b336ac572cfdfba14ef43842e6f2ec124b72f"
INCLUDE_ANCHOR = b"#include <sys/driver.h>\n"
INCLUDE_INSERT = b"""
/* Local GPL experiment only; never present in an ordinary WinFsp build. */
#if defined(FSP_TEST_FREEZE_PROBE)
#include "sprig-freeze-probe.h"
#endif
"""
DISPATCH_ANCHOR = b"""static NTSTATUS FspFsvolFileSystemControl(
    PDEVICE_OBJECT FsvolDeviceObject, PIRP Irp, PIO_STACK_LOCATION IrpSp)
{
    PAGED_CODE();

    NTSTATUS Result = STATUS_INVALID_DEVICE_REQUEST;
    switch (IrpSp->MinorFunction)
    {
    case IRP_MN_USER_FS_REQUEST:
        switch (IrpSp->Parameters.FileSystemControl.FsControlCode)
        {
"""
DISPATCH_INSERT = b"""#if defined(FSP_TEST_FREEZE_PROBE)
        case SPRIG_FSCTL_FREEZE_PROBE:
            Result = FspSprigFreezeProbe(FsvolDeviceObject, Irp, IrpSp);
            break;
#endif
"""


def no_reparse(path: Path) -> None:
    """Refuse symlinks and Windows reparse points in existing ancestors."""
    for item in (path, *path.parents):
        try:
            info = item.lstat()
        except FileNotFoundError:
            continue
        if stat.S_ISLNK(info.st_mode) or getattr(info, "st_file_attributes", 0) & 0x400:
            raise ValueError("source preparation refuses reparse paths")


def read_regular(path: Path) -> bytes:
    no_reparse(path)
    if not path.is_file():
        raise ValueError("expected source file missing")
    data = path.read_bytes()
    if len(data) > 1024 * 1024:
        raise ValueError("unexpectedly large source input")
    return data


def create_identical_or_new(path: Path, data: bytes) -> None:
    no_reparse(path)
    if path.exists():
        if read_regular(path) != data:
            raise ValueError("refusing to replace a different existing helper/protocol")
        return
    with path.open("xb") as stream:
        stream.write(data)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--source", required=True, type=Path)
    args = parser.parse_args()
    # Do not resolve away a reparse point before checking it.
    source = Path(os.path.abspath(args.source))
    package = Path(os.path.abspath(__file__)).parent
    no_reparse(source)
    no_reparse(package)
    if source.name != "winfsp-" + COMMIT or not source.is_dir():
        raise ValueError("expected the fixed official WinFsp source directory")
    target = source / "src" / "sys" / "fsctl.c"
    current = read_regular(target)
    # Idempotence is accepted only if stripping our two EXACT insertions restores
    # the pinned upstream hash. Never patch an arbitrary later upstream file.
    stripped = current
    if INCLUDE_INSERT in stripped or DISPATCH_INSERT in stripped:
        if stripped.count(INCLUDE_INSERT) != 1 or stripped.count(DISPATCH_INSERT) != 1:
            raise ValueError("partial or duplicated experimental patch")
        stripped = stripped.replace(INCLUDE_INSERT, b"", 1)
        stripped = stripped.replace(DISPATCH_INSERT, b"", 1)
    if hashlib.sha256(stripped).hexdigest() != BASELINE_FSCTL_SHA256:
        raise ValueError("fsctl.c does not match the pinned upstream baseline")
    if stripped.count(INCLUDE_ANCHOR) != 1 or stripped.count(DISPATCH_ANCHOR) != 1:
        raise ValueError("expected unique source anchors")
    patched = stripped.replace(INCLUDE_ANCHOR, INCLUDE_ANCHOR + INCLUDE_INSERT, 1)
    patched = patched.replace(DISPATCH_ANCHOR, DISPATCH_ANCHOR + DISPATCH_INSERT, 1)
    if current not in (stripped, patched):
        raise ValueError("unexpected placement of an existing experimental patch")
    helper = read_regular(package / "sprig-freeze-probe.h")
    protocol = read_regular(package / "probe_protocol.h")
    if b"SPDX-License-Identifier: GPL-3.0-only" not in helper:
        raise ValueError("experimental driver helper must retain its GPL notice")
    if b"SPRIG_FREEZE_PROBE_PROTOCOL_H" not in protocol:
        raise ValueError("missing expected protocol header")
    helper_target = source / "src" / "sys" / "sprig-freeze-probe.h"
    protocol_target = source.parent / "probe_protocol.h"
    # Validate every preexisting destination before changing any file.
    for dest, data in ((helper_target, helper), (protocol_target, protocol)):
        no_reparse(dest)
        if dest.exists() and read_regular(dest) != data:
            raise ValueError("different existing helper/protocol; no files changed")
    create_identical_or_new(helper_target, helper)
    create_identical_or_new(protocol_target, protocol)
    if current != patched:
        # Atomic replacement of the already validated source file. Recheck its
        # bytes immediately before replacement; an uncertain outcome is not retried.
        fd, temporary_name = tempfile.mkstemp(prefix=".sprig-probe-", dir=target.parent)
        temporary = Path(temporary_name)
        try:
            with os.fdopen(fd, "wb") as stream:
                stream.write(patched)
                stream.flush()
                os.fsync(stream.fileno())
            no_reparse(target)
            if read_regular(target) != current:
                raise ValueError("fsctl.c changed during preparation")
            os.replace(temporary, target)
        finally:
            if temporary.exists():
                temporary.unlink()  # only this invocation's new temp source file
    print("Prepared fixed-source test-only FSCTL; no build, execution or security changes.")
    print("fsctl_sha256=" + hashlib.sha256(patched).hexdigest())
    print("helper_sha256=" + hashlib.sha256(helper).hexdigest())
    print("protocol_sha256=" + hashlib.sha256(protocol).hexdigest())
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
