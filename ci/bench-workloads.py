#!/usr/bin/env python3
"""Timed workloads for the end-to-end benchmark (design §18 test 13).

Each subcommand runs one workload and prints one JSON object on stdout.
ci/bench.sh runs them through an agentfs mount and, as the baseline, in a
plain directory on the same filesystem. Standard library only.

    bench-workloads.py clone SRC DST
        `git clone file://SRC DST`: {"seconds", "files"} (files checked out,
        submodules not counted).
    bench-workloads.py npm DIR [--files N] [--seed S]
        A deterministic node_modules-like tree of N files (default 30000)
        under DIR, written with open/write/close: {"seconds", "files",
        "bytes", "files_per_s"}.
    bench-workloads.py create DIR N
        N 64-byte files in 100 directories: {"seconds", "files",
        "files_per_s"}.
    bench-workloads.py rm DIR
        `rm -rf DIR`: {"seconds"}.
    bench-workloads.py bigclose PATH [--size 1g] [--fsync] [--copy]
        Writes SIZE bytes (a fresh 1 MiB random block, varied per MiB),
        optionally fsyncs, then times close(2): {"write_s", "fsync_s",
        "close_s"}. With --copy, also times a copy of the file plus its
        fsync, the cost of a freeze by copy: {"copy_s"}.
    bench-workloads.py compare GIT_DIR REF TREE [--exclude NAME]...
        Exits 1 unless the tree at REF in GIT_DIR equals the files and
        symlinks under TREE (directories named .git and NAME skipped):
        {"equal", "files", "only_in_history", "only_in_tree", "differ"}.
"""

import argparse
import hashlib
import json
import os
import random
import shutil
import stat
import subprocess
import sys
import time

MIB = 1 << 20


def emit(obj):
    print(json.dumps(obj, sort_keys=True))
    sys.stdout.flush()


def parse_size(text):
    text = text.strip().lower()
    shift = {"k": 10, "m": 20, "g": 30}.get(text[-1:], 0)
    number = text[:-1] if shift else text
    return int(number) << shift


def cmd_clone(args):
    start = time.perf_counter()
    subprocess.run(
        ["git", "clone", "-q", "file://" + os.path.abspath(args.src), args.dst],
        check=True,
    )
    seconds = time.perf_counter() - start
    # Files checked out: index entries other than gitlinks (submodules,
    # which a plain clone leaves as empty directories).
    staged = subprocess.run(
        ["git", "-C", args.dst, "ls-files", "-s", "-z"],
        check=True,
        capture_output=True,
    ).stdout
    files = sum(
        1 for entry in staged.split(b"\0") if entry and not entry.startswith(b"160000 ")
    )
    emit({"seconds": seconds, "files": files})


def npm_plan(files, seed):
    """[(relative path, size)] for a node_modules-like tree: about one
    package per 50 files, each with a package.json, nested lib/ and dist/
    directories, sizes lognormal around 1.5 KiB."""
    rng = random.Random(seed)
    plan = []
    package = 0
    while len(plan) < files:
        name = "pkg-%04d" % package
        if package % 7 == 3:
            name = "@scope-%d/%s" % (package % 13, name)
        base = name
        if package % 11 == 5:
            # A nested dependency.
            base = "pkg-%04d/node_modules/%s" % (max(package - 1, 0), name)
        package += 1
        plan.append((base + "/package.json", 300 + rng.randrange(900)))
        for i in range(min(49, files - len(plan))):
            directory = ("lib", "dist", "lib/util", "src", "types")[rng.randrange(5)]
            extension = (".js", ".js", ".js", ".d.ts", ".map", ".md")[rng.randrange(6)]
            size = int(min(rng.lognormvariate(7.3, 1.1), 256 * 1024))
            plan.append(("%s/%s/f%02d%s" % (base, directory, i, extension), size))
    return plan[:files]


def cmd_npm(args):
    plan = npm_plan(args.files, args.seed)
    payload = (b"/* agentfs bench */\n" * 4096)
    made = set()
    total = 0
    start = time.perf_counter()
    for rel, size in plan:
        path = os.path.join(args.dir, rel)
        parent = os.path.dirname(path)
        if parent not in made:
            os.makedirs(parent, exist_ok=True)
            made.add(parent)
        fd = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o644)
        written = 0
        while written < size:
            written += os.write(fd, payload[: min(len(payload), size - written)])
        os.close(fd)
        total += size
    seconds = time.perf_counter() - start
    emit(
        {
            "seconds": seconds,
            "files": len(plan),
            "bytes": total,
            "files_per_s": len(plan) / seconds if seconds else 0.0,
        }
    )


def cmd_create(args):
    data = b"x" * 63 + b"\n"
    start = time.perf_counter()
    for d in range(100):
        os.makedirs(os.path.join(args.dir, "d%02d" % d), exist_ok=True)
    for i in range(args.n):
        path = os.path.join(args.dir, "d%02d" % (i % 100), "f%06d" % i)
        fd = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o644)
        os.write(fd, data)
        os.close(fd)
    seconds = time.perf_counter() - start
    emit({"seconds": seconds, "files": args.n, "files_per_s": args.n / seconds})


def cmd_rm(args):
    start = time.perf_counter()
    subprocess.run(["rm", "-rf", args.dir], check=True)
    emit({"seconds": time.perf_counter() - start})


