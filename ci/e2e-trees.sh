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
    [ -z "$pid" ] || { kill "$pid" 2>/dev/null || true; wait "$pid" 2>/dev/null || true; }
    if [ "$status" != 0 ]; then cat "$work"/*.log 2>/dev/null || true; fi
    rm -rf "$work"
}
trap cleanup EXIT
mkdir "$work/main" "$work/sandbox"
"$bin" --state "$work/state" trees init --volume t
# The sandbox is one keyed fork: the first mount makes it, every later mount
# under the key finds it.
key=e2e
start() {
    "$bin" --state "$work/state" trees mount "$work/main" \
        --fork-mount "$work/sandbox" --fork-key "$key" "$@" >>"$work/mount.log" 2>&1 &
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
    assert not (root / "journal").exists()
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
"$bin" --state "$work/state" trees init --volume t
head=$(git --git-dir="$work/state/history.git" rev-parse root)
test "$(tree_git_file "$work/state/history.git" root crash)" = acknowledged-before-kill
"$bin" --state "$work/state" trees init --volume t
test "$(git --git-dir="$work/state/history.git" rev-parse root)" = "$head"
start
test "$(cat "$work/main/crash")" = acknowledged-before-kill
test "$(cat "$work/main/isolation")" = sandbox
test "$(cat "$work/sandbox/isolation")" = sandbox
# The kill left the fork's writer attached: the restart rebinds it (same
# client), so the sandbox writes as before.
printf after-restart > "$work/sandbox/restarted"
test "$(cat "$work/sandbox/restarted")" = after-restart
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
# A clean unmount ended the fork's writer, and a fork has one writer for life:
# mounting it to write again exits 1, mounting nothing.
status=0
"$bin" --state "$work/state" trees mount "$work/main" --fork-mount "$work/sandbox" --fork-key "$key" \
    >"$work/taken.log" 2>&1 || status=$?
test "$status" -eq 1
grep -F 'this fork has had its writer; mount it with --fork-read-only' "$work/taken.log" >/dev/null
! mountpoint -q "$work/main"
! mountpoint -q "$work/sandbox"
# Read-only it mounts: reads answer, writes are refused.
start --fork-read-only
test "$(cat "$work/sandbox/restarted")" = after-restart
if printf refused 2>/dev/null > "$work/sandbox/refused"; then
    echo "a read-only sandbox took a write" >&2
    exit 1
fi
# The fork's last save has landed in the root: nothing of it stays queued.
python3 - "$work/main/restarted" <<'PY'
import sys, time
until = time.monotonic() + 10
while time.monotonic() < until:
    try:
        if open(sys.argv[1], 'rb').read() == b'after-restart':
            sys.exit(0)
    except FileNotFoundError:
        pass
    time.sleep(0.01)
sys.exit("the fork's save never landed")
PY
# While a mount holds the volume, ending the key waits: exit 75.
status=0
"$bin" --state "$work/state" trees end --fork-key "$key" 2>"$work/busy.log" || status=$?
test "$status" -eq 75
fusermount3 -u "$work/main"
fusermount3 -u "$work/sandbox"
wait "$pid"
pid=
# Ended with nothing queued and no file open, the keyed fork drops at once:
# its branch, queue and checkout go; its row chain stays, ending ForkDropped.
fork=$(sed -n 's/^fork tree: //p' "$work/mount.log" | sed -n 1p)
test -n "$fork"
"$bin" --state "$work/state" trees end --fork-key "$key"
git_dir="$work/state/history.git"
! git --git-dir="$git_dir" rev-parse -q --verify "refs/heads/$fork" >/dev/null
test -z "$(git --git-dir="$git_dir" for-each-ref "refs/queue/$fork/")"
test ! -e "$work/state/trees/$fork"
test "$(git --git-dir="$git_dir" log -1 --format=%s "refs/agentfs/trees/$fork")" = "ForkDropped $fork"
# Its key has ended: mounting under it exits 1, mounting nothing.
status=0
"$bin" --state "$work/state" trees mount "$work/main" --fork-mount "$work/sandbox" --fork-key "$key" \
    >"$work/ended.log" 2>&1 || status=$?
test "$status" -eq 1
grep -F 'the fork under this --fork-key has ended' "$work/ended.log" >/dev/null
! mountpoint -q "$work/main"
# A fork mounted with no key is new, and ends when its last client detaches.
: >"$work/unkeyed.log"
"$bin" --state "$work/state" trees mount "$work/main" --fork-mount "$work/sandbox" \
    >>"$work/unkeyed.log" 2>&1 &
pid=$!
for _ in $(seq 1 200); do
    if mountpoint -q "$work/main" && mountpoint -q "$work/sandbox"; then break; fi
    kill -0 "$pid"
    sleep 0.1
done
unkeyed=$(sed -n 's/^fork tree: //p' "$work/unkeyed.log")
test -n "$unkeyed" && test "$unkeyed" != "$fork"
fusermount3 -u "$work/sandbox"
fusermount3 -u "$work/main"
wait "$pid"
pid=
git --git-dir="$git_dir" log --format=%s "refs/agentfs/trees/$unkeyed" | grep -Fx "ForkEnded $unkeyed" >/dev/null
git --git-dir="$work/state/history.git" fsck --strict
test "$mmap_status" -eq 0
test "$tmpfile_status" -eq 0
echo "tree-view recording, object and crash scenarios passed"
