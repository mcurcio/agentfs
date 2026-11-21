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

/// The library before `pieces` placed each line from a source that holds it
/// (seeds 306 and 414 of `random_interleavings_rounds_phase1_scope`, found by
/// the full sweep): the head and the taken tree held runs of the same versions
/// with different extents, `pieces` looked only in the run that sorted last, a
/// shorter one, and the take failed (EIO) with "a live line no source holds".
/// Each fixture is the library's recording of one seed; `pieces-shorter-run.patch`
/// removes the fix for both.
const PIECES_SHORTER_RUN: [(&str, &str); 2] = [
    (
        include_str!("regressions/pieces-shorter-run-306.trace"),
        "call 38 (take t0 taken): ",
    ),
    (
        include_str!("regressions/pieces-shorter-run-414.trace"),
        "call 127 (take t0 taken): ",
    ),
];

/// The model makes the take the recording failed: it agrees with every record
/// before it and differs there.
#[test]
fn a_take_that_could_not_place_a_line_fails_in_the_model_at_its_call() {
    for (trace, at) in PIECES_SHORTER_RUN {
        let fixture = parse(trace).unwrap();
        let mut recorded = Recorder::default();
        let e = against_model(&fixture, &mut recorded).unwrap_err();
        assert!(e.starts_with(at), "{e}");
        let model = recorded.into_trace();
        assert_eq!(model.len(), fixture.len(), "every call made");
        assert_eq!(model[..model.len() - 1], fixture[..fixture.len() - 1]);
        let failed = fixture[fixture.len() - 1].failed;
        assert_eq!(failed.and_then(|f| f.errno), Some(libc::EIO), "{at}");
        assert_eq!(model[model.len() - 1].failed, None, "{at}");
    }
}

/// Today's library makes each fixture's calls as recorded up to the take, and
/// there makes the model's record.
#[test]
fn todays_library_places_every_line_of_the_take() {
    for (trace, at) in PIECES_SHORTER_RUN {
        let fixture = parse(trace).unwrap();
        let mut recorded = Recorder::default();
        let e = against_library(&fixture, &mut recorded).unwrap_err();
        assert!(e.starts_with(at), "{e}");
        let library = recorded.into_trace();
        let mut model = Recorder::default();
        against_model(&fixture, &mut model).unwrap_err();
        assert_eq!(library.len(), fixture.len(), "every call made");
        assert_eq!(library[..library.len() - 1], fixture[..fixture.len() - 1]);
        assert_eq!(library.last(), model.into_trace().last(), "{at}");
    }
}
