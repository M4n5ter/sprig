#!/usr/bin/env python3
"""Run small, deterministic Sprig demonstrations and emit measured JSON.

Build first: cargo build --release
Run: python3 examples/compare.py --binary target/release/sprig
Add --require-clone to require an actual native clone, without fallback.
Only this program's temporary synthetic files are created and removed.
"""

import argparse
import hashlib
import json
import os
import platform
import shutil
import subprocess
import sys
import tempfile
import time
from pathlib import Path


MIB = 1024 * 1024
TEXT = "first=base\nkeep=one\nkeep=two\nmiddle=base\nkeep=three\nkeep=four\nlast=base\n"


def check(condition, message):
    if not condition:
        raise AssertionError(message)


def byte_copy(source, destination):
    """Deliberately use userspace reads/writes, not shutil's native fast-copy."""
    with open(source, "rb") as src, open(destination, "wb") as dst:
        shutil.copyfileobj(src, dst, length=MIB)
    shutil.copystat(source, destination)
    return destination


def digest(path):
    result = hashlib.sha256()
    with path.open("rb") as stream:
        for block in iter(lambda: stream.read(MIB), b""):
            result.update(block)
    return result.hexdigest()


class Sprig:
    def __init__(self, binary, temporary, commands):
        self.binary = binary
        self.temporary = temporary
        self.store = temporary / "store"
        self.commands = commands

    def public(self, value):
        return str(value).replace(str(self.binary), "$SPRIG").replace(
            str(self.temporary), "$TMP"
        )

    def run(self, *args, mode=None, expected=0):
        argv = [str(self.binary), "--store", str(self.store), "--json"]
        if mode:
            argv.extend(["--copy-mode", mode])
        argv.extend(map(str, args))
        started = time.perf_counter()
        process = subprocess.run(argv, capture_output=True, text=True, check=False)
        record = {
            "argv": [self.public(arg) for arg in argv],
            "wall_seconds": round(time.perf_counter() - started, 6),
            "exit_code": process.returncode,
        }
        self.commands.append(record)
        check(
            process.returncode == expected,
            "Unexpected exit code: " + self.public(
                json.dumps(record) + " " + process.stdout + " " + process.stderr
            ),
        )
        envelope = json.loads(process.stdout)
        return envelope.get("result", envelope), record

    def result(self, *args, **kwargs):
        return self.run(*args, **kwargs)[0]

    def path(self, workspace):
        return Path(self.result("show", workspace)["path"])

    def stats(self):
        return self.result("stats")


