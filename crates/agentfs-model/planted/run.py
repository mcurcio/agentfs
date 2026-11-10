#!/usr/bin/env python3
"""Runs the planted-bug corpus (VERIFYING.md, "Planting bugs") and checks matrix.tsv.

For each bug: a copy of the repository with one find-and-replace applied, its own target
directory, and the model's two test targets. A bug counts as caught only when a named
check fails (an invariant of check.rs, the random runs' oracle or the ledger) or an
explicit assertion in a test file does. A panic in the model or the harness never counts,
in either test binary: a model panic in a scenario panics outside the test files, and one
in a random run is a "harness panic". An anchor that does not match exactly once reports
STALE and stops the run before any bug runs; a mutant nothing catches reports SURVIVED; a
bug past its time limit reports TIMEOUT; a run that could not start a process reports
HARNESS; each fails the run.

Fast mode (the default) runs each test in its own process, the scenarios first, then the
random runs, in name order, and stops at the first test that catches the bug. `--full`
runs every test of both targets once and records the check that tripped first most often.

Run it through ci/model-docker.sh, never a hand-written `docker run` (VERIFYING.md):

    ci/model-docker.sh -- python3 crates/agentfs-model/planted/run.py --work /work/planted \\
        [--jobs N] [--only a,b] [--full] [--write]

DIR (--work) holds each bug's result, copy, target and TMPDIR. A result is kept under a
hash of its corpus entry and of the model's sources and tests, so a stopped run goes on
where it left off and a changed model or bug runs again. Without --write the results must
equal planted/matrix.tsv and the pass must take at most 1.5 times its time in
planted/budget.json; --write records both. DIR and TMPDIR must not be on a host file share
(mounts.py refuses one, exit 2). Each bug prints `unit-start: ID` and `unit-done: ID ...`,
which ci/model-docker.sh's stall alarm reads.
"""

import argparse
import concurrent.futures
import hashlib
import json
import os
import pathlib
import re
import shutil
import signal
import subprocess
import sys
import threading
import time
import tomllib

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parent))
import mounts  # noqa: E402

ROOT = pathlib.Path(__file__).resolve().parents[3]
MODEL = ROOT / "crates/agentfs-model"
SRC = MODEL / "src"
CORPUS = MODEL / "planted/corpus.toml"
MATRIX = MODEL / "planted/matrix.tsv"
BUDGET = MODEL / "planted/budget.json"
# A pass may take this many times its budgeted seconds.
SLACK = 1.5
# The names a failure can carry: check.rs's invariants, the random runs' oracle
# (tests/interleavings.rs) and the ledger (tests/gitledger).
CHECKS = (
    "no_silent_loss no_flicker locks upstream_wins changes_whole syncs_deliver "
    "transactions_end_as_told transactions_undisturbed one_writer discarded_leave_no_trace "
    "rename_overs_whole full_lineage placed_and_unlinked step_checked check_converged "
    "losers_pushed oracle_no_call_waits oracle_bytes oracle_p1 oracle_deferred "
    "oracle_orphans oracle_crash oracle_drained ledger"
).split()
CHECK_RE = re.compile(r"(?:^|[\s:\"])(" + "|".join(CHECKS) + r"): ")
# Where a failing test panicked.
PANIC_AT = re.compile(r"panicked at (\S+?):\d+:\d+:")
# The test files whose explicit assertions count; a random run reports one line per seed.
SCENARIOS = "crates/agentfs-model/tests/scenarios.rs"
INTERLEAVINGS = "crates/agentfs-model/tests/interleavings.rs"
SEED_LINE = re.compile(r"seed \d+, step \d+: ")
# One bug's limit: a hang fails the run instead of sitting silently.
BUG_SECONDS = 30 * 60
# The block of one failing test in libtest's output.
FAILED = re.compile(r"^---- (\S+) stdout ----\n(.*?)(?=^---- |^failures:\n)", re.M | re.S)
SPAWN_FAILED = "Resource temporarily unavailable"
PRINT = threading.Lock()


def say(line):
    with PRINT:
        print(line, flush=True)


def anchors(bug, src=SRC):
    """The source files matched by bug's find text, with how many times."""
    hits = {}
    for path in sorted(src.rglob("*.rs")):
        count = path.read_text().count(bug["find"])
        if count:
            hits[path] = count
    return hits


def stale(bug, src=SRC):
    hits = anchors(bug, src)
    if sum(hits.values()) == 1:
        return None
    return f"STALE: {bug['id']} matches {sum(hits.values())} times in {list(map(str, hits))}"


def model_hash(model=MODEL):
    """A hash of everything a bug's result depends on in the model: its sources and tests."""
    h = hashlib.sha256()
    for d in (model / "src", model / "tests"):
        for path in sorted(d.rglob("*")):
            if path.is_file():
                h.update(str(path.relative_to(model)).encode() + b"\0")
                h.update(path.read_bytes() + b"\0")
    return h.hexdigest()


