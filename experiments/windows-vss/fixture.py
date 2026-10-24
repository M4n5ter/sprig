#!/usr/bin/env python3
"""Opt-in synthetic VSS oracle and real Sprig CLI CAS ingestion.

This helper does not create, delete, mount, expose, or revert snapshots/volumes.
The requester owns snapshot lifetime and passes its device root to ``capture``.
Only marker-validated, fixed-name ordinary synthetic files are read or mutated.
No claim is made that every unflushed mmap store reaches VSS, or that concurrent
writes across files constitute an application-atomic transaction.

Windows runtime contract (Python 3.9+, standard library):
  prepare --live-root NEW_DIRECTORY [--file-mib 8]
  writer --live-root DIRECTORY --seconds 120 --generation 1
         [--ready-file NEW_FILE] [--stop-file NONEXISTENT_FILE]
  capture SNAPSHOT_ROOT --live-root DIRECTORY --relative-path fixture
          --work-root NEW_OUTSIDE_DIRECTORY --store SHARED_OUTSIDE_STORE
          --sprig ABSOLUTE_EXE --label LABEL [--previous-report capture.json]
          [--direct-import]
  measure PATH [PATH ...]
  selftest                         # tiny local fixtures; never runs Sprig/VSS

Capture keeps its staging tree and Sprig's native restored workspace. It never
deletes existing directories. Timed-out CLI children are NOT killed or retried;
failure reports require retaining the snapshot/volume for operator inspection.
The post-token probe is restored after stable snapshot reads, only if its live
bytes still match our probe, making a subsequent settled capture comparable.
Direct import skips the staging tree, not the full snapshot/hash oracles. Its
first capture into a new store also verifies three rejected identity/root inputs.
"""

import argparse
import ctypes
import hashlib
import io
import json
import mmap
import os
from pathlib import Path, PureWindowsPath
import re
import shutil
import stat
import subprocess
import sys
import tempfile
import threading
import time
import uuid


SCHEMA = 1
KIND = "sprig-vss-synthetic-fixture-v1"
MARKER = ".sprig-vss-fixture.json"
PAYLOADS = ("sealed.bin", "mapped.bin", "written.bin")
TOKENS = ("pre-token.bin", "post-token.bin")
FILES = (MARKER,) + PAYLOADS + TOKENS
MIB = 1024 * 1024
BLOCK = 4096
MAX_FILE = 8 * MIB
MAX_FIXTURE = 64 * MIB
FIXED_NS = 1700000000000000000  # Exactly representable at Windows 100 ns precision.
MAX_MEASURE_FILES = 100000
MAX_MEASURE_BYTES = 2 * 1024 * MIB
MAX_CLI_COMMANDS = 12
MAX_CLI_LOG_BYTES = MIB // 2
VSS_ROOT = re.compile(r"^\\\\\?\\GLOBALROOT\\Device\\HarddiskVolumeShadowCopy[0-9]+\\?$", re.I)


class FixtureError(RuntimeError):
    def __init__(self, message, **details):
        super().__init__(message)
        self.details = details


def require(condition, message):
    if not condition:
        raise FixtureError(message)


def emit(value):
    print(json.dumps(value, sort_keys=True, indent=2), flush=True)


def json_new(path, value):
    encoded = (json.dumps(value, sort_keys=True, indent=2) + "\n").encode("utf-8")
    require(len(encoded) <= MIB, "JSON output exceeds the fixed 1 MiB file limit")
    with Path(path).open("xb") as handle:
        require(handle.write(encoded) == len(encoded), "Short bounded JSON write")
        handle.flush()
        os.fsync(handle.fileno())


def is_reparse(info):
    return bool(getattr(info, "st_file_attributes", 0) & 0x400)


def check_node(path, directory=False):
    info = Path(path).lstat()
    require(not stat.S_ISLNK(info.st_mode) and not is_reparse(info),
            "Links and reparse points are outside the fixture contract: " + str(path))
    require(stat.S_ISDIR(info.st_mode) if directory else stat.S_ISREG(info.st_mode),
            "Unexpected filesystem object: " + str(path))
    if not directory:
        require(info.st_nlink == 1, "Fixture hardlinks are not supported: " + str(path))
        require(bool(info.st_mode & stat.S_IWUSR), "Read-only fixture files are unsupported")
    return info


def restore_drive_path(value):
    """Normalize only Rust's verbatim local-drive result, never device/UNC paths."""
    require(isinstance(value, str), "Restore path must be a string")
    if value.startswith("\\\\?\\"):
        require(re.match(r"^\\\\\?\\[A-Za-z]:\\", value) is not None,
                "Restore path is not a verbatim local drive path")
        value = value[4:]
        require("/" not in value, "Verbatim path must use canonical separators")
        require(all(part and part not in (".", "..") and not part.endswith((".", " "))
                    for part in value[3:].split("\\")), "Ambiguous verbatim path component")
    return value


def ordinary_path(path, must_exist=True):
    """Reject aliases and reparse ancestors for writable ordinary local paths."""
    path = Path(os.path.abspath(path))
    if os.name == "nt":
        raw = str(path)
        require(re.match(r"^[A-Za-z]:\\", raw) is not None,
                "Expected an ordinary absolute local drive path")
        require(":" not in raw[2:], "Alternate stream paths are unsupported")
    chain = list(reversed(path.parents)) + [path]
    for node in chain:
        if node.exists() or node.is_symlink():
            info = node.lstat()
            require(not stat.S_ISLNK(info.st_mode) and not is_reparse(info),
                    "Path contains a link or reparse point: " + str(node))
        elif must_exist:
            raise FixtureError("Path does not exist: " + str(node))
    return path


