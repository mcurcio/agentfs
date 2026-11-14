#!/usr/bin/env python3
"""Writers in one tree through file descriptors and shared maps (ledger R1.4).

Usage: trees-pagecache.py MOUNT HISTORY_GIT SANDBOX. PAGECACHE_REPORT may name a
JSON report. Expected bytes come from the same calls on a plain Linux file, never
from literals. Where R1.1 or R9.16 parts from Linux on purpose the check says so.
"""
import json
import mmap
import os
from pathlib import Path
import sys
import tempfile
import time
import traceback

from tree_git_read import read

SIZE = mmap.PAGESIZE
INITIAL = b"abcdefgh" + bytes(SIZE - 8)
PAYLOADS = (b"WXYZ", b"1234", b"5678")
TIMEOUT = 30


def show(data):
    # Whole-page observations are compact without hiding unexpected bytes.
    if isinstance(data, bytes):
        return {"length": len(data), "hex": data.hex()}
    return data


class Case:
    """One case's trace, failures and cleanup."""

    def __init__(self, name):
        self.name = name
        self.trace = []
        self.failures = []
        self.descriptors = []
        self.mappings = []

    def log(self, call, observed=None, expected=None):
        entry = {"call": call, "observed": show(observed), "expected": show(expected)}
        self.trace.append(entry)
        print(json.dumps({"case": self.name, **entry}), flush=True)
        if expected is not None and observed != expected:
            self.failures.append(call)

    def open(self, path, flags=os.O_RDWR):
        fd = os.open(path, flags, 0o600)
        self.descriptors.append(fd)
        return fd

    def close(self, fd):
        os.close(fd)
        self.descriptors.remove(fd)

    def map(self, fd, length=SIZE, prot=mmap.PROT_READ | mmap.PROT_WRITE):
        view = mmap.mmap(fd, length, flags=mmap.MAP_SHARED, prot=prot)
        self.mappings.append(view)
        return view

    def unmap(self, view):
        view.close()
        self.mappings.remove(view)

    def open_writer(self, path, kind):
        fd = self.open(path)
        return fd, self.map(fd) if kind == "map" else None

    def finish(self, writer):
        fd, view = writer
        if view is None:
            self.close(fd)
        else:
            # An explicit writeback point; writeback may also happen earlier.
            view.flush()
            self.close(fd)
            self.unmap(view)

    def run(self, body):
        try:
            body(self)
        except Exception as exc:
            self.failures.append(f"exception: {type(exc).__name__}: {exc}")
            self.log("exception", traceback.format_exc())
        finally:
            for view in self.mappings:
                try:
                    view.close()
                except Exception as exc:
                    self.failures.append(f"unmap cleanup: {exc}")
            for fd in self.descriptors:
                try:
                    os.close(fd)
                except OSError as exc:
                    self.failures.append(f"close cleanup: {exc}")
        return {"case": self.name, "trace": self.trace, "failures": self.failures}


def store(writer, offset, data):
    fd, view = writer
    if view is None:
        return os.pwrite(fd, data, offset)
    view[offset:offset + len(data)] = data
    return len(data)


def own_read(writer, offset, length):
    fd, view = writer
    return os.pread(fd, length, offset) if view is None else view[offset:offset + length]


def byte_calls(calls):
    """What these (offset, data) pwrites leave on a plain Linux file."""
    with tempfile.TemporaryFile(buffering=0) as plain:
        plain.write(INITIAL)
        for offset, data in calls:
            os.pwrite(plain.fileno(), data, offset)
        return os.pread(plain.fileno(), SIZE, 0)


def linux_sequence(kinds, offsets, close_order):
    """The same opens, writes and closes on a plain Linux file."""
    case = Case("oracle")
    with tempfile.TemporaryDirectory() as tmp:
        path = Path(tmp) / "oracle"
        path.write_bytes(INITIAL)
        writers = [case.open_writer(path, kind) for kind in kinds]
        for i, writer in enumerate(writers):
            store(writer, offsets[i], PAYLOADS[i])
        for i in close_order:
            case.finish(writers[i])
        return path.read_bytes()