def compare(binary, temporary, require_clone, report):
    source = temporary / "source"
    source.mkdir()
    payload = hashlib.shake_256(b"sprig-demo-payload").digest(4 * MIB)
    for name in ("payload.bin", "duplicate.bin"):
        (source / name).write_bytes(payload)
    (source / "other.bin").write_bytes(
        hashlib.shake_256(b"sprig-demo-other").digest(4 * MIB)
    )
    (source / "shared.txt").write_text(TEXT, encoding="utf-8")
    # Fixed timestamps avoid depending on fixture creation speed.
    for path in [*source.iterdir(), source]:
        os.utime(path, ns=(1_700_000_000_000_000_000,) * 2)
    logical_bytes = sum(path.stat().st_size for path in source.iterdir())
    original_hash = digest(source / "payload.bin")
    report["fixture"] = {
        "logical_bytes": logical_bytes,
        "regular_files": 4,
        "distinct_payload_bytes": 8 * MIB,
        "payload_sha256": original_hash,
        "description": "Three 4 MiB blobs (two identical), plus a seven-line text file.",
    }
    sprig = Sprig(binary, temporary, report["commands"])
    sprig.result("init")
    report["environment"]["backend"] = sprig.result("backend")
    before_import = sprig.stats()
    imported, import_record = sprig.run("import", source, "main", mode="copy")
    after_import = sprig.stats()
    report["import"] = {
        "wall_seconds": import_record["wall_seconds"],
        "copy_stats": imported["copy_stats"],
        "cas_bytes_before": before_import["object_bytes"],
        "cas_bytes_after": after_import["object_bytes"],
    }
    check(after_import["object_bytes"] < logical_bytes, "Duplicate content did not deduplicate")

    ordinary = temporary / "ordinary"
    started = time.perf_counter()
    shutil.copytree(source, ordinary, copy_function=byte_copy)
    report["measurements"]["ordinary_copy"] = {
        "python_call": 'shutil.copytree("$TMP/source", "$TMP/ordinary", copy_function=byte_copy)',
        "wall_seconds": round(time.perf_counter() - started, 6),
        "logical_bytes": logical_bytes,
        "copy_stats": {"cloned": 0, "copied": 4, "symlinks": 0},
    }
    check(digest(ordinary / "payload.bin") == original_hash, "Ordinary copy changed bytes")

    fork_paths = {}
    modes = ["auto", "copy"] + (["clone"] if require_clone else [])
    for mode in modes:
        before = sprig.stats()
        fork, measured = sprig.run("fork", "main", "fork_" + mode, mode=mode)
        after = sprig.stats()
        counts = fork["copy_stats"]
        check(counts["cloned"] + counts["copied"] == 4, "Missing fork copy statistics")
        if mode == "copy":
            check(counts["cloned"] == 0, "Forced copy unexpectedly cloned")
        if mode == "clone":
            check(counts["cloned"] == 4 and counts["copied"] == 0, "Strict clone fell back")
        check(before["object_bytes"] == after["object_bytes"], "Unchanged fork grew CAS")
        fork_paths[mode] = Path(fork["path"])
        check(digest(fork_paths[mode] / "payload.bin") == original_hash, "Fork changed bytes")
        report["measurements"]["fork_" + mode] = {
            "wall_seconds": measured["wall_seconds"],
            "logical_bytes": logical_bytes,
            "copy_stats": counts,
            "cas_bytes_before": before["object_bytes"],
            "cas_bytes_after": after["object_bytes"],
        }

    # A shifted large file should reuse content-defined chunks, not store 4 MiB anew.
    old_chunks = sprig.result("stat", "main", "payload.bin")["kind"]["chunks"]
    prefix = b"prefix inserted by the reproducible Sprig example\n"
    (fork_paths["auto"] / "payload.bin").write_bytes(prefix + payload)
    check(digest(source / "payload.bin") == original_hash, "Fork edit reached source")
    check(digest(Path(imported["path"]) / "payload.bin") == original_hash, "Fork edit reached main")
    for mode in modes[1:]:
        check(digest(fork_paths[mode] / "payload.bin") == original_hash, "Fork edit reached sibling")
    before_prefix = sprig.stats()
    committed, prefix_record = sprig.run("checkpoint", "fork_auto", "-m", "Insert prefix")
    after_prefix = sprig.stats()
    new_chunks = sprig.result("stat", committed["revision"], "payload.bin")["kind"]["chunks"]
    retained = set(old_chunks) & set(new_chunks)
    added_bytes = after_prefix["object_bytes"] - before_prefix["object_bytes"]
    check(committed["committed"] and retained, "Prefix edit did not retain any chunks")
    check(0 < added_bytes < len(payload), "Prefix edit did not save CAS bytes")
    report["prefix_checkpoint"] = {
        "prefix_bytes": len(prefix),
        "file_bytes_before": len(payload),
        "file_bytes_after": len(payload) + len(prefix),
        "wall_seconds": prefix_record["wall_seconds"],
        "cas_bytes_before": before_prefix["object_bytes"],
        "cas_bytes_after": after_prefix["object_bytes"],
        "new_cas_bytes": added_bytes,
        "original_chunk_references": len(old_chunks),
        "new_chunk_references": len(new_chunks),
        "reused_distinct_chunks": len(retained),
        "reused_chunk_bytes": sum((sprig.store / "objects" / chunk).stat().st_size for chunk in retained),
    }
    # Verify a write in every materialization, including strict clone when requested.
    trees = {"ordinary_copy": ordinary, **{"fork_" + mode: path for mode, path in fork_paths.items()}}
    for name, tree in trees.items():
        original = (tree / "shared.txt").read_bytes()
        (tree / "shared.txt").write_text("isolated=" + name + "\n", encoding="utf-8")
        check((source / "shared.txt").read_text() == TEXT, "Edit reached source text")
        check((Path(imported["path"]) / "shared.txt").read_text() == TEXT, "Edit reached main text")
        for other_name, other_tree in trees.items():
            if other_name != name:
                check((other_tree / "shared.txt").read_text() == TEXT, "Edit reached sibling text")
        (tree / "shared.txt").write_bytes(original)
    report["isolation"] = {"checked_materializations": list(trees), "all_checks_passed": True}

    # Both native workspaces are attached to the SAME branch, not independent forks.
    writer = Path(sprig.result("checkout", "main", "writer")["path"])
    stale = Path(sprig.result("checkout", "main", "stale")["path"])
    (writer / "shared.txt").write_text(TEXT.replace("first=base", "first=writer"), encoding="utf-8")
    first = sprig.result("checkpoint", "writer", "-m", "Change first line")
    (stale / "shared.txt").write_text(TEXT.replace("last=base", "last=stale"), encoding="utf-8")
    merged = sprig.result("checkpoint", "stale", "-m", "Change last line")
    expected_text = TEXT.replace("first=base", "first=writer").replace("last=base", "last=stale")
    merged_path = sprig.path("stale")
    check(merged["committed"] and merged["previous_head"] == first["revision"], "Stale commit did not merge current head")
    check((merged_path / "shared.txt").read_text() == expected_text, "Non-overlapping edits were lost")
    check(bytes(sprig.result("read", "main", "shared.txt")).decode() == expected_text, "Branch does not contain both edits")

    winner = Path(sprig.result("checkout", "main", "winner")["path"])
    rival = Path(sprig.result("checkout", "main", "rival")["path"])
    winner_text = expected_text.replace("middle=base", "middle=winner")
    rival_text = expected_text.replace("middle=base", "middle=rival")
    (winner / "shared.txt").write_text(winner_text, encoding="utf-8")
    head = sprig.result("checkpoint", "winner", "-m", "Win overlapping edit")["revision"]
    (rival / "shared.txt").write_text(rival_text, encoding="utf-8")
    conflict, conflict_record = sprig.run("checkpoint", "rival", "-m", "Conflicting edit", expected=1)
    check("merge_conflict" in conflict.get("error", {}).get("message", ""), "Failure was not a merge conflict")
    pending = sprig.result("show", "rival")
    pending_path = Path(pending["path"])
    markers = (pending_path / "shared.txt").read_text()
    listing = sprig.result("list")
    retained_generations = [item for item in listing["trash"] if item["name"] == "pre-merge-rival"]
    check(listing["branches"]["main"] == head, "Conflict moved branch head")
    check(bytes(sprig.result("read", "main", "shared.txt")).decode() == winner_text, "Conflict changed committed content")
    check(pending_path != rival and pending["pending_conflicts"], "No pending conflict generation")
    check(all(marker in markers for marker in ("<<<<<<<", "=======", ">>>>>>>")), "No conflict markers")
    check(retained_generations and (rival / "shared.txt").read_text() == rival_text, "Original generation was not retained")
    recovered = sprig.result("recover", retained_generations[0]["token"], "recovered_rival", mode="copy")
    check((Path(recovered["path"]) / "shared.txt").read_text() == rival_text, "Recovery lost unpublished edit")
    report["shared_branch"] = {
        "nonoverlapping_edits_merged": True,
        "merged_text": expected_text,
        "overlapping_edit_exit_code": conflict_record["exit_code"],
        "branch_head_unchanged_on_conflict": True,
        "branch_content_unchanged_on_conflict": True,
        "conflict_markers_present": True,
        "pending_conflict_paths": [item["path"] for item in pending["pending_conflicts"]],
        "new_conflict_generation_active": True,
        "original_generation_retained": True,
        "original_unpublished_edit_recovered": True,
    }
    doctor = sprig.result("doctor")
    check(doctor["healthy"], "Final store integrity check failed")
    report["integrity_check_passed"] = True


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", default="target/release/sprig", help="Already-built Sprig executable")
    parser.add_argument("--require-clone", action="store_true", help="Also require native cloning without fallback")
    args = parser.parse_args()
    binary = Path(args.binary).expanduser().resolve()
    report = {
        "ok": False,
        "environment": {
            "system": platform.system(),
            "release": platform.release(),
            "machine": platform.machine(),
            "python": platform.python_version(),
            "binary_build_directory": binary.parent.name if binary.parent.name in ("release", "debug") else "unspecified",
            "strict_clone_requested": args.require_clone,
        },
        "measurements": {},
        "commands": [],
        "exclusive_physical_bytes": None,
        "caveats": [
            "One warm-cache-friendly synthetic run, fixed order; not a statistical benchmark or a speedup claim.",
            "Ordinary copy uses copyfileobj and copystat, without fsync, hashing, history, or transaction publication. It has no durability parity with Sprig.",
            "Sprig durations time whole CLI processes, including scans, hashing, synchronization and metadata work. Import is separate from fork; output validation and fixture generation are not timed.",
            "CAS bytes count logical chunk-file lengths only, not manifests, native working trees, retained generations or filesystem overhead.",
            "Exclusive physical allocation and shared extents were not measured. stat.st_blocks would not establish exclusive physical storage savings.",
            "$SPRIG denotes --binary; $TMP denotes this run's isolated temporary directory. Commands are recorded in execution order; all generated files are removed afterward.",
        ],
    }
    with tempfile.TemporaryDirectory(prefix="sprig-example-") as directory:
        temporary = Path(directory)
        try:
            compare(binary, temporary, args.require_clone, report)
            report["ok"] = True
        except Exception as error:
            report["failure"] = (type(error).__name__ + ": " + str(error)).replace(
                str(binary), "$SPRIG"
            ).replace(str(temporary), "$TMP")
    print(json.dumps(report, indent=2, sort_keys=True))
    return 0 if report["ok"] else 1


if __name__ == "__main__":
    sys.exit(main())
