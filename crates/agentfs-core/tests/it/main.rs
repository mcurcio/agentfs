//! agentfs-core's integration tests that can share one process, one module
//! each: one program to link instead of one per file (docs/development.md,
//! "Test programs"). A test file stays a program of its own when it needs the
//! process to itself: a process-wide counter or allocator, or a program CI
//! runs by name.

mod gitmodules_git;
mod lfs_attributes_git;
mod patch_fetch;
