#!/usr/bin/env python3
"""Checks every recorded version of a SQLite database in an agentfs history.

For each commit on BRANCH (newest first) that has PATH, the database and
the PATH-wal and PATH-journal from the same commit are written to a
scratch directory, opened with Python's sqlite3, and checked with
PRAGMA integrity_check. The table t must hold a whole number of
transactions (a multiple of ROWS rows). No commit may hold PATH-shm.

Prints one line per distinct version, "<commit> <rows>", newest first, and
fails if any version is damaged or fewer than MIN versions were recorded.

    sqlite-check.py GIT_DIR BRANCH PATH [--rows N] [--min N]
"""

import argparse
import os
import sqlite3
import subprocess
import sys
import tempfile


def git(git_dir, *args):
    return subprocess.run(
        ["git", "--git-dir", git_dir, *args], check=True, capture_output=True
    ).stdout


def blob(git_dir, commit, path):
    """The content of PATH at COMMIT, or None."""
    r = subprocess.run(
        ["git", "--git-dir", git_dir, "cat-file", "blob", f"{commit}:{path}"],
        capture_output=True,
    )
    return r.stdout if r.returncode == 0 else None


def check_version(files, name, rows):
    """Integrity and row count of the database in FILES ({suffix: bytes})."""
    with tempfile.TemporaryDirectory() as d:
        for suffix, content in files.items():
            with open(os.path.join(d, name + suffix), "wb") as f:
                f.write(content)
        con = sqlite3.connect(os.path.join(d, name))
        try:
            integrity = con.execute("PRAGMA integrity_check").fetchall()
            if integrity != [("ok",)]:
                return f"integrity_check: {integrity}", None
            tables = con.execute(
                "SELECT count(*) FROM sqlite_master WHERE name = 't'"
            ).fetchone()[0]
            if tables == 0:
                return None, 0
            count = con.execute("SELECT count(*) FROM t").fetchone()[0]
            if count % rows:
                return f"a torn transaction: {count} rows", count
            return None, count
        finally:
            con.close()


def main():
    p = argparse.ArgumentParser()
    p.add_argument("git_dir")
    p.add_argument("branch")
    p.add_argument("path")
    p.add_argument("--rows", type=int, default=50)
    p.add_argument("--min", type=int, default=2)
    a = p.parse_args()

    name = os.path.basename(a.path)
    commits = git(a.git_dir, "rev-list", a.branch).decode().split()
    seen = set()
    versions = 0
    failures = 0
    for commit in commits:
        if blob(a.git_dir, commit, a.path + "-shm") is not None:
            print(f"{commit} FAIL: {a.path}-shm was recorded")
            failures += 1
        files = {}
        for suffix in ["", "-wal", "-journal"]:
            content = blob(a.git_dir, commit, a.path + suffix)
            if content is not None:
                files[suffix] = content
        if "" not in files:
            continue
        key = tuple(sorted(files.items()))
        if key in seen:
            continue
        seen.add(key)
        versions += 1
        error, count = check_version(files, name, a.rows)
        members = "+".join(s or "db" for s in files)
        if error:
            print(f"{commit} FAIL ({members}): {error}")
            failures += 1
        else:
            print(f"{commit} {count} ({members})")
    if failures:
        sys.exit(f"{failures} damaged version(s) of {a.path}")
    if versions < a.min:
        sys.exit(f"only {versions} version(s) of {a.path}, wanted at least {a.min}")
    print(f"{versions} version(s) of {a.path}, all whole", file=sys.stderr)


if __name__ == "__main__":
    main()
