#!/usr/bin/env python3
"""Runs the planted-bug corpus (VERIFYING.md, "Planting bugs") and checks matrix.tsv.

For each bug: a copy of the repository with one find-and-replace applied, its own target
directory, and `cargo test -p agentfs-model --test scenarios --test interleavings`. A bug
counts as caught when a test's failure is an invariant or a scenario assertion; a build
error or a "harness panic" does not count. An anchor that does not match exactly once
reports STALE; a mutant nothing catches reports SURVIVED; a bug past its time limit
reports TIMEOUT; a run that could not start a process reports HARNESS; each fails the run.

Run in the agentfs-dev container, from the repository root, always with `docker run --init`
(without an init process, orphaned git processes pile up as zombies until nothing can
start, and the run hangs):

    python3 crates/agentfs-model/planted/run.py --work DIR [--jobs N] [--only a,b] [--write]

DIR holds each bug's result, copy, target and TMPDIR, so a run that is stopped goes on
where it left off (delete DIR to run again). Without --write the results must equal
planted/matrix.tsv. DIR must be on a disk inside the container (a named Docker volume),
never a bind mount: every run makes git repositories under it, and 8 seeds took 320 s
with TMPDIR on a Docker Desktop bind mount against 24 s on the container's /tmp.
"""

import argparse
import concurrent.futures
import os
import pathlib
import re
import shutil
import signal
import subprocess
import sys
import tomllib

ROOT = pathlib.Path(__file__).resolve().parents[3]
MODEL = ROOT / "crates/agentfs-model"
SRC = MODEL / "src"
MATRIX = MODEL / "planted/matrix.tsv"
CHECKS = (
    "no_silent_loss no_flicker locks upstream_wins changes_whole syncs_deliver "
    "transactions_end_as_told transactions_undisturbed one_writer discarded_leave_no_trace "
    "rename_overs_whole full_lineage placed_and_unlinked step_checked check_converged "
    "losers_pushed"
).split()
CHECK_RE = re.compile(r"(?:^|[\s:\"])(" + "|".join(CHECKS) + r"): ")
# One bug's limit: a hang fails the run instead of sitting silently.
BUG_SECONDS = 30 * 60
# The block of one failing test in libtest's output.
FAILED = re.compile(r"^---- (\S+) stdout ----\n(.*?)(?=^---- |^failures:\n)", re.M | re.S)


def anchors(bug):
    """The source files matched by bug's find text, with how many times."""
    hits = {}
    for path in sorted(SRC.rglob("*.rs")):
        count = path.read_text().count(bug["find"])
        if count:
            hits[path] = count
    return hits


def stale(bug):
    hits = anchors(bug)
    if sum(hits.values()) == 1:
        return None
    return f"STALE: {bug['id']} matches {sum(hits.values())} times in {list(map(str, hits))}"


def copy_of(work, bug):
    dst = work / "src" / bug["id"]
    shutil.rmtree(dst, ignore_errors=True)
    shutil.copytree(
        ROOT,
        dst,
        ignore=shutil.ignore_patterns(".git", ".beads", ".claude", "target", "docs", "__pycache__"),
    )
    return dst


def first_checks(output):
    """One (test, check) for each failing test and each failure inside it, in test order."""
    found = []
    for test, block in sorted(FAILED.findall(output)):
        body = [l for l in block.split("\n") if not l.startswith("  ")]
        names = CHECK_RE.findall("\n".join(l for l in body if "harness panic" not in l))
        if not names and "harness panic" not in block:
            found.append((test, "assert"))
        for name in names:
            found.append((test, name))
    return found


def cargo_test(dst, env):
    """(cargo's exit status or None if it hit the limit, its output)."""
    proc = subprocess.Popen(
        ["cargo", "test", "--locked", "-p", "agentfs-model", "--test", "scenarios",
         "--test", "interleavings", "--no-fail-fast"],
        cwd=dst, env={**os.environ, **env}, stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
        text=True, errors="replace", start_new_session=True,
    )
    try:
        output, _ = proc.communicate(timeout=BUG_SECONDS)
        return proc.returncode, output
    except subprocess.TimeoutExpired:
        os.killpg(proc.pid, signal.SIGKILL)
        output, _ = proc.communicate()
        return None, output


