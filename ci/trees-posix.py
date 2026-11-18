#!/usr/bin/env python3
"""Directories and modes through a tree view, then in its history (docs/trees.md,
"The view" and "What a commit holds").

    trees-posix.py view MOUNT      mkdir -p, files in subdirectories, a directory
                                   moved, rmdir of a directory with entries, chmod +x
                                   and a script run, modes as stat shows them
    trees-posix.py history GIT_DIR what the view did, read with git once history is
                                   built (after the view's server stopped)
"""
import errno
import hashlib
import os
import stat
import subprocess
import sys
from pathlib import Path


def fanned(path):
    """`<aa>/<bb>/<path>`: `aa` and `bb` hex digits 1-2 and 3-4 of `git
    hash-object --stdin` of the path."""
    data = path.encode()
    hexa = hashlib.sha1(b"blob %d\0" % len(data) + data).hexdigest()
    return f"{hexa[:2]}/{hexa[2:4]}/{path}"


def mode(path):
    return stat.S_IMODE(os.stat(path).st_mode)


def expect_errno(code, call, *args):
    try:
        call(*args)
    except OSError as e:
        assert e.errno == code, (call, args, e)
    else:
        raise AssertionError((call, args, "succeeded"))


def view(root):
    os.umask(0o022)
    os.makedirs(root / "posix/a/b")
    assert stat.S_ISDIR(os.stat(root / "posix/a/b").st_mode)
    assert mode(root / "posix/a/b") == 0o755, oct(mode(root / "posix/a/b"))
    script = root / "posix/a/b/run.sh"
    script.write_text("#!/bin/sh\necho hi\n")
    assert mode(script) == 0o644, oct(mode(script))
    (root / "posix/a/notes").write_bytes(b"notes\n")
    os.chmod(script, mode(script) | stat.S_IXUSR | stat.S_IXGRP | stat.S_IXOTH)
    assert mode(script) == 0o755, oct(mode(script))
    out = subprocess.run([str(script)], check=True, capture_output=True).stdout
    assert out == b"hi\n", out
    fd = os.open(root / "posix/a/private", os.O_CREAT | os.O_WRONLY, 0o640)
    os.close(fd)
    assert mode(root / "posix/a/private") == 0o640
    os.mkdir(root / "posix/a/locked", 0o700)
    assert mode(root / "posix/a/locked") == 0o700
    listing = sorted(os.listdir(root / "posix/a"))
    assert listing == ["b", "locked", "notes", "private"], listing
    # A directory with entries is not removed; a move takes everything.
    expect_errno(errno.ENOTEMPTY, os.rmdir, root / "posix/a")
    (root / "posix/top").write_bytes(b"top\n")
    expect_errno(errno.ENOTDIR, os.rename, root / "posix/a", root / "posix/top")
    expect_errno(errno.EISDIR, os.rename, root / "posix/top", root / "posix/a")
    os.unlink(root / "posix/top")
    os.rename(root / "posix/a", root / "posix/c")
    assert not (root / "posix/a").exists()
    assert sorted(os.listdir(root / "posix/c")) == listing
    moved = root / "posix/c/b/run.sh"
    assert moved.read_text() == "#!/bin/sh\necho hi\n"
    assert mode(moved) == 0o755
    assert subprocess.run([str(moved)], check=True, capture_output=True).stdout == b"hi\n"
    # `..` of a directory is its parent.
    assert os.stat(root / "posix/c/b/..").st_ino == os.stat(root / "posix/c").st_ino
    os.rmdir(root / "posix/c/locked")
    expect_errno(errno.ENOENT, os.stat, root / "posix/c/locked")
    expect_errno(errno.EISDIR, os.unlink, root / "posix/c/b")
    print("trees posix view passed")


def history(git_dir):
    def git(*args):
        return subprocess.check_output(["git", "--git-dir=" + str(git_dir), *args], text=True)

    head = git("rev-parse", "root").strip()
    assert "Agentfs-Format: envelope-2" in git("show", "-s", "--format=%B", head)
    modes = {}
    for line in git("ls-tree", "-r", "--full-tree", head).splitlines():
        meta, path = line.split("\t", 1)
        modes[path] = meta.split(" ")[0]
    run = "files/" + fanned("posix/c/b/run.sh")
    assert modes.get(run) == "100755", (run, modes.get(run))
    assert modes.get("files/" + fanned("posix/c/notes")) == "100644"
    aa, bb, _ = fanned("posix/c/b").split("/", 2)
    record = f"dirs/{aa}/{bb}/posix%2Fc%2Fb"
    assert record in modes, sorted(p for p in modes if p.startswith("dirs/"))
    meta = git("show", f"{head}:{record}")
    assert '"kind":"dir"' in meta and '"mode":"0755"' in meta, meta
    assert "dirs/root" in modes
    assert not any("posix/a/" in path for path in modes), "the move left an entry"
    assert not any(p.endswith("posix%2Fc%2Flocked") for p in modes), "rmdir left its record"
    print("trees posix history passed")


if __name__ == "__main__":
    {"view": view, "history": history}[sys.argv[1]](Path(sys.argv[2]))
