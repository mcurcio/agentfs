#!/usr/bin/env python3
"""Checks how often an fsync-class database was captured (design §8).

Lists every commit on BRANCH that changed PATH, PATH-wal or PATH-journal,
oldest first, with its author time (the time of its last record, whole
seconds) and its Agentfs-Trigger. Fails if:

- a commit with trigger fsync or interval follows the group's previous
  commit by less than COALESCE - 1 seconds (captures are at least one
  interval apart; one second is allowed for the commit window and the
  rounding of commit times);
- fewer than MIN or more than MAX commits changed the group;
- a trigger given with --want was never seen.

Prints one line per commit, "<commit> <time> <trigger>", then a summary.

    sqlite-captures.py GIT_DIR BRANCH PATH --coalesce N
                       [--min N] [--max N] [--want TRIGGER]...
"""

import argparse
import subprocess
import sys


def main():
    p = argparse.ArgumentParser()
    p.add_argument("git_dir")
    p.add_argument("branch")
    p.add_argument("path")
    p.add_argument("--coalesce", type=int, required=True)
    p.add_argument("--min", type=int, default=1)
    p.add_argument("--max", type=int, default=None)
    p.add_argument("--want", action="append", default=[])
    a = p.parse_args()

    out = subprocess.run(
        [
            "git", "--git-dir", a.git_dir, "log", "--reverse",
            "--format=%H %at %(trailers:key=Agentfs-Trigger,valueonly,separator=%x2C)",
            a.branch, "--",
            a.path, a.path + "-wal", a.path + "-journal",
        ],
        check=True, capture_output=True, text=True,
    ).stdout
    commits = []
    for line in out.splitlines():
        parts = line.split()
        if len(parts) < 2:
            continue
        commits.append((parts[0], int(parts[1]), parts[2] if len(parts) > 2 else "?"))

    failures = []
    previous = None
    seen = set()
    for commit, time, trigger in commits:
        seen.add(trigger)
        gap = "" if previous is None else f" (+{time - previous}s)"
        print(f"{commit} {time} {trigger}{gap}")
        if previous is not None and trigger in ("fsync", "interval"):
            if time - previous < a.coalesce - 1:
                failures.append(
                    f"{commit}: a {trigger} capture {time - previous}s after the "
                    f"previous one (coalesce {a.coalesce}s)"
                )
        previous = time
    if len(commits) < a.min:
        failures.append(f"only {len(commits)} capture(s), wanted at least {a.min}")
    if a.max is not None and len(commits) > a.max:
        failures.append(f"{len(commits)} captures, wanted at most {a.max}")
    for want in a.want:
        if want not in seen:
            failures.append(f"no capture with trigger {want} (saw {sorted(seen)})")
    print(f"{len(commits)} capture(s) of {a.path}: {sorted(seen)}", file=sys.stderr)
    if failures:
        sys.exit("\n".join(failures))


if __name__ == "__main__":
    main()
