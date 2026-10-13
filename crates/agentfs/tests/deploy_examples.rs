//! The operator files under `deploy/` (design §16, §17.1), checked through
//! the CLI exactly as the init container and the sidecar use them: the
//! config fragment is included with `agentfs init --include`, and the rule
//! files become `<state>/git/info/exclude` and `info/attributes`. The
//! sidecar manifest is checked for its crash recovery: `recover` mode and the
//! read-only emptyDir fence.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

struct Volume {
    dir: tempfile::TempDir,
}

impl Volume {
    /// A volume set up from `deploy/<rules>`: `config` included, `exclude`
    /// and `attributes` copied over the state area's own.
    fn new(rules: &str) -> Self {
        let rules = repo_root().join(rules);
        Self::from_files(
            &rules.join("config"),
            &rules.join("exclude"),
            &rules.join("attributes"),
        )
    }

    /// A CSI profile: `deploy/csi/profiles/<name>.{config,exclude,attributes}`,
    /// included and installed as the driver does (design §14.2).
    fn profile(name: &str) -> Self {
        let dir = repo_root().join("deploy/csi/profiles");
        Self::from_files(
            &dir.join(format!("{name}.config")),
            &dir.join(format!("{name}.exclude")),
            &dir.join(format!("{name}.attributes")),
        )
    }

    fn from_files(config: &Path, exclude: &Path, attributes: &Path) -> Self {
        let v = Self {
            dir: tempfile::tempdir().unwrap(),
        };
        let out = v.run(&[
            "init",
            "--volume",
            "example",
            "--include",
            config.to_str().unwrap(),
        ]);
        assert!(out.status.success(), "{}", text(&out.stderr));
        for (name, from) in [("exclude", exclude), ("attributes", attributes)] {
            std::fs::copy(from, v.dir.path().join("state/git/info").join(name)).unwrap();
        }
        v
    }

    fn tree(&self) -> PathBuf {
        self.dir.path().join("backing")
    }

    fn run(&self, args: &[&str]) -> Output {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_agentfs"));
        for (name, _) in std::env::vars_os() {
            let n = name.to_string_lossy();
            if n.starts_with("OTEL_") || n.starts_with("AGENTFS_") {
                cmd.env_remove(name);
            }
        }
        cmd.arg("--state")
            .arg(self.dir.path().join("state"))
            .arg("--root")
            .arg(self.tree())
            .args(args)
            .env("AGENTFS_REPLICA", "test-0")
            .output()
            .unwrap()
    }

    /// Writes each path under the backing tree, records them all, and
    /// returns the paths recorded and the paths skipped.
    fn record(&self, paths: &[&str]) -> (BTreeSet<String>, BTreeSet<String>) {
        for path in paths {
            let full = self.tree().join(path);
            std::fs::create_dir_all(full.parent().unwrap()).unwrap();
            std::fs::write(&full, format!("content of {path}\n")).unwrap();
        }
        let mut args = vec!["record"];
        args.extend_from_slice(paths);
        let out = self.run(&args);
        assert!(out.status.success(), "{}", text(&out.stderr));
        let stdout = text(&out.stdout);
        let mut recorded = BTreeSet::new();
        let mut skipped = BTreeSet::new();
        for line in stdout.lines() {
            if let Some(rest) = line.strip_prefix("recorded ") {
                recorded.insert(rest.split(" as seq ").next().unwrap().to_owned());
            } else if let Some(rest) = line.strip_prefix("skipped ") {
                skipped.insert(rest.split(": ").next().unwrap().to_owned());
            }
        }
        (recorded, skipped)
    }

    /// Every path in the branch head.
    fn history(&self) -> BTreeSet<String> {
        let repo = git2::Repository::open_bare(self.dir.path().join("state/git")).unwrap();
        let mut out = BTreeSet::new();
        let Ok(head) = repo.find_reference("refs/heads/agentfs/example") else {
            return out;
        };
        head.peel_to_commit()
            .unwrap()
            .tree()
            .unwrap()
            .walk(git2::TreeWalkMode::PreOrder, |root, e| {
                if e.kind() == Some(git2::ObjectType::Blob) {
                    out.insert(format!("{root}{}", e.name().unwrap()));
                }
                git2::TreeWalkResult::Ok
            })
            .unwrap();
        out
    }
}

