//! The landing rules of agentfs's tree views, pure: every line, name,
//! metadata and directory rule of a fork and its upstream
//! (docs/target-architecture/git-model.md §7, §8) as plans the tree engine
//! carries out (`land` gives a decision, `sync` a fork's snapshot, `touches`
//! a step's lock keys); the queue's order and locks (§6); and the values
//! they read: event numbers, line versions, file identities and name
//! histories. It reads history only through its `Landed` port, writes
//! nothing and does no I/O: outside its tests the crate is `no_std`, so its
//! code cannot name the file system, a process or the network. The tree
//! engine is its one user; it is not part of the library's consumer API.
//!
//! See docs/library/referee.md.
#![cfg_attr(not(test), no_std)]
