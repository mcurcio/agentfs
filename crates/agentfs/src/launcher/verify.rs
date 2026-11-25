//! `agentfs verify` (docs/trees.md, "When history cannot check a save"):
//! takes the volume's lock and opens only its `history.git`, so a release of
//! another state format can run it, then checks every save history flagged
//! unverified, whose flag still stands, with this binary
//! ([`agentfs_engine::tree_actor::verify_flagged`]). One line per flag:
//!
//! ```text
//! <step> <tree> <commit> verified|mismatch-confirmed|same-release|no-evidence <path as a JSON string>
//! - - <commit> unreadable -
//! ```
//!
//! the second for a commit whose verdict note does not parse.
//!
//! The exit status, this page its one home: 0 when no flag stands (none
//! flagged included); 2 when one does (unresolved, mismatch-confirmed,
//! unreadable or without evidence); 1 on an error; 75 while another process
//! holds the volume.

use std::path::Path;
use std::process::ExitCode;

use agentfs_engine::tree_actor::{self, Outcome};
use anyhow::{Context, Result};

use super::busy;
use crate::pod_args::PodArgs;

/// The exit status while a flag stands.
const EXIT_STANDING: u8 = 2;

/// `agentfs verify`'s flags.
#[derive(clap::Args)]
pub struct VerifyArgs {
    #[command(flatten)]
    pub pod: PodArgs,
}

/// A check's word in a line.
fn word(outcome: Outcome) -> &'static str {
    match outcome {
        Outcome::Wrote(verdict) => verdict.as_str(),
        Outcome::SameRelease => "same-release",
        Outcome::NoEvidence => "no-evidence",
    }
}

/// `agentfs verify`: every standing flag checked by this release, its
/// verdict appended to its note, one line each; the pod flags name the
/// verdicts' committer.
///
/// # Errors
///
/// A failed read of the store, or a verdict's write.
pub fn verify(state: &Path, args: VerifyArgs) -> Result<ExitCode> {
    let at = args.pod.context()?;
    let report = match tree_actor::verify_flagged(state, &at, tree_actor::release()) {
        Ok(report) => report,
        Err(tree_actor::Error::Busy) => return Ok(busy()),
        Err(e) => return Err(e).context("verifying the flagged saves"),
    };
    for checked in &report.checked {
        println!(
            "{} {} {} {} {}",
            checked.step.get(),
            checked.tree,
            checked.commit,
            word(checked.outcome),
            serde_json::to_string(&checked.path)?
        );
    }
    for commit in &report.unreadable {
        println!("- - {commit} unreadable -");
    }
    Ok(if report.standing == 0 {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(EXIT_STANDING)
    })
}