fn text(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

fn set(paths: &[&str]) -> BTreeSet<String> {
    paths.iter().map(|p| (*p).to_owned()).collect()
}

#[test]
fn the_sidecar_example_config_loads() {
    let v = Volume::new("deploy/sidecar/config");
    let out = v.run(&["status"]);
    let status = text(&out.stdout);
    assert!(out.status.success(), "{}", text(&out.stderr));
    assert!(
        status.contains("remote:    origin https://git.example.com/acme/agent-history.git"),
        "{status}"
    );
    assert!(!status.contains("misconfigured"), "{status}");
    // Its rules: *.log and tmp/ are not recorded when new, secrets/**
    // never.
    let kept = ["notes.md", "src/main.rs"];
    let dropped = ["app.log", "tmp/scratch", "secrets/token"];
    let (recorded, skipped) = v.record(&[&kept[..], &dropped[..]].concat());
    assert_eq!(recorded, set(&kept));
    assert_eq!(skipped, set(&dropped));
    assert_eq!(v.history(), set(&kept));
}

#[test]
fn the_hermes_rules_record_only_definitions() {
    let v = Volume::new("deploy/examples/hermes");
    let status = text(&v.run(&["status"]).stdout);
    assert!(
        status.contains("remote:    origin https://git.example.com/acme/hermes-history.git"),
        "{status}"
    );
    assert!(!status.contains("misconfigured"), "{status}");
    let kept = [
        "SOUL.md",
        "config.yaml",
        "skills/a/SKILL.md",
        "skills/a/scripts/run.sh",
        "profiles/p/SOUL.md",
        "profiles/p/config.yaml",
        "profiles/p/profile.yaml",
        "profiles/p/skills/x.md",
    ];
    let dropped = [
        ".env",
        "auth.json",
        "state.db",
        "state.db-wal",
        "sessions/s.json",
        "logs/x.log",
        "checkpoints/c",
        "memories/MEMORY.md",
        "cron/jobs.json",
        "profiles/p/memory.db",
        "profiles/p/.env",
        "profiles/p/auth.json",
        "profiles/p/sessions/s.json",
        "profiles/p/logs/x.log",
        // Under an allowed directory, the attributes still stop secrets
        // and databases.
        "skills/a/.env",
        "skills/a/cache.sqlite3",
        "profiles/p/skills/checkpoints/c",
    ];
    let (recorded, skipped) = v.record(&[&kept[..], &dropped[..]].concat());
    assert_eq!(recorded, set(&kept));
    assert_eq!(skipped, set(&dropped));
    assert_eq!(v.history(), set(&kept));
}

#[test]
fn the_csi_example_profile_loads() {
    let v = Volume::profile("example");
    let out = v.run(&["status"]);
    let status = text(&out.stdout);
    assert!(out.status.success(), "{}", text(&out.stderr));
    assert!(
        status.contains("remote:    origin https://git.example.com/acme/agent-history.git"),
        "{status}"
    );
    assert!(!status.contains("misconfigured"), "{status}");
    let kept = ["notes.md", "src/main.rs"];
    let dropped = ["app.log", "tmp/scratch", "secrets/token"];
    let (recorded, skipped) = v.record(&[&kept[..], &dropped[..]].concat());
    assert_eq!(recorded, set(&kept));
    assert_eq!(skipped, set(&dropped));
    assert_eq!(v.history(), set(&kept));
}

/// The lines of `deploy/sidecar/deployment.yaml` without comments.
fn sidecar_manifest() -> Vec<String> {
    std::fs::read_to_string(repo_root().join("deploy/sidecar/deployment.yaml"))
        .unwrap()
        .lines()
        .filter(|l| !l.trim_start().starts_with('#'))
        .map(str::to_owned)
        .collect()
}

/// The shipped sidecar recovers in place after a crash (design §17.1).
#[test]
fn the_sidecar_recovers_stale_mounts() {
    let lines = sidecar_manifest();
    assert!(
        lines
            .iter()
            .any(|l| l.contains("name: AGENTFS_ON_STALE_MOUNT") && l.contains("value: recover")),
        "deploy/sidecar/deployment.yaml must set AGENTFS_ON_STALE_MOUNT to recover"
    );
}

/// The workload mounts the shared emptyDir read-only: the fence while no
/// daemon is mounted (design §17.1).
#[test]
fn the_workload_mounts_the_emptydir_read_only() {
    let lines = sidecar_manifest();
    let start = lines
        .iter()
        .position(|l| l.trim() == "containers:")
        .expect("a containers: section");
    let workload = &lines[start..];
    let mount = workload
        .iter()
        .position(|l| l.contains("name: agentfs-mount"))
        .expect("the workload mounts agentfs-mount");
    let block: Vec<&String> = workload[mount..]
        .iter()
        .take_while(|l| !l.contains("volumes:"))
        .take(6)
        .collect();
    assert!(
        block.iter().any(|l| l.trim() == "readOnly: true"),
        "the workload's agentfs-mount must be readOnly: true: {block:?}"
    );
    assert!(
        block
            .iter()
            .any(|l| l.trim() == "recursiveReadOnly: Disabled"),
        "{block:?}"
    );
}
