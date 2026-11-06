#!/usr/bin/env python3
"""A close followed at once by an open reads the new content (git-model §18).

Usage: trees-close-open.py MOUNT. Busies the view with a large save, then 64
closes reopen at once while four mappings drive page writeback: in one process,
and with the open in another, told by a pipe after the close returned. RELEASE
is a background request; the view asks the kernel to keep it ahead of an OPEN.
Barriers and pipes order everything. No step waits on a timer.
"""
import mmap
import os
from pathlib import Path
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


def run_round(root, round_name, other_process):
    pipes = [os.pipe() for _ in range(CLOSERS)] if other_process else []
    reader = None
    if other_process:
        # Before any file is open here: a child holding one would keep its RELEASE back.
        reader = os.fork()
        if reader == 0:
            status = 1
            try:
                for _, write_end in pipes:
                    os.close(write_end)
                status = min(reader_process(root, round_name, pipes), 1)
            finally:
                os._exit(status)
        for read_end, _ in pipes:
            os.close(read_end)
    errors = []
    start = threading.Barrier(CLOSERS + MAPPERS + 1)
    stop = threading.Event()
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

    def drive_writeback(index):
        name = f"{round_name}-mapped-{index}"
        try:
            fd = os.open(root / name, os.O_CREAT | os.O_RDWR, 0o600)
            os.ftruncate(fd, mmap.PAGESIZE)
            with mmap.mmap(fd, mmap.PAGESIZE) as view:
                start.wait()
                count = 0
                while not stop.is_set():
                    count += 1
                    view[:8] = count.to_bytes(8, "little")
                    view.flush()
            os.close(fd)
        except Exception as exc:
            errors.append((name, repr(exc)))
            start.abort()

    threads = run_threads(
        [lambda i=i: close_then_read(i) for i in range(CLOSERS)]
        + [lambda i=i: drive_writeback(i) for i in range(MAPPERS)])
    large = root / f"{round_name}-large"
    fd = os.open(large, os.O_CREAT | os.O_RDWR, 0o600)
    chunk = (b"a large save keeps the view busy\n" * 32768)[:1 << 20]
    for _ in range(LARGE // len(chunk)):
        os.write(fd, chunk)
    # The large save starts here, and the view answers nothing else until it ends.
    os.close(fd)
    start.wait()
    closers = threads[:CLOSERS]
    for thread in closers:
        thread.join()
    if other_process:
        _, status = os.waitpid(reader, 0)
        code = os.waitstatus_to_exitcode(status)
        if code != 0:
            errors.append(("reader process", code))
        for _, write_end in pipes:
            os.close(write_end)
    stop.set()
    for thread in threads[CLOSERS:]:
        thread.join()
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
