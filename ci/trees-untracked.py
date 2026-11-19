#!/usr/bin/env python3
"""The operator's rules and the untracked area through tree views
(docs/trees.md, "Rules and the untracked area"). The views' volume carries
these rules (ci/e2e-trees.sh writes them before the first mount):

    history.git/info/exclude     *.log, /sessions/, /logs/
    history.git/info/attributes  *.db, *.db-wal, *.db-shm and .env -agentfs

    trees-untracked.py view MOUNT HOST     as root: a secret, a log, a session,
                                           a tracked note, files of uid 10000,
                                           SQLite in WAL mode written by two
                                           processes, a nested git repository,
                                           the merged listing, rmdir, moves and
                                           links across and within the halves,
                                           a FIFO and a unix socket, O_TMPFILE
    trees-untracked.py early-fork MOUNT    a fork made before any of them shows
                                           none of the root's untracked files
    trees-untracked.py writer DB           writes the database until killed
    trees-untracked.py remount MOUNT HOST  after a kill and a restart, the root's
                                           untracked files are still there
    trees-untracked.py history GIT_DIR     no untracked name is in history; the
                                           note and the repository's directory are;
                                           a file moved out was recorded, then its
                                           removal, and the area holds its bytes
    trees-untracked.py fork FORK MAIN      a fork made after them starts with a
                                           copy; later writes stay on their side;
                                           a database copied while being written
                                           opens and passes integrity_check
    trees-untracked.py badrules BIN DIR    a rule not built refuses the mount,
                                           naming its line
    trees-untracked.py untrack BIN DIR     a tracked file the rules come to mark
                                           -agentfs leaves history at the next
                                           mount and reads back as it was
    trees-untracked.py save BIN DIR        Python's atomic save from an excluded
                                           temporary file onto an allowlisted
                                           config.yaml (exclude /* and
                                           !/config.yaml): recorded as one save
                                           that keeps the unchanged lines'
                                           identities, the temporary name never
                                           in history, ESTALE through the
                                           temporary file's descriptor
    trees-untracked.py readonly MOUNT      a read-only view refuses each change
                                           with EROFS
"""
import ctypes
import errno
import hashlib
import json
import os
import socket
import sqlite3
import stat
import subprocess
import sys
import tempfile
import time
from pathlib import Path

USER = 10000
SECRET = b"TOKEN=never-in-history\n"
# Every name the rules leave out, as the view checks make them.
UNTRACKED = [".env", "app.log", "sessions", "sessions/s.json", "state.db",
             "state.db-wal", "state.db-shm", "busy.db", "repo/.git", "repo/x",
             "u/x.log", "u/.env", "d/x.log", "logs/moved.md", "logs/skills/SKILL.md",
             "draft.log", "app-link.log", "pipe", "sock", "t2/a.log", "t/a.log"]
MOVED = b"moved out of the tree\n"
RENAME_NOREPLACE = 1
AT_FDCWD = -100


def fanned(path):
    """`<aa>/<bb>/<path>`: `aa` and `bb` hex digits 1-2 and 3-4 of `git
    hash-object --stdin` of the path."""
    hexa = hashlib.sha1(b"blob %d\0" % len(path.encode()) + path.encode()).hexdigest()
    return f"{hexa[:2]}/{hexa[2:4]}/{path}"


def renameat2(old, new, flags):
    libc = ctypes.CDLL(None, use_errno=True)
    if libc.renameat2(AT_FDCWD, os.fsencode(old), AT_FDCWD, os.fsencode(new), flags) != 0:
        code = ctypes.get_errno()
        raise OSError(code, os.strerror(code), str(old))


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


# The view host's process id (the mount command's), set from argv.
HOST_PID = None


def untracked_owner(caller):
    """Who owns a new untracked file its caller made (docs/trees.md, "Rules
    and the untracked area"): the caller when the view host may give files
    away (CAP_CHOWN and CAP_FOWNER), else the host's own user and group."""
    status = Path(f"/proc/{HOST_PID}/status").read_text().splitlines()
    field = {line.split(":")[0]: line.split(":")[1].split() for line in status}
    caps = int(field["CapEff"][0], 16)
    if caps & (1 << 0) and caps & (1 << 3):
        return caller
    return (int(field["Uid"][1]), int(field["Gid"][1]))


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
        expected = (USER, USER) if name == "n.md" else untracked_owner((USER, USER))
        assert owner(root / "u" / name) == expected, name


