#!/usr/bin/env python3
"""The tree views' release figures (docs/performance.md, "Tree views, release build").

Runs three flat workloads through a mounted tree view, compares each figure
with the recording engine's budget in ci/trees-budget.json, and writes the
report as JSON. A figure over its budget sets `warn` and prints a
`::warning::` line; nothing here fails on a figure. ci/e2e-trees-tracer.sh
runs it as its step 4. Standard library only.

    trees-figures.py --view DIR --filesystem FS --image ID --budget PATH --output PATH
                     [--build release] [--only WORKLOAD]...

Every timed save ends with a stat(2) of the file. The view saves a file when
the kernel releases it, which reaches agentfs just after close(2) returns,
and the view answers requests one at a time and in order, so the stat
returns only once the save is committed. Each figure therefore includes the
commit, not only the close.

    close_4k     a 4 KiB file rewritten and closed 200 times: p50 and p99 of
                 one open, write, close and stat, in ms
    create_1000  1,000 files of one line each created in the root, then a
                 stat: total seconds, and that scaled linearly to 100,000
    close_64m    one 64 MiB file written in 1 MiB blocks (random bytes,
                 varied per MiB, as the benchmark's bigclose), closed and
                 statted: total seconds, and that scaled to 1 GiB
"""

import argparse
import json
import math
import os
import statistics
import sys
import time

MIB = 1 << 20
CLOSE_4K_COUNT = 200
CREATE_COUNT = 1000
BIG_BYTES = 64 * MIB
GUARANTEE = "committed before close returns"


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


def p99(samples):
    ordered = sorted(samples)
    return ordered[max(0, math.ceil(0.99 * len(ordered)) - 1)]


def close_4k(view):
    path = os.path.join(view, "fig-close4k")
    samples = []
    for n in range(1, CLOSE_4K_COUNT + 1):
        data = page(n)
        start = time.perf_counter()
        write_file(path, data)
        os.stat(path)
        samples.append((time.perf_counter() - start) * 1000)
    return {
        "count": CLOSE_4K_COUNT,
        "p50_ms": round(statistics.median(samples), 3),
        "p99_ms": round(p99(samples), 3),
    }


def create_1000(view):
    start = time.perf_counter()
    for n in range(1, CREATE_COUNT + 1):
        write_file(os.path.join(view, f"fig-create-{n}"), f"{n}\n".encode())
    os.stat(os.path.join(view, f"fig-create-{CREATE_COUNT}"))
    total = time.perf_counter() - start
    return {
        "count": CREATE_COUNT,
        "total_s": round(total, 3),
        "extrapolated_100000_s": round(total * 100000 / CREATE_COUNT, 3),
    }


def close_64m(view):
    path = os.path.join(view, "fig-64m")
    block = bytearray(os.urandom(MIB))
    start = time.perf_counter()
    fd = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o644)
    try:
        for n in range(BIG_BYTES // MIB):
            block[:8] = n.to_bytes(8, "little")
            chunk = memoryview(block)
            while chunk:
                chunk = chunk[os.write(fd, chunk):]
    finally:
        os.close(fd)
    os.stat(path)
    total = time.perf_counter() - start
    return {
        "bytes": BIG_BYTES,
        "total_s": round(total, 3),
        "extrapolated_1g_s": round(total * (1 << 30) / BIG_BYTES, 3),
    }


WORKLOADS = [("close_4k", close_4k), ("create_1000", create_1000), ("close_64m", close_64m)]


def limit(entry, filesystem):
    """The budget's number for this filesystem: a number, or one per filesystem."""
    value = entry["max"]
    return value[filesystem] if isinstance(value, dict) else value


def main():
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--view", required=True)
    parser.add_argument("--filesystem", required=True)
    parser.add_argument("--image", required=True)
    parser.add_argument("--budget", required=True)
    parser.add_argument("--output", required=True)
    parser.add_argument("--build", default="release",
                        help="the build measured, as the report names it (the image ships release)")
    parser.add_argument("--only", action="append", choices=[n for n, _ in WORKLOADS],
                        help="run only this workload (repeatable; a probe's shortcut)")
    args = parser.parse_args()
    budgets = json.load(open(args.budget))["budgets"]

    workloads = []
    for name, run in WORKLOADS:
        if args.only and name not in args.only:
            continue
        budget = budgets[name]
        figure = run(args.view)
        over = figure[budget["metric"]] > limit(budget, args.filesystem)
        workloads.append({"name": name, **figure, "budget": budget["text"],
                          "warn": over, "guarantee": GUARANTEE})
        print(f"{name} on {args.filesystem}: {json.dumps(figure)}", flush=True)
        if over:
            print(f"::warning title=tree views, {args.filesystem}::{name}: "
                  f"{budget['metric']} {figure[budget['metric']]} is over the recording "
                  f"engine's budget, {budget['text']}", flush=True)

    report = {"build": args.build, "filesystem": args.filesystem, "image": args.image,
              "workloads": workloads}
    temporary = args.output + ".tmp"
    with open(temporary, "w") as out:
        json.dump(report, out, indent=2)
        out.write("\n")
    os.replace(temporary, args.output)
    return 0


if __name__ == "__main__":
    sys.exit(main())
