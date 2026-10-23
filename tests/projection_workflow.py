#!/usr/bin/env python3
"""One rootful, synthetic Linux workflow; never force/lazy unmount on failure."""
import argparse
import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile


def run(binary):
    if os.geteuid() != 0:
        raise RuntimeError("this opt-in workflow requires an isolated rootful Linux runner")
    root = Path(tempfile.mkdtemp(prefix="sprig-projection-workflow-"))
    source = root / "source"
    source.mkdir()
    (source / "a.txt").write_text("alpha\nbeta\n")
    (source / "b.txt").write_text("original\n")
    (source / "directory").mkdir()
    (source / "directory" / "child").write_text("retained child\n")
    (source / "linked.txt").write_text("linked original\n")
    os.link(source / "linked.txt", source / "directory" / "linked-alias.txt")
    (source / "outside-link").symlink_to("/etc/passwd")
    store = root / "store"
    checks = []

    def call(*arguments, fail=False, cwd=None):
        proc = subprocess.run([str(binary), "--store", str(store), "--json", *arguments],
                              capture_output=True, text=True, timeout=60, cwd=cwd)
        if fail:
            assert proc.returncode != 0, (arguments, proc.stdout)
            return proc
        assert proc.returncode == 0, (arguments, proc.stdout, proc.stderr)
        return json.loads(proc.stdout)["result"]

    def path(name):
        info = call("show", name)
        assert info["backend"]["kind"] == "projection", info
        assert info["mount_state"] == "mounted", info
        return Path(info["path"])

    def read(reference, name):
        return bytes(call("read", reference, name)).decode()

    failure = None
    remaining = []
    try:
        call("init")
        main = call("--projected", "import", str(source), "main")
        initial = main["base_revision"]
        assert path("main").joinpath("a.txt").read_text() == "alpha\nbeta\n"
        assert os.readlink(path("main") / "outside-link") == "/etc/passwd"
        assert call("checkpoint", "main")["revision"] == initial
        # index=off must not split links on copy-up: grouped payloads start in
        # the private upper, and remount must never resurrect removed aliases.
        call("--projected", "fork", "main", "link-check")
        linked = path("link-check")
        assert (linked / "linked.txt").stat().st_ino == (linked / "directory/linked-alias.txt").stat().st_ino
        (linked / "directory/linked-alias.txt").write_text("upper group edit\n")
        assert (linked / "linked.txt").read_text() == "upper group edit\n"
        os.link(linked / "linked.txt", linked / "new-alias")
        (linked / "linked.txt").unlink()
        call("unmount", "link-check")
        call("mount", "link-check")
        assert not (linked / "linked.txt").exists()
        assert (linked / "new-alias").read_text() == "upper group edit\n"
        link_revision = call("checkpoint", "link-check")["revision"]
        assert read(link_revision, "directory/linked-alias.txt") == "upper group edit\n"
        assert call("checkpoint", "link-check")["revision"] == link_revision
        assert path("main").joinpath("linked.txt").read_text() == "linked original\n"

        call("--projected", "checkout", "main", "writer-a")
        call("--projected", "checkout", "main", "writer-b")
        (path("writer-a") / "a.txt").write_text("changed alpha\nbeta\n")
        first = call("checkpoint", "writer-a")["revision"]
        old_b = path("writer-b")
        (old_b / "b.txt").write_text("changed b\n")
        second = call("checkpoint", "writer-b")["revision"]
        assert second != first
        assert path("writer-b") != old_b
        assert (path("writer-b") / "a.txt").read_text() == "changed alpha\nbeta\n"
        assert read("main", "b.txt") == "changed b\n"
        checks.append("same-engine stale merge updates live generation and retains both edits")

        call("write", "writer-b", "sdk.txt", "--text", "SDK transaction\n")
        assert (path("writer-b") / "sdk.txt").read_text() == "SDK transaction\n"
        fork = call("fork", "writer-b", "fork", cwd=path("writer-b"))
        (path("fork") / "a.txt").write_text("fork only\n")
        assert (path("writer-b") / "a.txt").read_text() == "changed alpha\nbeta\n"
        call("checkpoint", "fork")
        with open(path("writer-b") / "a.txt", "rb"):
            call("unmount", "writer-b", fail=True)
            assert call("show", "writer-b")["mount_state"] == "mounted"
        old_epoch = os.getxattr(path("writer-b").parent / "lower", "user.sprig.session")
        call("unmount", "writer-b")
        assert call("show", "writer-b")["mount_state"] == "unmounted"
        call("mount", "writer-b")
        assert os.getxattr(path("writer-b").parent / "lower", "user.sprig.session") != old_epoch
        before_noop = call("list")["branches"]["main"]
        assert call("checkpoint", "writer-b")["revision"] == before_noop
        assert (path("writer-b") / "sdk.txt").read_text() == "SDK transaction\n"
        checks.append("SDK transaction, fork isolation, busy refusal and exact remount")

        call("--projected", "checkout", "main", "conflict-a")
        call("--projected", "checkout", "main", "conflict-b")
        (path("conflict-a") / "a.txt").write_text("left conflict\n")
        head = call("checkpoint", "conflict-a")["revision"]
        (path("conflict-b") / "a.txt").write_text("right conflict\n")
        call("checkpoint", "conflict-b", fail=True)
        assert call("list")["branches"]["main"] == head
        assert call("show", "conflict-b")["pending_conflicts"]
        assert "<<<<<<<" in (path("conflict-b") / "a.txt").read_text()
        call("write", "conflict-b", "a.txt", "--text", "resolved\n")
        call("resolve", "conflict-b", "a.txt")
        resolved = call("checkpoint", "conflict-b")["revision"]
        assert read("main", "a.txt") == "resolved\n"
        call("checkpoint", "conflict-b", "--expected-head", initial, fail=True)
        checks.append("conflict view, SDK resolution, head CAS and history")

        # Exercise trusted kernel namespace changes against the same core capture.
        tree = path("conflict-b")
        (tree / "directory").rename(tree / "moved")
        (tree / "b.txt").unlink()
        (tree / "moved" / "child").write_text("edited after move\n")
        namespace = call("checkpoint", "conflict-b")["revision"]
        assert read(namespace, "moved/child") == "edited after move\n"
        assert read(initial, "directory/child") == "retained child\n"
        assert not (path("conflict-b") / "b.txt").exists()
        call("write", "conflict-b", "outside-link/escape", "--text", "no", fail=True)
        restored = call("--projected", "restore", initial, "restored")
        assert (Path(restored["path"]) / "a.txt").read_text() == "alpha\nbeta\n"
        retained = Path(restored["path"]).parent
        call("unmount", "restored")
        (retained / "upper").rename(retained / "retained-upper")
        call("mount", "restored", fail=True)
        assert not (retained / "upper").exists()
        (retained / "retained-upper").rename(retained / "upper")
        call("mount", "restored")
        trash = call("drop", "workspace", "fork")
        recovered = call("recover", trash["token"], "recovered")
        assert Path(recovered["path"]).joinpath("a.txt").read_text() == "fork only\n"
        assert recovered["backend"]["kind"] == "projection"
        assert call("doctor")["healthy"]
        assert call("gc", "--dry-run")["dry_run"]
        assert call("log", "main")
        assert call("diff-files", initial, resolved)
        assert call("blame", resolved, "a.txt")
        checks.append("rename, whiteout, symlink safety, restore, trash recovery and inspection")
    except BaseException as error:
        failure = str(error)
    finally:
        # Include unpublished generations from failed preparations. No pid kill,
        # recursive deletion over mounts, umount -f, or lazy detach fallback.
        if store.exists():
            for pin in sorted((store / "native").glob("*/projection-base"), reverse=True):
                proc = subprocess.run([str(binary), "--store", str(store), "--json",
                                       "unmount-generation", pin.parent.name],
                                      capture_output=True, text=True, timeout=60)
                if proc.returncode:
                    remaining.append({"generation": pin.parent.name,
                                      "error": proc.stderr or proc.stdout})
        mounts = Path("/proc/self/mountinfo").read_text()
        if str(root) in mounts:
            remaining.append({"error": "mountinfo still references the synthetic root"})
        logs = {}
        if failure or remaining:
            for log in (store / "native").glob("*/projection-*.log"):
                if log.is_file() and not log.is_symlink():
                    logs[str(log.relative_to(store))] = log.read_text(errors="replace")[-65536:]
        result = {"ok": failure is None and not remaining, "checks": checks, "server_logs": logs,
                  "error": failure, "remaining_mounts": remaining,
                  "retained_root": str(root) if failure or remaining else None}
        if not failure and not remaining:
            shutil.rmtree(root)
        print(json.dumps(result, indent=2))
    if failure or remaining:
        raise SystemExit(1)


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, required=True)
    args = parser.parse_args()
    run(args.binary.resolve(strict=True))