def result_path(work, bug, model, mode):
    """Where bug's result is kept: keyed on its entry, the model and the mode, never on its
    id alone."""
    entry = json.dumps(bug, sort_keys=True) + model + mode
    return work / f"{bug['id']}.{hashlib.sha256(entry.encode()).hexdigest()[:16]}.result"


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
    """One (test, check) per failure that counts, in test order: a named check, or
    `assert` for an explicit assertion in a test file. Panics never count."""
    found = []
    for test, block in sorted(FAILED.findall(output)):
        at = PANIC_AT.search(block)
        where = at.group(1) if at else ""
        body = [l for l in block.split("\n") if not l.startswith("  ")]
        if where == INTERLEAVINGS:
            # A seed's failure, also inside another test's message (the commit-date test's).
            seeds = [l for l in body if SEED_LINE.search(l)]
            if not seeds:
                # A floor of the random runs: an explicit assertion.
                found.append((test, "assert"))
            for line in seeds:
                names = CHECK_RE.findall(line)
                if names and "harness panic" not in line:
                    found.append((test, names[0]))
        elif where == SCENARIOS:
            names = CHECK_RE.findall("\n".join(body))
            found.append((test, names[0] if names else "assert"))
        # Anywhere else (the model's sources, a helper): a panic, not a catch.
    return found


def classify(code, output):
    """(status, hits) of one test run's exit status (None: past its limit) and output."""
    if code is None:
        return "TIMEOUT", []
    if SPAWN_FAILED in output:
        return "HARNESS", []
    if "could not compile" in output or "error[E" in output:
        return "BUILD", []
    if code == 0:
        return "SURVIVED", []
    if code < 0 or "test result:" not in output:
        return "HARNESS", []
    hits = first_checks(output)
    # Tests failed, but only by panics: nothing caught it.
    return ("caught", hits) if hits else ("SURVIVED", [])


def run_limited(cmd, cwd, env, deadline):
    """(exit status or None past the deadline, output) of one process group."""
    inherited = {k: v for k, v in os.environ.items()
                 if k not in ("AGENTFS_MODEL_SEED", "AGENTFS_MODEL_SEEDS")}
    proc = subprocess.Popen(
        cmd, cwd=cwd, env={**inherited, **env}, stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT, text=True, errors="replace", start_new_session=True,
    )
    try:
        output, _ = proc.communicate(timeout=max(1, deadline - time.monotonic()))
        return proc.returncode, output
    except subprocess.TimeoutExpired:
        os.killpg(proc.pid, signal.SIGKILL)
        output, _ = proc.communicate()
        return None, output


CARGO = ["cargo", "test", "--locked", "-p", "agentfs-model", "--test", "scenarios",
         "--test", "interleavings"]


def fast(dst, env, deadline):
    """(status, hits, log): each test alone, scenarios first, stopping at the first catch."""
    code, built = run_limited(CARGO + ["--no-run"], dst, env, deadline)
    if code is None:
        return "TIMEOUT", [], built
    if code != 0:
        return ("HARNESS" if SPAWN_FAILED in built else "BUILD"), [], built
    bins = dict(re.findall(r"Executable tests/(\w+)\.rs \((.+)\)", built))
    log = [built]
    for target in ("scenarios", "interleavings"):
        listed = subprocess.run([bins[target], "--list"], capture_output=True, text=True,
                                check=True).stdout
        for test in sorted(l[:-len(": test")] for l in listed.splitlines() if l.endswith(": test")):
            code, out = run_limited([bins[target], "--exact", test], dst / "crates/agentfs-model",
                                    env, deadline)
            log.append(out)
            status, hits = classify(code, out)
            if status in ("caught", "TIMEOUT", "HARNESS"):
                return status, hits, "".join(log)
    return "SURVIVED", [], "".join(log)


def full(dst, env, deadline):
    """(status, hits, log): every test of both targets once."""
    code, out = run_limited(CARGO + ["--no-fail-fast"], dst, env, deadline)
    status, hits = classify(code, out)
    return status, hits, out


