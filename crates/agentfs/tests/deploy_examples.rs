//! The operator files under `deploy/` and the manifests that use them.
//!
//! The sidecar and Hermes run the tree engine (docs/trees.md): their rule
//! files are built into the tree engine's rule set as the volume builds them
//! from `<state>/history.git/info/{exclude,attributes}`, and a representative
//! layout is placed through it, each directory before what it holds, so a
//! path under an untracked directory is untracked too; their settings are
//! read as the operator settings file's include, and every key tree volumes
//! do not build is classified. The CSI profiles stay on the recording engine
//! (the driver's create, `agentfs_core::volume::init_with`) until the CSI
//! driver moves. The sidecar's manifest and the sidecar chart are checked
//! for crash recovery, the sealed mount point and the read-only emptyDir
//! fence, and no manifest names a backing tree.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

struct Volume {
    dir: tempfile::TempDir,
}

impl Volume {
    /// A CSI profile: `deploy/csi/profiles/<name>.{config,exclude,attributes}`,
    /// included and installed as the driver does.
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
        agentfs_core::volume::init_with(
            &agentfs_core::volume::StateDir::new(v.dir.path().join("state")),
            "example",
            None,
            Some(&[config.to_path_buf()]),
        )
        .unwrap();
        std::fs::create_dir_all(v.tree()).unwrap();
        for (name, from) in [("exclude", exclude), ("attributes", attributes)] {
            std::fs::copy(from, v.dir.path().join("state/git/info").join(name)).unwrap();
        }
        v
    }

    fn tree(&self) -> PathBuf {
        self.dir.path().join("backing")
    }

    fn run(&self, args: &[&str]) -> Output {
        self.run_as("state", "backing", args)
    }

    /// Runs `agentfs` on another volume beside this one: `state` and
    /// `root` under the same directory.
    fn run_as(&self, state: &str, root: &str, args: &[&str]) -> Output {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_agentfs"));
        for (name, _) in std::env::vars_os() {
            let n = name.to_string_lossy();
            if n.starts_with("OTEL_") || n.starts_with("AGENTFS_") {
                cmd.env_remove(name);
            }
        }
        cmd.arg("--state")
            .arg(self.dir.path().join(state))
            .arg("--root")
            .arg(self.dir.path().join(root))
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

/// The example unlinked fork class's own remotes (docs/helm.md, csi.md): `deploy/csi/profiles/sandbox-remotes.config` is accepted as a
/// fork's `remotes` file, and the fork's only remote is its own.
#[test]
fn the_csi_example_fork_remotes_load() {
    let v = Volume::profile("example");
    v.record(&["notes.md"]);
    let source = v.dir.path().join("state");
    let remotes = repo_root().join("deploy/csi/profiles/sandbox-remotes.config");
    let out = v.run_as(
        "fork",
        "fork-tree",
        &[
            "fork",
            "create",
            "--source-state",
            source.to_str().unwrap(),
            "--link",
            "unlinked",
            "--remotes",
            remotes.to_str().unwrap(),
        ],
    );
    assert!(out.status.success(), "{}", text(&out.stderr));
    let out = v.run_as("fork", "fork-tree", &["status"]);
    let status = text(&out.stdout);
    assert!(out.status.success(), "{}", text(&out.stderr));
    assert!(
        status.contains("remote:    sandbox https://git.example.com/acme/sandbox-history.git"),
        "{status}"
    );
    assert!(!status.contains("origin"), "never the source's: {status}");
    assert!(!status.contains("misconfigured"), "{status}");
}

/// The lines of a file under the repository without comments.
fn uncommented(path: &str) -> Vec<String> {
    std::fs::read_to_string(repo_root().join(path))
        .unwrap()
        .lines()
        .filter(|l| !l.trim_start().starts_with('#'))
        .map(str::to_owned)
        .collect()
}

/// The lines of `deploy/sidecar/deployment.yaml` without comments.
fn sidecar_manifest() -> Vec<String> {
    uncommented("deploy/sidecar/deployment.yaml")
}

/// The shipped sidecar recovers in place after a crash.
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

/// The shipped sidecar seals the bare mount point, the fence for subPath
/// consumers while no daemon is mounted.
#[test]
fn the_sidecar_seals_the_mount_point() {
    let lines = sidecar_manifest();
    assert!(
        lines
            .iter()
            .any(|l| l.contains("name: AGENTFS_SEAL_MOUNTPOINT") && l.contains("value: \"true\"")),
        "deploy/sidecar/deployment.yaml must set AGENTFS_SEAL_MOUNTPOINT to \"true\""
    );
}

/// The workload mounts the shared emptyDir read-only: the fence while no
/// daemon is mounted.
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

/// The tree engine's rule set from `deploy/<dir>`'s `exclude` and
/// `attributes`, as a volume builds it when it opens (nested repositories
/// excluded).
fn tree_rules(dir: &str) -> agentfs_rules::RuleSet {
    let dir = repo_root().join(dir);
    let texts = agentfs_rules::RuleTexts {
        exclude: std::fs::read(dir.join("exclude")).unwrap(),
        attributes: std::fs::read(dir.join("attributes")).unwrap(),
    };
    agentfs_rules::RuleSet::build(&texts, agentfs_rules::Nested::Exclude)
        .unwrap_or_else(|e| panic!("{}: the tree engine refuses the rules: {e}", dir.display()))
}

/// Places every file of `layout` through `rules` as a workload makes it,
/// each directory first (a directory holding a `.git` entry gets it before
/// its other entries, as a clone makes it), and answers whether each file
/// is recorded.
fn placed(rules: &agentfs_rules::RuleSet, layout: &[&str]) -> BTreeMap<String, bool> {
    use agentfs_rules::{NewKind, PathFacts, Placement};
    let holds_git = |dir: &str| {
        layout
            .iter()
            .any(|path| path.starts_with(&format!("{dir}/.git/")))
    };
    let mut dirs: BTreeMap<String, bool> = BTreeMap::new();
    let mut out = BTreeMap::new();
    for path in layout {
        let parts: Vec<&str> = path.split('/').collect();
        let mut untracked = false;
        let mut inside_repository = false;
        for depth in 1..=parts.len() {
            let name = parts[..depth].join("/");
            let is_file = depth == parts.len();
            let facts = PathFacts {
                parent_untracked: untracked,
                inside_repository,
            };
            let tracked = if is_file {
                rules.place(&name, NewKind::File, facts) == Placement::Tracked
            } else {
                *dirs.entry(name.clone()).or_insert_with(|| {
                    rules.place(&name, NewKind::Dir, facts) == Placement::Tracked
                })
            };
            if is_file {
                out.insert((*path).to_owned(), tracked);
            } else {
                untracked = !tracked;
                inside_repository |= holds_git(&name);
            }
        }
    }
    out
}

/// The keys of `deploy/<dir>/config`, read as the operator settings file's
/// include, that tree volumes log as not built: key and capability.
fn not_built(config: &str) -> BTreeSet<(String, String)> {
    let state = tempfile::tempdir().unwrap();
    agentfs_core::operator_settings::write_includes(state.path(), &[repo_root().join(config)])
        .unwrap();
    let settings = agentfs_core::operator_settings::OperatorSettings::read(state.path()).unwrap();
    assert!(settings.missing_includes().is_empty(), "{config}");
    settings
        .not_built()
        .into_iter()
        .map(|gap| (gap.key, gap.capability.as_str().to_owned()))
        .collect()
}

fn recorded(placed: &BTreeMap<String, bool>, want: bool) -> BTreeSet<String> {
    placed
        .iter()
        .filter(|(_, tracked)| **tracked == want)
        .map(|(path, _)| path.clone())
        .collect()
}

#[test]
fn the_sidecar_rules_place_paths_on_the_tree_engine() {
    let rules = tree_rules("deploy/sidecar/config");
    // *.log and tmp/ are not recorded when new, secrets/** never.
    let kept = ["notes.md", "src/main.rs"];
    let dropped = ["app.log", "tmp/scratch", "secrets/token"];
    let placed = placed(&rules, &[&kept[..], &dropped[..]].concat());
    assert_eq!(recorded(&placed, true), set(&kept));
    assert_eq!(recorded(&placed, false), set(&dropped));
}

#[test]
fn the_sidecar_settings_are_logged_as_not_built() {
    let gaps = not_built("deploy/sidecar/config/config");
    for key in [
        "remote.origin.url",
        "remote.origin.agentfscredentials",
        "agentfs.pushinterval",
    ] {
        assert!(
            gaps.contains(&(key.to_owned(), "push".to_owned())),
            "{key}: {gaps:?}"
        );
    }
}

#[test]
fn the_hermes_rules_record_only_definitions() {
    let rules = tree_rules("deploy/examples/hermes");
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
        "state.db-shm",
        "response_store.db",
        "sessions/s.json",
        "logs/x.log",
        "checkpoints/c",
        "memories/MEMORY.md",
        "cron/jobs.json",
        "sandboxes/docker/home/agentfs-home.txt",
        "profiles/p/memory.db",
        "profiles/p/.env",
        "profiles/p/auth.json",
        "profiles/p/sessions/s.json",
        "profiles/p/logs/x.log",
        // Under an allowed directory, the attributes still stop secrets,
        // databases, sessions, logs and checkpoints.
        "skills/a/.env",
        "skills/a/auth.json",
        "skills/a/cache.sqlite3",
        "skills/a/logs/run.log",
        "skills/a/sessions/s.json",
        "profiles/p/skills/checkpoints/c",
        // A nested repository: a skill cloned with git.
        "skills/cloned/.git/HEAD",
        "skills/cloned/SKILL.md",
    ];
    let placed = placed(&rules, &[&kept[..], &dropped[..]].concat());
    assert_eq!(recorded(&placed, true), set(&kept));
    assert_eq!(recorded(&placed, false), set(&dropped));
}

