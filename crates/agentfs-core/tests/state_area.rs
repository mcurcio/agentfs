//! `ensure` on a real state area (docs/trees.md, "Volume identity"): one id
//! however often it runs, and every refusal made before any write. The
//! lifecycle over it (docs/trees.md, "Volume lifecycle"): each move one
//! journal step with its row, and a drain that waits for the queues.
#![cfg_attr(
    not(feature = "failpoints"),
    allow(unused_imports, dead_code, reason = "some tests need failpoints")
)]

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use agentfs_contracts::caps::{LaunchCap, LaunchScope, Minter};
use agentfs_contracts::clients::{AttachedLookup, ClientSet};
use agentfs_contracts::name::{LauncherId, VolumeName};
use agentfs_contracts::pod::PodContext;
use agentfs_contracts::ports::VolumeStore;
use agentfs_contracts::ports::{Event, EventSink};
use agentfs_contracts::volume::{
    Blocker, Disposition, Drain, EnsureOutcome, Owner, ReleaseVerdict, SpecField, StartPoint,
    Volume, VolumeError, VolumeEvent, VolumeSpec, VolumeState,
};
use agentfs_core::state_area::TreeStateArea;
use agentfs_core::store::TreeStore;
use agentfs_core::tree_actor::{Error, Recording, TreeActor};
use agentfs_volume::VolumeService;
use std::sync::Arc;

#[path = "support/trees.rs"]
mod trees;
use trees::ATTRS;

struct NoClients;

impl AttachedLookup for NoClients {
    fn attached_any(&self) -> ClientSet {
        ClientSet::new()
    }
}

/// The events published, shared by every clone.
#[derive(Clone, Default)]
struct Events(std::rc::Rc<std::cell::RefCell<Vec<Event>>>);

impl EventSink for Events {
    fn publish(&mut self, event: Event) {
        self.0.borrow_mut().push(event);
    }
}

fn volume(root: &Path) -> VolumeService<TreeStateArea, NoClients, Events> {
    volume_with(root, Events::default())
}

fn volume_with(root: &Path, events: Events) -> VolumeService<TreeStateArea, NoClients, Events> {
    VolumeService::new(TreeStateArea::at(root), NoClients, events)
}

fn spec(name: &str) -> VolumeSpec {
    VolumeSpec {
        name: VolumeName::parse(name).unwrap(),
        start: StartPoint::Empty,
        root_owner: None,
    }
}

#[allow(
    clippy::disallowed_methods,
    reason = "a test is its own composition root"
)]
fn root_cap() -> LaunchCap {
    LaunchCap::mint(
        &Minter::new(),
        LauncherId::parse("cli").unwrap(),
        LaunchScope::Root,
    )
}

/// Every path under `root` with its bytes (a directory's are empty), and each
/// one's permission bits.
fn contents(root: &Path) -> BTreeMap<PathBuf, (Vec<u8>, u32)> {
    use std::os::unix::fs::PermissionsExt;
    let mut all = BTreeMap::new();
    let mut pending = vec![root.to_owned()];
    while let Some(dir) = pending.pop() {
        for entry in fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            let meta = fs::symlink_metadata(&path).unwrap();
            let bytes = if meta.is_dir() {
                pending.push(path.clone());
                Vec::new()
            } else {
                fs::read(&path).unwrap()
            };
            all.insert(path, (bytes, meta.permissions().mode()));
        }
    }
    all
}

#[test]
fn ensure_three_times_gives_one_id() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("state");
    let at = PodContext::default();
    let first = volume(&root).ensure(spec("t"), &at, &root_cap()).unwrap();
    assert_eq!(first.outcome, EnsureOutcome::Created);
    for _ in 0..2 {
        let again = volume(&root).ensure(spec("t"), &at, &root_cap()).unwrap();
        assert_eq!((again.id, again.outcome), (first.id, EnsureOutcome::Found));
    }
    let actor = TreeActor::open(&root, &at).unwrap();
    assert_eq!(actor.identity().id, first.id);
    assert_eq!(actor.identity().name.as_str(), "t");
    assert_eq!(
        fs::read_to_string(root.join("volume")).unwrap(),
        format!("{{\"name\":\"t\",\"id\":\"{}\"}}\n", first.id.0)
    );
}

