#!/usr/bin/env python3
"""Measures the cover behind crates/agentfs-model/tests/regression-seeds.txt (VERIFYING.md,
"Tiers") and prints the two lines it should hold. Run it through ci/model-docker.sh:

    ci/model-docker.sh -- python3 ci/regression-seeds.py --work /work/cover [--must 24]

It runs every random-run generator at every seed of the sweep, each seed in its own process
(AGENTFS_MODEL_SEED), and reads each run's tally (the counters of what the run met: takes
that waited, crashes in a transaction, rename-overs, ...) and, for the two phase 1
generators, its trace. The `random` line is a greedy set cover of every (generator,
counter) pair any seed of the sweep reached in random_interleavings_full_lineage,
_adjacent_lines and _phase1_scope, plus the seeds --must names (a planted bug only a seed
catches, from planted/run.py's log of that bug), plus a cover of the counters the mountable
seeds of _phase1_scope reach (lib.rs's `mountable`: one fork before any call, no open that
both appends and writes at an offset), so that real mounts replay seeds too. The `rounds`
line is the same over random_interleavings_rounds and _rounds_phase1_scope. A cover of
counters keeps every tally floor of the tests met, since each floor is a counter above 0.
"""

import argparse
import concurrent.futures
import os
import pathlib
import re
import subprocess
import sys

ROOT = pathlib.Path(__file__).resolve().parents[1]
MODEL = ROOT / "crates/agentfs-model"
RANDOM = ["full_lineage", "adjacent_lines", "phase1_scope"]
ROUNDS = ["rounds", "rounds_phase1_scope"]


def binary():
    out = subprocess.run(
        ["cargo", "test", "--locked", "-p", "agentfs-model", "--test", "interleavings", "--no-run"],
        cwd=ROOT, capture_output=True, text=True, check=True,
    ).stderr
    return re.search(r"Executable tests/interleavings\.rs \((.+)\)", out).group(1)


def run(bin, work, test, seed):
    """The counters above 0 of one seed of one generator, and whether its trace mounts."""
    traces = work / f"{test}.{seed}"
    env = {k: v for k, v in os.environ.items() if not k.startswith("AGENTFS_MODEL_")}
    env.update(AGENTFS_MODEL_SEED=str(seed), AGENTFS_MODEL_TRACE_DIR=str(traces))
    out = subprocess.run(
        [bin, f"random_interleavings_{test}", "--exact", "--nocapture", "--test-threads", "2"],
        cwd=MODEL, env=env, capture_output=True, text=True,
    )
    if out.returncode != 0:
        sys.exit(f"{test} at seed {seed} failed:\n{out.stdout}{out.stderr}")
    tally = re.search(r"Tally \{(.*?)\}", out.stderr).group(1)
    counters = {k for k, v in re.findall(r"(\w+): (\d+)", tally) if int(v) > 0}
    trace = next(traces.glob("*.trace"), None)
    print(f"unit-done: {test} {seed}", flush=True)
    return test, seed, counters, trace is not None and mountable(trace)


def mountable(trace):
    """lib.rs's `mountable`, on a trace's text."""
    calls = [line.split(" | ")[0].split() for line in trace.read_text().splitlines() if line]
    forks, called = 0, False
    for c in calls:
        if c[0] == "fork":
            forks += 1
            if called or forks > 1:
                return False
        elif c[0] != "take":
            called = True
    appends = {c[1] for c in calls if c[0] == "append"}
    return not appends & {c[1] for c in calls if c[0] == "pwrite"}


def cover(runs, tests, seeds, chosen):
    """Greedy: seeds added to `chosen` until every (test, counter) any of `seeds` reaches is."""
    reach = lambda s: {(t, c) for t in tests for c in runs[t, s]}
    need = set().union(*(reach(s) for s in seeds)) - set().union(set(), *(reach(s) for s in chosen))
    while need:
        best = max(seeds, key=lambda s: (len(reach(s) & need), -s))
        chosen.add(best)
        need -= reach(best)
    return chosen


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--work", required=True, type=pathlib.Path)
    ap.add_argument("--must", default="", help="random seeds to keep, comma separated")
    ap.add_argument("--seeds", type=int, default=64)
    ap.add_argument("--rounds", type=int, default=16)
    ap.add_argument("--jobs", type=int, default=os.cpu_count())
    args = ap.parse_args()
    args.work.mkdir(parents=True, exist_ok=True)
    if any(args.work.iterdir()):
        sys.exit(f"{args.work} is not empty")
    bin = binary()
    jobs = [(t, s) for t in RANDOM for s in range(1, args.seeds + 1)]
    jobs += [(t, s) for t in ROUNDS for s in range(1, args.rounds + 1)]
    runs, mounts = {}, {}
    with concurrent.futures.ThreadPoolExecutor(args.jobs) as pool:
        for test, seed, counters, mounted in pool.map(lambda j: run(bin, args.work, *j), jobs):
            runs[test, seed] = counters
            mounts[test, seed] = mounted
    random = cover(runs, RANDOM, range(1, args.seeds + 1), {int(s) for s in args.must.split(",") if s})
    rounds = cover(runs, ROUNDS, range(1, args.rounds + 1), set())
    # The mount tier's own cover, among the mountable seeds alone.
    for chosen, test, last in ((random, "phase1_scope", args.seeds),
                               (rounds, "rounds_phase1_scope", args.rounds)):
        mountable_seeds = [s for s in range(1, last + 1) if mounts[test, s]]
        chosen |= cover(runs, [test], mountable_seeds, chosen & set(mountable_seeds))
    print("random " + " ".join(map(str, sorted(random))))
    print("rounds " + " ".join(map(str, sorted(rounds))))


if __name__ == "__main__":
    main()
