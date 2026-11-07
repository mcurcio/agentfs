#!/usr/bin/env python3
"""Read workload bytes from an immutable published tree commit."""
import subprocess


def read(repo, ref, path):
    def git(*args):
        return subprocess.check_output(["git", "--git-dir=" + str(repo), *args])
    commit = git("rev-parse", ref).strip().decode("ascii")
    message = git("show", "-s", "--format=%B", commit).splitlines()
    prefix = "files/" if b"Agentfs-Format: envelope-1" in message else ""
    return git("show", commit + ":" + prefix + path)
