#!/usr/bin/env python3
"""Refuses a folder on a host file share (a Docker Desktop bind mount).

Every model run makes git repositories under TMPDIR and the corpus's work folder. On a
Docker Desktop bind mount (virtiofs, or the older file-sharing file systems) 8 seeds took
320 s against 24 s on the container's own disk, so a run there is refused: exit 2, with
the folder and its file system named.

    python3 crates/agentfs-model/planted/mounts.py PATH...

run.py imports `refuse`; ci/model-digest.sh runs this file on its TMPDIR.
"""

import os
import sys

# File systems that carry a host's folder into a container or VM.
HOST_SHARES = {"virtiofs", "fakeowner", "grpcfuse", "fuse.grpcfuse", "osxfs", "fuse.osxfs",
               "9p", "vboxsf", "prl_fs", "fuse.sshfs"}


def mount_of(path, mountinfo):
    """(mount point, file system type) of the mount holding `path`, from mountinfo text."""
    best = ("", "")
    for line in mountinfo.splitlines():
        left, _, right = line.partition(" - ")
        fields, rest = left.split(), right.split()
        if len(fields) < 5 or not rest:
            continue
        point = fields[4].replace("\\040", " ")
        inside = path == point or path.startswith(point.rstrip("/") + "/")
        if inside and len(point) > len(best[0]):
            best = (point, rest[0])
    return best


def refuse(paths, mountinfo=None):
    """The message for the first path on a host share, or None."""
    if mountinfo is None:
        try:
            with open("/proc/self/mountinfo") as f:
                mountinfo = f.read()
        except OSError:
            return None
    for p in paths:
        real = os.path.realpath(p)
        point, fstype = mount_of(real, mountinfo)
        if fstype in HOST_SHARES:
            return (f"{p} is on a host file share ({fstype} at {point}): model runs there are "
                    f"about 13 times slower (8 seeds: 320 s against 24 s). Use the container's "
                    f"/tmp and a named volume, through ci/model-docker.sh.")
    return None


if __name__ == "__main__":
    message = refuse(sys.argv[1:])
    if message:
        print(message, file=sys.stderr)
        sys.exit(2)
