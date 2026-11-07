#!/usr/bin/env python3
"""Immediate mount observations: no publication polling or repeated opens."""
import os
from pathlib import Path
import sys
import threading

root = Path(sys.argv[1])
failures = []


def check(name, actual, expected):
    print(f"{name}: observed={actual!r}, expected={expected!r}", flush=True)
    if actual != expected:
        failures.append(name)


def new(name):
    return os.open(root / name, os.O_CREAT | os.O_RDWR | os.O_TRUNC, 0o600)


def read(name):
    with open(root / name, "rb", buffering=0) as f:
        return f.read()


# R1.3 and R1.1: writer sees its own bytes; a separate open sees saved bytes.
f = new("same")
os.write(f, b"same")
check("same: own pread after write", os.pread(f, 4, 0), b"same")
check("same: separate open before close", read("same"), b"")
os.close(f)
check("same: close then immediate open/read", read("same"), b"same")

# Child reports only after close returns; parent opens once after receiving the byte.
r, w = os.pipe()
pid = os.fork()
if pid == 0:
    os.close(r)
    f = new("process")
    os.write(f, b"child")
    os.close(f)
    os.write(w, b"x")
    os._exit(0)
os.close(w)
assert os.read(r, 1) == b"x"
os.close(r)
check("process: child close, pipe signal, parent open/read", read("process"), b"child")
_, status = os.waitpid(pid, 0)
assert status == 0

f = new("dup")
os.write(f, b"dup")
d = os.dup(f)
os.close(f)
check("dup: first close then open/read", read("dup"), b"")
os.close(d)
check("dup: final close then immediate open/read", read("dup"), b"dup")

# Parent closes one reference, then orders the child's final close through pipes.
f = new("forked")
os.write(f, b"forked")
command_r, command_w = os.pipe()
reply_r, reply_w = os.pipe()
pid = os.fork()
if pid == 0:
    os.close(command_w)
    os.close(reply_r)
    assert os.read(command_r, 1) == b"x"
    os.close(f)
    os.write(reply_w, b"x")
    os._exit(0)
os.close(command_r)
os.close(reply_w)
os.close(f)
check("fork: parent close while child holds descriptor", read("forked"), b"")
os.write(command_w, b"x")
assert os.read(reply_r, 1) == b"x"
check("fork: child final close then parent open/read", read("forked"), b"forked")
os.close(command_w)
os.close(reply_r)
_, status = os.waitpid(pid, 0)
assert status == 0

# Independent ordinary operations create load while closes and opens are ordered.
barrier = threading.Barrier(5)
errors = []
def load(n):
    try:
        barrier.wait()
        for i in range(20):
            name = f"load-{n}-{i}"
            f = new(name)
            os.write(f, b"load")
            os.close(f)
            value = read(name)
            if value != b"load":
                errors.append((name, value))
    except Exception as exc:
        errors.append((n, repr(exc)))
threads = [threading.Thread(target=load, args=(n,)) for n in range(4)]
for thread in threads:
    thread.start()
barrier.wait()
for thread in threads:
    thread.join()
check("concurrent: 80 close/open reads", errors, [])
if failures:
    raise SystemExit("ordering failures: " + ", ".join(failures))
