//! `agentfs verify` (docs/recording.md, "When history cannot check a save"):
//! opens the volume as `fork end` does (exit 75 while a mount or host holds
//! it), checks every save history flagged unverified again with this binary,
//! and prints one line per step, `<step> <tree> <path> <commit> verified` or
//! `... mismatch-confirmed`. Exit 0 when nothing is left disagreeing (none
//! flagged included), 1 when any step is mismatch-confirmed.

use std::path::Path;
use std::process::ExitCode;

use agentfs_contracts::caps::Minter;
use anyhow::Result;

use crate::pod_args::PodArgs;

/// `agentfs verify`'s flags.
#[derive(clap::Args)]
#[allow(dead_code, reason = "frozen interface of agentfs-ln6.11")]
pub struct VerifyArgs {
    #[command(flatten)]
    pub pod: PodArgs,
}

/// `agentfs verify`: every unresolved step checked and its verdict appended
/// to its note, through [`agentfs_core::tree_actor::TreeActor::verify`].
///
/// # Errors
///
/// A failed recovery, read or note write.
#[allow(dead_code, reason = "frozen interface of agentfs-ln6.11")]
pub fn verify(state: &Path, args: VerifyArgs, minter: Minter) -> Result<ExitCode> {
    let _ = (state, args, minter);
    todo!("agentfs-ln6.11")
}
