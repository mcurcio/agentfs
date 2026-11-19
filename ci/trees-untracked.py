#!/usr/bin/env python3
"""The operator's rules and the untracked area through tree views
(docs/trees.md, "Rules and the untracked area"). The views' volume carries
these rules (ci/e2e-trees.sh writes them before the first mount):

    history.git/info/exclude     *.log, /sessions/
    history.git/info/attributes  *.db, *.db-wal, *.db-shm and .env -agentfs

    trees-untracked.py view MOUNT          as root: a secret, a log, a session,
                                           a tracked note, files of uid 10000,
                                           SQLite in WAL mode written by two
                                           processes, a nested git repository,
                                           the merged listing, rmdir, EXDEV
    trees-untracked.py early-fork MOUNT    a fork made before any of them shows
                                           none of the root's untracked files
    trees-untracked.py writer DB           writes the database until killed
    trees-untracked.py remount MOUNT       after a kill and a restart, the root's
                                           untracked files are still there
    trees-untracked.py history GIT_DIR     no untracked name is in history; the
                                           note and the repository's directory are
    trees-untracked.py fork FORK MAIN      a fork made after them starts with a
                                           copy; later writes stay on their side;
                                           a database copied while being written
                                           opens and passes integrity_check
    trees-untracked.py badrules BIN DIR    a rule not built refuses the mount,
                                           naming its line
    trees-untracked.py untrack BIN DIR     a tracked file the rules come to mark
                                           -agentfs leaves history at the next
                                           mount and reads back as it was
"""
import errno
import hashlib
import os
import sqlite3
import stat
import subprocess
import sys
import time
from pathlib import Path

USER = 10000
SECRET = b"TOKEN=never-in-history\n"
# Every name the rules leave out, as the view checks make them.
UNTRACKED = [".env", "app.log", "sessions", "sessions/s.json", "state.db",
             "state.db-wal", "state.db-shm", "busy.db", "repo/.git", "repo/x",
             "u/x.log", "u/.env", "d/x.log"]


def dir_record(path):
    """A directory's record: `dirs/<aa>/<bb>/<key>`, the key `git hash-object
    --stdin` of its path."""
    hexa = hashlib.sha1(b"blob %d\0" % len(path.encode()) + path.encode()).hexdigest()
    return f"dirs/{hexa[:2]}/{hexa[2:4]}/{hexa}"


def as_user(call, *args):
    """Runs `call(*args)` in a child with uid and gid USER; an exception there
    fails this process."""
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


def expect_errno(code, call, *args):
    try:
        call(*args)
    except OSError as e:
        assert e.errno == code, (call.__name__, args, e)
        return
    raise AssertionError(f"{call.__name__}{args} succeeded; expected {errno.errorcode[code]}")


def owner(path):
    st = os.lstat(path)
    return (st.st_uid, st.st_gid)


def write_rows(db, writer, rows):
    """One process's writes: `rows` inserts, each its own transaction."""
    con = sqlite3.connect(db, timeout=30)
    con.execute("PRAGMA busy_timeout=30000")
    for i in range(rows):
        con.execute("INSERT INTO t (writer, n) VALUES (?, ?)", (writer, i))
        con.commit()
    con.close()


def integrity(db):
    con = sqlite3.connect(db, timeout=30)
    assert con.execute("PRAGMA integrity_check").fetchone() == ("ok",)
    count = con.execute("SELECT count(*) FROM t").fetchone()[0]
    con.close()
    return count


def user_files(root):
    """uid 10000's files in a directory it may write: each owned by it."""
    for name in ("x.log", ".env", "n.md"):
        (root / "u" / name).write_bytes(b"mine")
        assert owner(root / "u" / name) == (USER, USER), name