#[test]
fn the_hermes_settings_log_the_remote_and_the_patch_source() {
    let gaps = not_built("deploy/examples/hermes/config");
    assert!(
        gaps.contains(&("remote.origin.url".to_owned(), "push".to_owned())),
        "{gaps:?}"
    );
    assert!(
        gaps.contains(&("agentfs.patchremote".to_owned(), "patches".to_owned())),
        "{gaps:?}"
    );
    // Nested repositories stay out on tree volumes: built, never listed.
    assert!(
        !gaps.iter().any(|(key, _)| key == "agentfs.nested"),
        "{gaps:?}"
    );
}

/// The sidecar chart sets what the manifest does: `recover`, the sealed
/// mount point and the workload's read-only emptyDir.
#[test]
fn the_sidecar_chart_recovers_seals_and_fences() {
    let values = uncommented("charts/agentfs-sidecar/values.yaml");
    assert!(
        values.iter().any(|l| l.trim() == "onStaleMount: recover"),
        "{values:?}"
    );
    assert!(
        values.iter().any(|l| l.trim() == "sealMountpoint: true"),
        "{values:?}"
    );
    let template =
        std::fs::read_to_string(repo_root().join("charts/agentfs-sidecar/templates/_sidecar.tpl"))
            .unwrap();
    for wanted in [
        "{ name: AGENTFS_ON_STALE_MOUNT, value: {{ .onStaleMount }} }",
        "{ name: AGENTFS_SEAL_MOUNTPOINT, value: {{ .sealMountpoint | toString | quote }} }",
        "args: [mount, /agentfs/workspace]",
        "mountPath: /data/state/history.git/info/exclude",
        "mountPath: /data/state/history.git/info/attributes",
    ] {
        assert!(template.contains(wanted), "the chart lacks {wanted}");
    }
    let mount = template
        .split("define \"agentfs-sidecar.workspaceMount\"")
        .nth(1)
        .and_then(|rest| rest.split("{{- end }}").next())
        .expect("the workspaceMount helper");
    assert!(mount.contains("readOnly: true"), "{mount}");
    assert!(mount.contains("recursiveReadOnly: Disabled"), "{mount}");
}

