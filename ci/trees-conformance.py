#!/usr/bin/env python3
"""Real calls for the one-parent/one-sandbox subset of git-model §18.

Git is the save barrier and independent record; queue convergence is bounded.
The expected outcomes mirror named model scenarios and do not derive from the
implementation's line maps or queue bookkeeping.
"""
import json
import os
from pathlib import Path
import random
import re
import subprocess
import sys
import tempfile
import time
import traceback

sys.path.insert(0, str(Path(__file__).resolve().parent))
from tree_git_read import fanned  # noqa: E402

MAIN, FORK, REPO = map(Path, sys.argv[1:4])
ROOT_REF = "refs/heads/root"
FORK_REF = "refs/heads/" + sys.argv[4]
METRICS = Path(sys.argv[5])
ORACLE = sys.argv[6]
TIMEOUT = 20
RESULTS = {}


def git(*args):
    return subprocess.run(["git", "--git-dir=" + str(REPO), *args],
                          stdout=subprocess.PIPE, stderr=subprocess.PIPE, check=True).stdout


def until(description, predicate):
    deadline = time.monotonic() + TIMEOUT
    last = None
    while time.monotonic() < deadline:
        try:
            last = predicate()
            if last:
                return last
        except (FileNotFoundError, subprocess.CalledProcessError):
            pass
        time.sleep(0.02)
    raise AssertionError(f"timed out waiting for {description}; last={last!r}")


def bytes_at(ref, name):
    commit = git("rev-parse", f"{ref}^{{commit}}").strip().decode()
    message = git("show", "-s", "--format=%B", commit).decode()
    path = f"files/{fanned(name)}" if "Agentfs-Format: envelope-1" in message.splitlines() else name
    return git("show", f"{commit}:{path}")


def wait_ref(ref, name, data):
    until(f"{ref}:{name} = {data!r}", lambda: bytes_at(ref, name) == data)


def direct_commit(ref, name, data):
    tree = ref.removeprefix("refs/heads/")
    for commit in git("rev-list", "--topo-order", ref).decode().splitlines():
        message = git("show", "-s", "--format=%B", commit).decode().splitlines()
        if "Agentfs-Cause: Direct" not in message or f"Agentfs-Tree: {tree}" not in message:
            continue
        try:
            if bytes_at(commit, name) == data:
                return commit
        except subprocess.CalledProcessError:
            continue
    raise AssertionError(("direct save absent", ref, name, data))


def write(path, data):
    with path.open("wb", buffering=0) as f:
        f.write(data)


def save(root, ref, name, data):
    write(root / name, data)
    wait_ref(ref, name, data)
    # RELEASE is asynchronous. Once history proves the save, check its view
    # directly rather than polling close-to-open visibility.
    assert (root / name).read_bytes() == data
    assert_aligned(direct_commit(ref, name, data), name)


def converged(name, expected):
    for ref in (ROOT_REF, FORK_REF):
        wait_ref(ref, name, expected)
    assert (MAIN / name).read_bytes() == expected
    assert (FORK / name).read_bytes() == expected


def seed(name, data):
    save(MAIN, ROOT_REF, name, data)
    converged(name, data)


def concurrent(name, base, parent, child, result, losing=False):
    """Model's parent/child concurrent saves, blocked at parent's writer."""
    model = json.loads(subprocess.check_output([ORACLE, name]))
    assert bytes.fromhex(model["root"]) == result
    assert bytes.fromhex(model["child"]) == result
    assert bool(model["collisions"]) == losing
    seed(name, base)
    with (MAIN / name).open("r+b", buffering=0) as held:
        save(FORK, FORK_REF, name, child)
        # Capture the exact losing save before the queue can decide it.
        child_commit = direct_commit(FORK_REF, name, child)
        held.seek(0)
        held.write(parent)
        held.truncate(len(parent))
    converged(name, result)
    # Compare lineage with the model using commits read from the actual Git
    # graph. The parent save may already have been followed by a take.
    parent_commit = direct_commit(ROOT_REF, name, parent)
    assert_aligned(parent_commit, name)
    for commit, key in ((parent_commit, "parent_reachable"), (child_commit, "child_reachable")):
        status = subprocess.run(["git", "--git-dir=" + str(REPO), "merge-base",
                                 "--is-ancestor", commit, ROOT_REF]).returncode
        assert status in (0, 1) and (status == 0) == model[key], (key, commit, status)
    if losing:
        refs = git("for-each-ref", "--format=%(refname)", "refs/collisions/").decode().splitlines()
        assert any(git("rev-parse", ref).strip().decode() == child_commit for ref in refs), (
            "losing save lacks collision ref", child_commit, refs)
        assert bytes_at(child_commit, name) == child
        messages = git("log", "--format=%B", ROOT_REF).decode()
        assert "Agentfs-Collisions:" in messages, "collision decision absent from history"



