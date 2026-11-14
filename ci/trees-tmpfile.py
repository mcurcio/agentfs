#!/usr/bin/env python3
"""O_TMPFILE and linkat(AT_EMPTY_PATH) on the mounted recording view."""
import ctypes
import os
from pathlib import Path
import sys
from tree_git_read import read

root = Path(sys.argv[1])
repo = sys.argv[2]
libc = ctypes.CDLL(None, use_errno=True)
libc.linkat.argtypes = [ctypes.c_int, ctypes.c_char_p, ctypes.c_int, ctypes.c_char_p, ctypes.c_int]
libc.linkat.restype = ctypes.c_int
fd = os.open(root, os.O_TMPFILE | os.O_RDWR, 0o600)
assert os.fstat(fd).st_nlink == 0
os.write(fd, b"anonymous data")
assert os.pread(fd, 14, 0) == b"anonymous data"
dirfd = os.open(root, os.O_RDONLY | os.O_DIRECTORY)
if libc.linkat(fd, b"", dirfd, b"linked", 0x1000) != 0:
    error = ctypes.get_errno()
    raise OSError(error, os.strerror(error))
os.close(dirfd)
assert os.fstat(fd).st_nlink == 1
assert (root / "linked").read_bytes() == b"", "open writes remain private after link"
os.close(fd)
assert (root / "linked").read_bytes() == b"anonymous data"
assert read(repo, "root", "linked", b"anonymous data") == b"anonymous data"
print("O_TMPFILE, linkat, private writes, final close and history: passed", flush=True)
