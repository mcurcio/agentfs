//! The operating-system leaves agentfs uses outside the recording engine
//! too: advisory file locks, atomic files and directory syncs, confined
//! access to a directory tree, the mount table, the shared-filesystem check,
//! listening Unix sockets, and the allocator's arenas (docs/library/core.md
//! explains each). Linux system calls only: nothing here knows git, a volume
//! or a tree.

pub mod allocator;
pub mod confined;
pub mod file_lock;
pub mod fsutil;
pub mod listening_socket;
pub mod mountinfo;
pub mod shared_fs;
