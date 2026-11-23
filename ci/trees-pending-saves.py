#!/usr/bin/env python3
"""Pending saves under failure (docs/trees.md, "Pending saves").

Usage: trees-pending-saves.py MOUNT HISTORY_GIT METRICS --case CASE
                              [--fork FORK --fork-ref REF]

MOUNT is a tree view of the root on a failpoints build started with
AGENTFS_FAILPOINTS=tree.close_decide=<action>, HISTORY_GIT its volume's
history.git, METRICS its --metrics-output file. ci/e2e-tree-pending-saves.sh
starts a fresh volume for each case and runs this once per case:

    error      (with --fork, the fork's view, and --fork-ref, its branch) once
               the fork has taken every root entry, the fork's client
               overwrites f in place and closes it; the decision fails. The client reads its new content; the metrics file says
               failed_saves 1 and pending_saves 1; the root's append to f syncs
               into the fork as the fork tree's next step, which saves the
               failed close first: the fork's history has the save, then the
               sync.
    panic      the root's client overwrites f in place and closes it; the save
               worker panics in the decision. The client reads its new content;
               the next step of the tree (a close of g) saves f; a later save of
               g, decided on the same worker, lands.
    kill       the root's client overwrites f in place and closes it; the
               decision kills agentfs. close(2) has returned by then.
    restarted  after the kill and a mount without failpoints: the view and
               history both hold f's new content.

A wait on the view's metrics file, a view or history reads again every 50 ms
until what it waits for holds, bounded by BOUND_S (a hang fails the case); no
step sleeps for a guessed time and then assumes something happened.
"""
import argparse
import json
import os
from pathlib import Path
import subprocess
import sys
import time

from tree_git_read import read, read_once

BOUND_S = 60.0

SEED = b"line1\nline2\nline3\n"
FORKED = b"LINE1\nline2\nline3\n"
APPENDED = b"line1\nline2\nline3\nmore\n"
MERGED = b"LINE1\nline2\nline3\nmore\n"
OLD = b"old content\n" * 4096
NEW = b"NEW CONTENT\n" * 4096
G1 = b"g first....\n" * 4096
G2 = b"g second...\n" * 4096


def check(name, observed, expected):
    shown = observed if len(repr(observed)) < 400 else f"{len(observed)} long"
    print(f"{name}: {shown!r}", flush=True)
    if observed != expected:
        raise SystemExit(f"FAILED: {name}: expected {expected!r:.120}")


def write_new(path, data):
    fd = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o644)
    try:
        os.write(fd, data)
    finally:
        os.close(fd)


def overwrite(path, data, offset=0):
    """`data` written over `path` at `offset`, its length kept: a close the
    view's save worker decides, not one saved at its start."""
    fd = os.open(path, os.O_WRONLY)
    try:
        assert os.pwrite(fd, data, offset) == len(data)
    finally:
        os.close(fd)


def append(path, data):
    fd = os.open(path, os.O_WRONLY | os.O_APPEND)
    try:
        os.write(fd, data)
    finally:
        os.close(fd)


def read_all(path):
    return Path(path).read_bytes()


def metrics_of(path):
    try:
        with open(path) as file:
            return json.load(file)
    except (OSError, ValueError):
        return {}


def metrics_until(path, what, ok):
    """The first metrics file written after this call began (two past the
    one seen now: the view writes it at each built event and every 100 ms)
    for which `ok` holds; fails the case past BOUND_S."""
    floor = metrics_of(path).get("seq", 0) + 2
    until = time.monotonic() + BOUND_S
    last = {}
    while time.monotonic() < until:
        last = metrics_of(path)
        if last.get("seq", 0) >= floor and ok(last):
            return last
        time.sleep(0.05)
    keys = ("pending_saves", "failed_saves", "oldest_pending_save_ns", "history")
    raise SystemExit(f"FAILED: {what}: {json.dumps({k: last.get(k) for k in keys})}")


def built(m):
    return m["history"]["built_through"] >= m["history"]["appended"]


def view_until(path, want, what):
    """`path` read through a view until it holds `want`, bounded."""
    until = time.monotonic() + BOUND_S
    got = None
    while time.monotonic() < until:
        try:
            got = read_all(path)
        except FileNotFoundError:
            got = None
        if got == want:
            return
        time.sleep(0.05)
    check(what, got, want)


def committed(git, ref, name):
    try:
        return read_once(git, ref, name)
    except subprocess.CalledProcessError:
        return None


