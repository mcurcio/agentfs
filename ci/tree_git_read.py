#!/usr/bin/env python3
"""Read workload bytes from an immutable published tree commit."""
import subprocess
import time


def read_once(repo, ref, path):
    def git(*args):
        return subprocess.check_output(["git", "--git-dir=" + str(repo), *args],
                                       stderr=subprocess.DEVNULL)
    commit = git("rev-parse", ref).strip().decode("ascii")
    message = git("show", "-s", "--format=%B", commit).splitlines()
    prefix = "files/" if b"Agentfs-Format: envelope-1" in message else ""
    return git("show", commit + ":" + prefix + path)


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