def run_bug(work, bug, model, mode):
    result = result_path(work, bug, model, mode)
    if result.exists():
        return result.read_text()
    say(f"unit-start: {bug['id']}")
    start = time.monotonic()
    dst = copy_of(work, bug)
    path = next(p for p in (dst / "crates/agentfs-model/src").rglob("*.rs")
                if p.read_text().count(bug["find"]))
    path.write_text(path.read_text().replace(bug["find"], bug["replace"]))
    target = work / "target" / bug["id"]
    (work / "tmp").mkdir(exist_ok=True)
    env = {"CARGO_TARGET_DIR": str(target), "TMPDIR": str(work / "tmp")}
    if bug["seeds"] != 64:
        env["AGENTFS_MODEL_SEEDS"] = str(bug["seeds"])
    status, hits, log = (fast if mode == "fast" else full)(dst, env, start + BUG_SECONDS)
    (work / f"{bug['id']}.log").write_text(log)
    shutil.rmtree(dst, ignore_errors=True)
    shutil.rmtree(target, ignore_errors=True)
    counts = {}
    for _, name in hits:
        counts[name] = counts.get(name, 0) + 1
    # The check that was first to trip the most often: names sort last to break ties.
    first = max(counts, key=lambda n: (counts[n], n)) if counts else "-"
    seconds = round(time.monotonic() - start)
    line = f"{bug['id']}\t{bug['rule']}\t{bug['seeds']}\t{status}\t{first}\n"
    # A run the machine broke is not a result: the next run tries that bug again.
    if status not in ("TIMEOUT", "HARNESS"):
        result.write_text(line)
        (work / f"{bug['id']}.seconds").write_text(f"{seconds}\n")
    say(f"unit-done: {bug['id']} {status} {seconds}s")
    return line


def load_budget():
    return json.loads(BUDGET.read_text()) if BUDGET.exists() else {}


def main(argv=None):
    ap = argparse.ArgumentParser()
    ap.add_argument("--work", required=True, type=pathlib.Path)
    ap.add_argument("--jobs", type=int, default=4)
    ap.add_argument("--only", default="")
    ap.add_argument("--full", action="store_true")
    ap.add_argument("--write", action="store_true")
    ap.add_argument("--anchors-only", action="store_true")
    ap.add_argument("--corpus", type=pathlib.Path, default=CORPUS)
    ap.add_argument("--src", type=pathlib.Path, default=SRC)
    args = ap.parse_args(argv)
    corpus = tomllib.loads(args.corpus.read_text())["bug"]
    only = {x for x in args.only.split(",") if x}
    bugs = [b for b in corpus if not only or b["id"] in only]
    bad = [p for p in (stale(b, args.src) for b in bugs) if p]
    for p in bad:
        print(p)
    # A stale anchor stops the run before any bug runs, cached result or not.
    if bad or args.anchors_only:
        if bad:
            print(f"{len(bad)} stale anchors: re-anchor them in corpus.toml")
        return 1 if bad else 0
    work = args.work
    work.mkdir(parents=True, exist_ok=True)
    (work / "tmp").mkdir(exist_ok=True)
    refused = mounts.refuse([str(work), str(work / "tmp")])
    if refused:
        print(refused, file=sys.stderr)
        return 2
    mode = "full" if args.full else "fast"
    model = model_hash()
    start = time.monotonic()
    with concurrent.futures.ThreadPoolExecutor(args.jobs) as pool:
        lines = list(pool.map(lambda b: run_bug(work, b, model, mode), bugs))
    elapsed = round(time.monotonic() - start)
    header = "id\trule\tseeds\tstatus\tcheck\n"
    body = header + "".join(sorted(lines))
    (work / "matrix.tsv").write_text(body)
    failed = 0
    for line in lines:
        fields = line.split("\t")
        if fields[3] != "caught":
            failed += 1
            print(f"{fields[3]}: {fields[0]}")
    print(f"pass: {elapsed}s ({mode}, {len(bugs)} bugs, {args.jobs} jobs)")
    if failed:
        print(f"{failed} of {len(bugs)} bugs were not caught")
        return 1
    if only:
        print(f"{len(bugs)} bugs caught ({work / 'matrix.tsv'}); --only runs are not checked against the matrix")
        return 0
    key = "corpus" if mode == "fast" else "corpus_full"
    budget = load_budget()
    if args.write:
        times = {b["id"]: int((work / f"{b['id']}.seconds").read_text())
                 for b in bugs if (work / f"{b['id']}.seconds").exists()}
        slowest = max(times, key=times.get)
        budget[key] = {"pass_seconds": elapsed, "jobs": args.jobs,
                       "slowest_bug": slowest, "slowest_bug_seconds": times[slowest]}
        # ci/model-docker.sh says when the times were measured beside another container.
        if os.environ.get("MODEL_BUDGET_NOTE"):
            budget[key]["note"] = os.environ["MODEL_BUDGET_NOTE"]
        BUDGET.write_text(json.dumps(budget, indent=2, sort_keys=True) + "\n")
        MATRIX.write_text(body)
        print(f"wrote {MATRIX} ({len(bugs)} bugs, all caught) and {BUDGET}")
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
    allowed = budget.get(key, {}).get("pass_seconds")
    if allowed is not None and elapsed > SLACK * allowed:
        print(f"the pass took {elapsed}s, over {SLACK} times its budget of {allowed}s ({BUDGET})")
        return 1
    print(f"{len(bugs)} bugs caught and {MATRIX} reproduced")
    return 0


if __name__ == "__main__":
    sys.exit(main())
