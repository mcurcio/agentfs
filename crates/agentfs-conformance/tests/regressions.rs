//! Regressions found on the engine, kept as recordings the model replays
//! (VERIFYING.md, "A regression found on the engine"). Each fixture is what the
//! engine did with one fix removed, recorded by `phase1-replay --record` through
//! its first call that differs from the model's. Beside each, a `.patch` removes
//! the fix and says, in its header, the base it applies to and how the fixture
//! was recorded.

use agentfs_conformance::{against_library, against_model};
use agentfs_model::calls::{Recorded, Recorder, TreeId, parse};

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

/// The fork's line the engine lost: saved in the fork's `d` (call 11), before
/// the root's landing (call 12).
const SAVED: &[u8] = b"n4\n";

/// What `r` shows in the fork's `d`.
fn forks_d(r: &Recorded) -> &[u8] {
    let shown = r.shown.as_ref().expect("a shown part");
    &shown.trees[&TreeId::new(1)]["d"]
}

fn holds(bytes: &[u8], line: &[u8]) -> bool {
    bytes.windows(line.len()).any(|w| w == line)
}

/// What the model does with the fixture's calls, through the call that differs.
fn models_record() -> Vec<Recorded> {
    let mut recorded = Recorder::default();
    let e = against_model(&fixture(), &mut recorded).unwrap_err();
    assert!(e.starts_with(AT), "{e}");
    assert!(e.contains("T1:d shows"), "{e}");
    recorded.into_trace()
}

/// The model shows otherwise at the fork's sync: the regression, found in the model
/// from the engine's own recording. The model keeps the fork's saved line, which
/// the recording lost.
#[test]
fn a_recorded_regression_fails_in_the_model_at_its_call() {
    let fixture = fixture();
    let model = models_record();
    assert_eq!(model.len(), fixture.len(), "every call made");
    assert_eq!(model[..model.len() - 1], fixture[..fixture.len() - 1]);
    let (at, recorded) = (&model[model.len() - 1], &fixture[fixture.len() - 1]);
    assert!(holds(forks_d(at), SAVED), "{:?}", forks_d(at));
    assert!(!holds(forks_d(recorded), SAVED), "{:?}", forks_d(recorded));
}

/// Today's library makes the fixture's calls as recorded up to that call, and
/// there makes the model's record: the fix holds, and the fixture is not stale.
#[test]
fn todays_library_differs_from_the_regression_only_at_its_call() {
    let fixture = fixture();
    let mut recorded = Recorder::default();
    let e = against_library(&fixture, &mut recorded).unwrap_err();
    assert!(e.starts_with(AT), "{e}");
    let library = recorded.into_trace();
    assert_eq!(library.len(), fixture.len(), "every call made");
    assert_eq!(library[..library.len() - 1], fixture[..fixture.len() - 1]);
    assert_eq!(library.last(), models_record().last());
}