def cmd_bigclose(args):
    size = parse_size(args.size)
    block = bytearray(os.urandom(MIB))
    fd = os.open(args.path, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o644)
    start = time.perf_counter()
    written = 0
    n = 0
    while written < size:
        block[:8] = n.to_bytes(8, "little")
        chunk = memoryview(block)[: min(MIB, size - written)]
        while chunk:
            done = os.write(fd, chunk)
            chunk = chunk[done:]
            written += done
        n += 1
    write_s = time.perf_counter() - start
    fsync_s = None
    if args.fsync:
        start = time.perf_counter()
        os.fsync(fd)
        fsync_s = time.perf_counter() - start
    start = time.perf_counter()
    os.close(fd)
    result = {
        "bytes": size,
        "write_s": write_s,
        "fsync_s": fsync_s,
        "close_s": time.perf_counter() - start,
    }
    if args.copy:
        copy = args.path + ".copy"
        start = time.perf_counter()
        shutil.copyfile(args.path, copy)
        cfd = os.open(copy, os.O_RDONLY)
        os.fsync(cfd)
        os.close(cfd)
        result["copy_s"] = time.perf_counter() - start
        os.unlink(copy)
    emit(result)


def blob_id(path, size):
    h = hashlib.sha1(b"blob %d\0" % size)
    with open(path, "rb") as f:
        while True:
            chunk = f.read(4 * MIB)
            if not chunk:
                break
            h.update(chunk)
    return h.hexdigest()


def tree_entries(root, excluded):
    entries = {}
    for directory, dirs, files in os.walk(root):
        dirs[:] = sorted(d for d in dirs if d != ".git" and d not in excluded)
        for name in sorted(files):
            path = os.path.join(directory, name)
            rel = os.path.relpath(path, root)
            st = os.lstat(path)
            if stat.S_ISLNK(st.st_mode):
                target = os.readlink(os.fsencode(path))
                h = hashlib.sha1(b"blob %d\0" % len(target) + target).hexdigest()
                entries[rel] = ("120000", h)
            elif stat.S_ISREG(st.st_mode):
                mode = "100755" if st.st_mode & stat.S_IXUSR else "100644"
                entries[rel] = (mode, blob_id(path, st.st_size))
        # Symlinks to directories are listed in dirs by os.walk, which does
        # not follow them: record them as symlinks.
        for name in list(dirs):
            path = os.path.join(directory, name)
            if os.path.islink(path):
                target = os.readlink(os.fsencode(path))
                h = hashlib.sha1(b"blob %d\0" % len(target) + target).hexdigest()
                entries[os.path.relpath(path, root)] = ("120000", h)
                dirs.remove(name)
    return entries


def history_entries(git_dir, ref):
    probe = subprocess.run(
        ["git", "--git-dir", git_dir, "rev-parse", "--verify", "-q", ref + "^{tree}"],
        capture_output=True,
    )
    if probe.returncode != 0:
        return {}
    out = subprocess.run(
        ["git", "--git-dir", git_dir, "ls-tree", "-r", "-z", "--full-tree", ref],
        check=True,
        capture_output=True,
    ).stdout
    entries = {}
    for record in out.split(b"\0"):
        if not record:
            continue
        meta, path = record.split(b"\t", 1)
        mode, _kind, oid = meta.decode().split()
        entries[os.fsdecode(path)] = (mode, oid)
    return entries


def cmd_compare(args):
    history = history_entries(args.git_dir, args.ref)
    tree = tree_entries(args.tree, set(args.exclude))
    only_history = sorted(set(history) - set(tree))
    only_tree = sorted(set(tree) - set(history))
    differ = sorted(p for p in set(history) & set(tree) if history[p] != tree[p])
    equal = not (only_history or only_tree or differ)
    emit(
        {
            "equal": equal,
            "files": len(tree),
            "only_in_history": only_history[:20],
            "only_in_tree": only_tree[:20],
            "differ": differ[:20],
        }
    )
    return 0 if equal else 1


def main():
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    sub = parser.add_subparsers(dest="command", required=True)
    p = sub.add_parser("clone")
    p.add_argument("src")
    p.add_argument("dst")
    p = sub.add_parser("npm")
    p.add_argument("dir")
    p.add_argument("--files", type=int, default=30000)
    p.add_argument("--seed", type=int, default=13)
    p = sub.add_parser("create")
    p.add_argument("dir")
    p.add_argument("n", type=int)
    p = sub.add_parser("rm")
    p.add_argument("dir")
    p = sub.add_parser("bigclose")
    p.add_argument("path")
    p.add_argument("--size", default="1g")
    p.add_argument("--fsync", action="store_true")
    p.add_argument("--copy", action="store_true")
    p = sub.add_parser("compare")
    p.add_argument("git_dir")
    p.add_argument("ref")
    p.add_argument("tree")
    p.add_argument("--exclude", action="append", default=[])
    args = parser.parse_args()
    handler = {
        "clone": cmd_clone,
        "npm": cmd_npm,
        "create": cmd_create,
        "rm": cmd_rm,
        "bigclose": cmd_bigclose,
        "compare": cmd_compare,
    }[args.command]
    return handler(args) or 0


if __name__ == "__main__":
    sys.exit(main())