def view(root):
    (root / ".env").write_bytes(SECRET)
    (root / "app.log").write_bytes(b"line 1\n")
    (root / "sessions").mkdir()
    (root / "sessions" / "s.json").write_bytes(b'{"id": 1}')
    (root / "notes.md").write_bytes(b"recorded\n")
    for name in (".env", "app.log", "sessions/s.json", "notes.md"):
        assert owner(root / name) == (0, 0), name
    # The secret is an ordinary file: its mode is the file's own.
    os.chmod(root / ".env", 0o600)
    assert stat.S_IMODE(os.stat(root / ".env").st_mode) == 0o600
    (root / "u").mkdir()
    os.chmod(root / "u", 0o777)
    as_user(user_files, root)

    # Linux semantics: a second open sees a write before the first closes.
    with open(root / "app.log", "ab", buffering=0) as first:
        first.write(b"line 2\n")
        assert (root / "app.log").read_bytes() == b"line 1\nline 2\n"
        os.fsync(first.fileno())

    # SQLite in WAL mode, its -shm shared-mapped, written by two processes at once.
    db = root / "state.db"
    con = sqlite3.connect(db)
    assert con.execute("PRAGMA journal_mode=WAL").fetchone() == ("wal",)
    con.execute("CREATE TABLE t (writer INTEGER, n INTEGER)")
    con.commit()
    rows = 200
    pids = []
    for writer in (1, 2):
        pid = os.fork()
        if pid == 0:
            try:
                write_rows(db, writer, rows)
            except BaseException as e:  # noqa: BLE001
                print(f"writer {writer}: {e!r}", file=sys.stderr)
                os._exit(1)
            os._exit(0)
        pids.append(pid)
    for pid in pids:
        _, status = os.waitpid(pid, 0)
        assert os.waitstatus_to_exitcode(status) == 0
    assert con.execute("SELECT count(*) FROM t").fetchone()[0] == 2 * rows
    con.close()
    assert integrity(db) == 2 * rows

    # A nested repository: its directory is recorded, its files are not.
    git = ["git", "-c", "user.name=t", "-c", "user.email=t@example.com",
           "-c", "init.defaultBranch=main"]
    subprocess.run([*git, "init", "-q", str(root / "repo")], check=True)
    (root / "repo" / "x").write_bytes(b"in the nested repository\n")
    subprocess.run([*git, "-C", str(root / "repo"), "add", "x"], check=True)
    subprocess.run([*git, "-C", str(root / "repo"), "commit", "-q", "-m", "x"], check=True)
    log = subprocess.check_output([*git, "-C", str(root / "repo"), "log", "--format=%s"], text=True)
    assert log == "x\n", log
    assert (root / "repo" / ".git" / "HEAD").exists()

    # One namespace: ls shows both halves.
    names = set(os.listdir(root))
    for name in (".env", "app.log", "sessions", "notes.md", "state.db", "repo", "u"):
        assert name in names, (name, names)
    assert set(os.listdir(root / "repo")) >= {".git", "x"}
    # A tracked directory holding an untracked file is not empty.
    (root / "d").mkdir()
    (root / "d" / "x.log").write_bytes(b"x")
    expect_errno(errno.ENOTEMPTY, os.rmdir, root / "d")
    # Across the halves: EXDEV; within the untracked half: a rename.
    expect_errno(errno.EXDEV, os.rename, root / "notes.md", root / "notes.log")
    expect_errno(errno.EXDEV, os.rename, root / "app.log", root / "app.txt")
    os.rename(root / "app.log", root / "app2.log")
    os.rename(root / "app2.log", root / "app.log")
    # A hard link to an untracked file is refused, as for a named tracked file.
    expect_errno(errno.EPERM, os.link, root / "app.log", root / "app-link.log")


def early_fork(root):
    names = set(os.listdir(root))
    for name in (".env", "app.log", "sessions", "state.db", "repo"):
        assert name not in names, (name, names)
    assert not (root / "u" / "x.log").exists()