/// The sidecar's rule files sit where the tree engine reads them.
#[test]
fn the_sidecar_mounts_the_rules_under_history_git() {
    let lines = sidecar_manifest();
    for name in ["exclude", "attributes"] {
        assert!(
            lines.iter().any(|l| l.contains(&format!(
                "mountPath: /data/state/history.git/info/{name}, subPath: {name}"
            ))),
            "deploy/sidecar/deployment.yaml must mount {name} at history.git/info"
        );
    }
}

/// No manifest of the tree engine names `AGENTFS_ROOT` or a backing tree:
/// `init` and `mount` exit 1 on `--root`, and a tree volume has none.
#[test]
fn no_tree_manifest_names_a_backing_tree() {
    let mut files = Vec::new();
    for dir in [
        "deploy/sidecar",
        "deploy/examples/helm-sidecar",
        "deploy/examples/hermes",
        "charts/agentfs-sidecar",
        "examples/docker",
        "ci/k8s",
        "ci/hermes/compose.yaml",
        "ci/hermes/k8s/before",
        "ci/hermes/k8s/common",
        "ci/hermes/k8s/sidecar",
        "ci/hermes/k8s/fence",
    ] {
        collect(&repo_root().join(dir), &mut files);
    }
    assert!(files.len() > 10, "{files:?}");
    for file in files {
        let Ok(text) = std::fs::read_to_string(&file) else {
            continue;
        };
        for line in text.lines().filter(|l| !l.trim_start().starts_with('#')) {
            assert!(
                !line.contains("AGENTFS_ROOT")
                    && !line.contains("/data/backing")
                    && !line.contains("/data/state/git/"),
                "{}: {line}",
                file.display()
            );
        }
    }
}

fn collect(path: &Path, out: &mut Vec<PathBuf>) {
    if path.is_dir() {
        for entry in std::fs::read_dir(path).unwrap() {
            collect(&entry.unwrap().path(), out);
        }
    } else {
        out.push(path.to_owned());
    }
}