/// The Root commit's record of the root directory.
fn root_record(root: &Path) -> String {
    let first = git(root, &["rev-list", "--max-parents=0", "--all"]);
    let commit = first.lines().next().expect("a Root commit");
    git(root, &["cat-file", "blob", &format!("{commit}:dirs/root")])
}

/// The root owner is what the Root commit records the root directory with;
/// without one, the creating process's ids. A found volume keeps its own.
#[test]
fn the_root_owner_is_recorded_at_create() {
    let tmp = tempfile::tempdir().unwrap();
    let at = PodContext::default();
    let owned = tmp.path().join("owned");
    let spec_owned = VolumeSpec {
        root_owner: Some(Owner {
            uid: 4242,
            gid: 4343,
        }),
        ..spec("t")
    };
    volume(&owned).ensure(spec_owned, &at, &root_cap()).unwrap();
    let record = root_record(&owned);
    assert!(record.contains(r#""uid":4242,"gid":4343"#), "{record}");
    let found = volume(&owned).ensure(spec("t"), &at, &root_cap()).unwrap();
    assert_eq!(found.outcome, EnsureOutcome::Found);
    assert_eq!(root_record(&owned), record);

    let plain = tmp.path().join("plain");
    volume(&plain).ensure(spec("t"), &at, &root_cap()).unwrap();
    // SAFETY: neither call has preconditions or fails.
    let ids = unsafe { (libc::geteuid(), libc::getegid()) };
    let record = root_record(&plain);
    assert!(
        record.contains(&format!(r#""uid":{},"gid":{}"#, ids.0, ids.1)),
        "{record}"
    );
}

#[test]
fn another_name_leaves_the_state_area_byte_identical() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("state");
    let at = PodContext::default();
    volume(&root).ensure(spec("t"), &at, &root_cap()).unwrap();
    let before = contents(&root);
    assert_eq!(
        volume(&root).ensure(spec("u"), &at, &root_cap()),
        Err(VolumeError::SpecMismatch(SpecField::Name))
    );
    assert_eq!(contents(&root), before);
}

#[test]
fn format_3_is_refused_unchanged() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("state");
    let at = PodContext::default();
    volume(&root).ensure(spec("t"), &at, &root_cap()).unwrap();
    fs::write(root.join("format"), "agentfs-trees-3\n").unwrap();
    let before = contents(&root);
    assert_eq!(
        volume(&root).ensure(spec("t"), &at, &root_cap()),
        Err(VolumeError::OtherFormat)
    );
    assert!(matches!(TreeActor::open(&root, &at), Err(Error::Format)));
    assert_eq!(contents(&root), before);
}

#[test]
fn a_create_cut_short_is_created_again() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("state");
    let at = PodContext::default();
    let first = volume(&root).ensure(spec("t"), &at, &root_cap()).unwrap();
    // As a crash before `format` leaves it.
    fs::remove_file(root.join("format")).unwrap();
    fs::write(root.join("initializing"), b"").unwrap();
    assert!(matches!(
        TreeActor::open(&root, &at),
        Err(Error::NotCreated)
    ));
    let again = volume(&root).ensure(spec("t"), &at, &root_cap()).unwrap();
    assert_eq!(again.outcome, EnsureOutcome::Created);
    assert_ne!(again.id, first.id);
    assert!(!root.join("initializing").exists());
}

#[test]
fn open_never_creates() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("state");
    let at = PodContext::default();
    assert!(matches!(
        TreeActor::open(&root, &at),
        Err(Error::NotCreated)
    ));
    assert!(!root.exists());
    fs::create_dir(&root).unwrap();
    assert!(matches!(
        TreeActor::open(&root, &at),
        Err(Error::NotCreated)
    ));
    assert_eq!(fs::read_dir(&root).unwrap().count(), 0);
}

/// A rule the volume does not build refuses `stage` with the rule itself,
/// file and line, never as a durability failure.
#[test]
fn a_rule_not_built_refuses_stage_naming_it() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("state");
    let at = PodContext::default();
    volume(&root).ensure(spec("t"), &at, &root_cap()).unwrap();
    let info = root.join("history.git/info");
    fs::create_dir_all(&info).unwrap();
    fs::write(info.join("attributes"), "*.db agentfs.group=x\n").unwrap();
    assert_eq!(
        volume(&root).stage(&at, &root_cap()),
        Err(VolumeError::RulesRefused(
            "info/attributes line 1: agentfs.group=x is not built for tree volumes".into()
        ))
    );
}

