#!/usr/bin/env python3
"""A close followed at once by an open reads the new content (git-model §18).

Usage: trees-close-open.py MOUNT. Busies the view with a large save, then 64
closes reopen at once while four mappings drive page writeback: in one process,
and with the open in another, told by a pipe after the close returned. RELEASE
is a background request; the view asks the kernel to keep it ahead of an OPEN.
Barriers and pipes order everything. No step waits on a timer.

The mappings live in processes of their own: CPython holds its lock through
mmap.flush, and one msync behind a busy view would pace every close to one per
save, so no queue would form and the test could not see the race.
"""
import mmap
import os
from pathlib import Path
import select
import sys
import threading

CLOSERS = 64
MAPPERS = 4
LARGE = 1 << 20


def content(round_name, index):
    return f"{round_name} {index}\n".encode() * 8


def read_all(path):
    fd = os.open(path, os.O_RDONLY)
    try:
        chunks = []
        while chunk := os.read(fd, 1 << 16):
            chunks.append(chunk)
        return b"".join(chunks)
    finally:
        os.close(fd)


def run_threads(targets):
    threads = [threading.Thread(target=target) for target in targets]
    for thread in threads:
        thread.start()
    return threads


def reader_process(root, round_name, pipes):
    """Another process's opens, each once its pipe says the close returned."""
    errors = []

    def read_one(index, signal):
        try:
            assert os.read(signal, 1) == b"x"
            name = f"{round_name}-{index}"
            value = read_all(root / name)
            if value != content(round_name, index):
                errors.append((name, value))
        except Exception as exc:
            errors.append((index, repr(exc)))

    threads = run_threads(
        [lambda i=i, r=r: read_one(i, r) for i, (r, _) in enumerate(pipes)])
    for thread in threads:
        thread.join()
    for error in errors:
        print(f"{round_name}: {error!r}", flush=True)
    return len(errors)


def drive_writeback(path, ready, stop):
    """Stores and msyncs until the stop pipe closes."""
    fd = os.open(path, os.O_CREAT | os.O_RDWR, 0o600)
    os.ftruncate(fd, mmap.PAGESIZE)
    with mmap.mmap(fd, mmap.PAGESIZE) as view:
        os.write(ready, b"x")
        count = 0
        while not select.select([stop], [], [], 0)[0]:
            count += 1
            view[:8] = count.to_bytes(8, "little")
            view.flush()
    os.close(fd)


def run_round(root, round_name, other_process):
    errors = []
    stop_read, stop_write = os.pipe()
    ready_read, ready_write = os.pipe()
    mappers = []
    # Before any file is open here: a child holding one would keep its RELEASE back.
    for index in range(MAPPERS):
        mapper = os.fork()
        if mapper == 0:
            status = 1
            try:
                os.close(stop_write)
                os.close(ready_read)
                drive_writeback(root / f"{round_name}-mapped-{index}", ready_write, stop_read)
                status = 0
            finally:
                os._exit(status)
        mappers.append(mapper)
    os.close(stop_read)
    os.close(ready_write)
    for _ in range(MAPPERS):
        if os.read(ready_read, 1) != b"x":
            errors.append(("a mapping process died before it was ready", round_name))
    os.close(ready_read)
    pipes = [os.pipe() for _ in range(CLOSERS)] if other_process else []
    reader = None
    if other_process:
        reader = os.fork()
        if reader == 0:
            status = 1
            try:
                os.close(stop_write)
                for _, write_end in pipes:
                    os.close(write_end)
                status = min(reader_process(root, round_name, pipes), 1)
            finally:
                os._exit(status)
        for read_end, _ in pipes:
            os.close(read_end)
    start = threading.Barrier(CLOSERS + 1)
    fds = []
    for index in range(CLOSERS):
        name = f"{round_name}-{index}"
        fd = os.open(root / name, os.O_CREAT | os.O_RDWR, 0o600)
        os.write(fd, content(round_name, index))
        fds.append(fd)

    def close_then_read(index):
        try:
            start.wait()
            os.close(fds[index])
            if other_process:
                os.write(pipes[index][1], b"x")
                return
            name = f"{round_name}-{index}"
            value = read_all(root / name)
            if value != content(round_name, index):
                errors.append((name, value))
        except Exception as exc:
            errors.append((index, repr(exc)))

    threads = run_threads([lambda i=i: close_then_read(i) for i in range(CLOSERS)])
    large = root / f"{round_name}-large"
    fd = os.open(large, os.O_CREAT | os.O_RDWR, 0o600)
    chunk = (b"a large save keeps the view busy\n" * 32768)[:1 << 20]
    for _ in range(LARGE // len(chunk)):
        os.write(fd, chunk)
    # The large save starts here, and the view answers nothing else until it ends.
    os.close(fd)
    start.wait()
    for thread in threads:
        thread.join()
    if other_process:
        _, status = os.waitpid(reader, 0)
        code = os.waitstatus_to_exitcode(status)
        if code != 0:
            errors.append(("reader process", code))
        for _, write_end in pipes:
            os.close(write_end)
    os.close(stop_write)
    for mapper in mappers:
        _, status = os.waitpid(mapper, 0)
        code = os.waitstatus_to_exitcode(status)
        if code != 0:
            errors.append(("mapping process", code))
    os.unlink(large)
    for error in errors:
        print(f"{round_name}: {error!r}", flush=True)
    label = "another process" if other_process else "one process"
    print(f"{round_name} ({label}): {CLOSERS} closes reopened, {len(errors)} wrong", flush=True)
    return len(errors)


def main():
    if len(sys.argv) != 2:
        raise SystemExit(__doc__)
    root = Path(sys.argv[1])
    wrong = sum(run_round(root, name, other) for name, other in (
        ("same", False), ("other", True)))
    return int(bool(wrong))


if __name__ == "__main__":
    sys.exit(main())
