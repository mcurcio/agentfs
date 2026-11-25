//! Line maps for tree landing (docs/target-architecture/git-model.md §7):
//! which version wrote each line of a file and which line it is, never the
//! line's bytes, aligned across saves by git's histogram diff
//! (`agentfs-align`). Pure apart from the diff, whose `git` child the
//! caller starts. docs/library/core.md explains its use.
