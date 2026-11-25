//! The workspace as `cargo metadata` describes it: its members, their
//! dependencies, features and targets, and the ranks of
//! `[workspace.metadata.agentfs]` (docs/architecture.md, "Ranks").
//!
//! [`Workspace::load`] asks cargo, so the checks read the manifests as
//! cargo reads them, not a list kept beside them. Every value is plain
//! data, so a test builds a [`Workspace`] by hand.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde::Deserialize;

/// The workspace's root directory: the one holding the root `Cargo.toml`.
pub fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("crates/agentfs-repo sits two directories below the workspace root")
        .to_path_buf()
}

/// The workspace's ranks and members.
#[derive(Debug, Clone)]
pub struct Workspace {
    /// The ranks in the order the workspace manifest lists them.
    pub ranks: Vec<Rank>,
    pub members: Vec<Member>,
}

/// A rank: a group of packages that may depend on the same ranks.
#[derive(Debug, Clone)]
pub struct Rank {
    pub name: String,
    /// The ranks this rank's packages may depend on.
    pub uses: Vec<String>,
}

/// A member package of the workspace.
#[derive(Debug, Clone)]
pub struct Member {
    pub name: String,
    /// Its directory, relative to [`root`] (`crates/agentfs-core`).
    pub dir: PathBuf,
    /// `[package.metadata.agentfs] rank`, when it names one.
    pub rank: Option<String>,
    /// Every dependency it declares, on members or not, one per kind.
    pub dependencies: Vec<Dependency>,
    /// Its features and what each enables.
    pub features: BTreeMap<String, Vec<String>>,
    pub targets: Vec<Target>,
}

/// A dependency, by the package it names.
#[derive(Debug, Clone)]
pub struct Dependency {
    pub name: String,
    pub kind: DependencyKind,
}

/// Which table of the manifest a dependency is in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DependencyKind {
    Normal,
    Build,
    Dev,
}

/// A build target: its name and kinds (`lib`, `bin`, `test`, ...).
#[derive(Debug, Clone)]
pub struct Target {
    pub name: String,
    pub kinds: Vec<String>,
}

impl Workspace {
    /// The workspace as `cargo metadata --no-deps` describes it. Panics when
    /// cargo fails or its output is not what this reads.
    pub fn load() -> Self {
        let root = root();
        let output = Command::new(env!("CARGO"))
            .args([
                "metadata",
                "--format-version",
                "1",
                "--no-deps",
                "--offline",
            ])
            .arg("--manifest-path")
            .arg(root.join("Cargo.toml"))
            .output()
            .unwrap_or_else(|e| panic!("running cargo metadata: {e}"));
        assert!(
            output.status.success(),
            "cargo metadata failed ({}):\n{}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        );
        let raw: RawMetadata = serde_json::from_slice(&output.stdout)
            .unwrap_or_else(|e| panic!("reading cargo metadata's output: {e}"));
        Self::from_raw(raw, &root)
    }

    /// The member named `name`.
    pub fn member(&self, name: &str) -> Option<&Member> {
        self.members.iter().find(|m| m.name == name)
    }

    /// The rank named `name`.
    pub fn rank(&self, name: &str) -> Option<&Rank> {
        self.ranks.iter().find(|r| r.name == name)
    }

    fn from_raw(raw: RawMetadata, root: &Path) -> Self {
        let ranks = raw
            .metadata
            .and_then(|m| m.agentfs)
            .map(|a| a.ranks)
            .unwrap_or_default()
            .into_iter()
            .map(|r| Rank {
                name: r.name,
                uses: r.uses,
            })
            .collect();
        let members = raw
            .packages
            .into_iter()
            .map(|p| {
                let dir = p
                    .manifest_path
                    .parent()
                    .and_then(|d| d.strip_prefix(root).ok())
                    .unwrap_or_else(|| {
                        panic!(
                            "{}'s manifest {} is not under {}",
                            p.name,
                            p.manifest_path.display(),
                            root.display()
                        )
                    })
                    .to_path_buf();
                Member {
                    name: p.name,
                    dir,
                    rank: p.metadata.and_then(|m| m.agentfs).and_then(|a| a.rank),
                    dependencies: p
                        .dependencies
                        .into_iter()
                        .map(|d| Dependency {
                            name: d.name,
                            kind: match d.kind.as_deref() {
                                None => DependencyKind::Normal,
                                Some("build") => DependencyKind::Build,
                                Some("dev") => DependencyKind::Dev,
                                Some(other) => panic!("unknown dependency kind {other:?}"),
                            },
                        })
                        .collect(),
                    features: p.features,
                    targets: p
                        .targets
                        .into_iter()
                        .map(|t| Target {
                            name: t.name,
                            kinds: t.kind,
                        })
                        .collect(),
                }
            })
            .collect();
        Self { ranks, members }
    }
}

#[derive(Deserialize)]
struct RawMetadata {
    packages: Vec<RawPackage>,
    metadata: Option<RawWorkspaceMetadata>,
}

#[derive(Deserialize)]
struct RawWorkspaceMetadata {
    agentfs: Option<RawWorkspaceAgentfs>,
}

#[derive(Deserialize)]
struct RawWorkspaceAgentfs {
    #[serde(default)]
    ranks: Vec<RawRank>,
}

#[derive(Deserialize)]
struct RawRank {
    name: String,
    #[serde(default)]
    uses: Vec<String>,
}

#[derive(Deserialize)]
struct RawPackage {
    name: String,
    manifest_path: PathBuf,
    dependencies: Vec<RawDependency>,
    features: BTreeMap<String, Vec<String>>,
    targets: Vec<RawTarget>,
    metadata: Option<RawPackageMetadata>,
}

#[derive(Deserialize)]
struct RawPackageMetadata {
    agentfs: Option<RawPackageAgentfs>,
}

#[derive(Deserialize)]
struct RawPackageAgentfs {
    rank: Option<String>,
}

#[derive(Deserialize)]
struct RawDependency {
    name: String,
    kind: Option<String>,
}

#[derive(Deserialize)]
struct RawTarget {
    name: String,
    kind: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loads_this_workspace() {
        let ws = Workspace::load();
        let me = ws.member("agentfs-repo").expect("agentfs-repo is a member");
        assert_eq!(me.dir, Path::new("crates/agentfs-repo"));
        assert_eq!(me.rank.as_deref(), Some("tests"));
        assert!(ws.rank("tests").is_some());
        assert!(root().join("Cargo.toml").is_file());
        let core = ws.member("agentfs-core").expect("agentfs-core is a member");
        assert!(core.features.contains_key("failpoints"));
        assert!(
            core.dependencies
                .iter()
                .any(|d| d.name == "tempfile" && d.kind == DependencyKind::Dev)
        );
        let cli = ws.member("agentfs-cli").expect("agentfs-cli is a member");
        assert_eq!(cli.dir, Path::new("crates/agentfs-cli"));
        let bins: Vec<&str> = cli
            .targets
            .iter()
            .filter(|t| t.kinds.iter().any(|k| k == "bin"))
            .map(|t| t.name.as_str())
            .collect();
        assert_eq!(bins, ["agentfs"]);
    }
}
