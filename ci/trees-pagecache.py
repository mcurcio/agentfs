#!/usr/bin/env python3
"""Observe two same-tree writers through actual file descriptors and shared maps.

Usage: trees-pagecache.py MOUNT HISTORY_GIT. PAGECACHE_REPORT may name a JSON
report. This tests no sandbox landing: both writers have the mount's default
label. Open-reader snapshots across saves are deliberately outside this probe.
"""
import json
import mmap
import os
from pathlib import Path
import sys
import tempfile
import traceback

from tree_git_read import read

SIZE = mmap.PAGESIZE
INITIAL = b"abcdefgh" + bytes(SIZE - 8)
PAYLOADS = (b"WXYZ", b"1234")


def show(data):
    # Whole-page observations are compact without hiding unexpected bytes.
    if isinstance(data, bytes):
        return {"length": len(data), "hex": data.hex()}
    return data


def run_case(root, repo, name, kinds, offsets):
    trace = []
    failures = []
    descriptors = []
    mappings = []
    path = root / ("pagecache-" + name)

    def log(call, observed=None, expected=None):
        entry = {"call": call, "observed": show(observed), "expected": show(expected)}
        trace.append(entry)
        print(json.dumps({"case": name, **entry}), flush=True)
        if expected is not None and observed != expected:
            failures.append(call)

    def open_writer(file_path, kind):
        fd = os.open(file_path, os.O_RDWR)
        descriptors.append(fd)
        view = None
        if kind == "map":
            view = mmap.mmap(fd, SIZE, flags=mmap.MAP_SHARED,
                             prot=mmap.PROT_READ | mmap.PROT_WRITE)
            mappings.append(view)
        return fd, view

    def write(writer, kind, offset, data):
        fd, view = writer
        if kind == "fd":
            return os.pwrite(fd, data, offset)
        view[offset:offset + len(data)] = data
        return len(data)

    def finish(writer, kind):
        fd, view = writer
        if kind == "map":
            # Flush is an explicit writeback point, not an assertion that a CPU
            # store itself was journalled; writeback may also happen earlier.
            view.flush()
            os.close(fd)
            descriptors.remove(fd)
            view.close()
            mappings.remove(view)
        else:
            os.close(fd)
            descriptors.remove(fd)

    try:
        # Generate each expected save independently with ordinary Linux calls.
        with tempfile.TemporaryDirectory() as tmp:
            oracle_path = Path(tmp) / "oracle"
            oracle_path.write_bytes(INITIAL)
            with open(oracle_path, "r+b", buffering=0) as oracle:
                os.pwrite(oracle.fileno(), PAYLOADS[0], offsets[0])
                first = os.pread(oracle.fileno(), SIZE, 0)
                os.pwrite(oracle.fileno(), PAYLOADS[1], offsets[1])
                final = os.pread(oracle.fileno(), SIZE, 0)
            # Verify the two-open sequence itself against the same byte calls.
            oracle_path.write_bytes(INITIAL)
            linux = [open_writer(oracle_path, kind) for kind in kinds]
            for i in range(2):
                write(linux[i], kinds[i], offsets[i], PAYLOADS[i])
            for i in range(2):
                finish(linux[i], kinds[i])
            log("Linux two opens, writes, close A, close B: final bytes",
                oracle_path.read_bytes(), final)

        path.write_bytes(INITIAL)
        a, b = [open_writer(path, kind) for kind in kinds]
        log("open A and B O_RDWR before either write", "ok", "ok")
        # This is a distinct open and mapping, not either writer's own handle.
        observer = os.open(path, os.O_RDONLY)
        descriptors.append(observer)
        observer_map = mmap.mmap(observer, SIZE, flags=mmap.MAP_SHARED,
                                 prot=mmap.PROT_READ)
        mappings.append(observer_map)
        for i, writer in enumerate((a, b)):
            written = write(writer, kinds[i], offsets[i], PAYLOADS[i])
            log(f"{'AB'[i]} {kinds[i]} write at {offsets[i]} returns length",
                written, len(PAYLOADS[i]))
            own = (os.pread(writer[0], 4, offsets[i]) if kinds[i] == "fd"
                   else writer[1][offsets[i]:offsets[i] + 4])
            log(f"{'AB'[i]} reads own write before next write", own, PAYLOADS[i])
        log("independent pread before first close (R1.1)",
            os.pread(observer, SIZE, 0), INITIAL)
        log("independent MAP_SHARED read before first close (R1.1)",
            observer_map[:], INITIAL)
        observer_map.close()
        mappings.remove(observer_map)
        os.close(observer)
        descriptors.remove(observer)
        finish(a, kinds[0])
        log(f"close A ({kinds[0]}; map flush, fd close, unmap)", "ok", "ok")
        log("fresh open after A closes while B remains open (R1.1, R8)",
            path.read_bytes(), first)
        finish(b, kinds[1])
        log(f"close B ({kinds[1]}; map flush, fd close, unmap)", "ok", "ok")
        log("fresh open after B closes (R8, R9.16; Linux byte calls)",
            path.read_bytes(), final)
        log("main history after B closes (R8)", read(repo, "main", path.name), final)
    except Exception as exc:
        failures.append(f"exception: {type(exc).__name__}: {exc}")
        log("exception", traceback.format_exc())
    finally:
        for view in mappings:
            try:
                view.close()
            except Exception as exc:
                failures.append(f"unmap cleanup: {exc}")
        for fd in descriptors:
            try:
                os.close(fd)
            except OSError as exc:
                failures.append(f"close cleanup: {exc}")
    return {"case": name, "kinds": kinds, "offsets": offsets,
            "trace": trace, "failures": failures}


def main():
    if len(sys.argv) != 3:
        raise SystemExit(__doc__)
    root, repo = Path(sys.argv[1]), sys.argv[2]
    results = []
    for kinds in (("fd", "fd"), ("fd", "map"), ("map", "fd"), ("map", "map")):
        for label, offsets in (("overlap", (0, 2)), ("disjoint", (0, 4))):
            name = "-".join((*kinds, label))
            results.append(run_case(root, repo, name, kinds, offsets))
    report = {"scope": "two writers in one tree, default label; no parent arbitration",
              "results": results}
    report_path = os.environ.get("PAGECACHE_REPORT")
    if report_path:
        Path(report_path).write_text(json.dumps(report, indent=2) + "\n")
    failed = sum(bool(result["failures"]) for result in results)
    print(f"{len(results) - failed}/{len(results)} cases passed; "
          "same-tree closes, no sandbox/parent decision", flush=True)
    return int(bool(failed))


if __name__ == "__main__":
    sys.exit(main())