def split_lines(image):
    parts = image.split(b"\n")
    return [part + b"\n" for part in parts[:-1]] + ([parts[-1]] if parts[-1] else [])


def histogram_hunks(old, new):
    # no-index compares only immutable images, not the implementation's maps.
    # --text forces Git to diff embedded NULs as bytes; LC_ALL fixes the header.
    with tempfile.TemporaryDirectory() as directory:
        a, b = Path(directory) / "old", Path(directory) / "new"
        a.write_bytes(old)
        b.write_bytes(new)
        env = dict(os.environ, LC_ALL="C", GIT_CONFIG_NOSYSTEM="1")
        proc = subprocess.run(["git", "-c", "core.attributesFile=/dev/null", "diff",
                               "--no-index", "--text", "--histogram", "--unified=0",
                               "--no-ext-diff", "--", str(a), str(b)],
                              stdout=subprocess.PIPE, stderr=subprocess.PIPE, env=env)
        assert proc.returncode in (0, 1), proc.stderr
    pattern = re.compile(rb"^@@ -(\d+)(?:,(\d+))? \+(\d+)(?:,(\d+))? @@")
    hunks = []
    for line in proc.stdout.split(b"\n"):
        match = pattern.match(line)
        if match:
            old_start, old_count, new_start, new_count = match.groups()
            old_count = int(old_count) if old_count is not None else 1
            new_count = int(new_count) if new_count is not None else 1
            old_start = int(old_start) - (old_count != 0)
            new_start = int(new_start) - (new_count != 0)
            hunks.append((old_start, old_count, new_start, new_count))
    assert (old == new) == (not hunks), (old, new, proc.stdout)
    return hunks


def nth(version, n):
    """The version n after `version` ("<event>:<index>") in its event's numbering."""
    if n == 0:
        return version
    event, index = version.split(":")
    return f"{event}:{int(index) + n}"


def expand(line_map, image):
    """A stored line map (runs, docs/trees.md "What a commit holds") line by
    line, each live line's bytes read from its file: a line ends at a newline,
    or after `open` bytes for a run's last line that has none."""
    lines, rest = [], image
    for run in line_map["lines"]:
        for i in range(run["count"]):
            data = None
            if not run.get("dead"):
                if "open" in run and i + 1 == run["count"]:
                    size = run["open"]
                else:
                    size = rest.index(b"\n") + 1
                data, rest = list(rest[:size]), rest[size:]
            lines.append({"identity": nth(run["identity"], i),
                          "version": nth(run["version"], i), "bytes": data})
    assert rest == b"", "bytes past the map's live lines"
    ancestors = {}
    for run in line_map["ancestors"]:
        for i in range(run["count"]):
            ancestors[nth(run["version"], i)] = nth(run["over"], i) if run.get("over") else None
    return {"lines": lines, "ancestors": ancestors}


