#!/usr/bin/env bash
# Recording slice of target git-model §18. Requires Linux FUSE; no sudo.
set -euo pipefail
. "$(dirname "$0")/tree-git-test-lib.sh"
bin=${AGENTFS_BIN:-$PWD/target/debug/agentfs}
work=$(mktemp -d)
pid=
client=
cleanup() {
    status=$?
    for mount in "$work/main" "$work/sandbox"; do
        mountpoint -q "$mount" && fusermount3 -uz "$mount" || true
    done
    [ -z "$client" ] || kill "$client" 2>/dev/null || true
    [ -z "$pid" ] || kill "$pid" 2>/dev/null || true
    if [ "$status" != 0 ]; then cat "$work"/*.log 2>/dev/null || true; fi
    rm -rf "$work"
}
trap cleanup EXIT
mkdir "$work/main" "$work/sandbox"
"$bin" --state "$work/state" trees init
fork=$("$bin" --state "$work/state" trees fork)
start() {
    "$bin" --state "$work/state" trees mount "$work/main" \
        --sandbox "$work/sandbox" --fork "$fork" >>"$work/mount.log" 2>&1 &
    pid=$!
    for _ in $(seq 1 200); do
        if mountpoint -q "$work/main" && mountpoint -q "$work/sandbox"; then return; fi
        kill -0 "$pid"
        sleep 0.1
    done
    echo "trees mounts did not start" >&2
    return 1
}
start
python3 - "$work/main" "$work/sandbox" <<'PY'
import errno
import os
from pathlib import Path
import sys
import tempfile
import time

main, sandbox = map(Path, sys.argv[1:])

def saved(path, expected):
    assert path.read_bytes() == expected, (path, expected)

def delivered(path, expected):
    # Queue delivery is asynchronous; own-tree close assertions above are immediate.
    until = time.monotonic() + 10
    while time.monotonic() < until:
        try:
            if path.read_bytes() == expected:
                return
        except FileNotFoundError:
            pass
        time.sleep(0.01)
    raise AssertionError((path, expected, path.read_bytes()))

for label, root in (("main-", main), ("sandbox-", sandbox)):
    p = root / (label + "a")
    with p.open("w+b", buffering=0) as writer:
        writer.write(b"foo")
        os.fsync(writer.fileno())
        assert writer.readable()
        writer.seek(0)
        assert writer.read() == b"foo"
        assert p.read_bytes() == b"", "fsync published before RELEASE"
    saved(p, b"foo")
    with p.open("ab", buffering=0) as writer:
        writer.write(b"bar")
    saved(p, b"foobar")

    # Rename over an open target; old writes must not reach the replacement.
    old = p.open("r+b", buffering=0)
    tmp = root / (label + "temp")
    tmp.write_bytes(b"replacement")
    saved(tmp, b"replacement")
    os.replace(tmp, p)
    old.write(b"BAD")
    os.fsync(old.fileno())
    old.close()
    saved(p, b"replacement")

    old = p.open("r+b", buffering=0)
    p.unlink()
    p.write_bytes(b"new-object")
    old.write(b"BAD")
    old.close()
    saved(p, b"new-object")

    # A rename of the open object follows it, even though its bytes save later.
    writer = p.open("ab", buffering=0)
    writer.write(b"-tail")
    q = root / (label + "renamed")
    p.rename(q)
    writer.close()
    saved(q, b"new-object-tail")

    # Byte semantics checked against a plain Linux file, including holes.
    with tempfile.TemporaryFile() as oracle, (root / (label + "bytes")).open("w+b", buffering=0) as f:
        for fd in (oracle.fileno(), f.fileno()):
            os.pwrite(fd, b"foo", 0)
            os.pwrite(fd, b"bar", 3)
            os.pwrite(fd, b"Z", 11)
            os.ftruncate(fd, 15)
            os.pwrite(fd, b"XY", 1)
            os.ftruncate(fd, 13)
        expected = os.pread(oracle.fileno(), 100, 0)
        assert os.pread(f.fileno(), 100, 0) == expected
    saved(root / (label + "bytes"), expected)
    assert not (root / ".git").exists()
    assert not (root / "calls.wal").exists()
    try:
        (root / "dir").mkdir()
    except OSError as e:
        assert e.errno in (errno.ENOSYS, errno.EOPNOTSUPP)
    else:
        raise AssertionError("unsupported mkdir succeeded")

(main / "isolation").write_bytes(b"parent")
saved(main / "isolation", b"parent")
delivered(sandbox / "isolation", b"parent")
(sandbox / "isolation").write_bytes(b"sandbox")
saved(sandbox / "isolation", b"sandbox")
delivered(main / "isolation", b"sandbox")
PY
mmap_status=0
python3 ci/trees-mmap.py "$work/main" "$work/state/history.git" || mmap_status=$?
tmpfile_status=0
python3 ci/trees-tmpfile.py "$work/main" "$work/state/history.git" || tmpfile_status=$?
# Keep the client handle open, then kill the actor after a returned write.
python3 - "$work/main/crash" "$work/ack" <<'PY' >"$work/client.log" 2>&1 &
import os
from pathlib import Path
import sys
import time
f = open(sys.argv[1], "wb", buffering=0)
f.write(b"acknowledged-before-kill")
os.fsync(f.fileno())
Path(sys.argv[2]).write_text("ack")
while True:
    time.sleep(1)
PY
client=$!
for _ in $(seq 1 200); do
    [ ! -f "$work/ack" ] || break
    kill -0 "$client"
    sleep 0.1
done
test -f "$work/ack"
kill -KILL "$pid"
wait "$pid" 2>/dev/null || true
pid=
kill "$client"
wait "$client" 2>/dev/null || true
client=
fusermount3 -uz "$work/main"
fusermount3 -uz "$work/sandbox"
"$bin" --state "$work/state" trees init
head=$(git --git-dir="$work/state/history.git" rev-parse root)
test "$(tree_git_file "$work/state/history.git" root crash)" = acknowledged-before-kill
"$bin" --state "$work/state" trees init
test "$(git --git-dir="$work/state/history.git" rev-parse root)" = "$head"
start
test "$(cat "$work/main/crash")" = acknowledged-before-kill
test "$(cat "$work/main/isolation")" = sandbox
test "$(cat "$work/sandbox/isolation")" = sandbox
fusermount3 -u "$work/main"
fusermount3 -u "$work/sandbox"
wait "$pid"
pid=
# Both roots replace and unlink an open object. Check the actual RELEASE
# outcomes after unmounting, and verify no orphan bytes entered history.
test "$(grep -c 'tree OrphanDropped:' "$work/mount.log")" -eq 4
if git --git-dir="$work/state/history.git" log --all -p --format= | grep '^+.*BAD' >/dev/null; then
    echo "orphan bytes entered history" >&2
    exit 1
fi
git --git-dir="$work/state/history.git" fsck --strict
test "$mmap_status" -eq 0
test "$tmpfile_status" -eq 0
echo "tree-view recording, object and crash scenarios passed"
