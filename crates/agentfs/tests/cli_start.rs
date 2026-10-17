//! End-to-end tests of a started volume through the `agentfs` binary
//! (F8 S1, design §11.3): `status` names its start point and, while it is
//! raised, the start-branch alarm; `push` raises the alarm and still
//! succeeds; `start accept --tip` clears it, and the accepted tip reaches
//! the remote under the volume's own name.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use agentfs_core::fork::identity::StartPoint;
use agentfs_core::fork::start::{StartRequest, start_from};
use agentfs_core::volume::StateDir;

/// One volume's state area and tree under `dir/<name>`, run through the
/// binary.
struct Volume {
    root: PathBuf,
}

impl Volume {
    fn state(&self) -> PathBuf {
        self.root.join("state")
    }

    fn tree(&self) -> PathBuf {
        self.root.join("tree")
    }

    fn run(&self, args: &[&str]) -> Output {
        let home = self.root.join("home");
        fs::create_dir_all(&home).unwrap();
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_agentfs"));
        cmd.arg("--state")
            .arg(self.state())
            .arg("--root")
            .arg(self.tree())
            .args(args)
            .env_remove("AGENTFS_STATE")
            .env_remove("AGENTFS_ROOT")
            .env("AGENTFS_REPLICA", "test-0")
            .env("HOME", &home)
            .env("XDG_CONFIG_HOME", home.join(".config"));
        for (name, _) in std::env::vars_os() {
            if name.to_string_lossy().starts_with("OTEL_") {
                cmd.env_remove(name);
            }
        }
        cmd.output().unwrap()
    }

    fn ok(&self, args: &[&str]) -> String {
        let out = self.run(args);
        assert!(out.status.success(), "{args:?}: {}", text(&out));
        text(&out)
    }

    fn record(&self, name: &str, content: &str) {
        fs::write(self.tree().join(name), content).unwrap();
        self.ok(&["record", name]);
    }
}

fn text(out: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

fn branch(git: &Path, name: &str) -> Option<git2::Oid> {
    git2::Repository::open_bare(git)
        .ok()?
        .refname_to_id(name)
        .ok()
}

#[test]
fn a_started_volume_alarms_until_the_moved_tip_is_accepted() {
    let dir = tempfile::tempdir().unwrap();
    let remote = dir.path().join("remote.git");
    git2::Repository::init_bare(&remote).unwrap();
    let profile = dir.path().join("profile.config");
    fs::write(
        &profile,
        format!("[remote \"origin\"]\n\turl = {}\n", remote.display()),
    )
    .unwrap();
    let include = profile.display().to_string();

    // The old worktree: v1 writes f, pushed.
    let v1 = Volume {
        root: dir.path().join("v1"),
    };
    fs::create_dir_all(v1.tree()).unwrap();
    v1.ok(&["init", "--volume", "v1", "--include", &include]);
    v1.record("f.txt", "f");
    v1.ok(&["push"]);
    let f = branch(&remote, "refs/heads/agentfs/v1").unwrap();

    // The new worktree, started from v1's branch at f (what CreateVolume
    // does for a start class).
    let v2 = Volume {
        root: dir.path().join("v2"),
    };
    let start = StartPoint::parse(&format!("origin:agentfs/v1@{f}")).unwrap();
    start_from(&StartRequest {
        state: &StateDir::new(v2.state()),
        tree: &v2.tree(),
        volume: "v2",
        includes: std::slice::from_ref(&profile),
        start: &start,
    })
    .unwrap();
    assert_eq!(fs::read_to_string(v2.tree().join("f.txt")).unwrap(), "f");
    let status = v2.ok(&["status"]);
    assert!(
        status.contains(&format!("start:     origin:agentfs/v1@{f}")),
        "{status}"
    );
    v2.record("h.txt", "h");
    v2.ok(&["push"]);
    assert!(!v2.ok(&["status"]).contains("start branch moved"));

    // The old node's late commit reaches its own branch.
    v1.record("g.txt", "g");
    v1.ok(&["push"]);
    let g = branch(&remote, "refs/heads/agentfs/v1").unwrap();
    let pushed = v2.ok(&["push"]);
    assert!(
        pushed.contains("start branch moved"),
        "the alarm is printed: {pushed}"
    );
    let status = v2.ok(&["status"]);
    assert!(
        status.contains(&format!(
            "start branch moved: {f}..{g} (agentfs start accept --tip {g})"
        )),
        "{status}"
    );
    assert_eq!(
        branch(&remote, "refs/heads/agentfs/v1"),
        Some(g),
        "v2 never writes v1's branch"
    );

    // Refused: not a full id, and not a commit of the start branch.
    let short = &g.to_string()[..12];
    let out = v2.run(&["start", "accept", "--tip", short]);
    assert_eq!(out.status.code(), Some(1), "{}", text(&out));
    assert!(
        text(&out).contains("not a full commit id"),
        "{}",
        text(&out)
    );
    let h = branch(&v2.state().join("git"), "refs/heads/agentfs/v2").unwrap();
    let out = v2.run(&["start", "accept", "--tip", &h.to_string()]);
    assert_eq!(out.status.code(), Some(1), "{}", text(&out));
    assert!(
        text(&out).contains("is not on agentfs/v1"),
        "{}",
        text(&out)
    );

    // Accepted: the next push clears the alarm and saves g under v2.
    let accepted = format!("refs/agentfs/v2/accepted/{g}");
    let said = v2.ok(&["start", "accept", "--tip", &g.to_string()]);
    assert!(said.contains(&accepted), "{said}");
    v2.ok(&["start", "accept", "--tip", &g.to_string()]);
    let pushed = v2.ok(&["push"]);
    assert!(!pushed.contains("start branch moved"), "{pushed}");
    assert!(!v2.ok(&["status"]).contains("start branch moved"));
    assert_eq!(branch(&remote, &accepted), Some(g));
}
