#!/usr/bin/env python3
"""A view-host restart with handles open (git-model §18's objects row).

Usage: trees-restart.py AGENTFS_BIN STATE MOUNT. Plain FUSE ends the connection
with its server, so an old handle's later writes fail, the file holds what
returned before the kill, saved once, and a file replaced through the new mount
stays unchanged. Each wait is on the host's pid or the mount it makes.
"""
import errno
import mmap
import os
from pathlib import Path
import signal
import subprocess
import sys
import tempfile
import time

from tree_git_read import read

DEAD = (errno.ENOTCONN, errno.ECONNABORTED)
SIZE = mmap.PAGESIZE


def mounted(root):
    return subprocess.run(["mountpoint", "-q", str(root)]).returncode == 0


def mount(binary, state, root, log):
    host = subprocess.Popen([binary, "--state", str(state), "trees", "mount", str(root)],
                            stdout=log, stderr=log)
    for _ in range(300):
        if mounted(root):
            return host
        if host.poll() is not None:
            raise SystemExit(f"the view host exited {host.returncode} before mounting")
        time.sleep(0.1)
    raise SystemExit("the mount did not appear")


def check(name, observed, expected):
    print(f"{name}: observed={observed!r}, expected={expected!r}", flush=True)
    if observed != expected:
        raise SystemExit(f"FAILED: {name}")


def write_fails(name, fd):
    try:
        os.pwrite(fd, b"late", 0)
    except OSError as exc:
        check(name, exc.errno in DEAD, True)
    else:
        raise SystemExit(f"FAILED: {name}: the write returned")


def linux_bytes():
    """The same calls on a plain Linux file."""
    with tempfile.TemporaryFile(buffering=0) as plain:
        os.pwrite(plain.fileno(), b"fsynced!", 0)
        os.ftruncate(plain.fileno(), SIZE)
        with mmap.mmap(plain.fileno(), SIZE) as view:
            view[100:106] = b"mapped"
            view.flush()
        return os.pread(plain.fileno(), SIZE + 1, 0)


def saves_of(state, name):
    out = subprocess.check_output(["git", "--git-dir", str(state / "history.git"), "log",
                                   "main", "--format=%s", "--", f"files/{name}"])
    return out.decode().splitlines()


def main():
    if len(sys.argv) != 4:
        raise SystemExit(__doc__)
    binary, state, root = sys.argv[1], Path(sys.argv[2]), Path(sys.argv[3])
    log = open(state.parent / "mount.log", "ab")
    host = mount(binary, state, root, log)
    expected = linux_bytes()

    # The held file: an fsynced write and an msync'd mapped store, on one open.
    held = root / "held"
    fd = os.open(held, os.O_CREAT | os.O_RDWR, 0o600)
    os.pwrite(fd, b"fsynced!", 0)
    os.fsync(fd)
    os.ftruncate(fd, SIZE)
    view = mmap.mmap(fd, SIZE)
    view[100:106] = b"mapped"
    view.flush()

    # A file replaced while an open of its old object lives. At restart that open
    # must find its object by number: by name it would write the replacement.
    replaced = root / "replaced-before"
    replaced.write_bytes(b"seed")
    old = os.open(replaced, os.O_RDWR)
    os.pwrite(old, b"old object", 0)
    os.fsync(old)
    (root / "replaced-before.new").write_bytes(b"replacement before the kill")
    os.rename(root / "replaced-before.new", replaced)

    host.send_signal(signal.SIGKILL)
    host.wait()
    write_fails("the old handle's write after the kill", fd)
    subprocess.check_call(["fusermount3", "-uz", str(root)])
    host = mount(binary, state, root, log)

    check("the file holds what returned before the kill", held.read_bytes(), expected)
    check("main history holds the same", read(state / "history.git", "main", "held"), expected)
    check("one save besides the create", len(saves_of(state, "held")), 2)
    check("a file replaced before the kill stays the replacement",
          replaced.read_bytes(), b"replacement before the kill")

    (root / "held.new").write_bytes(b"replacement")
    os.rename(root / "held.new", held)
    write_fails("the old handle's write after the replacement", fd)
    check("the replacement is unchanged", held.read_bytes(), b"replacement")
    check("main history holds the replacement",
          read(state / "history.git", "main", "held"), b"replacement")

    view.close()
    for stale in (fd, old):
        try:
            os.close(stale)
        except OSError as exc:
            check("closing an old handle", exc.errno in DEAD, True)
    check("the replacement survives the old handle's close", held.read_bytes(), b"replacement")
    subprocess.check_call(["fusermount3", "-u", str(root)])
    check("the view host exits cleanly", host.wait(), 0)
    print("view-host restart with handles open: passed", flush=True)


if __name__ == "__main__":
    main()