def writer(db):
    con = sqlite3.connect(db, timeout=30)
    con.execute("PRAGMA journal_mode=WAL")
    con.execute("CREATE TABLE IF NOT EXISTS t (writer INTEGER, n INTEGER, pad BLOB)")
    con.commit()
    n = 0
    while True:
        con.execute("INSERT INTO t (writer, n, pad) VALUES (3, ?, randomblob(3000))", (n,))
        con.commit()
        n += 1
        if n == 50:
            # The caller waits for this line before it kills the view host.
            print("writing", flush=True)


def remount(root):
    assert (root / ".env").read_bytes() == SECRET
    assert (root / "sessions" / "s.json").read_bytes() == b'{"id": 1}'
    assert (root / "app.log").read_bytes() == b"line 1\nline 2\n"
    assert integrity(root / "state.db") == 400
    assert (root / "notes.md").read_bytes() == b"recorded\n"
    assert owner(root / "u" / "x.log") == (USER, USER)
    assert (root / "repo" / ".git" / "HEAD").exists()


def history(git_dir):
    def git(*args):
        return subprocess.check_output(["git", "--git-dir=" + str(git_dir), *args], text=True)

    paths = {p for p in git("log", "--all", "--name-only", "--format=").splitlines() if p}
    leaked = [p for p in paths
              if p.startswith("files/") and any(p.endswith("/" + name) for name in UNTRACKED)]
    assert not leaked, leaked
    assert any(p.startswith("files/") and p.endswith("/notes.md") for p in paths)
    assert any(p.startswith("files/") and p.endswith("/u/n.md") for p in paths)
    assert dir_record("repo") in paths, "the repository's directory is recorded"
    for name in ("sessions", "repo/.git"):
        assert dir_record(name) not in paths, name
    text = git("log", "--all", "-p", "--format=")
    assert "never-in-history" not in text
    assert "in the nested repository" not in text


def fork(fork_root, main):
    # A copy of the root's area as it was when the fork was made.
    assert (fork_root / ".env").read_bytes() == SECRET
    assert (fork_root / "sessions" / "s.json").read_bytes() == b'{"id": 1}'
    assert integrity(fork_root / "state.db") == 400
    # Copied while a writer was writing it: it opens and passes integrity_check.
    assert integrity(fork_root / "busy.db") >= 0
    assert (fork_root / "notes.md").read_bytes() == b"recorded\n"
    # Later writes stay on their side.
    with open(main / "app.log", "ab") as f:
        f.write(b"upstream later\n")
    assert (fork_root / "app.log").read_bytes() == b"line 1\nline 2\n"
    (fork_root / ".env").write_bytes(b"TOKEN=fork\n")
    assert (main / ".env").read_bytes() == SECRET
    (fork_root / "fork-only.log").write_bytes(b"x")
    assert not (main / "fork-only.log").exists()


