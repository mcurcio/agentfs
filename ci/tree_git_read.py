#!/usr/bin/env python3
"""Read workload bytes from an immutable published tree commit."""
import hashlib
import subprocess
import time


def fanned(name):
    """`<aa>/<bb>/<name>`: where a commit keeps a file (docs/trees.md), `aa` and
    `bb` hex digits 1-2 and 3-4 of `git hash-object --stdin` of the name."""
    data = name.encode()
    hexa = hashlib.sha1(b"blob %d\0" % len(data) + data).hexdigest()
    return f"{hexa[:2]}/{hexa[2:4]}/{name}"


def read_once(repo, ref, path):
    def git(*args):
        return subprocess.check_output(["git", "--git-dir=" + str(repo), *args],
                                       stderr=subprocess.DEVNULL)
    commit = git("rev-parse", ref).strip().decode("ascii")
    message = git("show", "-s", "--format=%B", commit).splitlines()
    if b"Agentfs-Format: envelope-1" in message:
        path = "files/" + fanned(path)
    return git("show", commit + ":" + path)


def read(repo, ref, path, want=None, timeout=60.0):
    """The bytes of `path` at `ref`. History is built behind the calls
    (docs/trees.md), so with `want` this reads again until it holds `want` or
    `timeout` seconds pass, and returns the last read."""
    deadline = time.monotonic() + timeout
    while True:
        try:
            got = read_once(repo, ref, path)
        except subprocess.CalledProcessError:
            got = None
        if want is None or got == want or time.monotonic() >= deadline:
            if got is None:
                return read_once(repo, ref, path)
            return got
        time.sleep(0.1)