def run_bug(work, bug):
    result = work / f"{bug['id']}.result"
    if result.exists():
        return result.read_text()
    problem = stale(bug)
    if problem:
        return f"{bug['id']}\t{bug['rule']}\t{bug['seeds']}\tSTALE\t-\t0\t0\n"
    dst = copy_of(work, bug)
    path = next(p for p in (dst / "crates/agentfs-model/src").rglob("*.rs")
                if p.read_text().count(bug["find"]))
    path.write_text(path.read_text().replace(bug["find"], bug["replace"]))
    target = work / "target" / bug["id"]
    env = {"CARGO_TARGET_DIR": str(target), "TMPDIR": str(work / "tmp")}
    (work / "tmp").mkdir(exist_ok=True)
    if bug["seeds"] != 64:
        env["AGENTFS_MODEL_SEEDS"] = str(bug["seeds"])
    code, output = cargo_test(dst, env)
    log = work / f"{bug['id']}.log"
    log.write_text(output)
    shutil.rmtree(dst, ignore_errors=True)
    shutil.rmtree(target, ignore_errors=True)
    if code is None:
        status = "TIMEOUT"
    elif "Resource temporarily unavailable" in output:
        status = "HARNESS"
    elif "could not compile" in output or "error[E" in output:
        status = "BUILD"
    elif code == 0:
        status = "SURVIVED"
    elif code < 0 or "test result:" not in output:
        status = "HARNESS"
    else:
        status = "caught"
    hits = first_checks(output) if status == "caught" else []
    if status == "caught" and not hits:
        status = "HARNESS"
    tests = sorted({t for t, _ in hits})
    counts = {}
    for _, name in hits:
        counts[name] = counts.get(name, 0) + 1
    # The check that was first to trip the most often: names sort last to break ties.
    first = max(counts, key=lambda n: (counts[n], n)) if counts else "-"
    line = f"{bug['id']}\t{bug['rule']}\t{bug['seeds']}\t{status}\t{first}\t{len(tests)}\t{sum(counts.values())}\n"
    # A run the machine broke is not a result: the next run tries that bug again.
    if status not in ("TIMEOUT", "HARNESS"):
        result.write_text(line)
    return line


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--work", required=True, type=pathlib.Path)
    ap.add_argument("--jobs", type=int, default=4)
    ap.add_argument("--only", default="")
    ap.add_argument("--write", action="store_true")
    ap.add_argument("--anchors-only", action="store_true")
    args = ap.parse_args()
    work = args.work
    work.mkdir(parents=True, exist_ok=True)
    corpus = tomllib.loads((MODEL / "planted/corpus.toml").read_text())["bug"]
    only = {x for x in args.only.split(",") if x}
    bugs = [b for b in corpus if not only or b["id"] in only]
    bad = [p for p in (stale(b) for b in bugs) if p]
    for p in bad:
        print(p)
    if args.anchors_only:
        return 1 if bad else 0
    with concurrent.futures.ThreadPoolExecutor(args.jobs) as pool:
        lines = list(pool.map(lambda b: run_bug(work, b), bugs))
    header = "id\trule\tseeds\tstatus\tfirst\ttests\thits\n"
    body = header + "".join(sorted(lines))
    (work / "matrix.tsv").write_text(body)
    failed = 0
    for line in lines:
        fields = line.split("\t")
        if fields[3] != "caught":
            failed += 1
            print(f"{fields[3]}: {fields[0]}")
    if failed:
        print(f"{failed} of {len(bugs)} bugs were not caught")
        return 1
    if only:
        print(f"{len(bugs)} bugs caught ({work / 'matrix.tsv'}); --only runs are not checked against the matrix")
        return 0
    if args.write:
        MATRIX.write_text(body)
        print(f"wrote {MATRIX}: {len(bugs)} bugs, all caught")
        return 0
    if not MATRIX.exists():
        print(f"no {MATRIX}: pass --write once")
        return 1
    if MATRIX.read_text() != body:
        print(f"the results differ from {MATRIX}:")
        for ours, theirs in zip(MATRIX.read_text().splitlines(), body.splitlines()):
            if ours != theirs:
                print(f"  matrix: {ours}\n  run:    {theirs}")
        return 1
    print(f"{len(bugs)} bugs caught and {MATRIX} reproduced")
    return 0


if __name__ == "__main__":
    sys.exit(main())