def check_histogram(before, after, old_image, new_image):
    old = [x for x in before["lines"] if x["bytes"] is not None]
    new = [x for x in after["lines"] if x["bytes"] is not None]
    old_bytes, new_bytes = split_lines(old_image), split_lines(new_image)
    assert [bytes(x["bytes"]) for x in old] == old_bytes
    assert [bytes(x["bytes"]) for x in new] == new_bytes
    old_index = new_index = 0
    for start, count, nstart, ncount in histogram_hunks(old_image, new_image):
        assert start >= old_index and nstart >= new_index
        # Unchanged runs, including equal edges a hunk encompasses, keep
        # the exact record. Changed pairs keep identity and advance version.
        assert start - old_index == nstart - new_index
        for i in range(start - old_index):
            assert old[old_index + i] == new[new_index + i]
        while count and ncount and old_bytes[start] == new_bytes[nstart]:
            assert old[start] == new[nstart]
            start += 1; nstart += 1; count -= 1; ncount -= 1
        while count and ncount and old_bytes[start + count - 1] == new_bytes[nstart + ncount - 1]:
            assert old[start + count - 1] == new[nstart + ncount - 1]
            count -= 1; ncount -= 1
        for i in range(min(count, ncount)):
            assert old[start + i]["identity"] == new[nstart + i]["identity"]
            assert after["ancestors"][new[nstart + i]["version"]] == old[start + i]["version"]
        for i in range(min(count, ncount), count):
            removed = next(x for x in after["lines"] if x["identity"] == old[start + i]["identity"])
            assert removed["bytes"] is None
        for i in range(min(count, ncount), ncount):
            line = new[nstart + i]
            assert line["identity"] == line["version"] and after["ancestors"][line["version"]] is None
        old_index, new_index = start + count, nstart + ncount
        # Include a trimmed equal suffix in the next unchanged run.
    assert len(old) - old_index == len(new) - new_index
    assert old[old_index:] == new[new_index:]


def assert_aligned(commit, name):
    """Read direct save and parent maps from Git; independently diff their images."""
    parent = git("rev-parse", commit + "^1").strip().decode()
    def read_map(rev, creation_allowed=False):
        message = git("show", "-s", "--format=%B", rev).decode().splitlines()
        if "Agentfs-Format: envelope-1" not in message:
            assert creation_allowed
            probe = subprocess.run(["git", "--git-dir=" + str(REPO), "cat-file", "-e", f"{rev}:{name}"], stderr=subprocess.DEVNULL)
            assert probe.returncode != 0, (rev, name, "legacy file has no map")
            return {"lines": [], "ancestors": {}}, b""
        has_map = subprocess.run(["git", "--git-dir=" + str(REPO), "cat-file", "-e", f"{rev}:lines/{fanned(name)}"], stderr=subprocess.DEVNULL)
        if has_map.returncode != 0:
            assert creation_allowed
            probe = subprocess.run(["git", "--git-dir=" + str(REPO), "cat-file", "-e", f"{rev}:files/{fanned(name)}"], stderr=subprocess.DEVNULL)
            assert probe.returncode != 0, (rev, name, "file missing its line map")
            return {"lines": [], "ancestors": {}}, b""
        image = bytes_at(rev, name)
        line_map = expand(json.loads(git("show", f"{rev}:lines/{fanned(name)}"))[1], image)
        lines = line_map["lines"]
        ids = [line["identity"] for line in lines]
        versions = [line["version"] for line in lines]
        assert len(ids) == len(set(ids)) and len(versions) == len(set(versions))
        assert b"".join(bytes(line["bytes"]) for line in lines if line["bytes"] is not None) == image
        assert all(v in line_map["ancestors"] for v in versions)
        return line_map, image
    before, old_image = read_map(parent, creation_allowed=True)
    after, new_image = read_map(commit)
    old = {line["identity"]: line for line in before["lines"]}
    new = {line["identity"]: line for line in after["lines"]}
    for identity, line in old.items():
        assert identity in new, ("line vanished", identity)
        current = new[identity]
        assert current == line or after["ancestors"][current["version"]] == line["version"], (line, current)
    for identity, line in new.items():
        if identity not in old:
            assert identity == line["version"] and after["ancestors"][line["version"]] is None and line["bytes"] is not None, line
    assert [x["identity"] for x in before["lines"]] == [x["identity"] for x in after["lines"] if x["identity"] in old]
    check_histogram(before, after, old_image, new_image)


def flow():
    # Two-way flow and a save over a delivered version (model r3).
    seed("flow", b"initial\n")
    save(FORK, FORK_REF, "flow", b"sandbox\n")
    converged("flow", b"sandbox\n")
    save(MAIN, ROOT_REF, "flow", b"parent\n")
    converged("flow", b"parent\n")
    save(FORK, FORK_REF, "flow", b"after-sync\n")
    converged("flow", b"after-sync\n")


