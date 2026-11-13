//! The model's recorded calls against the real system (git-model.md §18, the
//! first row; VERIFYING.md, "The model as the conformance suite").
//!
//! The model runs its scenarios and seeds unchanged and records each call with
//! what each tree showed (`agentfs_model::calls`). [`Library`] and [`Mounted`]
//! make the same calls of the real library and of a real mount. The real run
//! must show the same, and passes invariants of its own, read from git
//! ([`invariants`]). The replay records what each system did into a sink
//! (`agentfs_model::calls::Recorder`), so a run recorded on the engine is a
//! trace the model replays (VERIFYING.md, "A regression found on the engine").
//!
//! The replay does not prove what no call of it makes: shared mappings (the
//! page-cache job), a client call waiting on a take, which a one-step take never
//! meets in the model and the replay order stands for (the mounted P1 and P2
//! cases), a crash inside a step or with unsynced loss
//! (the recording-faults and power-loss jobs), `ENOSPC` and `EIO` (the enospc
//! job).

pub mod engine_names;
pub mod git_history;
pub mod invariants;
pub mod library;
pub mod mounted;

pub use engine_names::EngineNames;
pub use git_history::EngineHistory;
pub use library::Library;
pub use mounted::Mounted;

use agentfs_model::calls::{Recorded, RecordedCall, Sink, TreeCalls, replay};
use agentfs_model::world::{ModelHistory, Policies, TreeNames, World};
use invariants::Invariants;
use std::path::{Path, PathBuf};

/// The trace against `t`, recorded into `sink`, with its history and the real
/// system's invariants read from `repo`.
fn against_real<T: TreeCalls>(
    trace: &[Recorded],
    t: &mut T,
    repo: &Path,
    sink: &mut dyn Sink,
) -> Result<(), String> {
    let mut history = EngineHistory::new(repo, EngineNames::new())?;
    let mut invariants = Invariants::new(repo, EngineNames::new())?;
    replay(trace, t, &mut history, sink, &mut |t, r| {
        invariants.after(t, r)
    })
}

/// The trace against a new real library; `sink` gets what the library did.
pub fn against_library(trace: &[Recorded], sink: &mut dyn Sink) -> Result<(), String> {
    let dir = tempfile::tempdir().map_err(|e| e.to_string())?;
    // A mismatch to read: `PHASE1_REPLAY_KEEP=DIR` leaves the state there.
    let state = std::env::var_os("PHASE1_REPLAY_KEEP")
        .map_or_else(|| dir.path().join("state"), PathBuf::from);
    let mut library = Library::new(&state, EngineNames::new()).map_err(|e| e.to_string())?;
    let repo = library.repo();
    against_real(trace, &mut library, &repo, sink)
}

/// The trace against a new real mount, `bin` a failpoints build of agentfs;
/// `sink` gets what the mount did.
pub fn against_mount(trace: &[Recorded], bin: &Path, sink: &mut dyn Sink) -> Result<(), String> {
    let dir = tempfile::tempdir().map_err(|e| e.to_string())?;
    let with_sandbox = trace
        .iter()
        .any(|r| matches!(r.call, RecordedCall::Fork { .. }));
    let mut mounted = Mounted::new(
        bin,
        dir.path(),
        EngineNames::new(),
        with_sandbox,
        opens_that_append(trace),
    )
    .map_err(|e| e.to_string())?;
    let repo = mounted.repo();
    let result = against_real(trace, &mut mounted, &repo, sink);
    let log = mounted.log_text();
    let finished = mounted.finish().map_err(|e| e.to_string());
    result
        .and(finished)
        .map_err(|e| format!("{e}\nmount log:\n{log}"))
}

/// The same replay driving a fresh model shows its own trace (criterion 6): its
/// trees, collisions and heads. `sink` gets what the model did, so a trace
/// recorded on the engine replays here.
pub fn against_model(trace: &[Recorded], sink: &mut dyn Sink) -> Result<(), String> {
    let mut world = World::new(Policies::default(), TreeNames::new());
    replay(trace, &mut world, &mut ModelHistory, sink, &mut |_, _| {
        Ok(())
    })
}

/// Whether a trace fits "the root and one sandbox" on a real mount: at most one
/// fork, made before the first client call, and no open that both appends and
/// writes at an offset. Linux has no such descriptor: `pwrite` on an `O_APPEND`
/// file appends, and the view reads `O_APPEND` when the file opens.
pub fn mountable(trace: &[Recorded]) -> bool {
    let (mut forks, mut called) = (0, false);
    for r in trace {
        match r.call {
            RecordedCall::Fork { .. } => {
                forks += 1;
                if called || forks > 1 {
                    return false;
                }
            }
            RecordedCall::Take { .. } => {}
            _ => called = true,
        }
    }
    !one_open_mixes_append_and_offset(trace)
}

fn one_open_mixes_append_and_offset(trace: &[Recorded]) -> bool {
    let appends = writes_of(trace, |c| matches!(c, RecordedCall::Append { .. }));
    let offsets = writes_of(trace, |c| matches!(c, RecordedCall::Pwrite { .. }));
    appends.iter().any(|h| offsets.contains(h))
}

fn writes_of(trace: &[Recorded], kind: fn(&RecordedCall) -> bool) -> Vec<u64> {
    trace
        .iter()
        .filter(|r| kind(&r.call))
        .filter_map(|r| match r.call {
            RecordedCall::Append { h, .. } | RecordedCall::Pwrite { h, .. } => Some(h),
            _ => None,
        })
        .collect()
}

/// For each open of the trace in order, whether it appends: what a real open needs
/// to know, since `O_APPEND` is set when a file opens.
pub fn opens_that_append(trace: &[Recorded]) -> Vec<bool> {
    let appends = writes_of(trace, |c| matches!(c, RecordedCall::Append { .. }));
    trace
        .iter()
        .filter_map(|r| match r.call {
            RecordedCall::OpenWrite { h, .. }
            | RecordedCall::OpenTrunc { h, .. }
            | RecordedCall::Create { h, .. } => Some(appends.contains(&h)),
            _ => None,
        })
        .collect()
}
