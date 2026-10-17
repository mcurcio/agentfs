//! `agentfs start`: a started volume's answer to the start-branch alarm
//! (design §11.3, F8 D2; docs/runbooks/start-branch-moved.md).
//!
//! `start accept --tip OID` saves OID, a commit of the branch this volume
//! started from, as `refs/agentfs/<volume>/accepted/<OID>`
//! ([`agentfs_core::fork::start::accept`]): the next push to the start
//! remote pushes it create-only with the branch, and the alarm stops for
//! every commit OID reaches. It takes no state-area lock (one ref and its
//! objects, both atomic in git), so it runs beside a running history
//! service, in the CSI node plugin too.

use std::process::ExitCode;

use anyhow::{Context as _, Result};
use clap::Subcommand;

use agentfs_core::fork::identity::full_commit;
use agentfs_core::fork::start;
use agentfs_core::volume::StateDir;

#[derive(Subcommand)]
pub enum StartCommand {
    /// Accept TIP, a commit of the branch this volume started from: save it
    /// as refs/agentfs/<volume>/accepted/<TIP> (created once, never moved),
    /// fetching it from the start remote. The next push to that remote
    /// pushes it and no longer raises the start-branch alarm for commits
    /// TIP reaches. Exit 0 accepted (or already), 1 refused.
    Accept {
        /// The full commit id the alarm names (`start branch moved:
        /// <pinned>..<tip>` in `agentfs status`).
        #[arg(long, value_name = "OID")]
        tip: String,
    },
}

pub fn run(command: StartCommand, state: &StateDir) -> Result<ExitCode> {
    match command {
        StartCommand::Accept { tip } => {
            let oid = full_commit(&tip).with_context(|| {
                format!("--tip {tip:?} is not a full commit id (40 lower-case hex digits)")
            })?;
            let name = start::accept(state, oid)
                .with_context(|| format!("accepting {tip} in {}", state.root().display()))?;
            println!(
                "accepted {tip} as {name}: the next push to the start remote pushes it, and \
                 the start-branch alarm no longer names the commits it reaches"
            );
            Ok(ExitCode::SUCCESS)
        }
    }
}
