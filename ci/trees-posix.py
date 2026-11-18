#!/usr/bin/env python3
"""Directories, modes, owners, links and times through a tree view, then in its
history (docs/trees.md, "The view" and "What a commit holds"). Run as root on
a view mounted with allow_other: the non-root cases run in a forked child as
uid and gid 10000.

    trees-posix.py view MOUNT      mkdir -p, files in subdirectories, a directory
                                   moved, rmdir of a directory with entries, chmod +x
                                   and a script run, modes as stat shows them;
                                   owners, chown and chgrp, set-group-ID
                                   directories, symbolic links, times moved by
                                   saves and creates
    trees-posix.py history GIT_DIR what the view did, read with git once history is
                                   built (after the view's server stopped)
    trees-posix.py touch MOUNT     touch and touch -d set times (no commit: the
                                   caller checks that history's head stays)
    trees-posix.py touched MOUNT   the times touch set, after a restart
"""
import errno
import hashlib
import json
import os
import stat
import subprocess
import sys
import time
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


USER = 10000
TOUCHED = "posix/c/notes"
# touch -d @1000000000, in nanoseconds.
EPOCH_NS = 1_000_000_000 * 1_000_000_000


def as_user(call, *args):
    """Runs `call(*args)` in a child with uid and gid USER and no supplementary
    groups; an exception there fails this process."""
    pid = os.fork()
    if pid == 0:
        try:
            os.setgroups([])
            os.setresgid(USER, USER, USER)
            os.setresuid(USER, USER, USER)
            call(*args)
        except BaseException as e:  # noqa: BLE001 - reported to the parent
            print(f"as uid {USER}: {call.__name__}{args}: {e!r}", file=sys.stderr)
            os._exit(1)
        os._exit(0)
    _, status = os.waitpid(pid, 0)
    assert os.waitstatus_to_exitcode(status) == 0, (call.__name__, args)


def owner(path):
    st = os.lstat(path)
    return (st.st_uid, st.st_gid)


def owners(root):
    """Files keep their owners; the kernel lets each user do what the modes
    allow; root chowns, the owner chgrps."""
    base = root / "posix/o"
    os.mkdir(base)
    os.chmod(base, 0o777)
    as_user(lambda: (base / "mine").write_bytes(b"mine\n"))
    assert owner(base / "mine") == (USER, USER), owner(base / "mine")
    as_user(os.mkdir, base / "mydir")
    assert owner(base / "mydir") == (USER, USER), owner(base / "mydir")
    (base / "rootfile").write_bytes(b"root\n")
    os.mkdir(base / "rootdir", 0o755)
    assert mode(base / "rootfile") == 0o644 and owner(base / "rootfile") == (0, 0)

    def refused():
        expect_errno(errno.EACCES, os.open, base / "rootfile", os.O_WRONLY)
        expect_errno(errno.EACCES, os.open, base / "rootdir/x", os.O_CREAT | os.O_WRONLY, 0o644)

    as_user(refused)
    os.chown(base / "rootfile", USER, USER)
    os.chown(base / "rootdir", USER, USER)
    assert owner(base / "rootfile") == (USER, USER), owner(base / "rootfile")

    def allowed():
        (base / "rootfile").write_bytes(b"user\n")
        (base / "rootdir/x").write_bytes(b"x\n")

    as_user(allowed)
    assert (base / "rootfile").read_bytes() == b"user\n"
    assert owner(base / "rootdir/x") == (USER, USER)
    # A chgrp by the owner, to a group it is in; to another it is refused.
    os.chown(base / "mine", USER, 0)

    def chgrp():
        os.chown(base / "mine", -1, USER)
        expect_errno(errno.EPERM, os.chown, base / "mine", -1, 0)
        expect_errno(errno.EPERM, os.chown, base / "mine", 0, -1)

    as_user(chgrp)
    assert owner(base / "mine") == (USER, USER), owner(base / "mine")


def setgid(root):
    """A set-group-ID directory gives new entries its group, and new
    subdirectories keep the bit."""
    shared = root / "posix/shared"
    os.mkdir(shared)
    os.chown(shared, 0, 4000)
    os.chmod(shared, 0o2777)
    assert mode(shared) == 0o2777, oct(mode(shared))

    def make():
        (shared / "f").write_bytes(b"f\n")
        os.mkdir(shared / "sub")

    as_user(make)
    assert owner(shared / "f") == (USER, 4000), owner(shared / "f")
    assert owner(shared / "sub") == (USER, 4000), owner(shared / "sub")
    assert mode(shared / "sub") & stat.S_ISGID, oct(mode(shared / "sub"))


