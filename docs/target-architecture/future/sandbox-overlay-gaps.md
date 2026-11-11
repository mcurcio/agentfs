# Sandbox overlay: shared locks, inode gaps, hosting on a front

**Proposed, not built.** This page proposes closing three limits of the sandbox overlay. The commit that lands this slice deletes this page and moves the behaviour into the library or operator docs.

## What stands until then

- **Locks are not shared between the two mounts.** Each kernel keeps its own POSIX and BSD locks, so a lock taken
  through the mount point is not seen through the overlay.
- **Shown entries' inode numbers skip one per hidden entry.** This tells a sandbox how many hidden entries a
  directory holds, not their names. Not tested.
- **The overlay is served only by a single-process `mount`**, so not on CSI volumes, where every pod's mount is a front.

## What it proposes

- Serve locks in the daemon (`getlk`, `setlk`, `flock`) for both sessions.
- Give node ids only to shown entries.
- Host the overlay on a front, when a consumer needs it.
