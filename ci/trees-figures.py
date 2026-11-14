#!/usr/bin/env python3
"""The tree views' release figures (docs/performance.md, "Tree views, release build").

Runs three flat workloads through a mounted tree view, each against its own
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
    return figure[budget["metric"]] > limit


def history(metrics):
    """The view's history metrics, or None while the file is not written yet."""
    try:
        with open(metrics) as file:
            return json.load(file).get("history")
    except (OSError, ValueError):
        return None


def drain(metrics, cap):
    """Seconds from now until the built point reaches the record last appended
    now, read from the metrics file the view rewrites; None past `cap`. A probe
    of a measurement, not a coordination."""
    start = time.monotonic()
    now = history(metrics)
    while now is None and time.monotonic() - start < cap:
        time.sleep(0.1)
        now = history(metrics)
    if now is None:
        return None
    target = now["appended"]
    while time.monotonic() - start < cap:
        built = history(metrics)
        if built and built["built_through"] >= target:
            return round(time.monotonic() - start, 3)
        time.sleep(0.1)
    return None


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


WORKLOADS = [("close_4k", close_4k), ("create", create), ("close_64m", close_64m)]


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

        def worker(work=work, run=run, target=budget.get("count")):
            try:
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
        warn = missed(figure, budget, args.filesystem)
        workloads.append({**entry, **figure, "warn": warn})
        print(f"{name} on {args.filesystem}: {json.dumps(figure)}", flush=True)
        if warn:
            print(f"::warning title=tree views, {args.filesystem}::{name} misses its bar, "
                  f"{budget['text']}: {json.dumps({k: v for k, v in figure.items() if k != 'cost_by_tree_size'})}",
                  flush=True)

    report = {"build": args.build, "filesystem": args.filesystem, "image": args.image,
              "connection_aborted": aborted, "workloads": workloads}
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