def standard_cases():
    cases = [
        ("loss", b"a0\na1\na2\n", b"root\na1\na2\n", b"child\na1\nclean\n", b"root\na1\na2\n", True),
        ("neighbors", b"a0\na1\na2\n", b"root\na1\na2\n", b"a0\nchild\na2\n", b"root\nchild\na2\n", False),
        ("inserts", b"a0\na1\n", b"a0\nroot\na1\n", b"a0\nchild\na1\n", b"a0\nroot\nchild\na1\n", False),
        ("unfinished", b"base\n", b"base\n}", b"base\nchild\n", b"base\n}", True),
        ("edited-place", b"a0\na1\n", b"a0\na1\nparent\n", b"a0\nchild\n", b"a0\nchild\nparent\n", False),
    ]
    failures = []
    for args in cases:
        try:
            concurrent(*args)
            RESULTS[args[0]] = {"status": "pass"}
            print(f"PASS {args[0]}", flush=True)
        except Exception:
            failures.append(args[0])
            RESULTS[args[0]] = {"status": "fail", "error": traceback.format_exc()}
            print(f"FAIL {args[0]}", file=sys.stderr, flush=True)
            traceback.print_exc()
    if failures:
        raise AssertionError("scenario failures: " + ", ".join(failures))


def undecided():
    # R10.6: deliver the parent's unfinished line while the child's save
    # is still waiting at the parent. The sync must not splice those bytes.
    seed("undecided", b"base\n")
    with (MAIN / "undecided").open("r+b", buffering=0):
        save(FORK, FORK_REF, "undecided", b"base\nchild\n")
        save(MAIN, ROOT_REF, "undecided", b"base\n}")
        parent_commit = git("rev-parse", ROOT_REF).strip().decode()
        def parent_reached_fork():
            return subprocess.run(["git", "--git-dir=" + str(REPO), "merge-base",
                                   "--is-ancestor", parent_commit, FORK_REF]).returncode == 0
        until("undecided sync", parent_reached_fork)
        assert bytes_at(FORK_REF, "undecided") == b"base\nchild\n"
        assert (FORK / "undecided").read_bytes() == b"base\nchild\n"
    converged("undecided", b"base\n}")


def progress_p1():
    # P1: unrelated changes advance while a long-lived main writer is open;
    # once an earlier same-source change waits, its successor must not pass.
    for name in ("log", "stream", "later"):
        seed(name, b"base\n")
    with (MAIN / "log").open("r+b", buffering=0) as held:
        for i in range(3):
            data = f"stream-{i}\n".encode()
            save(FORK, FORK_REF, "stream", data)
            converged("stream", data)
        before = json.loads(METRICS.read_text())["waits"]
        save(FORK, FORK_REF, "log", b"sandbox-log\n")
        until("landing wait metric", lambda: json.loads(METRICS.read_text())["waits"] > before)
        save(FORK, FORK_REF, "later", b"later\n")
        # The queued predecessor provides the observation barrier, while the
        # open writer prevents the take from completing.
        assert bytes_at(ROOT_REF, "log") == b"base\n"
        assert bytes_at(ROOT_REF, "later") == b"base\n"
        # Ordinary cooperating programs: the parent holds the waited file,
        # while a child appends; both must finish without a queue deadlock.
        subprocess.run([sys.executable, "-c",
                        "import sys; open(sys.argv[1], 'ab').write(b'child\\n')",
                        str(MAIN / "log")], check=True, timeout=TIMEOUT)
        wait_ref(ROOT_REF, "log", b"base\nchild\n")
        assert_aligned(direct_commit(ROOT_REF, "log", b"base\nchild\n"), "log")
        subprocess.run(["sh", "-c", 'printf pipeline | cat > "$1"', "sh",
                        str(MAIN / "pipeline")], check=True, timeout=TIMEOUT)
        wait_ref(ROOT_REF, "pipeline", b"pipeline")
        assert_aligned(direct_commit(ROOT_REF, "pipeline", b"pipeline"), "pipeline")
    converged("later", b"later\n")
    expected = bytes.fromhex(json.loads(subprocess.check_output([ORACLE, "p1-append"]))["root"])
    converged("log", expected)


