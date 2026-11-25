//! Line maps for tree landing (docs/target-architecture/git-model.md §7):
//! which version wrote each line of a file and which line it is, never the
//! line's bytes, aligned across saves by git's histogram diff
//! (`agentfs-align`). It reads the images it is given, which may be files,
//! and starts no process itself: a diff's `git` child is started by the
//! spawner its caller passes. docs/library/core.md explains its use.
