//! One volume composed through the service, on a real state area and no
//! FUSE (docs/library/service.md): a bound client writes through its
//! session, a stop drains and releases, a keyed fork is found again and
//! ends, and the state area's lock refuses a second opener.

use std::path::Path;

use agentfs_contracts::TreeId;
use agentfs_contracts::caps::{LaunchCap, LaunchScope, Minter};
use agentfs_contracts::clients::{Access, FileOwner};
use agentfs_contracts::name::{ClientLabel, ForkKey, LauncherId, VolumeName};
use agentfs_contracts::pod::PodContext;
use agentfs_contracts::trees::{ForkPhase, ForkSettings, Landing, TreeError};
use agentfs_contracts::volume::{EnsureOutcome, StartPoint, VolumeError, VolumeSpec};
use agentfs_engine::operator_settings::OperatorSettings;
use agentfs_engine::tree_actor::{Attrs, Perm};
use agentfs_service::{Domains, Launcher, ServiceError, Unstaged, end_fork, init};

#[expect(
    clippy::disallowed_methods,
    reason = "a test is its own composition root"
)]
fn launcher() -> Launcher {
    let minter = Minter::new();
    let cap = LaunchCap::mint(
        &minter,
        LauncherId::parse("cli").unwrap(),
        LaunchScope::Root,
    );
    Launcher::new(minter, cap, PodContext::default())
}

fn spec() -> VolumeSpec {
    VolumeSpec {
        name: VolumeName::parse("notes").unwrap(),
        start: StartPoint::Empty,
        root_owner: None,
    }
}

/// A new volume in a fresh directory.
fn volume() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let ensured = init(dir.path(), spec(), &launcher(), &[]).unwrap();
    assert_eq!(ensured.outcome, EnsureOutcome::Created);
    dir
}

fn stage(state: &Path) -> Result<Domains, ServiceError> {
    let settings = OperatorSettings::read(state).unwrap();
    Domains::stage(state, launcher(), &settings)
}

fn label(text: &str) -> ClientLabel {
    ClientLabel::parse(text).unwrap()
}

fn key(text: &str) -> ForkKey {
    ForkKey::parse(text).unwrap()
}

#[test]
fn a_bound_client_writes_through_its_session() {
    let dir = volume();
    let mut domains = stage(dir.path()).unwrap();
    let binding = domains
        .bind(
            TreeId::Root,
            label("docs"),
            Access::ReadWrite,
            PodContext::default(),
        )
        .unwrap();
    let session = domains.sessions().session(binding.session).unwrap();
    let file = Attrs {
        perm: Perm::from_mode(0o644),
        owner: FileOwner {
            uid: 1000,
            gid: 1000,
        },
    };
    let handle = {
        let mut guard = session.lock().unwrap();
        let handle = guard.write_open("hello.txt", Some(file)).unwrap();
        guard.write(handle, 0, b"hello\n", false).unwrap();
        handle
    };
    session.close(handle).unwrap();
    assert_eq!(
        session.lock().unwrap().read("hello.txt").unwrap(),
        b"hello\n"
    );
}

#[test]
fn unstage_drains_and_releases() {
    let dir = volume();
    let mut domains = stage(dir.path()).unwrap();
    let binding = domains
        .bind(
            TreeId::Root,
            label("docs"),
            Access::ReadWrite,
            PodContext::default(),
        )
        .unwrap();
    assert!(!domains.released());
    assert_eq!(
        domains.unstage(std::slice::from_ref(&binding.client)),
        Unstaged::Draining
    );
    assert!(domains.released());
    assert!(!domains.stopped());
    // The view's client was detached, not left to the reconcile that
    // follows: its end is recorded as `Detached` on its tree's clients row.
    domains.sessions().recording().settle().unwrap();
    let ends = clients_row_subjects(dir.path(), TreeId::Root);
    let detached = format!("AttachmentEnded {} Detached", binding.client);
    assert!(ends.contains(&detached), "{ends:?}");
}

