//! An executable model of the agentfs target architecture's landing rules
//! (docs/target-architecture: system.md binds, domains/*.md refine).
//!
//! The owner, 2026-11-02: "Since the model is effectively “just git” with
//! some overlaid lock/merging rules, I propose a set of functional tests
//! (recorded, runnable in the repo test suite) that mimic the agentfs model
//! via native git operations."
//!
//! [`world`] drives the real `git` program in a temporary directory: main
//! is a branch, each fork a branch with its own worktree, each save a commit
//! naming one writer, each landing a merge built from whole commits. The
//! design's rules (ordering, holding, freezing by write-opens, path bases,
//! transactions as child forks, the history setting) are explicit code on
//! top, each citing the doc section it comes from. [`check`] holds the
//! global invariants, read back from the repository after every step.
//!
//! This crate depends on no other agentfs crate: it models the target
//! design, not today's code. What it leaves out is listed in its tests'
//! module docs.

pub mod check;
pub mod content;
pub mod git;
pub mod world;