def view(root):
    (root / ".env").write_bytes(SECRET)
    (root / "app.log").write_bytes(b"line 1\n")
    (root / "sessions").mkdir()
    (root / "sessions" / "s.json").write_bytes(b'{"id": 1}')
    (root / "notes.md").write_bytes(b"recorded\n")
    # A tracked file is its caller's; an untracked one is the caller's only
    # when the view host may give files away.
    assert owner(root / "notes.md") == (0, 0)
    for name in (".env", "app.log", "sessions/s.json"):
        assert owner(root / name) == untracked_owner((0, 0)), name
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
    # Within the untracked half: a rename; RENAME_NOREPLACE onto a name there.
    os.rename(root / "app.log", root / "app2.log")
    os.rename(root / "app2.log", root / "app.log")
    (root / "other.log").write_bytes(b"other")
    expect_errno(errno.EEXIST, renameat2, root / "other.log", root / "app.log", RENAME_NOREPLACE)
    os.unlink(root / "other.log")
    # A hard link within the untracked half.
    os.link(root / "app.log", root / "app-link.log")
    assert os.lstat(root / "app.log").st_nlink == 2
    assert (root / "app-link.log").read_bytes() == (root / "app.log").read_bytes()
    os.unlink(root / "app-link.log")
    assert os.lstat(root / "app.log").st_nlink == 1
    # A tracked file moved into an excluded directory: its removal is recorded
    # and the area holds its bytes (history checks both).
    (root / "logs").mkdir()
    (root / "moved.md").write_bytes(MOVED)
    subprocess.run(["mv", str(root / "moved.md"), str(root / "logs" / "moved.md")], check=True)
    assert (root / "logs" / "moved.md").read_bytes() == MOVED
    assert not (root / "moved.md").exists()
    # An untracked file renamed to a name the rules record: recorded.
    (root / "draft.log").write_bytes(b"draft\n")
    os.rename(root / "draft.log", root / "draft.txt")
    assert (root / "draft.txt").read_bytes() == b"draft\n"
    assert not (root / "draft.log").exists()
    # Directories across the halves answer EXDEV, and mv copies.
    (root / "skills").mkdir()
    (root / "skills" / "SKILL.md").write_bytes(b"skill\n")
    expect_errno(errno.EXDEV, os.rename, root / "skills", root / "logs" / "skills")
    subprocess.run(["mv", str(root / "skills"), str(root / "logs" / "skills")], check=True)
    assert (root / "logs" / "skills" / "SKILL.md").read_bytes() == b"skill\n"
    assert not (root / "skills").exists()
    # O_TMPFILE in an untracked directory has no tracked name to take.
    expect_errno(errno.EOPNOTSUPP, os.open, root / "sessions", os.O_TMPFILE | os.O_RDWR, 0o600)
    away = Path(tempfile.mkdtemp())
    subprocess.run(["mv", str(root / "sessions"), str(away / "x")], check=True)
    assert not (root / "sessions").exists()
    assert (away / "x" / "s.json").read_bytes() == b'{"id": 1}'
    subprocess.run(["mv", str(away / "x"), str(root / "sessions")], check=True)
    assert (root / "sessions" / "s.json").read_bytes() == b'{"id": 1}'
    os.rmdir(away)
    # A tracked directory's untracked children move with it.
    (root / "t").mkdir()
    (root / "t" / "a.log").write_bytes(b"child")
    os.rename(root / "t", root / "t2")
    assert (root / "t2" / "a.log").read_bytes() == b"child"
    assert "a.log" in os.listdir(root / "t2")
    assert not (root / "t").exists()
    # A FIFO and a unix socket live in the untracked area.
    os.mkfifo(root / "pipe", 0o600)
    assert stat.S_ISFIFO(os.lstat(root / "pipe").st_mode)
    reader = os.open(root / "pipe", os.O_RDONLY | os.O_NONBLOCK)
    writer_fd = os.open(root / "pipe", os.O_WRONLY | os.O_NONBLOCK)
    os.write(writer_fd, b"through the pipe")
    assert os.read(reader, 100) == b"through the pipe"
    os.close(writer_fd)
    os.close(reader)
    server = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    server.bind(str(root / "sock"))
    server.listen(1)
    assert stat.S_ISSOCK(os.lstat(root / "sock").st_mode)
    client = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    client.connect(str(root / "sock"))
    accepted, _ = server.accept()
    client.sendall(b"hello")
    assert accepted.recv(5) == b"hello"
    for sock in (accepted, client, server):
        sock.close()


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
    assert owner(root / "u" / "x.log") == untracked_owner((USER, USER))
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
    # Moved out of the tree: recorded, then its removal; the area holds it.
    assert f"files/{fanned('moved.md')}" in paths
    head = set(git("ls-tree", "-r", "--name-only", "root").splitlines())
    assert f"files/{fanned('moved.md')}" not in head, "the move out recorded no removal"
    area = Path(git_dir).parent / "untracked" / "root"
    assert (area / "logs" / "moved.md").read_bytes() == MOVED
    # Moved into the tree: recorded under its new name alone.
    assert git("show", f"root:files/{fanned('draft.txt')}") == "draft\n"
    assert (area / "t2" / "a.log").read_bytes() == b"child"


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


