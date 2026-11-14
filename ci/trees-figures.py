#!/usr/bin/env python3
"""The tree views' release figures (docs/performance.md, "Tree views, release build").

Runs four flat workloads through a mounted tree view, each against its own
deadline, compares each figure with its bar in ci/trees-budget.json, and
writes the report as JSON. A figure that misses its bar sets `warn` and
prints a `::warning::` line; nothing here fails on a figure. A workload that
runs out of time reports what it reached by its deadline; no figure is
extrapolated, because a save's cost grows with the tree. ci/e2e-trees-tracer.sh
runs it as its step 4. Standard library only.

    trees-figures.py --view DIR --filesystem FS --image ID --budget PATH --output PATH
                     [--build release] [--only WORKLOAD]...

Every timed save ends with a stat(2) of the file. The view saves a file when
the kernel releases it, which reaches agentfs just after close(2) returns,
and the view answers requests one at a time and in order, so the stat
returns only once the save is committed. Each figure therefore includes the
commit, not only the close.

    close_4k   a 4 KiB file rewritten and closed `count` times: p50 and p99 of
               one open, write, close and stat, in ms
    create     one-line files created in the root until `count` or the
               deadline: the count reached, and the cost of a save against
               the files in the tree
    close_64m  one 64 MiB file written in 1 MiB blocks (random bytes, varied
               per MiB, as the benchmark's bigclose), closed and statted: total
               seconds, or the bytes written by the deadline
    close_1g   one 1 GiB file written the same way, then closed and statted:
               the seconds of the close and stat alone (the benchmark's dirty
               bigclose), the total, or the bytes written by the
               deadline. Before it, 32 files of 1 MiB written
               in one write each: `write_1g` is the median of the big file's
               last 64 writes over the median of those (a write costs its own
               bytes at any file size)
    history_1g the 1 GiB save's drain_s: from its close and stat to the built
               point reaching it (needs --metrics)
    memory_1g  the view process's peak resident memory over its resident
               memory before the 1 GiB workload, through its save and its
               build, in MiB: the kernel's high-water mark, reset first
               (/proc/PID/clear_refs, 5) and read once (needs --view-pid)

Each workload runs on a worker thread. One the deadline catches inside a
save gets `grace_s` more to come back; one still blocked then is stuck in
the view, so the FUSE connection is aborted (/sys/fs/fuse/connections, as
root) to free it, and the workloads after it are reported as not run.
"""

import argparse
import json
import math
import os
import statistics
import subprocess
import sys
import threading
import time

MIB = 1 << 20
BIG_BYTES = 64 * MIB
HUGE_BYTES = 1024 * MIB
SMALL_FILES = 32
LAST_WRITES = 64
GUARANTEE = ("saved before the view answers any later request; "
             "history built later (drain_s: from the workload's end to the built point reaching it)")
FUSECTL = "/sys/fs/fuse/connections"


def write_file(path, data):
    fd = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o644)
    try:
        while data:
            data = data[os.write(fd, data):]
    finally:
        os.close(fd)


