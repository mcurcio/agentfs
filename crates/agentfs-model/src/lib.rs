//! An executable model of the owner's landing rules (2026-11-02 and
//! 2026-11-03), in plain words in `MODEL.md` (the rules table names each
//! rule's tests).
//!
//! The owner, 2026-11-02: "Since the model is effectively “just git” with
//! some overlaid lock/merging rules, I propose a set of functional tests
//! (recorded, runnable in the repo test suite) that mimic the agentfs model
//! via native git operations." And, the same day: "I generally like the
//! metaphor of a typical Linux filesystem. I want to try that out."
//!
//! [`world`] drives the real `git` program in a temporary repository: main
//! and each fork are branches, each save a commit naming one writer. A
//! collision between a fork and its upstream is decided only at the upstream's
//! landing queue, per line: a line written over the upstream's replaces it,
//! and of two concurrent lines the upstream's wins; a change (a save, the
//! whole file at one close, or a transaction) with a line that lost loses
//! whole. A sync decides nothing:
//! the fork takes the upstream's decided version, keeping only its own lines
//! still on their way to that queue. Every losing commit keeps a collision
//! ref and a record in its take's result commit, which a separate
//! Replication step pushes with main to a bare shared remote. A file is a
//! byte buffer and a program's calls are POSIX byte calls ([`recording::Write`]):
//! a save is those calls applied at close to its tree's head, line
//! identity derived from the bytes then: an append is exact (finishing a
//! partial last line, then new lines), every other byte write is aligned
//! with the head's lines once by git's histogram diff and recorded, so
//! each line keeps an identity through later edits around it; a file's
//! name and existence are one more line (unlink, rename); a
//! sync or landing into a tree first waits for that tree's write-opens of
//! the files it will overwrite to close, then holds them while it writes;
//! an entry waits only for its own files and earlier waiting entries it
//! shares a file or a source with, and a waiting take comes before new
//! writers of its files. Files are created; an open holds an object, so an
//! editor's atomic save (a rewrite of the replaced file) never receives an
//! old writer's calls. Every write call is journaled, and a crash saves
//! each open from the journal once.
//! [`check`] holds the invariants, read back from the repository after
//! every step.
//!
//! This crate depends on no other agentfs crate: it models the rules, not
//! today's code. What it leaves out is listed in `MODEL.md`.

pub mod calls;
pub mod check;
pub mod content;
pub mod digest;
pub mod git;
pub mod program;
pub mod queue;
pub mod recording;
mod referee;
pub mod world;