def stream_check(path):
    """Reject named data streams instead of silently dropping them during stage."""
    if os.name != "nt":
        return
    from ctypes import wintypes
    class StreamData(ctypes.Structure):
        _fields_ = [("size", ctypes.c_longlong), ("name", wintypes.WCHAR * 296)]
    kernel = ctypes.WinDLL("kernel32", use_last_error=True)
    first = kernel.FindFirstStreamW
    first.argtypes = [wintypes.LPCWSTR, ctypes.c_int, ctypes.POINTER(StreamData), wintypes.DWORD]
    first.restype = wintypes.HANDLE
    next_stream = kernel.FindNextStreamW
    next_stream.argtypes = [wintypes.HANDLE, ctypes.POINTER(StreamData)]
    next_stream.restype = wintypes.BOOL
    close = kernel.FindClose
    close.argtypes = [wintypes.HANDLE]
    close.restype = wintypes.BOOL
    data = StreamData()
    handle = first(str(path), 0, ctypes.byref(data), 0)
    if handle == ctypes.c_void_p(-1).value:
        error = ctypes.get_last_error()
        if error == 38:  # ERROR_HANDLE_EOF; directories need not have a data stream.
            return
        raise ctypes.WinError(error)
    try:
        while True:
            require(data.name == "::$DATA", "Named data stream is unsupported: " + str(path))
            if not next_stream(handle, ctypes.byref(data)):
                error = ctypes.get_last_error()
                require(error == 38, "Cannot enumerate file streams, Windows error " + str(error))
                break
    finally:
        close(handle)


def volume(path):
    """Return volume GUID and mounted root, using the nearest existing ancestor."""
    require(os.name == "nt", "VSS capture requires Windows")
    from ctypes import wintypes
    path = ordinary_path(path, must_exist=False)
    while not path.exists():
        path = path.parent
    kernel = ctypes.WinDLL("kernel32", use_last_error=True)
    get_path = kernel.GetVolumePathNameW
    get_path.argtypes = [wintypes.LPCWSTR, wintypes.LPWSTR, wintypes.DWORD]
    get_path.restype = wintypes.BOOL
    get_name = kernel.GetVolumeNameForVolumeMountPointW
    get_name.argtypes = [wintypes.LPCWSTR, wintypes.LPWSTR, wintypes.DWORD]
    get_name.restype = wintypes.BOOL
    mount = ctypes.create_unicode_buffer(32768)
    name = ctypes.create_unicode_buffer(32768)
    if not get_path(str(path), mount, len(mount)):
        raise ctypes.WinError(ctypes.get_last_error())
    if not get_name(mount.value, name, len(name)):
        raise ctypes.WinError(ctypes.get_last_error())
    return name.value.casefold(), mount.value


def outside_volume(live_root, target):
    require(volume(live_root)[0] != volume(target)[0],
            "Staging, CAS, reports and writer control files must be outside the source volume")


def snapshot_identity(snapshot_root):
    """Read the serial and exact synthetic volume marker from one snapshot HANDLE."""
    require(os.name == "nt" and VSS_ROOT.fullmatch(str(snapshot_root)) is not None,
            "Direct import identity requires the requester's VSS device root")
    marker = Path(snapshot_root) / ".sprig-vss-synthetic-volume"
    check_node(marker)
    stream_check(marker)
    from ctypes import wintypes

    class HandleInfo(ctypes.Structure):
        _fields_ = [("attributes", ctypes.c_uint32),
                    ("creation_time", ctypes.c_uint32 * 2),
                    ("access_time", ctypes.c_uint32 * 2),
                    ("write_time", ctypes.c_uint32 * 2),
                    ("volume_serial", ctypes.c_uint32),
                    ("size_high", ctypes.c_uint32), ("size_low", ctypes.c_uint32),
                    ("links", ctypes.c_uint32),
                    ("index_high", ctypes.c_uint32), ("index_low", ctypes.c_uint32)]

    assert ctypes.sizeof(HandleInfo) == 52 and HandleInfo.volume_serial.offset == 28
    kernel = ctypes.WinDLL("kernel32", use_last_error=True)
    open_file = kernel.CreateFileW
    open_file.argtypes = [wintypes.LPCWSTR, wintypes.DWORD, wintypes.DWORD,
                          ctypes.c_void_p, wintypes.DWORD, wintypes.DWORD, wintypes.HANDLE]
    open_file.restype = wintypes.HANDLE
    get_info = kernel.GetFileInformationByHandle
    get_info.argtypes = [wintypes.HANDLE, ctypes.POINTER(HandleInfo)]
    get_info.restype = wintypes.BOOL
    read = kernel.ReadFile
    read.argtypes = [wintypes.HANDLE, ctypes.c_void_p, wintypes.DWORD,
                     ctypes.POINTER(wintypes.DWORD), ctypes.c_void_p]
    read.restype = wintypes.BOOL
    close = kernel.CloseHandle
    close.argtypes = [wintypes.HANDLE]
    close.restype = wintypes.BOOL
    handle = open_file(str(marker), 0x80000080, 7, None, 3, 0x00200000, None)
    if handle is None or handle == ctypes.c_void_p(-1).value:
        raise ctypes.WinError(ctypes.get_last_error())
    try:
        info = HandleInfo()
        if not get_info(handle, ctypes.byref(info)):
            raise ctypes.WinError(ctypes.get_last_error())
        expected_size = len(b"sprig-vss-synthetic-only-v1\n") + 32 + 1
        require(not info.attributes & (0x400 | 0x10) and info.links == 1
                and info.size_high == 0 and info.size_low == expected_size,
                "Snapshot volume marker is not a single-link ordinary file of the exact expected size")
        buffer = ctypes.create_string_buffer(expected_size)
        count = wintypes.DWORD()
        if not read(handle, buffer, expected_size, ctypes.byref(count), None):
            raise ctypes.WinError(ctypes.get_last_error())
        require(count.value == expected_size, "Short snapshot volume marker read")
        match = re.fullmatch(rb"sprig-vss-synthetic-only-v1\n([0-9A-Fa-f]{32})\n", buffer.raw)
        require(match is not None, "Invalid snapshot volume marker format")
        return {"serial": "%08X" % info.volume_serial,
                "marker_token": match.group(1).decode("ascii"), "source": "snapshot_marker_handle"}
    finally:
        if not close(handle):
            raise ctypes.WinError(ctypes.get_last_error())


def deterministic(domain, index, size=BLOCK):
    return hashlib.shake_256((domain + ":" + str(index)).encode("ascii")).digest(size)


