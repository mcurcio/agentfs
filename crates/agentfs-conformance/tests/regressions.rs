//! Regressions found on the engine, kept as recordings the model replays
//! (VERIFYING.md, "A regression found on the engine"). Each fixture is what the
//! engine did with one fix removed, recorded by `phase1-replay --record` through
//! its first call that differs from the model's.

use agentfs_conformance::{against_library, against_model};
use agentfs_model::calls::{Recorded, Recorder, parse};

/// The library without the shows-the-same guard of e9ab747 (6049b1b's regression,
/// finding 51; phase-1 run 42547765643, seeds 123, 172 and 198), on the scenario
/// `finding_51_a_sync_that_cannot_show_the_upstreams_commit_merges`: the fork's sync
/// fast-forwarded when it could not show the upstream's commit, and lost the fork's
/// saved line.
const FINDING_51: &str = include_str!("regressions/finding-51-sync-fast-forward.trace");

/// Where the recording differs from the model: the fork's sync.
const AT: &str = "call 15 (take t1 taken): ";

fn fixture() -> Vec<Recorded> {
    parse(FINDING_51).unwrap()
}

/// The model shows otherwise at the fork's sync: the regression, found in the model
/// from the engine's own recording.
#[test]
fn a_recorded_regression_fails_in_the_model_at_its_call() {
    let mut recorded = Recorder::default();
    let e = against_model(&fixture(), &mut recorded).unwrap_err();
    assert!(e.starts_with(AT), "{e}");
    assert!(e.contains("T1:d shows"), "{e}");
    assert_eq!(recorded.trace().len(), fixture().len(), "every call made");
}

/// Today's library makes the fixture's calls as recorded up to that call, and
/// differs there: the fix holds, and the fixture is not stale.
#[test]
fn todays_library_differs_from_the_regression_only_at_its_call() {
    let fixture = fixture();
    let mut recorded = Recorder::default();
    let e = against_library(&fixture, &mut recorded).unwrap_err();
    assert!(e.starts_with(AT), "{e}");
    let (before, at) = recorded.trace().split_at(fixture.len() - 1);
    assert_eq!(before, &fixture[..fixture.len() - 1]);
    assert_ne!(at, &fixture[fixture.len() - 1..]);
}
