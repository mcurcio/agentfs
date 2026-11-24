//! The `agentfs` binary's integration tests that can share one process, one
//! module each: one program to link instead of one per file
//! (docs/development.md, "Test programs"). `launcher_mount`, `launcher_host`,
//! `csi_trees` and `crash_daemon` stay programs of their own, because CI runs
//! them by name (`ci/fuse-tests.sh`, `ci/crash-tests.sh`).

mod fork_setup;

mod cli_checkpoint;
mod cli_fork;
mod cli_fork_deliver;
mod cli_history;
mod cli_identity;
mod cli_init;
mod cli_lfs;
mod cli_lock;
mod cli_mount;
mod cli_patch;
mod cli_push;
mod cli_record;
mod cli_restore;
mod cli_serve_git;
mod cli_start;
mod cli_telemetry;
mod cli_verify;
mod deploy_examples;
mod serve_git_edges;