def expected_payload_hash(name, size):
    digest = hashlib.sha256()
    for index in range((size + BLOCK - 1) // BLOCK):
        digest.update(deterministic("sprig-vss:" + name, index, min(BLOCK, size - index * BLOCK)))
    return digest.hexdigest()


def token(kind, fixture_id):
    head = ("SPRIG-VSS-" + kind + ":" + fixture_id + "\n").encode("ascii")
    return head + b"\0" * (BLOCK - len(head))


def file_hash(path):
    digest = hashlib.sha256()
    with Path(path).open("rb") as handle:
        while True:
            chunk = handle.read(MIB)
            if not chunk:
                break
            digest.update(chunk)
    return digest.hexdigest()


def metadata(info, directory=False):
    # Sprig's supported Windows modes, not an assertion about ACL equivalence.
    mode = (0o755 if directory else 0o644) if os.name == "nt" else stat.S_IMODE(info.st_mode)
    return {"size": info.st_size, "mtime_ns": info.st_mtime_ns, "mode": mode}


def load_fixture(root):
    root = Path(root)
    check_node(root, directory=True)
    require(set(p.name for p in root.iterdir()) == set(FILES),
            "Directory is not the exact fixed-name synthetic fixture")
    for name in FILES:
        info = check_node(root / name)
        require(info.st_size <= MAX_FILE if name in PAYLOADS else info.st_size <= 16384,
                "Unexpected fixture file size")
        stream_check(root / name)
    stream_check(root)
    marker = json.loads((root / MARKER).read_text(encoding="utf-8"))
    require(marker.get("schema_version") == SCHEMA and marker.get("kind") == KIND,
            "Unknown fixture marker")
    fixture_id = marker.get("fixture_id")
    require(isinstance(fixture_id, str) and str(uuid.UUID(fixture_id)) == fixture_id,
            "Invalid fixture identity")
    size = marker.get("payload_bytes")
    require(type(size) is int and BLOCK <= size <= MAX_FILE and size % BLOCK == 0,
            "Invalid bounded payload size")
    require(marker.get("files") == list(FILES), "Marker file list differs from fixed contract")
    for name in PAYLOADS:
        require((root / name).stat().st_size == size, "Payload changed size")
    for name in TOKENS:
        require((root / name).stat().st_size == BLOCK, "Token changed size")
    require(sum((root / name).stat().st_size for name in FILES) <= MAX_FIXTURE,
            "Fixture exceeds 64 MiB")
    require(file_hash(root / "sealed.bin") == expected_payload_hash("sealed.bin", size),
            "Deterministic sealed file mismatch")
    require((root / "pre-token.bin").read_bytes() == token("PRE", fixture_id),
            "Flushed pre-token missing or corrupt")
    return marker


def inventory(root):
    marker = load_fixture(root)
    result = {}
    for name in FILES:
        before = check_node(Path(root) / name)
        result[name] = dict(metadata(before), sha256=file_hash(Path(root) / name))
        after = check_node(Path(root) / name)
        require(metadata(before) == metadata(after), "File changed while hashing: " + name)
        require(after.st_mtime_ns % 100 == 0, "Fixture mtime is not Windows-compatible")
    return {"fixture_id": marker["fixture_id"], "files": result}


def normalized_time(path, ns=FIXED_NS):
    os.utime(path, ns=(ns, ns))


def create_fixture(root, size):
    require(BLOCK <= size <= MAX_FILE and size % BLOCK == 0, "Invalid fixture size")
    root = ordinary_path(root, must_exist=False)
    require(root.parent.is_dir(), "Fixture parent must already exist")
    root.mkdir(mode=0o755)  # Never adopt, empty, or replace an existing directory.
    fixture_id = str(uuid.uuid4())
    for name in PAYLOADS:
        with (root / name).open("xb") as handle:
            for index in range(size // BLOCK):
                handle.write(deterministic("sprig-vss:" + name, index))
            handle.flush()
            os.fsync(handle.fileno())
    for name, kind in (("pre-token.bin", "PRE"), ("post-token.bin", "BASE")):
        with (root / name).open("xb") as handle:
            handle.write(token(kind, fixture_id))
            handle.flush()
            os.fsync(handle.fileno())
    marker = {"schema_version": SCHEMA, "kind": KIND, "fixture_id": fixture_id,
              "payload_bytes": size, "files": list(FILES)}
    json_new(root / MARKER, marker)
    for name in FILES:
        os.chmod(root / name, 0o644)
        normalized_time(root / name)
    os.chmod(root, 0o755)
    normalized_time(root)
    return {"schema_version": SCHEMA, "status": "prepared", "root": str(root),
            "fixture_id": fixture_id, "inventory": inventory(root),
            "storage": measure(root), "pre_token_flushed": True}


def writer(args):
    root = ordinary_path(args.live_root)
    marker = load_fixture(root)
    require(0 < args.seconds <= 300 and 5 <= args.interval_ms <= 1000,
            "Writer must be bounded to 300 seconds and use a 5..1000 ms interval")
    require(1 <= args.generation <= 1000000, "Invalid generation")
    controls = []
    for raw in (args.ready_file, args.stop_file):
        if raw:
            path = ordinary_path(raw, must_exist=False)
            require(path.parent.is_dir() and not path.exists(), "Control path must be new")
            if os.name == "nt":
                outside_volume(root, path)
            else:
                require(root not in path.parents, "Control files must be outside fixture")
            controls.append(path)
    require(len(controls) < 2 or controls[0] != controls[1], "Control paths must differ")
    # Reflush the known token before the ready signal; writer never edits token bytes.
    with (root / "pre-token.bin").open("r+b") as pre:
        pre.flush()
        os.fsync(pre.fileno())
    stop = threading.Event()
    ready = [threading.Event(), threading.Event()]
    failures = []
    counts = [0, 0]
    deadline = time.monotonic() + args.seconds

    def run_one(slot, name, mapped):
        try:
            with (root / name).open("r+b", buffering=0) as handle:
                mapping = mmap.mmap(handle.fileno(), 0, access=mmap.ACCESS_WRITE) if mapped else None
                try:
                    index = 0
                    while not stop.is_set() and time.monotonic() < deadline:
                        offset = (index * 17 % (marker["payload_bytes"] // BLOCK)) * BLOCK
                        content = deterministic("writer:%s:%s" % (name, args.generation), index)
                        if mapping is not None:
                            mapping[offset:offset + BLOCK] = content
                        else:
                            handle.seek(offset)
                            require(handle.write(content) == BLOCK, "Short synthetic write")
                        if index == 0 or index % 16 == 0:
                            if mapping is not None:
                                mapping.flush()
                            os.fsync(handle.fileno())
                        counts[slot] += 1
                        ready[slot].set()
                        index += 1
                        stop.wait(args.interval_ms / 1000.0)
                    if mapping is not None:
                        mapping.flush()
                    os.fsync(handle.fileno())
                finally:
                    if mapping is not None:
                        mapping.close()
            info = (root / name).stat()
            os.utime(root / name, ns=(info.st_atime_ns // 100 * 100, info.st_mtime_ns // 100 * 100))
        except BaseException as error:
            failures.append(str(error))
            stop.set()
            ready[slot].set()

    threads = [threading.Thread(target=run_one, args=(0, "mapped.bin", True)),
               threading.Thread(target=run_one, args=(1, "written.bin", False))]
    for thread in threads:
        thread.start()
    try:
        for event in ready:
            require(event.wait(min(args.seconds + 5, 30)), "Writer readiness timed out")
        require(not failures and all(counts), "Writer failed before readiness: " + repr(failures))
        if args.ready_file:
            json_new(args.ready_file, {"schema_version": SCHEMA, "fixture_id": marker["fixture_id"],
                      "writer_pid": os.getpid(), "pre_token_flushed": True,
                      "first_mmap_and_write_flushed": True})
        while any(thread.is_alive() for thread in threads):
            if args.stop_file and Path(args.stop_file).exists():
                stop.set()
            stop.wait(0.05) if not stop.is_set() else time.sleep(0.05)
    finally:
        stop.set()
        for thread in threads:
            thread.join()  # No helper can report exit before mappings/handles close.
    require(not failures, "Writer failed: " + repr(failures))
    return {"schema_version": SCHEMA, "status": "writer_stopped", "cleanup_safe": True,
            "fixture_id": marker["fixture_id"], "mmap_writes": counts[0], "ordinary_writes": counts[1],
            "handles_closed": True, "final_flush_completed": True,
            "claim": "Concurrent synthetic writes only; no cross-file transaction guarantee."}


def allocation(path, info):
    class StandardInfo(ctypes.Structure):
        _fields_ = [("allocation_size", ctypes.c_int64),
                    ("end_of_file", ctypes.c_int64),
                    ("number_of_links", ctypes.c_uint32),
                    ("delete_pending", ctypes.c_uint8),
                    ("directory", ctypes.c_uint8)]

    # FILE_STANDARD_INFO uses byte-sized BOOLEANs, not Win32 BOOLs. Fixed-width
    # fields let this ABI check run locally without loading or calling WinAPI.
    assert ctypes.sizeof(StandardInfo) == 24
    assert [getattr(StandardInfo, name).offset for name, _ in StandardInfo._fields_] == [0, 8, 16, 20, 21]
    if os.name != "nt":
        return getattr(info, "st_blocks", None) * 512 if hasattr(info, "st_blocks") else None
    if not stat.S_ISREG(info.st_mode) or is_reparse(info):
        return None
    from ctypes import wintypes
    kernel = ctypes.WinDLL("kernel32", use_last_error=True)
    open_file = kernel.CreateFileW
    open_file.argtypes = [wintypes.LPCWSTR, wintypes.DWORD, wintypes.DWORD,
                          ctypes.c_void_p, wintypes.DWORD, wintypes.DWORD, wintypes.HANDLE]
    open_file.restype = wintypes.HANDLE
    get_info = kernel.GetFileInformationByHandleEx
    get_info.argtypes = [wintypes.HANDLE, ctypes.c_int, ctypes.c_void_p, wintypes.DWORD]
    get_info.restype = wintypes.BOOL
    close = kernel.CloseHandle
    close.argtypes = [wintypes.HANDLE]
    close.restype = wintypes.BOOL
    # FILE_READ_ATTRIBUTES, share read/write/delete, OPEN_EXISTING, and
    # FILE_FLAG_OPEN_REPARSE_POINT: query this entry without following links.
    handle = open_file(str(path), 0x80, 0x7, None, 3, 0x00200000, None)
    if handle is None or handle == ctypes.c_void_p(-1).value:
        return None
    result = None
    try:
        standard = StandardInfo()
        if get_info(handle, 1, ctypes.byref(standard), ctypes.sizeof(standard)):
            if not standard.directory and not standard.delete_pending and standard.allocation_size >= 0:
                result = standard.allocation_size
    finally:
        if not close(handle):
            result = None
    # Unknown allocation stays unknown; never substitute logical file length.
    return result


def measure(root):
    root = Path(root)
    if not root.exists():
        return {"exists": False, "files": 0, "logical_bytes": 0, "unique_file_logical_bytes": 0,
                "allocated_bytes": 0, "hardlink_aliases": 0}
    seen = set()
    files = logical = unique_logical = allocated = aliases = missing = 0
    stack = [root]
    while stack:
        path = stack.pop()
        info = path.lstat()
        require(not stat.S_ISLNK(info.st_mode) and not is_reparse(info),
                "Storage measurement refuses links/reparse points: " + str(path))
        if stat.S_ISDIR(info.st_mode):
            stack.extend(path.iterdir())
            require(len(stack) + files <= MAX_MEASURE_FILES, "Measurement entry bound exceeded")
            continue
        require(stat.S_ISREG(info.st_mode), "Measurement expects ordinary files")
        files += 1
        logical += info.st_size
        require(files <= MAX_MEASURE_FILES and logical <= MAX_MEASURE_BYTES,
                "Measurement exceeds bounded synthetic run budget")
        identity = (info.st_dev, info.st_ino) if info.st_ino else (str(path),)
        if identity in seen:
            aliases += 1
            continue
        seen.add(identity)
        unique_logical += info.st_size
        used = allocation(path, info)
        if used is None:
            missing += 1
        else:
            allocated += used
    return {"exists": True, "files": files, "logical_bytes": logical,
            "unique_file_logical_bytes": unique_logical, "allocated_bytes": allocated if not missing else None,
            "known_allocated_bytes": allocated, "allocation_unavailable_files": missing,
            "hardlink_aliases": aliases,
            "allocation_note": "Referenced file allocation, hardlinks counted once; not exclusive CoW extents, filesystem metadata or VSS diff-area usage."}


def stage_snapshot(source, target, expected):
    source, target = Path(source), Path(target)
    target.mkdir(mode=0o755)
    for name in FILES:
        before = check_node(source / name)
        with (source / name).open("rb") as incoming, (target / name).open("xb") as outgoing:
            shutil.copyfileobj(incoming, outgoing, MIB)
            outgoing.flush()
            os.fsync(outgoing.fileno())
        mode = expected["files"][name]["mode"]
        require(mode == 0o644, "Only ordinary Windows-compatible fixture modes are supported")
        os.chmod(target / name, mode)
        os.utime(target / name, ns=(before.st_atime_ns // 100 * 100, before.st_mtime_ns))
    root_info = source.stat()
    os.chmod(target, 0o755)
    os.utime(target, ns=(root_info.st_atime_ns // 100 * 100, root_info.st_mtime_ns // 100 * 100))
    require(inventory(target) == expected, "Staging bytes or metadata differ from immutable snapshot")


def post_probe(live, snapshot, expected):
    live, snapshot = Path(live), Path(snapshot)
    target = live / "post-token.bin"
    original = target.read_bytes()
    original_info = check_node(target)
    probe = token("POST-" + uuid.uuid4().hex, expected["fixture_id"])
    require(probe != original, "Post-token probe must differ")
    with target.open("r+b", buffering=0) as handle:
        require(handle.read() == original, "Live token changed before probe")
        handle.seek(0)
        require(handle.write(probe) == BLOCK, "Short post-token write")
        os.fsync(handle.fileno())
    try:
        require(target.read_bytes() == probe, "Live post-token probe is not readable")
        require(inventory(snapshot) == expected, "Snapshot changed after flushed live post-token write")
        require(inventory(snapshot) == expected, "Snapshot changed on repeated post-write full read")
    finally:
        # Never overwrite a later third-party change. Only our exact own token is restored.
        with target.open("r+b", buffering=0) as handle:
            require(handle.read() == probe, "Post-token changed externally; refusing to restore it")
            handle.seek(0)
            require(handle.write(original) == BLOCK, "Short post-token restoration")
            os.fsync(handle.fileno())
        os.utime(target, ns=(original_info.st_atime_ns, original_info.st_mtime_ns))
    require(target.read_bytes() == original, "Live post-token restoration failed")
    return {"live_probe_sha256": hashlib.sha256(probe).hexdigest(), "probe_flushed": True,
            "snapshot_full_reads_before": 2, "snapshot_full_reads_after": 2,
            "live_original_token_restored": True, "original_mtime_restored": True}


class Sprig:
    def __init__(self, binary, store, logs, timeout):
        self.binary, self.store, self.logs = str(binary), str(store), Path(logs)
        self.timeout, self.commands = timeout, []

    def run(self, *args, expect_failure=False):
        require(len(self.commands) < MAX_CLI_COMMANDS,
                "Capture exceeded its fixed 12-command Sprig limit; review the storage budget before adding commands")
        command = [self.binary, "--store", self.store, "--json"] + list(args)
        number = len(self.commands) + 1
        out = self.logs / ("sprig-%02d.stdout.log" % number)
        err = self.logs / ("sprig-%02d.stderr.log" % number)
        def stream_state():
            return {"bytes_seen": 0, "bytes_logged": 0, "overflow": False,
                    "eof": False, "error": None, "log_limit_bytes": MAX_CLI_LOG_BYTES}

        record = {"argv": command, "stdout_log": str(out), "stderr_log": str(err),
                  "exited": False, "streams_drained": False,
                  "stdout": stream_state(), "stderr": stream_state()}
        self.commands.append(record)

        def drain(pipe, log, state):
            # Each worker holds at most one 64 KiB read and writes <= 512 KiB.
            # Continue reading/discarding on overflow (or a log-write failure)
            # so a full OS pipe cannot deadlock the child or cause unbounded RAM.
            try:
                while True:
                    data = pipe.read(64 * 1024)
                    if not data:
                        state["eof"] = True
                        break
                    state["bytes_seen"] += len(data)
                    if state["bytes_seen"] > MAX_CLI_LOG_BYTES:
                        state["overflow"] = True
                    remaining = MAX_CLI_LOG_BYTES - state["bytes_logged"]
                    if remaining > 0 and state["error"] is None:
                        try:
                            part = data[:remaining]
                            require(log.write(part) == len(part), "Short bounded CLI log write")
                            state["bytes_logged"] += len(part)
                        except BaseException as error:
                            state["error"] = str(error)
            except BaseException as error:
                state["error"] = str(error)
            finally:
                for handle in (pipe, log):
                    try:
                        handle.close()
                    except BaseException as error:
                        state["error"] = str(error)

        stdout = out.open("xb")
        try:
            stderr = err.open("xb")
        except BaseException:
            stdout.close()
            raise
        try:
            process = subprocess.Popen(command, stdout=subprocess.PIPE, stderr=subprocess.PIPE, shell=False)
        except BaseException:
            stdout.close()
            stderr.close()
            raise
        record["pid"] = process.pid
        threads = [threading.Thread(target=drain, args=(process.stdout, stdout, record["stdout"]), daemon=True),
                   threading.Thread(target=drain, args=(process.stderr, stderr, record["stderr"]), daemon=True)]
        for thread in threads:
            thread.start()
        deadline = time.monotonic() + self.timeout
        try:
            status = process.wait(timeout=self.timeout)
        except subprocess.TimeoutExpired:
            # Daemon drainers remain bounded while the requester holds its job.
            # Do not terminate, retry, wait indefinitely, or report cleanup-safe.
            raise FixtureError("Sprig timed out; do not delete snapshots or detach the source volume",
                               uncertain_child=True, child_pid=process.pid, argv=command)
        record.update(exited=True, returncode=status)
        for thread in threads:
            thread.join(max(0.0, deadline - time.monotonic()))
        if any(thread.is_alive() for thread in threads):
            raise FixtureError("Sprig exited but output pipes did not drain before the deadline; retain state",
                               uncertain_child=True, child_pid=process.pid, argv=command)
        record["streams_drained"] = all(record[name]["eof"] for name in ("stdout", "stderr"))
        require(record["streams_drained"] and not any(record[name]["error"] for name in ("stdout", "stderr")),
                "Sprig output drain failed; inspect bounded log metadata")
        require(not any(record[name]["overflow"] for name in ("stdout", "stderr")),
                "Sprig output exceeded the 512 KiB per-stream limit; logs retained, JSON not parsed")
        require(status != 0 if expect_failure else status == 0,
                "Sprig unexpectedly accepted a rejection case" if expect_failure
                else "Sprig failed; see retained logs: " + str(err))
        require(out.stat().st_size <= MAX_CLI_LOG_BYTES, "Unexpectedly large bounded Sprig log")
        envelope = json.loads(out.read_text(encoding="utf-8"))
        if expect_failure:
            require(envelope.get("schema_version") == 1 and isinstance(envelope.get("error"), dict),
                    "Expected a structured Sprig rejection, not an unknown process failure")
            record["expected_failure"] = True
            return envelope["error"]
        require(envelope.get("schema_version") == 1 and "result" in envelope,
                "Unexpected Sprig JSON envelope")
        result = envelope["result"]
        require(not isinstance(result, dict) or not result.get("mount_error"),
                "Sprig reported a partial mount outcome")
        return result


def delta(before, after, keys):
    return {key: after[key] - before[key]
            if isinstance(before.get(key), int) and isinstance(after.get(key), int) else None
            for key in keys}


def cas_file_content(cli, revision, expected):
    """Use the real CLI manifest view; entry IDs are not payload identities."""
    entries = cli.run("ls", revision)
    require(isinstance(entries, list) and {entry["path"] for entry in entries} == set(FILES),
            "CAS root listing differs from fixed fixture files")
    result = {}
    for item in entries:
        name, entry = item["path"], item["entry"]
        kind = entry["kind"]
        require(kind.get("type") == "file" and kind.get("size") == expected["files"][name]["size"],
                "CAS manifest file kind or size differs from snapshot")
        require(entry.get("mode") == expected["files"][name]["mode"]
                and entry.get("modified_ns") == expected["files"][name]["mtime_ns"],
                "CAS manifest metadata differs from snapshot")
        chunks = kind.get("chunks")
        require(isinstance(chunks, list) and chunks and all(
            isinstance(chunk, str) and re.fullmatch(r"[0-9a-f]{64}", chunk) for chunk in chunks),
            "Invalid CAS payload chunk identities")
        result[name] = {"size": kind["size"], "chunks": chunks}
    return result


def capture(args):
    capture_started = time.monotonic()
    require(os.name == "nt", "capture requires a Windows VSS device root; selftest is platform-independent")
    require(VSS_ROOT.fullmatch(args.snapshot_root) is not None,
            "Snapshot root must be the requester-provided VSS GLOBALROOT device path")
    require(re.fullmatch(r"[a-z][a-z0-9-]{0,31}", args.label) is not None, "Unsafe capture label")
    require(1 <= args.timeout <= 300, "CLI timeout must be 1..300 seconds")
    live = ordinary_path(args.live_root)
    relative = PureWindowsPath(args.relative_path)
    require(not relative.is_absolute() and not relative.drive and relative.parts
            and all(part not in (".", "..") and ":" not in part for part in relative.parts),
            "Expected a relative fixture path without aliases")
    live_volume, mount_root = volume(live)
    require(os.path.normcase(str(Path(mount_root).joinpath(*relative.parts))) == os.path.normcase(str(live)),
            "Relative fixture path does not identify the live fixture on the source volume")
    snapshot = Path(args.snapshot_root).joinpath(*relative.parts)
    # Check each component below the trusted requester device root without following reparse paths.
    cursor = Path(args.snapshot_root)
    for part in relative.parts:
        cursor = cursor / part
        check_node(cursor, directory=True)
    live_marker, snapshot_marker = load_fixture(live), load_fixture(snapshot)
    require(live_marker == snapshot_marker, "Live and snapshot fixture identities differ")
    direct_identity = snapshot_identity(args.snapshot_root) if args.direct_import else None
    work = ordinary_path(args.work_root, must_exist=False)
    store = ordinary_path(args.store, must_exist=False)
    binary = ordinary_path(args.sprig)
    require(binary.is_file(), "Sprig must be an existing executable file")
    for target in (work, store):
        outside_volume(live, target)
        require(target.parent.is_dir(), "Output parent must already exist")
    require(work != store and work not in store.parents and store not in work.parents,
            "Work and store roots must not overlap")
    require(not work.exists(), "Capture work directory must be new")
    owner = Path(str(store) + ".sprig-vss-owner.json")
    expected_owner = {"schema_version": SCHEMA, "kind": KIND,
                      "fixture_id": live_marker["fixture_id"], "store": str(store)}
    new_store = not store.exists()
    if not new_store:
        check_node(owner)
        require(owner.stat().st_size <= 16384, "Invalid store ownership marker size")
        require(json.loads(owner.read_text(encoding="utf-8")) == expected_owner,
                "Existing store does not belong to this synthetic fixture")
    else:
        require(not owner.exists(), "Orphan store owner marker requires operator inspection")
    previous = None
    if args.previous_report:
        report_path = ordinary_path(args.previous_report)
        outside_volume(live, report_path)
        require(report_path.stat().st_size <= 8 * MIB, "Previous report too large")
        previous = json.loads(report_path.read_text(encoding="utf-8"))
        require(previous.get("status") == "passed" and previous.get("store") == str(store)
                and previous.get("fixture_id") == live_marker["fixture_id"],
                "Previous report is not a successful capture of this fixture/store")
    work.mkdir(mode=0o755)
    logs = work / "logs"
    logs.mkdir()
    cli = Sprig(binary, store, logs, args.timeout)
    report = {"schema_version": SCHEMA, "status": "failed", "cleanup_safe": False,
              "label": args.label, "fixture_id": live_marker["fixture_id"],
              "snapshot_root": args.snapshot_root, "snapshot_fixture": str(snapshot),
              "live_root": str(live), "store": str(store), "work_root": str(work),
              "staging_mode": "none" if args.direct_import else "metadata_preserving_copy",
              "cas_import_mode": "direct_snapshot" if args.direct_import else "staged_writable",
              "commands": cli.commands,
              "limitations": ["Not a volume rollback or VSS native revert test.",
                  "No guarantee of inclusion of every unflushed mmap store.",
                  "No application-atomic cross-file transaction claim.",
                  "Snapshot read/staging is distinct from Sprig CAS revision restore.",
                  "Allocation excludes filesystem metadata and VSS diff-area usage."]}
    try:
        started = time.monotonic()
        expected = inventory(snapshot)
        require(inventory(snapshot) == expected, "Snapshot differs between first two full hash reads")
        report["snapshot_inventory"] = expected
        if previous:
            require(previous["snapshot_inventory"] == expected,
                    "Repeated snapshot differs from settled capture; dedup comparison invalid")
        report["post_token_test"] = post_probe(live, snapshot, expected)
        report["snapshot_oracle_seconds"] = time.monotonic() - started
        stage = work / "stage"
        if not args.direct_import:
            staging_started = time.monotonic()
            stage_snapshot(snapshot, stage, expected)
            report["staging_seconds"] = time.monotonic() - staging_started
            report["snapshot_staging_seconds"] = time.monotonic() - started
        else:
            report["staging_seconds"] = 0.0
            report["direct_identity"] = direct_identity
        report["staging_storage"] = measure(stage)
        report["snapshot_read_verified"] = True
        report["snapshot_read_and_staging_verified"] = not args.direct_import
        if new_store:
            # Sidecar is adjacent, exclusively created; never pre-create Sprig's store itself.
            json_new(owner, expected_owner)
            cli.run("init")
        before = cli.run("stats")
        if args.direct_import and new_store:
            wrong_serial = "%08X" % (int(direct_identity["serial"], 16) ^ 1)
            marker_token = direct_identity["marker_token"]
            wrong_marker = ("1" if marker_token[0] == "0" else "0") + marker_token[1:]
            rejected = []
            for label, root_arg, serial, marker_token in (
                ("wrong-serial", str(snapshot), wrong_serial, direct_identity["marker_token"]),
                ("wrong-marker", str(snapshot), direct_identity["serial"], wrong_marker),
                ("invalid-root", str(live), direct_identity["serial"], direct_identity["marker_token"]),
            ):
                error = cli.run("snapshot-import-experiment", root_arg,
                                "vss-reject-" + label + "-" + uuid.uuid4().hex,
                                "--serial", serial, "--marker-token", marker_token, expect_failure=True)
                rejected.append({"case": label, "error": error})
            after_rejections = cli.run("stats")
            require(all(after_rejections[key] == before[key] for key in ("branches", "revisions", "workspaces")),
                    "Rejected direct imports changed branch, revision or workspace counts")
            report["direct_rejection_checks"] = {"cases": rejected,
                "stats_before": {key: before[key] for key in ("branches", "revisions", "workspaces")},
                "stats_after": {key: after_rejections[key] for key in ("branches", "revisions", "workspaces")},
                "counts_unchanged": True, "verification": "Combined before/after check for exactly three rejected calls"}
            before = after_rejections
        report["sprig_stats_before_import"] = before
        report["store_storage_before_import"] = measure(store)
        name = "vss-" + args.label + "-" + uuid.uuid4().hex
        started = time.monotonic()
        if args.direct_import:
            imported = cli.run("snapshot-import-experiment", str(snapshot), name,
                               "--serial", direct_identity["serial"],
                               "--marker-token", direct_identity["marker_token"])
            report["direct_snapshot_import_seconds"] = time.monotonic() - started
        else:
            imported = cli.run("--writable", "import", str(stage), name)
        require(imported.get("backend", {}).get("kind") == "writable"
                and imported.get("mount_state") == "unmounted", "Import is not unmounted writable CAS")
        revision = imported.get("base_revision")
        require(isinstance(revision, str) and re.fullmatch(r"[0-9a-f]{64}", revision),
                "Invalid imported revision identity")
        report.update(import_name=name, imported_revision=revision,
                      cas_import_seconds=time.monotonic() - started)
        report["cas_file_content"] = cas_file_content(cli, revision, expected)
        after_import = cli.run("stats")
        report["sprig_stats_after_import"] = after_import
        report["store_storage_after_import"] = measure(store)
        report["import_storage_delta"] = delta(before, after_import,
            ("object_bytes", "object_storage_bytes", "objects", "store_logical_bytes", "store_allocated_bytes"))
        started = time.monotonic()
        restored = cli.run("--copy", "restore", revision, name + "-restore")
        require(restored.get("backend", {}).get("kind") == "native"
                and restored.get("mount_state") == "native", "Restore is not native materialization")
        restore = ordinary_path(restore_drive_path(restored["path"]))
        require(store in restore.parents, "Sprig restore path is outside the owned store")
        outside_volume(live, restore)
        require(inventory(restore) == expected, "Native CAS restore bytes/mtime/mode differ from snapshot")
        report.update(cas_restore_path=str(restore), cas_restore_seconds=time.monotonic() - started,
                      cas_native_restore_verified=True, restore_storage=measure(restore))
        report["sprig_stats_after_restore"] = cli.run("stats")
        report["store_storage_after_restore"] = measure(store)
        report["work_storage_before_report"] = measure(work)
        if previous:
            require(previous.get("cas_file_content") == report["cas_file_content"],
                    "Repeated identical files do not reference identical CAS payload chunks")
            report["repeated_capture"] = {"previous_report": str(report_path),
                "full_bytes_and_metadata_equal": True,
                "per_file_cas_payload_chunk_lists_equal": True,
                "total_object_count_delta": after_import["objects"] - before["objects"],
                "total_object_storage_delta_bytes": after_import["object_storage_bytes"] - before["object_storage_bytes"],
                "note": "Total objects include metadata pages. Independent imports can add fresh entry identities and metadata despite sharing every file payload chunk. Native restore, staging, revision, branch and journal overhead is retained and measured; zero total growth is not asserted."}
        report.update(status="passed", cleanup_safe=True, all_direct_cli_children_exited=True,
                      capture_total_seconds=time.monotonic() - capture_started)
    except BaseException as error:
        report["error"] = str(error)
        report.update(getattr(error, "details", {}))
        report["all_direct_cli_children_exited"] = all(c.get("exited") for c in cli.commands)
        json_new(work / "capture.json", report)
        raise FixtureError(str(error), report_path=str(work / "capture.json"), **getattr(error, "details", {}))
    json_new(work / "capture.json", report)
    return report


def selftest():
    # Only deletes this function's newly-owned temporary directory on exit.
    with tempfile.TemporaryDirectory(prefix="sprig-vss-oracle-selftest-") as temporary:
        root = Path(temporary)
        live = root / "live"
        create_fixture(live, 2 * BLOCK)
        before = inventory(live)
        snapshot = root / "snapshot"
        shutil.copytree(live, snapshot)
        # Linux temporary fixtures also have exact Windows-compatible timestamps.
        require(inventory(snapshot) == before, "Selftest snapshot copy mismatch")
        require(inventory(snapshot) == before, "Selftest repeated hash mismatch")
        probe = post_probe(live, snapshot, before)
        require(inventory(live) == before, "Selftest token restoration changed metadata")
        stage_snapshot(snapshot, root / "stage", before)
        require(inventory(root / "stage") == before, "Selftest stage mismatch")
        args = argparse.Namespace(live_root=str(live), seconds=0.1, interval_ms=5,
                                  generation=2, ready_file=None, stop_file=None)
        written = writer(args)
        require(written["mmap_writes"] > 0 and written["ordinary_writes"] > 0,
                "Selftest writers did not run")
        require(inventory(snapshot) == before, "Selftest immutable copy changed")
        require(file_hash(live / "mapped.bin") != before["files"]["mapped.bin"]["sha256"],
                "Selftest mmap payload did not change")
        require(file_hash(live / "written.bin") != before["files"]["written.bin"]["sha256"],
                "Selftest ordinary-write payload did not change")
        (live / "unexpected").write_bytes(b"unexpected")
        try:
            load_fixture(live)
        except FixtureError:
            pass
        else:
            raise FixtureError("Selftest accepted an unexpected fixture file")
        usage = measure(root)
        require(usage["logical_bytes"] < MIB, "Selftest grew beyond tiny fixture budget")
        # A normal mock response and one bounded local Python output child test
        # the actual drainers. Neither executes Sprig nor touches VSS.
        logs = root / "mock-cli-logs"
        logs.mkdir()
        cli = Sprig("not-executed", "not-opened", logs, 5)
        real_popen = subprocess.Popen

        class FakeProcess:
            pid = 0

            def __init__(self, stdout, stderr):
                self.stdout, self.stderr = io.BytesIO(stdout), io.BytesIO(stderr)

            def wait(self, timeout):
                return 0

        try:
            subprocess.Popen = lambda *a, **k: FakeProcess(
                b'{"schema_version":1,"result":{"ok":true}}', b"")
            require(cli.run("stats") == {"ok": True}, "Selftest bounded CLI JSON failed")
            require(cli.commands[-1]["streams_drained"], "Selftest pipes did not reach EOF")
            output_program = (
                "import sys; "
                "sys.stdout.buffer.write(b'x' * %d); sys.stdout.buffer.flush(); "
                "sys.stderr.buffer.write(b'y' * %d); sys.stderr.buffer.flush()"
                % (MAX_CLI_LOG_BYTES + 13, MAX_CLI_LOG_BYTES + 29))
            subprocess.Popen = lambda *a, **k: real_popen([sys.executable, "-c", output_program], **k)
            try:
                cli.run("stats")
            except FixtureError as error:
                require("512 KiB per-stream limit" in str(error), "Selftest overflow failed for wrong reason")
            else:
                raise FixtureError("Selftest accepted overflowing CLI streams")
            record = cli.commands[-1]
            require(record["exited"] and record["streams_drained"], "Selftest did not drain overflow")
            for name in ("stdout", "stderr"):
                require(record[name]["overflow"] and record[name]["bytes_logged"] == MAX_CLI_LOG_BYTES,
                        "Selftest stream cap metadata differs")
                require(Path(record[name + "_log"]).stat().st_size == MAX_CLI_LOG_BYTES,
                        "Selftest CLI log exceeded its cap")
            cli.commands = [{} for _ in range(MAX_CLI_COMMANDS)]
            try:
                cli.run("stats")
            except FixtureError as error:
                require("12-command" in str(error), "Selftest command cap failed for wrong reason")
            else:
                raise FixtureError("Selftest accepted excess CLI command")
        finally:
            subprocess.Popen = real_popen
        oversized = root / "oversized.json"
        try:
            json_new(oversized, {"payload": "z" * MIB})
        except FixtureError as error:
            require("1 MiB file limit" in str(error), "Selftest JSON cap failed for wrong reason")
            require(not oversized.exists(), "Oversized JSON was written before size validation")
        else:
            raise FixtureError("Selftest accepted oversized JSON")
    return {"schema_version": SCHEMA, "status": "selftest_passed", "sprig_executed": False,
            "vss_executed": False, "tiny_fixture_only": True, "local_python_output_child_executed": True,
            "checks": ["deterministic sealed bytes", "exact marker/files", "flushed pre-token",
                       "full repeated hashes", "post-token restoration", "stage metadata preservation",
                       "bounded mmap and ordinary writes", "unexpected-file rejection", "storage measurement",
                       "local Python child drains stdout/stderr beyond 512 KiB without deadlock",
                       "CLI command ceiling", "JSON size rejected before file creation"]}


def parser():
    result = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    commands = result.add_subparsers(dest="command", required=True)
    prepare = commands.add_parser("prepare", help="Create a NEW bounded synthetic fixture")
    prepare.add_argument("--live-root", required=True)
    prepare.add_argument("--file-mib", type=int, choices=range(1, 9), default=8)
    write = commands.add_parser("writer", help="Bounded concurrent synthetic mmap and ordinary writes")
    write.add_argument("--live-root", required=True)
    write.add_argument("--seconds", type=float, default=120)
    write.add_argument("--interval-ms", type=int, default=10)
    write.add_argument("--generation", type=int, default=1)
    write.add_argument("--ready-file")
    write.add_argument("--stop-file")
    capture_parser = commands.add_parser("capture", help="Read held VSS root, stage, import real CAS, restore and compare")
    capture_parser.add_argument("snapshot_root")
    for option in ("live-root", "relative-path", "work-root", "store", "sprig", "label"):
        capture_parser.add_argument("--" + option, required=True)
    capture_parser.add_argument("--previous-report")
    capture_parser.add_argument("--direct-import", action="store_true",
                                help="Use the explicit experimental snapshot importer without a staging copy")
    capture_parser.add_argument("--timeout", type=int, default=120)
    usage = commands.add_parser("measure", help="Read-only bounded logical/allocated storage measurement")
    usage.add_argument("paths", nargs="+")
    commands.add_parser("selftest", help="Tiny local test; never runs Sprig or VSS")
    return result


def main():
    args = parser().parse_args()
    try:
        if args.command == "prepare":
            result = create_fixture(args.live_root, args.file_mib * MIB)
        elif args.command == "writer":
            result = writer(args)
        elif args.command == "capture":
            result = capture(args)
        elif args.command == "measure":
            result = {"schema_version": SCHEMA, "paths": {str(Path(p)): measure(p) for p in args.paths}}
        else:
            result = selftest()
        emit(result)
        return 0
    except BaseException as error:
        emit({"schema_version": SCHEMA, "status": "failed", "cleanup_safe": False,
              "error": str(error), **getattr(error, "details", {}),
              "instruction": "Retain all owned state. Do not force-stop children, delete snapshots, or force-detach volumes on an uncertain outcome."})
        return 1


if __name__ == "__main__":
    sys.exit(main())
