//! The operator files under `deploy/` and the manifests that use them.
//!
//! The sidecar and Hermes run the tree engine (docs/trees.md): their rule
//! files are built into the tree engine's rule set as the volume builds them
//! from `<state>/history.git/info/{exclude,attributes}`, and a representative
//! layout is placed through it, each directory before what it holds, so a
//! path under an untracked directory is untracked too; their settings are
//! read as the operator settings file's include, and every key tree volumes
//! do not build is classified. The CSI example profile is set up as the
//! driver sets a volume up: `agentfs init --include`, then its rule files
//! installed into the state area. The sidecar's manifest and the sidecar chart are checked
//! for crash recovery, the sealed mount point and the read-only emptyDir
//! fence, and no manifest names a backing tree.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn text(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

fn set(paths: &[&str]) -> BTreeSet<String> {
    paths.iter().map(|p| (*p).to_owned()).collect()
}

/// The CSI example profile as the driver sets a volume up: `agentfs init
/// --include <profile>.config`, then the profile's `.exclude` and
/// `.attributes` installed into the tree state area. Its settings that tree
/// volumes do not build are logged as not built, and its rules place paths.
#[test]
fn the_csi_example_profile_loads() {
    let profiles = repo_root().join("deploy/csi/profiles");
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("state");
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_agentfs"));
    for (name, _) in std::env::vars_os() {
        let n = name.to_string_lossy();
        if n.starts_with("OTEL_") || n.starts_with("AGENTFS_") {
            cmd.env_remove(name);
        }
    }
    let out: Output = cmd
        .arg("--state")
        .arg(&state)
        .args(["init", "--volume", "example", "--include"])
        .arg(profiles.join("example.config"))
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", text(&out.stderr));
    agentfs_core::operator_settings::install_rules(
        &state,
        agentfs_core::operator_settings::RuleFiles {
            exclude: Some(&profiles.join("example.exclude")),
            attributes: Some(&profiles.join("example.attributes")),
        },
    )
    .unwrap();

    let settings = agentfs_core::operator_settings::OperatorSettings::read(&state).unwrap();
    assert!(settings.missing_includes().is_empty());
    let gaps: BTreeSet<(String, String)> = settings
        .not_built()
        .into_iter()
        .map(|gap| (gap.key, gap.capability.as_str().to_owned()))
        .collect();
    for key in [
        "agentfs.pushinterval",
        "remote.origin.url",
        "remote.origin.agentfscredentials",
    ] {
        assert!(
            gaps.contains(&(key.to_owned(), "push".to_owned())),
            "{key}: {gaps:?}"
        );
    }

    let rules = rule_set(&agentfs_core::tree_actor::rules_dir(&state));
    let kept = ["notes.md", "src/main.rs"];
    let dropped = ["app.log", "tmp/scratch", "secrets/token"];
    let placed = placed(&rules, &[&kept[..], &dropped[..]].concat());
    assert_eq!(recorded(&placed, true), set(&kept));
    assert_eq!(recorded(&placed, false), set(&dropped));
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
    rule_set(&repo_root().join(dir))
}

/// The tree engine's rule set from the `exclude` and `attributes` files in
/// `dir` (nested repositories excluded).
fn rule_set(dir: &Path) -> agentfs_rules::RuleSet {
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