def parents_of(git, ref, name):
    """`name`'s content in each parent of `ref`'s commit, sorted."""
    line = subprocess.check_output(
        ["git", "--git-dir=" + str(git), "show", "-s", "--format=%P", ref], text=True)
    return sorted(committed(git, parent, name) or b"" for parent in line.split())


def case_error(root, git, metrics, fork, fork_ref):
    write_new(root / "f", SEED)
    view_until(fork / "f", SEED, "error: f synced into the fork")
    # A sync takes the root as built, so the create's sync can show SEED
    # while the save's sync is still queued. Every take is a step of the
    # fork tree, which saves a failed close first: the failed save stays
    # pending only while no take is queued.
    metrics_until(metrics, "error: the fork took every root entry",
                  lambda m: m["queued"] == 0 and built(m))
    overwrite(fork / "f", b"LINE1")
    check("error: the fork's client reads its new content", read_all(fork / "f"), FORKED)
    metrics_until(metrics, "error: failed_saves 1 and pending_saves 1",
                  lambda m: m["failed_saves"] == 1 and m["pending_saves"] == 1)
    append(root / "f", b"more\n")
    check("error: the root reads its append", read_all(root / "f"), APPENDED)
    # The sync into f is the fork tree's next step: it saves the failed close
    # first and lands after it, so the fork's history has the seed, the save,
    # then the sync, and nothing is lost.
    check("error: the sync lands", read(git, fork_ref, "f", MERGED, BOUND_S), MERGED)
    # The sync's commit joins the root's append and the fork's save.
    check("error: the fork's history has the save before the sync",
          parents_of(git, fork_ref, "f"), sorted([FORKED, APPENDED]))
    view_until(fork / "f", MERGED, "error: the fork's client reads the save and the sync")
    metrics_until(metrics, "error: no save pending, one failed",
                  lambda m: m["pending_saves"] == 0 and m["failed_saves"] == 1)


def case_panic(root, git, metrics):
    write_new(root / "f", OLD)
    write_new(root / "g", G1)
    overwrite(root / "f", NEW)
    check("panic: the client reads its new content", read_all(root / "f") == NEW, True)
    metrics_until(metrics, "panic: failed_saves 1 and pending_saves 1",
                  lambda m: m["failed_saves"] == 1 and m["pending_saves"] == 1 and built(m))
    check("panic: history keeps f's old content", committed(git, "root", "f") == OLD, True)
    # The tree's next step, a close of g, saves the panicked save first.
    overwrite(root / "g", G2)
    check("panic: the next step saved f", read(git, "root", "f", NEW, BOUND_S) == NEW, True)
    metrics_until(metrics, "panic: no save pending after the next step",
                  lambda m: m["pending_saves"] == 0)
    # The view goes on: a later save, decided on the same save worker, lands.
    overwrite(root / "g", G1)
    check("panic: a later save of g lands", read(git, "root", "g", G1, BOUND_S) == G1, True)
    metrics_until(metrics, "panic: the later save was not a failure",
                  lambda m: m["pending_saves"] == 0 and m["failed_saves"] == 1)


def case_kill(root, metrics):
    write_new(root / "f", OLD)
    metrics_until(metrics, "kill: f's first save built", built)
    # close(2) returns before the decision, which kills the process.
    overwrite(root / "f", NEW)
    print("kill: close(2) returned before the decision", flush=True)


def case_restarted(root, git):
    check("restarted: the view reads the save", read_all(root / "f") == NEW, True)
    check("restarted: history has the save", read(git, "root", "f", NEW, BOUND_S) == NEW, True)


def main():
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("mount", type=Path)
    parser.add_argument("history_git", type=Path)
    parser.add_argument("metrics")
    parser.add_argument("--case", required=True, choices=("error", "panic", "kill", "restarted"))
    parser.add_argument("--fork", type=Path, help="with --case error: the fork's view")
    parser.add_argument("--fork-ref", help="with --case error: the fork's branch")
    args = parser.parse_args()
    if args.case == "error":
        if args.fork is None or args.fork_ref is None:
            parser.error("--case error needs --fork and --fork-ref")
        case_error(args.mount, args.history_git, args.metrics, args.fork, args.fork_ref)
    elif args.case == "panic":
        case_panic(args.mount, args.history_git, args.metrics)
    elif args.case == "kill":
        case_kill(args.mount, args.metrics)
    else:
        case_restarted(args.mount, args.history_git)
    print(f"{args.case}: ok", flush=True)
    return 0


if __name__ == "__main__":
    sys.exit(main())
