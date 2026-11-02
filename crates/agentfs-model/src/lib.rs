//! An executable model of the owner's landing rules of 2026-11-02, in
//! plain words in `MODEL.md` (the rules table names each rule's tests).
//!
//! The owner, 2026-11-02: "Since the model is effectively “just git” with
//! some overlaid lock/merging rules, I propose a set of functional tests
//! (recorded, runnable in the repo test suite) that mimic the agentfs model
//! via native git operations." And, the same day: "I generally like the
//! metaphor of a typical Linux filesystem. I want to try that out."
//!
//! [`world`] drives the real `git` program in a temporary repository: main
//! and each fork are branches, each save a commit naming one writer, each
//! landing or sync a fast-forward or a line merge in which the newer line
//! wins by git ancestry and, of two concurrent lines, the incoming one.
//! [`check`] holds the invariants, read back from the repository after
//! every step.
//!
//! This crate depends on no other agentfs crate: it models the rules, not
//! today's code. What it leaves out is listed in `MODEL.md`.

pub mod check;
pub mod content;
pub mod git;
pub mod world;