def two_writers(root, repo, name, kinds, offsets, reverse):
    close_order = (1, 0) if reverse else (0, 1)
    calls = [(offsets[i], PAYLOADS[i]) for i in range(2)]
    first = byte_calls(calls[:1])
    final = byte_calls(calls)

    def body(case):
        linux_bytes = linux_sequence(kinds, offsets, close_order)
        case.log("Linux two opens, writes, closes: final bytes", linux_bytes, final)
        path = root / ("pagecache-" + name)
        path.write_bytes(INITIAL)
        writers = [case.open_writer(path, kind) for kind in kinds]
        # A distinct open and mapping, not either writer's own handle.
        observer = case.open(path, os.O_RDONLY)
        observer_map = case.map(observer, prot=mmap.PROT_READ)
        for i, writer in enumerate(writers):
            case.log(f"{'AB'[i]} {kinds[i]} write at {offsets[i]} returns length",
                     store(writer, offsets[i], PAYLOADS[i]), len(PAYLOADS[i]))
            case.log(f"{'AB'[i]} reads own write before next write",
                     own_read(writer, offsets[i], 4), PAYLOADS[i])
        case.log("pread(C) before close: saved bytes (R1.1)",
                 os.pread(observer, SIZE, 0), INITIAL)
        case.log("C's MAP_SHARED read before close: the file's pages (R1.4)",
                 observer_map[:], final)
        case.unmap(observer_map)
        case.close(observer)
        case.finish(writers[close_order[0]])
        if reverse:
            case.finish(writers[close_order[1]])
            # R9.16: of descriptor calls, the later close is final. A mapped write
            # stands against it, so these two end as Linux does.
            expected = final
            if kinds == ("fd", "fd"):
                expected = byte_calls([calls[1], calls[0]])
            why = "R9.16, the later close" if kinds == ("fd", "fd") else "Linux"
            case.log(f"fresh open after both closes, B first ({why})",
                     path.read_bytes(), expected)
            case.log(f"root history after both closes ({why})",
                     read(repo, "root", path.name), expected)
            return
        # A's close saves what the kernel flushed, which holds B's mapped stores.
        case.log("fresh open after A closes while B remains open (R1.1, R1.4)",
                 path.read_bytes(), final if kinds[1] == "map" else first)
        case.finish(writers[1])
        case.log("fresh open after B closes (R8, R9.16; Linux byte calls)",
                 path.read_bytes(), final)
        case.log("root history after B closes (R8)", read(repo, "root", path.name), final)

    return Case(name).run(body)


def three_writers(root, repo, name, close_order):
    kinds = ("fd", "map", "fd")
    offsets = (0, 2, 4)
    final = byte_calls([(offsets[i], PAYLOADS[i]) for i in range(3)])

    def body(case):
        case.log("Linux three opens: final bytes",
                 linux_sequence(kinds, offsets, close_order), final)
        path = root / ("pagecache-" + name)
        path.write_bytes(INITIAL)
        writers = [case.open_writer(path, kind) for kind in kinds]
        for i, writer in enumerate(writers):
            store(writer, offsets[i], PAYLOADS[i])
        for i in close_order:
            case.finish(writers[i])
        case.log("fresh open after all close", path.read_bytes(), final)
        case.log("root history after all close", read(repo, "root", path.name), final)

    return Case(name).run(body)


def in_child(case, description, body):
    """Run `body` in a child, so that a SIGBUS is this case's failure alone."""
    pid = os.fork()
    if pid == 0:
        status = 1
        try:
            body()
            status = 0
        except BaseException:
            traceback.print_exc()
        finally:
            os._exit(status)
    _, status = os.waitpid(pid, 0)
    case.log(description, os.waitstatus_to_exitcode(status), 0)


def grown_mapping(root, repo):
    def body(case):
        offset = 2 * SIZE + 5
        with tempfile.TemporaryFile(buffering=0) as plain:
            plain.write(b"seed")
            os.ftruncate(plain.fileno(), 3 * SIZE)
            with mmap.mmap(plain.fileno(), 3 * SIZE) as view:
                view[offset:offset + 4] = PAYLOADS[0]
                view.flush()
            expected = os.pread(plain.fileno(), 3 * SIZE + 1, 0)
        path = root / "pagecache-grown-mapping"
        path.write_bytes(b"seed")

        def grow():
            fd = os.open(path, os.O_RDWR)
            os.ftruncate(fd, 3 * SIZE)
            with mmap.mmap(fd, 3 * SIZE) as view:
                view[offset:offset + 4] = PAYLOADS[0]
                # Another process stats the path; a smaller size would drop the store.
                pid = os.fork()
                if pid == 0:
                    os._exit(0 if os.stat(path).st_size == 3 * SIZE else 1)
                _, status = os.waitpid(pid, 0)
                assert os.waitstatus_to_exitcode(status) == 0, "stat from another process"
                assert view[offset:offset + 4] == PAYLOADS[0], "the store survives the stat"
                view.flush()
                os.close(fd)

        in_child(case, "ftruncate past the page, map, store, stat elsewhere", grow)
        case.log("fresh open after the save", path.read_bytes(), expected)
        case.log("root history", read(repo, "root", path.name), expected)

    return Case("grown-mapping").run(body)


def create_then_map(root, repo):
    def body(case):
        path = root / "pagecache-create-then-map"

        def create():
            fd = os.open(path, os.O_CREAT | os.O_RDWR | os.O_EXCL, 0o600)
            os.ftruncate(fd, SIZE)
            with mmap.mmap(fd, SIZE) as view:
                view[:4] = PAYLOADS[0]
                view.flush()
                assert os.pread(fd, 4, 0) == PAYLOADS[0], "own flushed store"
            os.close(fd)

        in_child(case, "create, ftruncate, map, store in one program", create)
        expected = PAYLOADS[0] + bytes(SIZE - 4)
        case.log("fresh open after the save", path.read_bytes(), expected)
        case.log("root history", read(repo, "root", path.name), expected)

    return Case("create-then-map").run(body)


