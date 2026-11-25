//! The landing rules of agentfs's tree views, pure: every line, name,
//! metadata and directory rule of a fork and its upstream
//! (docs/target-architecture/git-model.md §7, §8) as plans the tree engine
//! carries out (`land` gives a decision, `sync` a fork's snapshot, `touches`
//! a step's lock keys); the queue's order and locks (§6) and each fork's
//! upstream; and the values they read. It reads history only through its
//! `Landed` port, writes nothing and does no I/O: outside its tests the crate
//! is `no_std`, so its code cannot name the file system, a process or the
//! network.
//!
//! See docs/library/referee.md.
#![cfg_attr(not(test), no_std)]
