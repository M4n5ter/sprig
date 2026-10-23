#!/usr/bin/env python3
"""One opt-in rootful Linux writable-CAS workflow on disposable synthetic data.

Uses the real CLI and kernel FUSE path. Cleanup only normally unmounts verified
generations; a busy or uncertain mount retains the entire root for inspection.
"""

import argparse
import errno
import fcntl
import json
import mmap
import os
from pathlib import Path
import shutil
import signal
import sys
import subprocess
import tempfile


def run(binary):
    if os.geteuid() != 0 or not Path("/proc/self/mountinfo").exists():
        raise RuntimeError("this opt-in workflow requires an isolated rootful Linux runner")
    root = Path(tempfile.mkdtemp(prefix="sprig-writable-workflow-"))
    source = root / "source"
    source.mkdir()
    (source / "a.txt").write_text("alpha\nbeta\n")
    (source / "b.txt").write_text("original\n")
    (source / "directory").mkdir()
    (source / "directory" / "child").write_text("retained child\n")
    (source / "linked.txt").write_text("linked original\n")
    os.link(source / "linked.txt", source / "directory" / "linked-alias.txt")
    # An actual host ELF tests execution/mapping capability; a shell script
    # would only test its interpreter's ordinary reads and give a false signal.
    shutil.copyfile("/bin/true", source / "native-tool")
    (source / "native-tool").chmod(0o755)
    store = root / "store"
    checks = []
    capabilities = {}

    def invoke(*arguments, machine=True, fail=False, cwd=None):
        command = [str(binary), "--store", str(store)]
        if machine:
            command.append("--json")
        proc = subprocess.run(command + list(arguments), cwd=cwd or root,
                              capture_output=True, text=True, timeout=60)
        if fail:
            assert proc.returncode != 0, (arguments, proc.stdout)
            return proc
        assert proc.returncode == 0, (arguments, proc.stdout, proc.stderr)
        if not machine:
            return proc.stdout
        result = json.loads(proc.stdout)["result"]
        # A published commit plus remount failure must never be mistaken for
        # working ordinary-tool access, nor retried as another mutation.
        if isinstance(result, dict):
            assert not result.get("mount_error"), result
        return result

    def path(name, mounted=True):
        info = invoke("show", name)
        assert info["backend"]["kind"] == "writable", info
        assert info["mount_state"] == ("mounted" if mounted else "unmounted"), info
        return Path(info["path"])

    def read(reference, name):
        return bytes(invoke("read", reference, name)).decode()

    def head():
        return invoke("list")["branches"]["main"]

    def busy_checkpoint():
        before = head()
        proc = invoke("checkpoint", "main", fail=True)
        message = json.loads(proc.stdout)["error"]["message"].lower()
        assert "busy" in message or "os error 16" in message, message
        assert head() == before
        path("main")

    failure = None
    remaining = []
    try:
        invoke("init")
        imported = invoke("--writable", "import", str(source), "main")
        initial = imported["base_revision"]
        unmounted = path("main", mounted=False)
        assert not list(unmounted.iterdir()), "import materialized a writable native tree"
        invoke("mount", "main")
        tree = path("main")
        assert (tree / "a.txt").read_text() == "alpha\nbeta\n"
        assert (tree / "linked.txt").stat().st_ino == (tree / "directory/linked-alias.txt").stat().st_ino
        os.link(tree / "linked.txt", tree / "third-link.txt")
        assert (tree / "linked.txt").stat().st_nlink == 3
        (tree / "third-link.txt").write_text("all aliases\n")
        assert (tree / "directory/linked-alias.txt").read_text() == "all aliases\n"
        (tree / "third-link.txt").unlink()
        assert (tree / "linked.txt").stat().st_nlink == 2


        invoke("exec", "main", "--", "sh", "-eu", "-c",
               "printf 'atomic\nbeta\n' > .save; mv .save a.txt; "
               "printf 'append\n' >> a.txt; chmod 640 a.txt; touch a.txt; "
               "mkdir created; printf 'new\n' > created/file; "
               "mv directory moved; ln -s a.txt pointer", machine=False)
        assert (tree / "a.txt").read_text() == "atomic\nbeta\nappend\n"
        assert (tree / "a.txt").stat().st_mode & 0o777 == 0o640
        assert os.readlink(tree / "pointer") == "a.txt"
        assert (tree / "moved" / "child").read_text() == "retained child\n"

        random = os.open(tree / "random.bin", os.O_CREAT | os.O_EXCL | os.O_RDWR, 0o600)
        try:
            os.write(random, b"abcdef")
            assert os.pwrite(random, b"XY", 2) == 2
            assert os.pread(random, 6, 0) == b"abXYef"
            original_flags = fcntl.fcntl(random, fcntl.F_GETFL)
            fcntl.fcntl(random, fcntl.F_SETFL, original_flags | os.O_APPEND)
            os.lseek(random, 0, os.SEEK_SET)
            assert os.write(random, b"++") == 2
            assert os.pread(random, 8, 0) == b"abXYef++"
            fcntl.fcntl(random, fcntl.F_SETFL, original_flags & ~os.O_APPEND)
            assert os.pwrite(random, b"Z", 0) == 1
            assert os.pread(random, 8, 0) == b"ZbXYef++"
            os.ftruncate(random, 3)
            os.ftruncate(random, 9)
            assert os.pread(random, 9, 0) == b"ZbX" + b"\0" * 6
            assert os.pwrite(random, b"Q", 7) == 1
            assert os.pread(random, 9, 0) == b"ZbX" + b"\0" * 4 + b"Q\0"
            os.fsync(random)
        finally:
            os.close(random)
        directory = os.open(tree, os.O_RDONLY | os.O_DIRECTORY)
        try:
            os.fsync(directory)
        finally:
            os.close(directory)

        # Rename replacement and unlink must not redirect existing handles to
        # the new path's node, recycle the inode, or lose its private contents.
        victim = tree / "victim"
        victim.write_bytes(b"old victim")
        (tree / "replacement").write_bytes(b"new victim")
        with victim.open("rb") as old:
            old_inode = os.fstat(old.fileno()).st_ino
            os.replace(tree / "replacement", victim)
            with victim.open("rb") as new:
                assert os.fstat(new.fileno()).st_ino != old_inode
                victim.unlink()
                assert old.read() == b"old victim"
                assert new.read() == b"new victim"
                assert os.fstat(old.fileno()).st_nlink == 0
                assert os.fstat(new.fileno()).st_nlink == 0

        # The ordinary page cache is shared by aliases and processes. msync is
        # a durability boundary; checkpoint still requires every mapping closed.
        mapped_path = tree / "mapped.bin"
        mapped_path.write_bytes(b"A" * 8192)
        os.link(mapped_path, tree / "mapped-alias.bin")
        descriptor = os.open(mapped_path, os.O_RDWR | os.O_APPEND)
        mapping = mmap.mmap(descriptor, 8192, flags=mmap.MAP_SHARED,
                            prot=mmap.PROT_READ | mmap.PROT_WRITE)
        os.close(descriptor)
        mapping[0:4] = b"MAP!"
        mapping.flush()
        assert mapped_path.stat().st_size == 8192, "mmap writes incorrectly appended"
        assert (tree / "mapped-alias.bin").read_bytes()[:4] == b"MAP!"
        child = subprocess.run([sys.executable, "-c",
            "import mmap,sys; f=open(sys.argv[1],'r+b'); m=mmap.mmap(f.fileno(),0); "
            "assert m[:4]==b'MAP!'; m[4:8]=b'CHLD'; m.flush(); m.close(); f.close()",
            str(tree / "mapped-alias.bin")], capture_output=True, text=True, timeout=15)
        assert child.returncode == 0, child.stderr
        assert mapping[4:8] == b"CHLD"
        protection_source = root / "mapped-protection.c"
        protection_source.write_text('#include <sys/mman.h>\n#include <fcntl.h>\n#include <unistd.h>\n'
            'int main(int n,char **v){(void)n;int f=open(v[1],O_RDWR);if(f<0)return 1;'
            'char *p=mmap(0,4096,PROT_READ,MAP_SHARED,f,0);if(p==MAP_FAILED)return 2;'
            'if(mprotect(p,4096,PROT_READ|PROT_WRITE))return 3;p[20]=82;'
            'if(msync(p,4096,MS_SYNC))return 4;close(f);return munmap(p,4096)!=0;}\n')
        protection_binary = root / "mapped-protection"
        subprocess.run([shutil.which("cc"), str(protection_source), "-o", str(protection_binary)],
                       check=True, capture_output=True, timeout=30)
        subprocess.run([str(protection_binary), str(tree / "mapped-alias.bin")],
                       check=True, capture_output=True, timeout=15)
        assert mapping[20] == ord("R")
        with (tree / "mapped-alias.bin").open("r+b") as opened:
            opened.seek(8)
            opened.write(b"FILE")
            opened.flush()
            os.fsync(opened.fileno())
        assert mapping[8:12] == b"FILE"
        busy_checkpoint()  # Original fd is closed; the mapping itself pins it.
        os.rename(tree / "mapped-alias.bin", tree / "mapped-renamed.bin")
        mapped_path.unlink()
        mapping[12:16] = b"LINK"
        mapping.flush()
        assert (tree / "mapped-renamed.bin").read_bytes()[12:16] == b"LINK"
        with (tree / "mapped-renamed.bin").open("r+b") as opened:
            private = mmap.mmap(opened.fileno(), 8192, flags=mmap.MAP_PRIVATE,
                                prot=mmap.PROT_READ | mmap.PROT_WRITE)
            private[0:4] = b"PRIV"
            private.close()
            assert mapping[:4] == b"MAP!"
            os.ftruncate(opened.fileno(), 4096)
            os.ftruncate(opened.fileno(), 8192)
            assert mapping[4096:] == b"\0" * 4096
            os.fsync(opened.fileno())
        # SIGBUS is checked in an isolated child, never in this controller.
        child = subprocess.run([sys.executable, "-c",
            "import mmap,os,resource,sys; resource.setrlimit(resource.RLIMIT_CORE,(0,0)); "
            "f=open(sys.argv[1],'r+b'); m=mmap.mmap(f.fileno(),0); "
            "os.ftruncate(f.fileno(),4096); print(m[5000])",
            str(tree / "mapped-renamed.bin")], capture_output=True, text=True, timeout=15)
        assert child.returncode == -signal.SIGBUS, (child.returncode, child.stderr)
        mapping[16:20] = b"LAST"  # No explicit msync: normal close must drain.
        mapping.close()
        capabilities["writable_shared_mmap"] = {"supported": True, "msync": True,
            "closed_fd_mapping_busy": True, "truncate_sigbus": True,
            "alias_and_process_coherence": True}
        for label, flags in [("read_only_private_mmap", mmap.MAP_PRIVATE),
                             ("read_only_shared_mmap", mmap.MAP_SHARED)]:
            with (tree / "a.txt").open("rb") as opened:
                mapping = mmap.mmap(opened.fileno(), 0, flags=flags, prot=mmap.PROT_READ)
                assert mapping[:] == b"atomic\nbeta\nappend\n"
                mapping.close()
                capabilities[label] = {"supported": True}
        try:
            executable = subprocess.run([str(tree / "native-tool")], cwd=root,
                                        capture_output=True, text=True, timeout=15)
        except OSError as error:
            capabilities["elf_execution"] = {"supported": False, "errno": error.errno}
        else:
            capabilities["elf_execution"] = {"supported": executable.returncode == 0,
                                             "returncode": executable.returncode,
                                             "stderr": executable.stderr[-4096:]}
        compiler = shutil.which("cc")
        assert compiler, "host C compiler is required for the actual build capability check"
        (tree / "hello.c").write_text('#include <stdio.h>\nint main(void) { puts("sprig-owned-build"); return 0; }\n')
        built = subprocess.run([compiler, "hello.c", "-o", "hello"], cwd=tree,
                               capture_output=True, text=True, timeout=60)
        capabilities["compile_in_workspace"] = {"supported": built.returncode == 0,
            "returncode": built.returncode, "stdout": built.stdout[-4096:], "stderr": built.stderr[-4096:]}
        if built.returncode == 0:
            try:
                executed = subprocess.run([str(tree / "hello")], cwd=root,
                                          capture_output=True, text=True, timeout=15)
            except OSError as error:
                capabilities["execute_compiled_output"] = {"supported": False, "errno": error.errno}
            else:
                capabilities["execute_compiled_output"] = {"supported": executed.returncode == 0 and executed.stdout == "sprig-owned-build\n",
                    "returncode": executed.returncode, "stdout": executed.stdout, "stderr": executed.stderr[-4096:]}
        previous_cwd = Path.cwd()
        try:
            os.chdir(tree)
            busy_checkpoint()
        finally:
            os.chdir(previous_cwd)

        retained = tree.parent
        journal = (retained / "writable.journal").read_bytes()
        # A synthetic backup-path collision fails before State replacement. The
        # original journal/head and mounted edited view must remain authoritative.
        generation = invoke("status")["generation"]
        obstruction = store / "backups" / f"{generation}.json.zst"
        assert not obstruction.exists()
        obstruction.mkdir()
        try:
            invoke("checkpoint", "main", fail=True)
            assert head() == initial
            assert path("main") == tree
            assert (tree / "a.txt").read_bytes() == b"atomic\nbeta\nappend\n"
            assert (retained / "writable.journal").read_bytes() == journal
        finally:
            obstruction.rmdir()  # Only the empty obstruction created above.
        committed = invoke("checkpoint", "main")["revision"]
        assert committed != initial and head() == committed
        assert path("main") != tree, "checkpoint did not publish a new clean generation"
        assert (retained / "writable.journal").read_bytes() == journal
        assert read(committed, "a.txt") == "atomic\nbeta\nappend\n"
        assert read(initial, "a.txt") == "alpha\nbeta\n"
        assert read(committed, "moved/child") == "retained child\n"
        assert bytes(invoke("read", committed, "random.bin")) == b"ZbX" + b"\0" * 4 + b"Q\0"
        assert bytes(invoke("read", committed, "mapped-renamed.bin"))[:20] == b"MAP!CHLDFILELINKLAST"
        assert len(bytes(invoke("read", committed, "mapped-renamed.bin"))) == 4096
        assert not (path("main") / "victim").exists()
        checks.append("ordinary tools, positional/append I/O, truncate zero-fill, fsyncdir, inode pins, mmap policy and busy checkpoint")

        tree = path("main")
        with (tree / "journal.txt").open("wb") as opened:
            opened.write(b"durable before checkpoint\n")
            opened.flush()
            os.fsync(opened.fileno())
        invoke("unmount", "main")
        assert head() == committed
        assert not list(path("main", mounted=False).iterdir())
        invoke("mount", "main")
        assert (path("main") / "journal.txt").read_bytes() == b"durable before checkpoint\n"
        reopened = invoke("checkpoint", "main")["revision"]
        assert read(reopened, "journal.txt") == "durable before checkpoint\n"

        for name in ["writer-a", "writer-b"]:
            invoke("--writable", "checkout", "main", name)
            path(name, mounted=False)
            invoke("mount", name)
        (path("writer-a") / "a.txt").write_text("writer a\nbeta\n")
        first = invoke("checkpoint", "writer-a")["revision"]
        old_b = path("writer-b")
        (old_b / "b.txt").write_text("writer b\n")
        merged = invoke("checkpoint", "writer-b")["revision"]
        assert merged != first and path("writer-b") != old_b
        assert (path("writer-b") / "a.txt").read_text() == "writer a\nbeta\n"
        assert read("main", "b.txt") == "writer b\n"
        invoke("write", "writer-b", "sdk.txt", "--text", "SDK apply\n")
        assert (path("writer-b") / "sdk.txt").read_text() == "SDK apply\n"

        # Reuse the same two writers: writer-a is stale again. Their second
        # overlapping edit must activate a recoverable conflict, not move head.
        (path("writer-b") / "a.txt").write_text("left conflict\n")
        left = invoke("checkpoint", "writer-b")["revision"]
        (path("writer-a") / "a.txt").write_text("right conflict\n")
        invoke("checkpoint", "writer-a", fail=True)
        assert head() == left
        assert invoke("show", "writer-a")["pending_conflicts"]
        assert "<<<<<<<" in (path("writer-a") / "a.txt").read_text()
        resolved_edit = invoke("write", "writer-a", "a.txt", "--text", "resolved\n")
        assert not resolved_edit["committed"] and head() == left
        invoke("resolve", "writer-a", "a.txt")
        assert not invoke("show", "writer-a")["pending_conflicts"]
        resolved = invoke("checkpoint", "writer-a")["revision"]
        assert read(resolved, "a.txt") == "resolved\n"
        assert read(resolved, "b.txt") == "writer b\n"
        assert read(resolved, "sdk.txt") == "SDK apply\n"
        assert invoke("exec", "writer-a", "--", "cat", "a.txt", machine=False) == "resolved\n"
        path("writer-a")
        invoke("checkpoint", "writer-a", "--expected-head", initial, fail=True)
        assert head() == resolved
        checks.append("journal reopen, stale merge, managed apply, conflict resolution and exec automount")

        invoke("--writable", "restore", initial, "restored")
        path("restored", mounted=False)
        invoke("mount", "restored")
        assert (path("restored") / "a.txt").read_text() == "alpha\nbeta\n"
        invoke("--projected", "fork", "writer-a", "projected-fork")
        assert (path("writer-a") / "a.txt").read_text() == "resolved\n"
        invoke("mount", "projected-fork")
        assert (Path(invoke("show", "projected-fork")["path"]) / "a.txt").read_text() == "resolved\n"
        projected = Path(invoke("show", "projected-fork")["path"])
        assert (projected / "linked.txt").stat().st_ino == (projected / "moved/linked-alias.txt").stat().st_ino
        (projected / "linked.txt").write_text("projected aliases\n")
        assert (projected / "moved/linked-alias.txt").read_text() == "projected aliases\n"
        invoke("unmount", "projected-fork")
        invoke("mount", "projected-fork")
        assert (projected / "moved/linked-alias.txt").read_text() == "projected aliases\n"
        invoke("checkpoint", "projected-fork")
        invoke("unmount", "projected-fork")
        invoke("--writable", "fork", "writer-a", "forked")
        path("forked", mounted=False)
        invoke("mount", "forked")
        (path("forked") / "a.txt").write_text("fork only\n")
        fork_revision = invoke("checkpoint", "forked")["revision"]
        assert read(fork_revision, "a.txt") == "fork only\n"
        assert read("main", "a.txt") == "resolved\n"
        # Real Cargo output and incremental caches stay inside the owned view.
        # Record unsupported compiler operations rather than silently disabling
        # incremental compilation or replacing this with a native build.
        cargo = shutil.which("cargo")
        assert cargo, "host Cargo is required for the actual Rust capability check"
        for incremental in ("0", "1"):
            name = "cargo-" + incremental
            invoke("--writable", "restore", initial, name)
            invoke("mount", name)
            cargo_tree = path(name)
            (cargo_tree / "src").mkdir()
            (cargo_tree / "Cargo.toml").write_text(
                '[package]\nname="owned-cargo-check"\nversion="0.1.0"\nedition="2021"\n')
            source_file = cargo_tree / "src" / "main.rs"
            source_file.write_text('fn main() { println!("before"); }\n')
            environment = dict(os.environ, CARGO_INCREMENTAL=incremental,
                               CARGO_TARGET_DIR=str(cargo_tree / "target"))
            stages = []
            for stage in ("clean", "noop", "edit", "restored"):
                if stage == "edit":
                    source_file.write_text('fn main() { println!("after"); }\n')
                if stage == "restored":
                    revision = invoke("checkpoint", name)["revision"]
                    restored = name + "-restored"
                    invoke("--writable", "restore", revision, restored)
                    invoke("mount", restored)
                    cargo_tree = path(restored)
                    environment["CARGO_TARGET_DIR"] = str(cargo_tree / "target")
                built = subprocess.run([cargo, "build", "--offline", "--verbose"],
                                       cwd=cargo_tree, env=environment,
                                       capture_output=True, text=True, timeout=120)
                item = {"stage": stage, "returncode": built.returncode,
                        "stdout": built.stdout, "stderr": built.stderr}
                stages.append(item)
                assert "hard linking files" not in built.stderr, built.stderr
                if built.returncode:
                    break
                output = subprocess.run([str(cargo_tree / "target/debug/owned-cargo-check")],
                                        cwd=cargo_tree, capture_output=True, text=True, timeout=15)
                assert output.returncode == 0 and output.stdout == (
                    "after\n" if stage in {"edit", "restored"} else "before\n"), output
            capabilities["cargo_incremental_" + incremental] = {
                "supported": len(stages) == 4 and all(item["returncode"] == 0 for item in stages),
                "stages": stages}
        invoke("unmount-generation", retained.name)
        assert (retained / "writable.journal").read_bytes() == journal
        for pin in sorted((store / "native").glob("*/writable-base")):
            invoke("unmount-generation", pin.parent.name)
        assert invoke("doctor")["healthy"]
        assert invoke("gc", "--dry-run")["dry_run"]
        assert (source / "a.txt").read_text() == "alpha\nbeta\n"
        checks.append("writable restore/fork isolation, retained journal integrity and normal cleanup")
        # Exhaust only this synthetic generation's journal, not the host disk.
        # A background mmap write failure must survive daemon exit and block
        # ordinary publication; explicit recovery keeps the journal evidence.
        invoke("--writable", "restore", initial, "quota")
        invoke("mount", "quota")
        quota_tree = path("quota")
        quota_generation = quota_tree.parent
        quota_journal = quota_generation / "writable.journal"
        quota_file = quota_tree / "quota.bin"
        quota_file.write_bytes(b"Q" * (2 * 1024 * 1024))
        with quota_file.open("r+b", buffering=0) as opened:
            payload = b"P" * (1024 * 1024)
            limit = 256 * 1024 * 1024
            while limit - quota_journal.stat().st_size > 2 * 1024 * 1024:
                assert os.pwrite(opened.fileno(), payload, 0) == len(payload)
            mapping = mmap.mmap(opened.fileno(), 0, flags=mmap.MAP_SHARED,
                                prot=mmap.PROT_READ | mmap.PROT_WRITE)
            mapping[:] = b"Z" * len(mapping)
            try:
                mapping.flush()
            except OSError as error:
                assert error.errno in {errno.ENOSPC, errno.EIO}, error
            else:
                raise AssertionError("quota-limited mmap synchronization succeeded")
            mapping.close()
        quota_head = invoke("show", "quota")["base_revision"]
        invoke("unmount", "quota", fail=True)
        assert path("quota", mounted=False) == quota_tree
        assert (quota_generation / "writable-session.json").is_file()
        invoke("checkpoint", "quota", fail=True)
        assert invoke("show", "quota")["base_revision"] == quota_head
        assert invoke("writable-status", "quota")["read_only"]
        preserved = quota_journal.stat().st_size
        recovered = invoke("recover-writable", "quota", "quota-recovered")
        assert recovered["path"] != str(quota_tree)
        assert quota_journal.stat().st_size == preserved
        assert (quota_generation / "writable-session.json").is_file()
        capabilities["mmap_failure_guard"] = {"quota_error_observed": True,
            "publication_blocked_after_unmount": True, "journal_bytes_retained": preserved}
        checks.append("shared mappings synchronize through journal; failed mapped writes require explicit recovery")

    except BaseException as error:
        failure = f"{type(error).__name__}: {error}"
    finally:
        # Include unpublished generations and all retained journals, not just
        # active workspace names. Never force/lazy unmount or kill a daemon.
        if store.exists():
            for pin in sorted(list((store / "native").glob("*/writable-base")) +
                              list((store / "native").glob("*/projection-base")), reverse=True):
                try:
                    proc = subprocess.run(
                        [str(binary), "--store", str(store), "--json",
                         "unmount-generation", pin.parent.name],
                        capture_output=True, text=True, timeout=60, cwd=root)
                    if proc.returncode:
                        remaining.append({"generation": pin.parent.name,
                                          "error": proc.stderr or proc.stdout})
                except subprocess.TimeoutExpired:
                    remaining.append({"generation": pin.parent.name, "error": "normal unmount timed out"})
        if str(root) in Path("/proc/self/mountinfo").read_text():
            remaining.append({"error": "mountinfo still references the synthetic root"})
        logs = {}
        if failure or remaining:
            for log in (store / "native").glob("*/writable-*.log"):
                if log.is_file() and not log.is_symlink():
                    logs[str(log.relative_to(store))] = log.read_text(errors="replace")[-65536:]
        result = {"ok": failure is None and not remaining, "checks": checks,
                  "measured_capabilities": capabilities,
                  "server_logs": logs, "error": failure, "remaining_mounts": remaining,
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
