//! agentfs-core's integration tests that can share one process, one module
//! each: one program to link instead of one per file (docs/development.md,
//! "Test programs"). A test file stays a program of its own when it needs the
//! process to itself: a process-wide counter or allocator, or a program CI
//! runs by name.

/// The libtest name of test `$name` in the calling module, as `--exact`
/// matches it: a test that runs another test of this program in a child
/// process (to kill it, or to give it its own limits) names it this way.
macro_rules! test_name {
    ($name:expr) => {
        $crate::qualified(module_path!(), $name)
    };
}

/// `name` qualified by `module` (a `module_path!()`) without the program's
/// own first segment.
fn qualified(module: &str, name: &str) -> String {
    match module.split_once("::") {
        Some((_, path)) => format!("{path}::{name}"),
        None => name.to_owned(),
    }
}

#[path = "../support/trees.rs"]
mod trees;

mod gitmodules_git;
mod lfs_attributes_git;
mod patch_fetch;
mod state_area;
mod tree_actor;
mod tree_clients;
mod tree_dir_landing;
mod tree_dirs;
mod tree_git;
mod tree_line_rules;
mod tree_overlay_limit;
mod tree_pages;
mod tree_pending_saves;
mod tree_reconcile;
mod tree_replace;
mod tree_rules;
mod tree_store;
mod tree_untrack;
mod tree_verify;