/// The subjects of `tree`'s clients row in the volume's history, newest
/// first.
fn clients_row_subjects(state: &Path, tree: TreeId) -> Vec<String> {
    let output = std::process::Command::new("git")
        .env("GIT_DIR", state.join("history.git"))
        .args([
            "log",
            "--format=%s",
            &format!("refs/agentfs/clients/{tree}"),
        ])
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .map(str::to_owned)
        .collect()
}

#[test]
fn a_second_unstage_is_unchanged() {
    let dir = volume();
    let mut domains = stage(dir.path()).unwrap();
    assert_eq!(domains.unstage(&[]), Unstaged::Draining);
    assert!(domains.released());
    assert_eq!(domains.unstage(&[]), Unstaged::Unchanged);
}

#[test]
fn a_keyed_fork_is_found_again_then_ends() {
    let dir = volume();
    let mut domains = stage(dir.path()).unwrap();
    let automatic = ForkSettings {
        landing: Landing::Automatic,
    };
    let fork = domains
        .fork(TreeId::Root, Some(key("pvc-1")), automatic)
        .unwrap();
    assert_ne!(fork, TreeId::Root);
    assert_eq!(domains.keyed(&key("pvc-1")), Some((fork, ForkPhase::Open)));
    assert_eq!(
        domains
            .fork(TreeId::Root, Some(key("pvc-1")), automatic)
            .unwrap(),
        fork
    );
    domains.end_keyed(&key("pvc-1")).unwrap();
    let (ended, phase) = domains.keyed(&key("pvc-1")).unwrap();
    assert_eq!(ended, fork);
    assert_ne!(phase, ForkPhase::Open);
    assert_eq!(domains.end_keyed(&key("pvc-2")), Err(TreeError::KeyUnknown));
}

#[test]
fn a_second_stage_is_busy() {
    let dir = volume();
    let _domains = stage(dir.path()).unwrap();
    assert!(matches!(
        stage(dir.path()),
        Err(ServiceError::Volume(VolumeError::Busy))
    ));
    assert!(matches!(
        init(dir.path(), spec(), &launcher(), &[]),
        Err(ServiceError::Volume(VolumeError::Busy))
    ));
}

#[test]
fn end_fork_while_staged_is_busy() {
    let dir = volume();
    let _domains = stage(dir.path()).unwrap();
    assert!(matches!(
        end_fork(dir.path(), launcher(), &key("pvc-1")),
        Err(ServiceError::Volume(VolumeError::Busy))
    ));
}

#[test]
fn init_writes_the_includes() {
    let dir = volume();
    let include = dir.path().join("operator.conf");
    std::fs::write(&include, "[agentfs]\n\toverlayLimit = 1m\n").unwrap();
    let found = init(
        dir.path(),
        spec(),
        &launcher(),
        std::slice::from_ref(&include),
    )
    .unwrap();
    assert_eq!(found.outcome, EnsureOutcome::Found);
    let settings = OperatorSettings::read(dir.path()).unwrap();
    assert_eq!(settings.includes(), [include]);
    assert_eq!(settings.overlay_limit().unwrap(), 1 << 20);
    // The lock is released once init returns.
    drop(stage(dir.path()).unwrap());
}

#[test]
fn end_fork_refuses_an_unknown_key() {
    let dir = volume();
    assert!(matches!(
        end_fork(dir.path(), launcher(), &key("pvc-1")),
        Err(ServiceError::Fork(TreeError::KeyUnknown))
    ));
    let mut domains = stage(dir.path()).unwrap();
    let automatic = ForkSettings {
        landing: Landing::Automatic,
    };
    let fork = domains
        .fork(TreeId::Root, Some(key("pvc-1")), automatic)
        .unwrap();
    drop(domains);
    end_fork(dir.path(), launcher(), &key("pvc-1")).unwrap();
    let domains = stage(dir.path()).unwrap();
    assert_ne!(domains.keyed(&key("pvc-1")), Some((fork, ForkPhase::Open)));
}