def orphan_fstat(root):
    def body(case):
        path = root / "pagecache-orphan-fstat"
        path.write_bytes(INITIAL)
        writer = case.open_writer(path, "map")
        store(writer, 0, PAYLOADS[0])
        os.unlink(path)
        case.log("fstat of an unlinked mapped file", os.fstat(writer[0]).st_size, SIZE)
        store(writer, 4, PAYLOADS[1])
        case.log("the mapping still reads its stores", writer[1][:8],
                 PAYLOADS[0] + PAYLOADS[1])
        case.finish(writer)
        case.log("the name stays gone", path.exists(), False)

    return Case("orphan-fstat").run(body)


def until(description, predicate):
    """A mapping has no event of its own to wait on: observe it, bounded."""
    deadline = time.monotonic() + TIMEOUT
    last = None
    while time.monotonic() < deadline:
        try:
            last = predicate()
            if last:
                return True
        except FileNotFoundError:
            pass
        time.sleep(0.02)
    print(f"timed out waiting for {description}; last={last!r}", flush=True)
    return False


def take_refreshes_mapping(root, sandbox):
    def refreshes(case, writer_root, mapped_root, direction):
        name = f"pagecache-take-{direction}"
        old, new = b"old!" + bytes(SIZE - 4), b"new!" + bytes(SIZE - 4)
        (writer_root / name).write_bytes(old)
        delivered = until(f"{name} delivered",
                          lambda: (mapped_root / name).read_bytes() == old)
        case.log(f"{direction}: the first save arrives", delivered, True)
        fd = case.open(mapped_root / name, os.O_RDONLY)
        view = case.map(fd, prot=mmap.PROT_READ)
        case.log(f"{direction}: the read-only mapping reads it", view[:4], b"old!")
        (writer_root / name).write_bytes(new)
        case.log(f"{direction}: a take refreshes the mapping with no reopen",
                 until(f"{name} mapping", lambda: view[:4] == b"new!"), True)
        case.log(f"{direction}: the descriptor keeps its snapshot (R1.1)",
                 os.pread(fd, 4, 0), b"old!")
        case.unmap(view)
        case.close(fd)

    def body(case):
        refreshes(case, root, sandbox, "sync")
        refreshes(case, sandbox, root, "landing")

    return Case("take-refreshes-mapping").run(body)


def sendfile_while_writing(root):
    def body(case):
        path = root / "pagecache-sendfile-while-writing"
        path.write_bytes(INITIAL)
        writer = case.open(path)
        os.pwrite(writer, PAYLOADS[0], 0)
        reader = case.open(path, os.O_RDONLY)
        unsaved = byte_calls([(0, PAYLOADS[0])])
        case.log("pread on the read-only descriptor: saved bytes (R1.1)",
                 os.pread(reader, SIZE, 0), INITIAL)
        with tempfile.TemporaryFile(buffering=0) as out:
            sent = os.sendfile(out.fileno(), reader, 0, SIZE)
            case.log("sendfile returns a page", sent, SIZE)
            # R1.4 as the owner chose it: sendfile reads what a mapping reads.
            case.log("sendfile reads the file's pages, unsaved bytes included (R1.4)",
                     os.pread(out.fileno(), SIZE, 0), unsaved)
        case.close(reader)
        case.close(writer)
        case.log("after the close", path.read_bytes(), unsaved)

    return Case("sendfile-while-writing").run(body)


def main():
    if len(sys.argv) != 4:
        raise SystemExit(__doc__)
    root, repo, sandbox = Path(sys.argv[1]), sys.argv[2], Path(sys.argv[3])
    results = []
    for kinds in (("fd", "fd"), ("fd", "map"), ("map", "fd"), ("map", "map")):
        for label, offsets in (("overlap", (0, 2)), ("disjoint", (0, 4))):
            name = "-".join((*kinds, label))
            results.append(two_writers(root, repo, name, kinds, offsets, False))
            if kinds != ("map", "map"):
                results.append(two_writers(root, repo, name + "-reversed", kinds, offsets, True))
    results.append(three_writers(root, repo, "three-writers", (0, 1, 2)))
    results.append(three_writers(root, repo, "three-writers-reversed", (2, 1, 0)))
    results.append(grown_mapping(root, repo))
    results.append(create_then_map(root, repo))
    results.append(orphan_fstat(root))
    results.append(take_refreshes_mapping(root, sandbox))
    results.append(sendfile_while_writing(root))
    report = {"scope": "writers in one tree, default label; R1.4", "results": results}
    report_path = os.environ.get("PAGECACHE_REPORT")
    if report_path:
        Path(report_path).write_text(json.dumps(report, indent=2) + "\n")
    failed = [result["case"] for result in results if result["failures"]]
    print(f"{len(results) - len(failed)}/{len(results)} cases passed; failed: {failed}", flush=True)
    return int(bool(failed))


if __name__ == "__main__":
    sys.exit(main())
