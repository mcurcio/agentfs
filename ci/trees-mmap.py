#!/usr/bin/env python3
"""Strict shared mapping and saved-reader observations on a Linux mount."""
import mmap
import os
from pathlib import Path
import sys
from tree_git_read import read

root = Path(sys.argv[1])
repo = sys.argv[2]
path = root / "mapped"
old = b"old!" + bytes(mmap.PAGESIZE - 4)
new = b"new!" + old[4:]
with open(path, "wb", buffering=0) as f:
    f.write(old)
prior = open(path, "rb", buffering=0)
fd = os.open(path, os.O_RDWR)
view = mmap.mmap(fd, mmap.PAGESIZE, flags=mmap.MAP_SHARED, prot=mmap.PROT_READ | mmap.PROT_WRITE)
view[:4] = b"new!"
assert view[:4] == b"new!", "mapping must read its own stores"
assert os.pread(fd, 4, 0) == b"new!", "writer must read its own mapping stores"
assert path.read_bytes() == old, "new reader must see saved content before msync"
view.flush()
assert os.pread(fd, 4, 0) == b"new!", "writer must read its own flushed stores"
assert path.read_bytes() == old, "msync must not publish the open mapping"
os.close(fd)
assert path.read_bytes() == old, "closing fd must not publish while mapping lives"
view.close()
assert path.read_bytes() == new, "last unmap must publish before immediate open"
assert prior.read() == old, "reader opened before mapping must retain its bytes"
prior.close()
recorded = read(repo, "root", "mapped")
assert recorded == new, "history must contain the mapped write"
print("MAP_SHARED store, msync, saved readers, fd close, final unmap and history: passed", flush=True)