def badrules(binary, work):
    work = Path(work)
    state, mount = work / "bad-state", work / "bad-mount"
    mount.mkdir()
    subprocess.run([binary, "--state", str(state), "trees", "init", "--volume", "bad"], check=True,
                   stdout=subprocess.DEVNULL)
    (state / "history.git" / "info").mkdir(exist_ok=True)
    (state / "history.git" / "info" / "attributes").write_text("*.db agentfs.group=x\n")
    proc = subprocess.Popen([binary, "--state", str(state), "trees", "mount", str(mount)],
                            stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
    try:
        _, stderr = proc.communicate(timeout=60)
    except subprocess.TimeoutExpired:
        # It mounted: the rule was taken. Unmount, and fail.
        subprocess.run(["fusermount3", "-uz", str(mount)], check=False)
        proc.kill()
        proc.communicate()
        raise AssertionError("a volume with agentfs.group=x mounted")
    assert proc.returncode != 0, proc.returncode
    assert "line 1" in stderr and "agentfs.group=x" in stderr, stderr
    assert not os.path.ismount(mount)


def serve(binary, state, mount, log):
    """Mounts the root of the volume at `state` on `mount`: the server, once
    the mount is up."""
    proc = subprocess.Popen([binary, "--state", str(state), "trees", "mount", str(mount)],
                            stdout=subprocess.DEVNULL, stderr=log)
    for _ in range(600):
        if os.path.ismount(mount):
            return proc
        assert proc.poll() is None, f"trees mount exited with {proc.returncode}"
        time.sleep(0.1)
    proc.kill()
    raise AssertionError("trees mount did not come up")


def unmount(proc):
    proc.terminate()
    assert proc.wait(timeout=120) == 0, proc.returncode


def untrack(binary, work):
    work = Path(work)
    state, mount = work / "untrack-state", work / "untrack-mount"
    mount.mkdir()
    subprocess.run([binary, "--state", str(state), "trees", "init", "--volume", "untrack"],
                   check=True, stdout=subprocess.DEVNULL)

    def git(*args):
        return subprocess.check_output(["git", "--git-dir=" + str(state / "history.git"), *args],
                                       text=True)

    def unlinks():
        return [s for s in git("log", "--format=%s", "root").splitlines()
                if s.startswith("Unlink")]

    with open(work / "untrack.log", "ab") as log:
        # Recorded with no attributes.
        proc = serve(binary, state, mount, log)
        (mount / "secret.env").write_bytes(SECRET)
        os.chmod(mount / "secret.env", 0o600)
        (mount / "keep.md").write_bytes(b"kept\n")
        unmount(proc)
        listed = git("ls-tree", "-r", "--name-only", "root")
        assert "secret.env" in listed and "keep.md" in listed, listed
        kept = [e for e in git("ls-tree", "-r", "root").splitlines() if e.endswith("keep.md")]
        info = state / "history.git" / "info"
        info.mkdir(exist_ok=True)
        with open(info / "attributes", "a") as f:
            f.write("*.env -agentfs\n")
        # The next mount moves it to the untracked area and records its unlink.
        proc = serve(binary, state, mount, log)
        assert (mount / "secret.env").read_bytes() == SECRET
        assert stat.S_IMODE(os.lstat(mount / "secret.env").st_mode) == 0o600
        assert (mount / "keep.md").read_bytes() == b"kept\n"
        unmount(proc)
    listed = git("ls-tree", "-r", "--name-only", "root")
    assert "secret.env" not in listed, listed
    assert [e for e in git("ls-tree", "-r", "root").splitlines() if e.endswith("keep.md")] == kept
    assert unlinks() == ["Unlink by agentfs"], unlinks()
    assert git("log", "-1", "--format=%s", "root").strip() == "Unlink by agentfs"
    text = (work / "untrack.log").read_text()
    assert "tree Untracked: tree=root path=secret.env: marked -agentfs" in text, text
    # A third mount records nothing new.
    head = git("rev-parse", "root")
    with open(work / "untrack.log", "ab") as log:
        proc = serve(binary, state, mount, log)
        assert (mount / "secret.env").read_bytes() == SECRET
        unmount(proc)
    assert git("rev-parse", "root") == head
    assert len(unlinks()) == 1


if __name__ == "__main__":
    commands = {
        "view": lambda: view(Path(sys.argv[2])),
        "early-fork": lambda: early_fork(Path(sys.argv[2])),
        "writer": lambda: writer(Path(sys.argv[2])),
        "remount": lambda: remount(Path(sys.argv[2])),
        "history": lambda: history(Path(sys.argv[2])),
        "fork": lambda: fork(Path(sys.argv[2]), Path(sys.argv[3])),
        "badrules": lambda: badrules(sys.argv[2], sys.argv[3]),
        "untrack": lambda: untrack(sys.argv[2], sys.argv[3]),
    }
    if sys.argv[1] in ("view", "remount", "fork") and os.geteuid() != 0:
        sys.exit(f"trees-untracked.py {sys.argv[1]} must run as root (euid {os.geteuid()})")
    commands[sys.argv[1]]()
