//! The operating-system leaves both of agentfs's engines share: advisory
//! file locks, atomic files and directory syncs, confined access to a
//! directory tree, the mount table, the shared-filesystem check, listening
//! and local Unix sockets, and the allocator's arenas
//! (docs/library/core.md explains each). Linux system calls only: nothing
//! here knows git, a volume or a tree.