def links(root):
    """ln -s, readlink, a link to a directory followed by the kernel, mv and
    rm of a link."""
    link = root / "posix/c/link"
    os.symlink("b/run.sh", link)
    assert stat.S_ISLNK(os.lstat(link).st_mode)
    assert os.lstat(link).st_size == len("b/run.sh")
    assert os.readlink(link) == "b/run.sh", os.readlink(link)
    assert link.read_text() == "#!/bin/sh\necho hi\n"
    os.symlink("other", root / "posix/c/other-link")
    assert os.readlink(root / "posix/c/other-link") == "other"
    assert os.readlink(link) == "b/run.sh", "readlink read another link"
    dirlink = root / "posix/dirlink"
    os.symlink("c/b", dirlink)
    assert stat.S_ISDIR(os.stat(dirlink).st_mode)
    assert os.listdir(dirlink) == ["run.sh"], os.listdir(dirlink)
    expect_errno(errno.EEXIST, os.symlink, "x", dirlink)
    os.rename(link, root / "posix/c/moved-link")
    expect_errno(errno.ENOENT, os.lstat, link)
    assert os.readlink(root / "posix/c/moved-link") == "b/run.sh"
    os.unlink(dirlink)
    os.unlink(root / "posix/c/other-link")
    expect_errno(errno.ENOENT, os.lstat, dirlink)
    assert os.path.isdir(root / "posix/c/b")


def times(root):
    """A save moves its file's mtime, and a create its directory's."""
    notes = root / "posix/c/notes"
    os.utime(notes, ns=(EPOCH_NS, EPOCH_NS))
    assert os.stat(notes).st_mtime_ns == EPOCH_NS
    notes.write_bytes(b"notes, saved again\n")
    assert os.stat(notes).st_mtime_ns > EPOCH_NS, os.stat(notes).st_mtime_ns
    folder = root / "posix/c"
    os.utime(folder, ns=(EPOCH_NS, EPOCH_NS))
    (folder / "new").write_bytes(b"new\n")
    assert os.stat(folder).st_mtime_ns > EPOCH_NS, os.stat(folder).st_mtime_ns


def touch(root):
    path = root / TOUCHED
    before = time.time_ns()
    subprocess.run(["touch", str(path)], check=True)
    st = os.stat(path)
    assert st.st_mtime_ns >= before and st.st_atime_ns >= before, (before, st)
    subprocess.run(["touch", "-d", "@1000000000", str(path)], check=True)
    st = os.stat(path)
    assert (st.st_mtime_ns, st.st_atime_ns) == (EPOCH_NS, EPOCH_NS), st
    print("trees posix touch passed")


def touched(root):
    st = os.stat(root / TOUCHED)
    assert (st.st_mtime_ns, st.st_atime_ns) == (EPOCH_NS, EPOCH_NS), st
    print("trees posix touched passed")


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
    owners(root)
    setgid(root)
    links(root)
    times(root)
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
    # Owners: in the lines/ entry's meta and the directory records.
    entry = json.loads(git("show", f"{head}:lines/" + fanned("posix/o/mine")))
    assert entry[2] == {"kind": "file", "mode": "0644", "uid": USER, "gid": USER}, entry
    rootfile = json.loads(git("show", f"{head}:lines/" + fanned("posix/o/rootfile")))
    assert (rootfile[2]["uid"], rootfile[2]["gid"]) == (USER, USER), rootfile
    aa, bb, _ = fanned("posix/shared/sub").split("/", 2)
    sub = json.loads(git("show", f"{head}:dirs/{aa}/{bb}/posix%2Fshared%2Fsub"))
    assert (sub["uid"], sub["gid"]) == (USER, 4000) and int(sub["mode"], 8) & stat.S_ISGID, sub
    # A link is a 120000 blob holding its target; removed links are gone.
    link = "files/" + fanned("posix/c/moved-link")
    assert modes.get(link) == "120000", (link, modes.get(link))
    assert git("show", f"{head}:{link}") == "b/run.sh"
    for gone in ("posix/c/link", "posix/dirlink", "posix/c/other-link"):
        assert "files/" + fanned(gone) not in modes, gone
    print("trees posix history passed")


if __name__ == "__main__":
    commands = {"view": view, "history": history, "touch": touch, "touched": touched}
    commands[sys.argv[1]](Path(sys.argv[2]))