def progress_p2():
    # P2: the sandbox receives the prefix before the first open file, and
    # receives that file and the suffix only when the writer closes.
    for name in ("prefix", "held", "suffix"):
        seed(name, b"base\n")
    with (FORK / "held").open("r+b", buffering=0):
        save(MAIN, ROOT_REF, "prefix", b"prefix\n")
        wait_ref(FORK_REF, "prefix", b"prefix\n")
        before = json.loads(METRICS.read_text())["waits"]
        save(MAIN, ROOT_REF, "held", b"held\n")
        until("sync wait metric", lambda: json.loads(METRICS.read_text())["waits"] > before)
        save(MAIN, ROOT_REF, "suffix", b"suffix\n")
        assert bytes_at(FORK_REF, "held") == b"base\n"
        assert bytes_at(FORK_REF, "suffix") == b"base\n"
    converged("held", b"held\n")
    converged("suffix", b"suffix\n")


def byte_calls():
    # Separate close boundaries make the partial line, append, overlap, hole,
    # growth and shrink observable as distinct saves against plain Linux.
    with tempfile.TemporaryFile() as plain:
        operations = [
            ("wb", lambda fd: (os.write(fd, b"f"), os.write(fd, b"oo"))),
            ("ab", lambda fd: (os.write(fd, b"b"), os.write(fd, b"ar"))),
            ("r+b", lambda fd: (os.pwrite(fd, b"XY", 1), os.pwrite(fd, b"Z", 12))),
            ("r+b", lambda fd: os.ftruncate(fd, 20)),
            ("r+b", lambda fd: os.ftruncate(fd, 9)),
        ]
        for mode, operation in operations:
            # O_APPEND is a property of the open file description; duplicate
            # the plain file through /proc for the same mode and flags.
            flags = os.O_WRONLY | (os.O_APPEND if mode == "ab" else 0)
            if mode == "wb":
                flags |= os.O_TRUNC
            plain_fd = os.open(f"/proc/self/fd/{plain.fileno()}", flags)
            try:
                operation(plain_fd)
            finally:
                os.close(plain_fd)
            with (MAIN / "byte-explicit").open(mode, buffering=0) as mounted:
                operation(mounted.fileno())
            expected = os.pread(plain.fileno(), 256, 0)
            wait_ref(ROOT_REF, "byte-explicit", expected)
            assert_aligned(direct_commit(ROOT_REF, "byte-explicit", expected), "byte-explicit")
            converged("byte-explicit", expected)

    # A fixed deterministic byte-call workload. The plain Linux file is the
    # independent oracle; no random model seeds are added here.
    seed("bytes", b"foo")
    rng = random.Random(118)
    with tempfile.TemporaryFile() as oracle, (MAIN / "bytes").open("r+b", buffering=0) as f:
        os.write(oracle.fileno(), b"foo")
        for _ in range(80):
            if rng.randrange(4) == 0:
                size = rng.randrange(80)
                os.ftruncate(oracle.fileno(), size)
                os.ftruncate(f.fileno(), size)
            else:
                offset = rng.randrange(80)
                payload = bytes(rng.randrange(32, 127) for _ in range(rng.randrange(1, 8)))
                assert os.pwrite(oracle.fileno(), payload, offset) == len(payload)
                assert os.pwrite(f.fileno(), payload, offset) == len(payload)
        expected = os.pread(oracle.fileno(), 256, 0)
        assert os.pread(f.fileno(), 256, 0) == expected
    wait_ref(ROOT_REF, "bytes", expected)
    converged("bytes", expected)
    # Inspect the actual direct save and its first parent, not actor state.
    commits = git("rev-list", "--first-parent", ROOT_REF).decode().splitlines()
    commit = next(c for c in commits if bytes_at(c, "bytes") == expected and
                  "Agentfs-Cause: Direct" in git("show", "-s", "--format=%B", c).decode())
    assert_aligned(commit, "bytes")