def live_lines(git, rev, path):
    """`path`'s live lines at `rev` as (identity, version) pairs, from its
    lines/ entry: a run numbers both on from its first line."""
    def numbered(start, k):
        epoch, first = start.rsplit(":", 1)
        return f"{epoch}:{int(first) + k}"

    line_map = json.loads(git("show", f"{rev}:lines/{fanned(path)}"))[1]
    return [(numbered(run["identity"], k), numbered(run["version"], k))
            for run in line_map["lines"] if not run.get("dead")
            for k in range(run["count"])]


def save(binary, work):
    work = Path(work)
    state, mount = work / "save-state", work / "save-mount"
    mount.mkdir()
    subprocess.run([binary, "--state", str(state), "trees", "init", "--volume", "save"],
                   check=True, stdout=subprocess.DEVNULL)
    info = state / "history.git" / "info"
    info.mkdir(exist_ok=True)
    # The Hermes allowlist's shape: every top-level name excluded but one.
    (info / "exclude").write_text("/*\n!/config.yaml\n")

    def git(*args):
        return subprocess.check_output(["git", "--git-dir=" + str(state / "history.git"), *args],
                                       text=True)

    lines = [f"key{i}: {i}\n" for i in range(10)]
    with open(work / "save.log", "ab") as log:
        proc = serve(binary, state, mount, log)
        (mount / "config.yaml").write_text("".join(lines))
        lines[4] = "key4: changed\n"
        with tempfile.NamedTemporaryFile(dir=mount, prefix=".config.yaml.", suffix=".tmp",
                                         delete=False) as f:
            f.write("".join(lines).encode())
            f.flush()
            os.fsync(f.fileno())
            os.replace(f.name, mount / "config.yaml")
            # The temporary file moved into the tree: its descriptor writes no more.
            expect_errno(errno.ESTALE, os.write, f.fileno(), b"late\n")
            assert os.pread(f.fileno(), 100, 0) == "".join(lines).encode()
        assert (mount / "config.yaml").read_text() == "".join(lines)
        assert not os.path.exists(f.name)
        assert sorted(os.listdir(mount)) == ["config.yaml"], os.listdir(mount)
        unmount(proc)
    text = git("log", "--all", "-p", "--name-only", "--format=%B")
    assert ".config.yaml." not in text, "the temporary name reached history"
    assert git("show", f"root:files/{fanned('config.yaml')}") == "".join(lines)
    commits = git("log", "--format=%H", "root", "--", f"lines/{fanned('config.yaml')}").split()
    # The newest two: the replace, and the save before it.
    after, before = (live_lines(git, rev, "config.yaml") for rev in commits[:2])
    # Every line keeps its identity; the nine unchanged keep their versions too.
    assert [i for i, _ in before] == [i for i, _ in after] and len(after) == 10, (before, after)
    assert len(set(before) & set(after)) == 9, (before, after)


def readonly(root):
    for name, call, args in (
        ("create", os.open, (root / "ro-new", os.O_CREAT | os.O_WRONLY, 0o644)),
        ("rename", os.rename, (root / "restarted", root / "ro-renamed")),
        ("link", os.link, (root / "restarted", root / "ro-linked")),
        ("mkfifo", os.mkfifo, (root / "ro-pipe",)),
        ("mkdir", os.mkdir, (root / "ro-dir",)),
        ("tmpfile", os.open, (root, os.O_TMPFILE | os.O_RDWR, 0o600)),
        ("unlink", os.unlink, (root / "restarted",)),
    ):
        try:
            call(*args)
        except OSError as e:
            assert e.errno == errno.EROFS, (name, e)
        else:
            raise AssertionError(f"a read-only view took {name}")
    sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    expect_errno(errno.EROFS, sock.bind, str(root / "ro-sock"))
    sock.close()


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
        "save": lambda: save(sys.argv[2], sys.argv[3]),
        "readonly": lambda: readonly(Path(sys.argv[2])),
    }
    if sys.argv[1] in ("view", "remount", "fork") and os.geteuid() != 0:
        sys.exit(f"trees-untracked.py {sys.argv[1]} must run as root (euid {os.geteuid()})")
    if sys.argv[1] in ("view", "remount"):
        HOST_PID = int(sys.argv[3])
    commands[sys.argv[1]]()
