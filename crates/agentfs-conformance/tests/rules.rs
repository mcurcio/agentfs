//! The model's scenarios of rules 13, 14 and 15 (directories, metadata and links),
//! as the model recorded them, replayed on a fresh model and on the real library:
//! each tree's files, directories, kinds and metadata, and each collision with its
//! cause, must be what the model showed after every call, and the engine's own
//! invariants (D1 among them) hold. Each fixture in `tests/rules/` is the model's
//! trace of the scenario of the same name in `crates/agentfs-model/tests/scenarios.rs`
//! (VERIFYING.md, "The model as the conformance suite"). `ci/e2e-tree-replay.sh
//! crates/agentfs-conformance/tests/rules 5 0` replays the five a mount can make
//! (`mountable`) on real mounts.
//!
//! Re-record after a change to one of these scenarios or to the trace format:
//! `AGENTFS_MODEL_TRACE_DIR=$T cargo test -p agentfs-model --test scenarios -- r13_ r14_
//! r15_`, then copy `$T/r1[345]_*.trace` here.

use agentfs_conformance::{against_library, against_model};
use agentfs_model::calls::{Recorder, parse};
use std::path::Path;

/// One trace per scenario: rule 13's five, rule 14's nine, rule 15's three.
const SCENARIOS: usize = 17;

#[test]
fn the_rules_scenarios_replay_on_the_model_and_the_library() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/rules");
    let mut names: Vec<_> = std::fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|x| x == "trace"))
        .collect();
    names.sort();
    assert_eq!(names.len(), SCENARIOS, "{names:?}");
    for path in names {
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        let trace = parse(&std::fs::read_to_string(&path).unwrap()).unwrap();
        let mut on_model = Recorder::default();
        against_model(&trace, &mut on_model).unwrap_or_else(|e| panic!("{name}: model: {e}"));
        assert_eq!(on_model.trace(), trace.as_slice(), "{name}: the model's");
        let mut on_library = Recorder::default();
        against_library(&trace, &mut on_library).unwrap_or_else(|e| panic!("{name}: library: {e}"));
        assert_eq!(
            on_library.trace(),
            trace.as_slice(),
            "{name}: the library's"
        );
    }
}