// Slice S6: the lifecycle.

const WORKER: &str = "AGENTFS_STATE_AREA_WORKER";

/// `git` in the state area's store: its stdout, trimmed.
fn git(root: &Path, args: &[&str]) -> String {
    let output = std::process::Command::new("git")
        .env("GIT_DIR", root.join("history.git"))
        .args(args)
        .output()
        .unwrap();
    String::from_utf8(output.stdout)
        .unwrap()
        .trim_end()
        .to_owned()
}

/// The volume's row as the reopened journal leaves it.
fn row(root: &Path) -> Option<VolumeState> {
    let recording = Recording::open(root, &PodContext::default()).unwrap();
    TreeStore::new(recording)
        .load()
        .unwrap()
        .map(|row| row.value)
}

/// A created volume, then its stage: whichever failpoint the spec names
/// kills it on the way.
#[test]
fn stage_worker() {
    let Some(dir) = std::env::var_os(WORKER) else {
        return;
    };
    let root = Path::new(&dir);
    let at = PodContext::default();
    volume(root).ensure(spec("t"), &at, &root_cap()).unwrap();
    let mut volume = volume(root);
    volume.stage(&at, &root_cap()).unwrap();
    volume.area().recording().unwrap().settle().unwrap();
    unreachable!("the failpoint kills this process");
}

/// Runs `stage_worker` in a child process with the failpoint `spec`, which
/// kills it; returns once the state area's lease is free.
fn run_killed(root: &Path, spec: &str) {
    use std::os::unix::process::ExitStatusExt;
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "stage_worker", "--nocapture"])
        .env(WORKER, root)
        .env(agentfs_core::failpoints::ENV_VAR, spec)
        .output()
        .unwrap();
    assert_eq!(
        output.status.signal(),
        Some(libc::SIGKILL),
        "{spec} did not kill the worker: {output:?}"
    );
    trees::wait_for_lease(root);
}

/// A lifecycle move and its row are one record: killed before its step,
/// the volume has no row and no event; killed once its bytes are in the
/// journal, it is staged, though nothing had built its event commit.
#[cfg(feature = "failpoints")]
#[test]
fn a_lifecycle_move_and_its_row_are_one_record() {
    let tmp = tempfile::tempdir().unwrap();
    let before = tmp.path().join("before");
    run_killed(&before, "tree.before_step=kill@1");
    assert_eq!(row(&before), None);
    assert_eq!(git(&before, &["for-each-ref", "refs/agentfs/volume"]), "");

    let after = tmp.path().join("after");
    run_killed(&after, "tree.wal_after_payload=kill@1");
    assert_eq!(git(&after, &["for-each-ref", "refs/agentfs/volume"]), "");
    assert_eq!(row(&after), Some(VolumeState::Staged));
    assert_eq!(
        git(&after, &["log", "--format=%s", "refs/agentfs/volume"]),
        "VolumeStaged"
    );
}

