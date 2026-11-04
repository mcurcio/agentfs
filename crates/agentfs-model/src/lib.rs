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
//! and each fork are branches, each save a commit naming one writer and
//! holding the file exactly as the program's write calls left it. A
//! collision between a fork and its parent is decided only at the parent's
//! landing queue, by git's own three-way merge of the landing commit's
//! change, the parent's side winning each conflicting hunk. A sync rebases
//! the fork's commits the parent has not taken onto the parent's head
//! (`git pull --rebase`), the parent's side winning a hunk there too; it
//! records nothing. Every losing commit keeps a collision ref, every
//! pre-rebase head a ref, and a separate Replication step pushes them with
//! main to a bare shared remote. A sync or landing into a tree first waits
//! for that tree's write-opens of the paths it changes to close, then
//! holds them while it writes. [`check`] holds the invariants, read back
//! from the repository after every step through [`content::Oracle`], the
//! checks' own record of who wrote what over what.
//!
//! This crate depends on no other agentfs crate: it models the rules, not
//! today's code. What it leaves out is listed in `MODEL.md`.

pub mod check;
pub mod content;
pub mod git;
pub mod world;
