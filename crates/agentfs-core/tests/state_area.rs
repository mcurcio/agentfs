//! `ensure` on a real state area (docs/trees.md, "Volume identity"): one id
//! however often it runs, and every refusal made before any write.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use agentfs_contracts::caps::{LaunchCap, LaunchScope, Minter};
use agentfs_contracts::clients::{AttachedLookup, ClientSet};
use agentfs_contracts::name::{LauncherId, VolumeName};
use agentfs_contracts::pod::PodContext;
use agentfs_contracts::ports::{Event, EventSink};
use agentfs_contracts::volume::{
    EnsureOutcome, SpecField, StartPoint, Volume, VolumeError, VolumeSpec,
};
use agentfs_core::state_area::TreeStateArea;
use agentfs_core::tree_actor::{Error, TreeActor};
use agentfs_volume::VolumeService;

struct NoClients;

impl AttachedLookup for NoClients {
    fn attached_any(&self) -> ClientSet {
        ClientSet::new()
    }
}

struct Events;

impl EventSink for Events {
    fn publish(&mut self, _: Event) {}
}

fn volume(root: &Path) -> VolumeService<TreeStateArea, NoClients, Events> {
    VolumeService::new(TreeStateArea::at(root), NoClients, Events)
}

fn spec(name: &str) -> VolumeSpec {
    VolumeSpec {
        name: VolumeName::parse(name).unwrap(),
        start: StartPoint::Empty,
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