/// The lifecycle on record: stage, unstage and release are three event
/// commits on `refs/agentfs/volume`, each with its row, its principal and
/// the history service's pod context.
#[test]
fn the_lifecycle_is_a_chain_of_events() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("state");
    let at = PodContext {
        pod: Some(agentfs_contracts::name::Label::parse("p-1").unwrap()),
        ..PodContext::default()
    };
    volume(&root).ensure(spec("t"), &at, &root_cap()).unwrap();
    let events = Events::default();
    let mut volume = volume_with(&root, events.clone());
    assert_eq!(volume.stage(&at, &root_cap()), Ok(VolumeState::Staged));
    assert_eq!(
        volume.unstage(&root_cap()),
        Ok(VolumeState::Released(Disposition::Keep))
    );
    assert_eq!(
        *events.0.borrow(),
        [
            Event::Volume(VolumeEvent::VolumeStaged),
            Event::Volume(VolumeEvent::VolumeDraining(Drain::Stop)),
            Event::Volume(VolumeEvent::VolumeReleased(Disposition::Keep)),
        ]
    );
    volume.area().recording().unwrap().settle().unwrap();
    assert_eq!(
        git(
            &root,
            &[
                "log",
                "--format=%s|%(trailers:key=Agentfs-By,valueonly,separator=)|%(trailers:key=Agentfs-Pod,valueonly,separator=)",
                "refs/agentfs/volume"
            ]
        ),
        "VolumeReleased Keep|agentfs|p-1\nVolumeDraining Stop|launcher cli|p-1\nVolumeStaged|launcher cli|p-1"
    );
    let shown = git(&root, &["show", "refs/agentfs/volume:row.json"]);
    assert!(
        shown.contains("\"value\":{\"Released\":\"Keep\"}"),
        "{shown}"
    );
    drop(volume);
    // A new run stages a released volume again.
    let mut volume = volume_with(&root, Events::default());
    assert_eq!(volume.stage(&at, &root_cap()), Ok(VolumeState::Staged));
}

/// Principle 5 and the drain's wait: with the builder failing, a queued
/// landing cannot be taken, so the drain waits (`Draining`, nothing
/// released) while recording goes on; cleared, the landing is taken and
/// the re-judgement releases.
#[cfg(feature = "failpoints")]
#[test]
fn a_drain_goes_on_while_the_builder_fails() {
    use agentfs_contracts::ports::ForkStore;
    use agentfs_contracts::trees::{ForkSettings, Landing};
    use agentfs_core::tree_actor::{Progress, TreeId};

    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("state");
    let at = PodContext::default();
    volume(&root).ensure(spec("t"), &at, &root_cap()).unwrap();
    let held = agentfs_core::failpoints::configure("tree.build_before_batch=error@*").unwrap();
    let events = Events::default();
    let mut volume = volume_with(&root, events.clone());
    assert_eq!(volume.stage(&at, &root_cap()), Ok(VolumeState::Staged));
    let recording = volume.area().recording().unwrap();
    let mut store = TreeStore::new(Arc::clone(&recording));
    let fork = ForkStore::create(
        &mut store,
        TreeId::Root,
        None,
        ForkSettings {
            landing: Landing::Automatic,
        },
        &LauncherId::parse("cli").unwrap(),
        &at,
    )
    .unwrap()
    .value
    .fork;
    {
        let mut actor = recording.actor().unwrap();
        let h = actor
            .write_open(fork.into(), "f", &trees::writer("w"), Some(ATTRS))
            .unwrap();
        actor.write(h, 0, b"landing\n", false).unwrap();
        actor.close(h).unwrap();
    }
    assert_eq!(recording.progress().unwrap(), Progress::Waiting);
    assert_eq!(
        volume.unstage(&root_cap()),
        Ok(VolumeState::Draining(Drain::Stop))
    );
    assert_eq!(
        volume.release_verdict(),
        ReleaseVerdict::Wait(vec![Blocker::QueuesNotEmpty(vec![TreeId::Root])])
    );
    assert_eq!(volume.rejudge(), Ok(VolumeState::Draining(Drain::Stop)));
    assert!(
        !events
            .0
            .borrow()
            .contains(&Event::Volume(VolumeEvent::VolumeReleased(
                Disposition::Keep
            )))
    );
    // Recording goes on while the drain waits.
    {
        let mut actor = recording.actor().unwrap();
        let h = actor
            .write_open(TreeId::Root, "during", &trees::writer("w"), Some(ATTRS))
            .unwrap();
        actor.write(h, 0, b"saved\n", false).unwrap();
        actor.close(h).unwrap();
    }
    drop(held);
    loop {
        recording.settle().unwrap();
        match recording.progress().unwrap() {
            Progress::Landed => {}
            Progress::Empty => break,
            Progress::Waiting => panic!("every entry is built and nothing is open"),
        }
    }
    assert_eq!(
        volume.rejudge(),
        Ok(VolumeState::Released(Disposition::Keep))
    );
    assert_eq!(
        events.0.borrow().last(),
        Some(&Event::Volume(VolumeEvent::VolumeReleased(
            Disposition::Keep
        )))
    );
}