def finding_32():
    # Exact calls of r10_an_unfinished_line_before_a_kept_line_loses_whole (R10.8).
    # Both open handles hold the relevant automatic takes until the saves exist.
    model = json.loads(subprocess.check_output([ORACLE, "finding-32"]))
    seed("finding32", bytes.fromhex(model["base"]))
    with (MAIN / "finding32").open("r+b", buffering=0):
        save(FORK, FORK_REF, "finding32", b"y")
        first = direct_commit(FORK_REF, "finding32", b"y")
        with (FORK / "finding32").open("r+b", buffering=0) as edit:
            save(MAIN, ROOT_REF, "finding32", bytes.fromhex(model["parent"]))
            assert os.pwrite(edit.fileno(), b"n6", 0) == 2
        wait_ref(FORK_REF, "finding32", b"n6")
        assert_aligned(direct_commit(FORK_REF, "finding32", b"n6"), "finding32")
        second = direct_commit(FORK_REF, "finding32", b"n6")
    def queues_empty():
        return not git("for-each-ref", "--format=%(refname)", "refs/queue/root/", "refs/queue/" + sys.argv[4] + "/").strip()
    until("finding 32 queue drain", queues_empty)
    commit = git("rev-parse", ROOT_REF).strip().decode()
    refs = git("for-each-ref", "--format=%(objectname)", "refs/collisions/").decode().splitlines()
    assert first in refs, ("first save did not lose whole", first, refs)
    assert second in refs, ("second save did not lose whole", second, refs)
    for save_commit in (first, second):
        status = subprocess.run(["git", "--git-dir=" + str(REPO), "merge-base",
                                 "--is-ancestor", save_commit, commit]).returncode
        assert status == 0, ("losing save left history", save_commit, commit, status)
    assert model["root"] == model["parent"], ("the model's root took a losing save", model)
    assert bytes_at(commit, "finding32") == bytes.fromhex(model["root"]), (
        "finding 32 bytes differ from model", bytes_at(commit, "finding32"), model["root"])
    message = git("show", "-s", "--format=%B", commit).decode().splitlines()
    assert "Agentfs-Format: envelope-1" in message
    lines = expand(json.loads(git("show", f"{commit}:lines/{fanned('finding32')}"))[1],
                   bytes_at(commit, "finding32"))["lines"]
    live = [bytes(line["bytes"]) for line in lines if line["bytes"] is not None]
    assert all(line.endswith(b"\n") for line in live[:-1]), (
        "finding 32: an unfinished line precedes another line", live, bytes_at(commit, "finding32"))
    assert b"".join(live) == bytes_at(commit, "finding32")
    converged("finding32", bytes.fromhex(model["parent"]))


def finding_33():
    # R10.9: deliver the parent's later line while the child's unfinished line
    # is still waiting at the parent. The sync must not join them as `ym7\n`.
    seed("finding33", b"a\nb\n")
    with (MAIN / "finding33").open("r+b", buffering=0):
        save(FORK, FORK_REF, "finding33", b"y")
        child = direct_commit(FORK_REF, "finding33", b"y")
        save(MAIN, ROOT_REF, "finding33", b"a\nb\nm7\n")
        parent_commit = git("rev-parse", ROOT_REF).strip().decode()
        def parent_reached_fork():
            return subprocess.run(["git", "--git-dir=" + str(REPO), "merge-base",
                                   "--is-ancestor", parent_commit, FORK_REF]).returncode == 0
        until("finding 33 sync", parent_reached_fork)
        assert bytes_at(FORK_REF, "finding33") == b"y"
        assert (FORK / "finding33").read_bytes() == b"y"
    converged("finding33", b"a\nb\nm7\n")
    refs = git("for-each-ref", "--format=%(objectname)", "refs/collisions/").decode().splitlines()
    assert child in refs, ("child save did not lose whole", child, refs)


def main():
    failed = []
    try:
        until("metrics output", lambda: METRICS.exists())
        for case in (flow, standard_cases, undecided, progress_p1, progress_p2, byte_calls, finding_32, finding_33):
            try:
                case()
                RESULTS[case.__name__] = {"status": "pass"}
                print(f"PASS {case.__name__}", flush=True)
            except Exception:
                failed.append(case.__name__)
                RESULTS[case.__name__] = {"status": "fail", "error": traceback.format_exc()}
                print(f"FAIL {case.__name__}", file=sys.stderr, flush=True)
                traceback.print_exc()
    except Exception:
        failed.append("setup")
        RESULTS["setup"] = {"status": "fail", "error": traceback.format_exc()}
        traceback.print_exc()
    finally:
        report = os.environ.get("AGENTFS_CONFORMANCE_REPORT")
        if report:
            Path(report).write_text(json.dumps({"cases": RESULTS}, indent=2) + "\n")
    if failed:
        raise SystemExit("conformance failures: " + ", ".join(failed))
    print("mounted model scenario subset and Linux byte oracle passed")


if __name__ == "__main__":
    main()