def page(n):
    """4 KiB of text whose first line is n, so every rewrite is a change."""
    head = f"{n}\n".encode()
    line = b"x" * 63 + b"\n"
    body = head + line * (4096 // 64)
    return body[:4096]


def ms(seconds):
    return round(seconds * 1000, 3)


def p99(samples):
    ordered = sorted(samples)
    return ordered[max(0, math.ceil(0.99 * len(ordered)) - 1)]


def tree_files(view):
    return sum(len(files) for _, _, files in os.walk(view))


class Run:
    """One workload's progress, shared between its worker and the referee.

    `samples` holds the seconds of each save that ended by the deadline;
    `step` advances the shared state only while the deadline has not passed.
    """

    def __init__(self, deadline):
        self.deadline = deadline
        self.samples = []
        self.extra = {}
        self.finished = False
        self.error = None
        self.lock = threading.Lock()

    def open(self):
        return time.monotonic() < self.deadline

    def done(self, start):
        end = time.monotonic()
        with self.lock:
            if end <= self.deadline:
                self.samples.append(end - start)
                return True
            return False


def close_4k(view, run, target):
    path = os.path.join(view, "fig-close4k")
    run.extra["tree_files"] = tree_files(view)
    for n in range(1, target + 1):
        if not run.open():
            return
        data = page(n)
        start = time.monotonic()
        write_file(path, data)
        os.stat(path)
        if not run.done(start):
            return
    run.finished = True


def create(view, run, target):
    run.extra["tree_files_at_start"] = tree_files(view)
    for n in range(1, target + 1):
        if not run.open():
            return
        path = os.path.join(view, f"fig-create-{n}")
        start = time.monotonic()
        write_file(path, f"{n}\n".encode())
        os.stat(path)
        if not run.done(start):
            return
    run.finished = True


def close_64m(view, run, _target):
    path = os.path.join(view, "fig-64m")
    block = bytearray(os.urandom(MIB))
    run.extra["bytes_written"] = 0
    start = time.monotonic()
    fd = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o644)
    try:
        for n in range(BIG_BYTES // MIB):
            if not run.open():
                return
            block[:8] = n.to_bytes(8, "little")
            chunk = memoryview(block)
            while chunk:
                chunk = chunk[os.write(fd, chunk):]
            run.extra["bytes_written"] += MIB
    finally:
        os.close(fd)
    os.stat(path)
    run.finished = run.done(start)


def metrics_of(path):
    """The view's metrics file, or None."""
    if not path:
        return None
    try:
        with open(path) as file:
            return json.load(file)
    except (OSError, ValueError):
        return None


def close_1g(view, run, _target, metrics=None):
    block = bytearray(os.urandom(MIB))
    run.extra.update(bytes_written=0)
    small = []
    for n in range(SMALL_FILES):
        if not run.open():
            return
        block[:8] = n.to_bytes(8, "little")
        fd = os.open(os.path.join(view, f"fig-1m-{n}"), os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o644)
        try:
            start = time.monotonic()
            written = os.write(fd, block)
            small.append(time.monotonic() - start)
            assert written == MIB, "one write of 1 MiB"
        finally:
            os.close(fd)
    path = os.path.join(view, "fig-1g")
    writes = []
    start = time.monotonic()
    fd = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o644)
    closing = None
    try:
        for n in range(HUGE_BYTES // MIB):
            if not run.open():
                return
            block[:8] = n.to_bytes(8, "little")
            chunk = memoryview(block)
            began = time.monotonic()
            while chunk:
                chunk = chunk[os.write(fd, chunk):]
            writes.append(time.monotonic() - began)
            run.extra["bytes_written"] += MIB
        closing = time.monotonic()
    finally:
        os.close(fd)
    os.stat(path)
    end = time.monotonic()
    if end > run.deadline:
        return
    run.samples.append(end - closing)
    run.extra["total_s"] = round(end - start, 3)
    last = statistics.median(writes[-LAST_WRITES:])
    first = statistics.median(small)
    run.extra["write_1g"] = {"p50_last_ms": ms(last), "p50_1m_ms": ms(first),
                             "ratio": round(last / first, 3) if first else None}
    run.finished = True


def by_tree_size(samples, start_files):
    """The cost of a save against the files in the tree, in windows of saves
    ending at 1, 2 and 5 times each power of ten, and the last save."""
    edges = []
    edge = 1
    while edge < len(samples):
        for step in (1, 2, 5):
            if edge * step < len(samples):
                edges.append(edge * step)
        edge *= 10
    edges.append(len(samples))
    rows, low = [], 0
    for high in edges:
        window = samples[low:high]
        if window:
            rows.append({"tree_files": start_files + high, "saves": len(window),
                         "p50_ms": ms(statistics.median(window)), "max_ms": ms(max(window))})
        low = high
    return rows


def figures(name, run, budget):
    """The report's figure for one workload, from what it reached by its deadline."""
    samples = run.samples
    figure = {"deadline_s": budget["deadline_s"], "finished": run.finished, **run.extra}
    if name == "close_4k":
        figure.update(target=budget["count"], count=len(samples))
        if samples:
            figure.update(p50_ms=ms(statistics.median(samples)), p99_ms=ms(p99(samples)))
    elif name == "create":
        figure.update(target=budget["count"], count=len(samples),
                      total_s=round(sum(samples), 3),
                      cost_by_tree_size=by_tree_size(samples, run.extra["tree_files_at_start"]))
    elif name == "close_1g":
        figure.update(bytes=HUGE_BYTES)
        if samples:
            figure.update(close_s=round(samples[0], 3))
    else:
        figure.update(bytes=BIG_BYTES)
        if samples:
            figure.update(total_s=round(samples[0], 3))
    if run.error:
        figure["error"] = run.error
    return figure


def missed(figure, budget, filesystem):
    """Whether a figure misses its bar: a finish is required, and a metric,
    when it has one, is within its `max` (one number, or one per filesystem)."""
    if not figure["finished"]:
        return True
    if "metric" not in budget:
        return False
    value = budget["max"]
    limit = value[filesystem] if isinstance(value, dict) else value
    found = figure.get(budget["metric"])
    return found is None or found > limit


class Renames:
    """The view's metrics file being renamed into place, as inotify events
    (the view writes it at each built or failed event and every 100 ms
    between): a reader waits on the event, never on a timer."""
    IN_MOVED_TO = 0x80

    def __init__(self, path):
        import ctypes
        self.libc = ctypes.CDLL(None, use_errno=True)
        self.name = os.path.basename(path).encode()
        self.fd = self.libc.inotify_init1(os.O_CLOEXEC)
        if self.fd < 0:
            raise OSError(ctypes.get_errno(), "inotify_init1")
        directory = os.path.dirname(os.path.abspath(path)).encode()
        if self.libc.inotify_add_watch(self.fd, directory, self.IN_MOVED_TO) < 0:
            raise OSError(ctypes.get_errno(), "inotify_add_watch")

    def wait(self, until):
        """Returns once the file was renamed into place again, or False at
        the monotonic time `until` (a bound on a hang)."""
        import select
        import struct
        while True:
            left = until - time.monotonic()
            if left <= 0:
                return False
            ready, _, _ = select.select([self.fd], [], [], left)
            if not ready:
                return False
            events = os.read(self.fd, 65536)
            at = 0
            while at < len(events):
                _, _, _, size = struct.unpack_from("iIII", events, at)
                name = events[at + 16:at + 16 + size].rstrip(b"\0")
                at += 16 + size
                if name == self.name:
                    return True

    def close(self):
        os.close(self.fd)


def drain(metrics, cap):
    """Seconds from now until the built point reaches the record last appended
    now, read from the metrics file the view rewrites; None past `cap`. Each
    file carries `seq`: one numbered two past the file read now was written
    from a snapshot taken after now, so its `appended` covers the workload."""
    start = time.monotonic()
    until = start + cap
    renames = Renames(metrics)
    try:
        seen = metrics_of(metrics)
        floor = (seen or {}).get("seq", 0) + 2
        target = None
        while True:
            now = metrics_of(metrics)
            if now and now.get("seq", 0) >= floor:
                if target is None:
                    target = now["history"]["appended"]
                if now["history"]["built_through"] >= target:
                    return round(time.monotonic() - start, 3)
            if not renames.wait(until):
                return None
    finally:
        renames.close()


def host():
    """The runner's CPU model and count: runners of one label differ, and a
    figure is compared across runs only with this beside it."""
    model = None
    try:
        with open("/proc/cpuinfo") as cpuinfo:
            for line in cpuinfo:
                if line.startswith("model name"):
                    model = line.split(":", 1)[1].strip()
                    break
    except OSError:
        pass
    return {"cpu": model, "cpus": os.cpu_count()}


def proc_kib(pid, field):
    """A /proc/PID/status field in KiB, or None."""
    try:
        with open(f"/proc/{pid}/status") as status:
            for line in status:
                if line.startswith(field + ":"):
                    return int(line.split()[1])
    except OSError:
        pass
    return None


def reset_peak(pid):
    """Resets the process's resident high-water mark; returns its resident
    memory now, in KiB, or None."""
    try:
        with open(f"/proc/{pid}/clear_refs", "w") as control:
            control.write("5")
    except OSError as error:
        print(f"::warning title=tree views::could not reset the view's peak memory: {error}",
              flush=True)
        return None
    return proc_kib(pid, "VmRSS")


def abort_connection(dev):
    """Abort the view's FUSE connection, the one way to free a thread blocked
    in a request the view never answers. Returns whether it was aborted."""
    if not os.path.isdir(FUSECTL) or not os.listdir(FUSECTL):
        subprocess.run(["mount", "-t", "fusectl", "none", FUSECTL], check=False)
    try:
        with open(os.path.join(FUSECTL, str(os.minor(dev)), "abort"), "w") as control:
            control.write("1")
        return True
    except OSError as error:
        print(f"::warning title=tree views::could not abort the FUSE connection: {error}",
              flush=True)
        return False


WORKLOADS = [("close_4k", close_4k), ("create", create), ("close_64m", close_64m),
             ("close_1g", close_1g)]


def main():
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--view", required=True)
    parser.add_argument("--filesystem", required=True)
    parser.add_argument("--image", required=True)
    parser.add_argument("--budget", required=True)
    parser.add_argument("--output", required=True)
    parser.add_argument("--build", default="release",
                        help="the build measured, as the report names it (the image ships release)")
    parser.add_argument("--metrics",
                        help="the view's --metrics-output file: each workload then reports drain_s")
    parser.add_argument("--drain-cap", type=float, default=900.0,
                        help="how long to wait for history to be built after a workload")
    parser.add_argument("--view-pid", type=int,
                        help="the view's process: close_1g then reports its peak memory")
    parser.add_argument("--only", action="append", choices=[n for n, _ in WORKLOADS],
                        help="run only this workload (repeatable; a probe's shortcut)")
    args = parser.parse_args()
    with open(args.budget) as file:
        policy = json.load(file)
    budgets, grace = policy["budgets"], policy["grace_s"]
    # Read before any workload: a stat of the view blocks while it is stuck.
    dev = os.stat(args.view).st_dev

    workloads, stopped, aborted = [], False, False
    for name, work in WORKLOADS:
        if args.only and name not in args.only:
            continue
        budget = budgets[name]
        entry = {"name": name, "budget": budget["text"], "guarantee": GUARANTEE}
        if stopped:
            workloads.append({**entry, "deadline_s": budget["deadline_s"], "finished": False,
                              "not_run": "an earlier workload was stuck in the view", "warn": True})
            continue
        run = Run(time.monotonic() + budget["deadline_s"])
        baseline = reset_peak(args.view_pid) if args.view_pid and name == "close_1g" else None

        def worker(work=work, run=run, target=budget.get("count")):
            try:
                if work is close_1g:
                    work(args.view, run, target, args.metrics)
                else:
                    work(args.view, run, target)
            except OSError as error:
                run.error = str(error)

        thread = threading.Thread(target=worker, name=name, daemon=True)
        thread.start()
        thread.join(max(0.0, run.deadline - time.monotonic()) + grace)
        if thread.is_alive():
            # Nothing else can reach the view: free it, report and leave.
            stopped, aborted = True, abort_connection(dev)
            thread.join(grace)
            entry["stuck"] = f"a save was still running {grace} s after the deadline"
        with run.lock:
            figure = figures(name, run, budget)
        if args.metrics and not stopped:
            figure["drain_s"] = drain(args.metrics, args.drain_cap)
        found = [(name, budget, figure)]
        if name == "close_1g":
            # write_1g: the same run's writes, held to its own bar.
            derived = budgets["write_1g"]
            found.append(("write_1g", derived,
                          {"finished": figure["finished"], **figure.get("write_1g", {})}))
            # history_1g and memory_1g: its build, held to theirs.
            drained = figure.get("drain_s")
            found.append(("history_1g", budgets["history_1g"],
                          {"finished": figure["finished"] and drained is not None,
                           "drain_s": drained}))
            peak = proc_kib(args.view_pid, "VmHWM") if baseline is not None else None
            found.append(("memory_1g", budgets["memory_1g"],
                          {"finished": figure["finished"] and peak is not None,
                           "peak_extra_mib": None if peak is None else round((peak - baseline) / 1024, 1),
                           "peak_mib": None if peak is None else round(peak / 1024, 1)}))
        for shown, held, value in found:
            warn = missed(value, held, args.filesystem)
            workloads.append({**entry, "name": shown, "budget": held["text"], **value, "warn": warn})
            print(f"{shown} on {args.filesystem}: {json.dumps(value)}", flush=True)
            if warn:
                print(f"::warning title=tree views, {args.filesystem}::{shown} misses its bar, "
                      f"{held['text']}: {json.dumps({k: v for k, v in value.items() if k != 'cost_by_tree_size'})}",
                      flush=True)

    report = {"build": args.build, "filesystem": args.filesystem, "image": args.image,
              "host": host(), "connection_aborted": aborted, "workloads": workloads}
    temporary = args.output + ".tmp"
    with open(temporary, "w") as out:
        json.dump(report, out, indent=2)
        out.write("\n")
    os.replace(temporary, args.output)
    sys.stdout.flush()
    # A worker still blocked in the view must not hold the exit.
    os._exit(0)


if __name__ == "__main__":
    main()
