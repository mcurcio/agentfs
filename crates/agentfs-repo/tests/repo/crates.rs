//! The workspace's crates: their dependencies against the ranks
//! (docs/architecture.md, "Ranks"), the library front `agentfs` holding
//! re-exports only, of every library of the ranks it uses but the recording
//! engine, the domains composed by the service crate alone, the
//! `failpoints` feature forwarded to
//! every dependency that has it, every cargo command the repository
//! writes down naming what the workspace has, every failpoint call listed,
//! the tree engine's `git` children all counted, and the tree engine naming
//! no module of the recording engine. Each check is a pure
//! function over a [`Workspace`], a line or a source, with a fixture test
//! beside it.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use agentfs_repo::workspace::{Dependency, DependencyKind, Member, Rank, Target, Workspace};

use crate::{Comments, files_under, is_separator, lex, root};

/// Where `workspace` breaks its ranks: a rank listed twice, a rank using
/// one listed after it, a member naming no rank or an unknown one, a
/// dependency of any kind on a member whose rank no rank uses, its own
/// included (the crate `agentfs`, which only consumers use), a normal or build
/// dependency on a member in a rank its own rank does not use, and a
/// dev-dependency that builds another member of its own rank when
/// that rank does not use itself: the sibling itself, or any member whose
/// normal and build dependencies reach it (one engine's tests would build
/// the other). Other dev-dependencies may point anywhere.
fn rank_problems(workspace: &Workspace) -> Vec<String> {
    let mut problems = Vec::new();
    let mut seen = BTreeSet::new();
    for (i, rank) in workspace.ranks.iter().enumerate() {
        if !seen.insert(rank.name.as_str()) {
            problems.push(format!("rank {} is listed twice", rank.name));
        }
        for used in &rank.uses {
            let earlier = workspace.ranks[..i].iter().any(|r| r.name == *used);
            if *used != rank.name && !earlier {
                problems.push(format!(
                    "rank {} uses {used}, which is not a rank listed before it",
                    rank.name
                ));
            }
        }
    }
    let mut edges = BTreeSet::new();
    for member in &workspace.members {
        let Some(name) = &member.rank else {
            problems.push(format!(
                "{} names no rank: add [package.metadata.agentfs] rank",
                member.name
            ));
            continue;
        };
        let Some(rank) = workspace.rank(name) else {
            problems.push(format!(
                "{} names rank {name}, which [workspace.metadata.agentfs] ranks does not list",
                member.name
            ));
            continue;
        };
        for dependency in &member.dependencies {
            let Some(their) = workspace
                .member(&dependency.name)
                .and_then(|d| d.rank.as_deref())
                .filter(|r| workspace.rank(r).is_some())
            else {
                continue;
            };
            if !workspace
                .ranks
                .iter()
                .any(|r| r.uses.iter().any(|u| u == their))
            {
                edges.insert(format!(
                    "{} -> {} (no rank uses {their})",
                    member.name, dependency.name
                ));
                continue;
            }
            let used = rank.uses.iter().any(|u| u == their);
            if dependency.kind == DependencyKind::Dev {
                // A test may drive anything but what builds a sibling the
                // rank keeps apart: one engine's tests would build the other.
                if rank.uses.iter().any(|u| u == name) {
                    continue;
                }
                for sibling in built_by(workspace, &dependency.name) {
                    let of_rank = workspace
                        .member(&sibling)
                        .is_some_and(|s| s.rank.as_deref() == Some(name.as_str()));
                    if sibling == member.name || !of_rank {
                        continue;
                    }
                    edges.insert(if sibling == dependency.name {
                        format!(
                            "{} -> {} (dev; {name} does not use {name})",
                            member.name, dependency.name
                        )
                    } else {
                        format!(
                            "{} -> {} (dev; it builds {sibling}, and {name} does not use {name})",
                            member.name, dependency.name
                        )
                    });
                }
                continue;
            }
            if !used {
                edges.insert(format!(
                    "{} -> {} ({name} does not use {their})",
                    member.name, dependency.name
                ));
            }
        }
    }
    problems.extend(edges);
    problems
}

/// The members building `from` builds: itself, and every member its normal
/// and build dependencies reach. A member's dev-dependencies are built only
/// for its own tests, so they are not followed.
fn built_by(workspace: &Workspace, from: &str) -> BTreeSet<String> {
    let mut built = BTreeSet::new();
    let mut next = vec![from.to_owned()];
    while let Some(name) = next.pop() {
        let Some(m) = workspace.member(&name) else {
            continue;
        };
        if !built.insert(name) {
            continue;
        }
        next.extend(
            m.dependencies
                .iter()
                .filter(|d| d.kind != DependencyKind::Dev)
                .map(|d| d.name.clone()),
        );
    }
    built
}

#[test]
fn dependencies_follow_the_ranks() {
    let workspace = Workspace::load();
    assert!(
        workspace.ranks.len() > 3 && workspace.members.len() > 10,
        "is cargo metadata read right? {workspace:?}"
    );
    let problems = rank_problems(&workspace);
    assert!(
        problems.is_empty(),
        "dependencies against the ranks (docs/architecture.md, \"Ranks\"):\n  {}",
        problems.join("\n  ")
    );
}

/// A member of a fixture: its rank, its features, its normal dependencies.
fn member(name: &str, rank: &str, normal: &[&str]) -> Member {
    Member {
        name: name.to_owned(),
        dir: PathBuf::from("crates").join(name),
        rank: Some(rank.to_owned()),
        dependencies: normal
            .iter()
            .map(|d| Dependency {
                name: (*d).to_owned(),
                kind: DependencyKind::Normal,
            })
            .collect(),
        features: Default::default(),
        targets: vec![Target {
            name: name.replace('-', "_"),
            kinds: vec!["lib".into()],
        }],
    }
}

/// The ranks of a fixture, as the workspace manifest lists them today.
fn ranks() -> Vec<Rank> {
    [
        ("line-identity", &["line-identity"][..]),
        ("leaf", &["line-identity", "leaf"]),
        ("shared-kernel", &[]),
        ("domain", &["shared-kernel"]),
        (
            "engine",
            &["line-identity", "leaf", "shared-kernel", "domain"],
        ),
        ("service", &["shared-kernel", "domain", "engine"]),
        (
            "filesystem",
            &[
                "leaf",
                "shared-kernel",
                "domain",
                "engine",
                "service",
                "filesystem",
            ],
        ),
        (
            "kubernetes",
            &["leaf", "shared-kernel", "engine", "filesystem"],
        ),
        (
            "umbrella",
            &["shared-kernel", "domain", "engine", "service"],
        ),
        (
            "tests",
            &[
                "line-identity",
                "leaf",
                "shared-kernel",
                "domain",
                "engine",
                "service",
                "filesystem",
                "kubernetes",
                "tests",
            ],
        ),
    ]
    .into_iter()
    .map(|(name, uses)| Rank {
        name: name.to_owned(),
        uses: uses.iter().map(|u| (*u).to_owned()).collect(),
    })
    .collect()
}

/// Today's shape, small: the line maps on the alignment, a leaf, the
/// shared kernel, two domains, the engine, the FUSE layer, the CSI driver,
/// the library crate `agentfs`.
fn today() -> Workspace {
    Workspace {
        ranks: ranks(),
        members: vec![
            member("agentfs-align", "line-identity", &[]),
            member("agentfs-lines", "line-identity", &["agentfs-align"]),
            member("agentfs-git", "leaf", &["agentfs-align"]),
            member("agentfs-os", "leaf", &[]),
            member("agentfs-contracts", "shared-kernel", &[]),
            member("agentfs-volume", "domain", &["agentfs-contracts"]),
            member("agentfs-trees", "domain", &["agentfs-contracts"]),
            member(
                "agentfs-core",
                "engine",
                &["agentfs-lines", "agentfs-git", "agentfs-contracts"],
            ),
            member("agentfs-fuse", "filesystem", &["agentfs-core"]),
            member(
                "agentfs-csi",
                "kubernetes",
                &["agentfs-core", "agentfs-fuse"],
            ),
            member(
                "agentfs",
                "umbrella",
                &["agentfs-contracts", "agentfs-volume"],
            ),
        ],
    }
}

/// `workspace` with a normal dependency of `from` on `to`.
fn with_edge(mut workspace: Workspace, from: &str, to: &str) -> Workspace {
    let m = workspace
        .members
        .iter_mut()
        .find(|m| m.name == from)
        .unwrap();
    m.dependencies.push(Dependency {
        name: to.to_owned(),
        kind: DependencyKind::Normal,
    });
    workspace
}

#[test]
fn the_ranks_refuse_sideways_and_upward_edges() {
    assert_eq!(rank_problems(&today()), Vec::<String>::new());
    for (from, to, why) in [
        (
            "agentfs-lines",
            "agentfs-git",
            "line-identity does not use leaf",
        ),
        (
            "agentfs-trees",
            "agentfs-volume",
            "domain does not use domain",
        ),
        (
            "agentfs-contracts",
            "agentfs-os",
            "shared-kernel does not use leaf",
        ),
        (
            "agentfs-fuse",
            "agentfs-csi",
            "filesystem does not use kubernetes",
        ),
        // No member depends on the library crate, whatever its rank uses.
        ("agentfs-fuse", "agentfs", "no rank uses umbrella"),
        ("agentfs-csi", "agentfs", "no rank uses umbrella"),
    ] {
        assert_eq!(
            rank_problems(&with_edge(today(), from, to)),
            [format!("{from} -> {to} ({why})")]
        );
    }
    let mut two_engines = with_edge(today(), "agentfs-core", "agentfs-engine");
    two_engines
        .members
        .push(member("agentfs-engine", "engine", &[]));
    assert_eq!(
        rank_problems(&two_engines),
        ["agentfs-core -> agentfs-engine (engine does not use engine)"]
    );
    // A test may drive anything a rank uses...
    let mut dev = today();
    dev.members[0].dependencies.push(Dependency {
        name: "agentfs-csi".into(),
        kind: DependencyKind::Dev,
    });
    // ...a sibling of a rank that uses itself included...
    dev.members[0].dependencies.push(Dependency {
        name: "agentfs-lines".into(),
        kind: DependencyKind::Dev,
    });
    assert_eq!(rank_problems(&dev), Vec::<String>::new());
    // ...but a sibling of a rank that does not.
    let dev_edge = |workspace: &mut Workspace, from: &str, to: &str| {
        let m = workspace
            .members
            .iter_mut()
            .find(|m| m.name == from)
            .unwrap();
        m.dependencies.push(Dependency {
            name: to.to_owned(),
            kind: DependencyKind::Dev,
        });
    };
    // ...and never the library crate, which no rank uses.
    let mut umbrella = today();
    dev_edge(&mut umbrella, "agentfs-fuse", "agentfs");
    assert_eq!(
        rank_problems(&umbrella),
        ["agentfs-fuse -> agentfs (no rank uses umbrella)"]
    );
    let mut domains = today();
    dev_edge(&mut domains, "agentfs-trees", "agentfs-volume");
    assert_eq!(
        rank_problems(&domains),
        ["agentfs-trees -> agentfs-volume (dev; domain does not use domain)"]
    );
    let mut engines = today();
    engines
        .members
        .push(member("agentfs-engine", "engine", &[]));
    dev_edge(&mut engines, "agentfs-core", "agentfs-engine");
    assert_eq!(
        rank_problems(&engines),
        ["agentfs-core -> agentfs-engine (dev; engine does not use engine)"]
    );
    // Nor a member above the rank that builds the sibling: the FUSE layer
    // links the recording engine, so the tree engine's tests would build it.
    let mut above = today();
    above.members.push(member("agentfs-engine", "engine", &[]));
    dev_edge(&mut above, "agentfs-engine", "agentfs-fuse");
    assert_eq!(
        rank_problems(&above),
        [
            "agentfs-engine -> agentfs-fuse (dev; it builds agentfs-core, and engine does not use engine)"
        ]
    );
    // Nor a test helper whose own dependencies reach the sibling.
    let mut helper = today();
    helper.members.push(member("agentfs-engine", "engine", &[]));
    helper
        .members
        .push(member("agentfs-test-metrics", "tests", &["agentfs-core"]));
    dev_edge(&mut helper, "agentfs-engine", "agentfs-test-metrics");
    assert_eq!(
        rank_problems(&helper),
        [
            "agentfs-engine -> agentfs-test-metrics (dev; it builds agentfs-core, and engine does not use engine)"
        ]
    );
    // A dev cycle back to the member itself is no sibling.
    let mut cycle = today();
    dev_edge(&mut cycle, "agentfs-core", "agentfs-fuse");
    assert_eq!(rank_problems(&cycle), Vec::<String>::new());
    // A build dependency counts.
    let mut build = today();
    build.members[0].dependencies.push(Dependency {
        name: "agentfs-csi".into(),
        kind: DependencyKind::Build,
    });
    assert_eq!(rank_problems(&build).len(), 1);
}

#[test]
fn the_ranks_themselves_are_checked() {
    let mut twice = today();
    twice.ranks.push(twice.ranks[0].clone());
    assert_eq!(
        rank_problems(&twice),
        ["rank line-identity is listed twice"]
    );
    let mut forward = today();
    forward.ranks[0].uses.push("leaf".into());
    assert_eq!(
        rank_problems(&forward),
        ["rank line-identity uses leaf, which is not a rank listed before it"]
    );
    let mut unranked = today();
    unranked.members[0].rank = None;
    unranked.members[1].rank = Some("nowhere".into());
    assert_eq!(
        rank_problems(&unranked),
        [
            "agentfs-align names no rank: add [package.metadata.agentfs] rank",
            "agentfs-lines names rank nowhere, which [workspace.metadata.agentfs] ranks does not list",
        ]
    );
}

/// The front's rank: the library under one name (`agentfs`), which no rank
/// uses (docs/architecture.md, "Ranks").
const FRONT_RANK: &str = "umbrella";

/// The libraries of the ranks the front uses that it does not re-export:
/// the recording engine, until M9 deletes it
/// (docs/target-architecture/future/crate-split.md).
const NOT_REEXPORTED: &[&str] = &["agentfs-core"];

/// Where `front`, a member of [`FRONT_RANK`] whose `src/lib.rs` is
/// `source`, is more or less than re-exports: its source holds anything
/// but comments and `pub use <crate> as <module>;` items; it does not
/// re-export, as its own module named for it without `agentfs_`, every
/// library of the ranks its rank uses but [`NOT_REEXPORTED`]; or it
/// depends (normal or build) on anything else.
fn front_problems(workspace: &Workspace, front: &Member, source: &str) -> Vec<String> {
    let mut problems = Vec::new();
    let file = front.dir.join("src/lib.rs");
    let mut reexported = BTreeSet::new();
    for (n, line) in source.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with("//") {
            continue;
        }
        let words: Vec<&str> = line
            .strip_suffix(';')
            .unwrap_or_default()
            .split_whitespace()
            .collect();
        match words[..] {
            ["pub", "use", krate, "as", module] => {
                reexported.insert((krate.to_owned(), module.to_owned()));
            }
            _ => problems.push(format!(
                "{}:{}: not a re-export: {line}",
                file.display(),
                n + 1
            )),
        }
    }
    let uses = front
        .rank
        .as_deref()
        .and_then(|r| workspace.rank(r))
        .map(|r| r.uses.clone())
        .unwrap_or_default();
    let mut expected = BTreeSet::new();
    let mut crates = BTreeSet::new();
    for m in &workspace.members {
        let used = m.rank.as_ref().is_some_and(|r| uses.contains(r));
        let lib = m
            .targets
            .iter()
            .find(|t| t.kinds.iter().any(|k| k == "lib"));
        let (true, Some(lib)) = (used, lib) else {
            continue;
        };
        if NOT_REEXPORTED.contains(&m.name.as_str()) {
            continue;
        }
        let module = lib.name.strip_prefix("agentfs_").unwrap_or(&lib.name);
        expected.insert((lib.name.clone(), module.to_owned()));
        crates.insert(m.name.clone());
    }
    for (krate, module) in expected.difference(&reexported) {
        problems.push(format!(
            "{} does not re-export {krate} as {module}",
            front.name
        ));
    }
    for (krate, module) in reexported.difference(&expected) {
        problems.push(format!(
            "{} re-exports {krate} as {module}: not a library of a rank it uses, under its own name",
            front.name
        ));
    }
    let depends: BTreeSet<String> = front
        .dependencies
        .iter()
        .filter(|d| d.kind != DependencyKind::Dev)
        .map(|d| d.name.clone())
        .collect();
    for name in crates.difference(&depends) {
        problems.push(format!("{} does not depend on {name}", front.name));
    }
    for name in depends.difference(&crates) {
        problems.push(format!(
            "{} -> {name}: not a crate it re-exports",
            front.name
        ));
    }
    problems
}

#[test]
fn the_front_only_reexports_the_ranks_it_uses() {
    let workspace = Workspace::load();
    let fronts: Vec<&Member> = workspace
        .members
        .iter()
        .filter(|m| m.rank.as_deref() == Some(FRONT_RANK))
        .collect();
    assert_eq!(fronts.len(), 1, "one member of rank {FRONT_RANK}");
    let front = fronts[0];
    let source = std::fs::read_to_string(root().join(&front.dir).join("src/lib.rs"))
        .unwrap_or_else(|e| panic!("reading {}'s src/lib.rs: {e}", front.name));
    let problems = front_problems(&workspace, front, &source);
    assert!(
        problems.is_empty(),
        "the library front (docs/library/agentfs.md):\n  {}",
        problems.join("\n  ")
    );
}

#[test]
fn a_front_that_is_more_or_less_than_reexports_is_found() {
    let workspace = today();
    let front = workspace.member("agentfs").unwrap().clone();
    let source = "//! The library.\n//! ```\n//! let x = 1;\n//! ```\n\n\
        pub use agentfs_contracts as contracts;\n\
        pub use agentfs_trees as trees;\n\
        pub use agentfs_volume as volume;\n";
    let mut whole = front.clone();
    whole.dependencies.push(Dependency {
        name: "agentfs-trees".into(),
        kind: DependencyKind::Normal,
    });
    whole.dependencies.push(Dependency {
        name: "tempfile".into(),
        kind: DependencyKind::Dev,
    });
    assert_eq!(
        front_problems(&workspace, &whole, source),
        Vec::<String>::new()
    );
    // A domain left out, in the source and the manifest.
    assert_eq!(
        front_problems(&workspace, &front, source),
        ["agentfs does not depend on agentfs-trees"]
    );
    let missing = source.replace("pub use agentfs_trees as trees;\n", "");
    assert_eq!(
        front_problems(&workspace, &whole, &missing),
        ["agentfs does not re-export agentfs_trees as trees"]
    );
    // Code of its own, a renamed module, a crate it should not reach.
    let code = format!("{source}pub fn open() {{}}\n");
    assert_eq!(
        front_problems(&workspace, &whole, &code),
        ["crates/agentfs/src/lib.rs:9: not a re-export: pub fn open() {}"]
    );
    let renamed = source.replace("as trees;", "as forks;");
    assert_eq!(
        front_problems(&workspace, &whole, &renamed),
        [
            "agentfs does not re-export agentfs_trees as trees",
            "agentfs re-exports agentfs_trees as forks: not a library of a rank it uses, under its own name",
        ]
    );
    let core = with_edge(workspace.clone(), "agentfs", "agentfs-core");
    let mut reaching = core.member("agentfs").unwrap().clone();
    reaching.dependencies.extend(whole.dependencies.clone());
    assert_eq!(
        front_problems(
            &core,
            &reaching,
            &format!("{source}pub use agentfs_core as core;\n")
        ),
        [
            "agentfs re-exports agentfs_core as core: not a library of a rank it uses, under its own name",
            "agentfs -> agentfs-core: not a crate it re-exports",
        ]
    );
}

/// The rank of the one crate that composes a volume's domains
/// (docs/library/service.md).
const SERVICE_RANK: &str = "service";

/// The ranks that may depend on the domains the service composes: the
/// service, the library front that re-exports them, and the tests.
const COMPOSERS: &[&str] = &[SERVICE_RANK, FRONT_RANK, "tests"];

/// Where a crate outside [`COMPOSERS`] depends (normal or build) on a
/// domain the [`SERVICE_RANK`] crate depends on: it could wire a volume's
/// domains a second way. The domains are derived from the service's
/// dependencies, so a domain the service comes to compose is held too.
fn composition_problems(workspace: &Workspace) -> Vec<String> {
    let rank_of = |name: &str| workspace.member(name).and_then(|m| m.rank.clone());
    let composed: BTreeSet<String> = workspace
        .members
        .iter()
        .filter(|m| m.rank.as_deref() == Some(SERVICE_RANK))
        .flat_map(|m| &m.dependencies)
        .filter(|d| d.kind != DependencyKind::Dev)
        .filter(|d| rank_of(&d.name).as_deref() == Some("domain"))
        .map(|d| d.name.clone())
        .collect();
    let mut problems = BTreeSet::new();
    for member in &workspace.members {
        if member
            .rank
            .as_deref()
            .is_some_and(|r| COMPOSERS.contains(&r))
        {
            continue;
        }
        for dependency in &member.dependencies {
            if dependency.kind != DependencyKind::Dev && composed.contains(&dependency.name) {
                problems.insert(format!(
                    "{} -> {}: only the {SERVICE_RANK} crate composes the domains \
                     (ranks {} may depend on them)",
                    member.name,
                    dependency.name,
                    COMPOSERS.join(", ")
                ));
            }
        }
    }
    problems.into_iter().collect()
}

#[test]
fn only_the_service_composes_the_domains() {
    let workspace = Workspace::load();
    assert!(
        workspace
            .members
            .iter()
            .any(|m| m.rank.as_deref() == Some(SERVICE_RANK) && !m.dependencies.is_empty()),
        "no {SERVICE_RANK} crate with dependencies: is cargo metadata read right?"
    );
    let problems = composition_problems(&workspace);
    assert!(
        problems.is_empty(),
        "one composition of a volume (docs/library/service.md):\n  {}",
        problems.join("\n  ")
    );
}

#[test]
fn a_second_composer_of_the_domains_is_found() {
    let mut workspace = today();
    workspace
        .members
        .push(member("agentfs-rules", "domain", &[]));
    workspace.members.push(member(
        "agentfs-service",
        "service",
        &["agentfs-contracts", "agentfs-volume", "agentfs-trees"],
    ));
    let mut composed = with_edge(workspace.clone(), "agentfs", "agentfs-trees");
    // A domain the service does not compose, and a test's dev edge.
    composed = with_edge(composed, "agentfs-fuse", "agentfs-rules");
    composed
        .members
        .iter_mut()
        .find(|m| m.name == "agentfs-fuse")
        .unwrap()
        .dependencies
        .push(Dependency {
            name: "agentfs-volume".into(),
            kind: DependencyKind::Dev,
        });
    assert_eq!(composition_problems(&composed), Vec::<String>::new());
    let second = with_edge(composed, "agentfs-fuse", "agentfs-volume");
    assert_eq!(
        composition_problems(&second),
        [
            "agentfs-fuse -> agentfs-volume: only the service crate composes the domains \
          (ranks service, umbrella, tests may depend on them)"
        ]
    );
}

/// The members whose `failpoints` feature does not turn on that of a
/// normal dependency on a member that has one (`<dep>/failpoints` or
/// `<dep>?/failpoints`). Code gated on the feature in a crate without it is
/// rustc's `unexpected_cfgs`, refused under `-D warnings`.
fn unforwarded_failpoints(workspace: &Workspace) -> Vec<String> {
    const FEATURE: &str = "failpoints";
    let mut problems = BTreeSet::new();
    for member in &workspace.members {
        let Some(enabled) = member.features.get(FEATURE) else {
            continue;
        };
        for dependency in &member.dependencies {
            if dependency.kind != DependencyKind::Normal {
                continue;
            }
            let has = workspace
                .member(&dependency.name)
                .is_some_and(|d| d.features.contains_key(FEATURE));
            let name = &dependency.name;
            let forwarded = enabled
                .iter()
                .any(|f| *f == format!("{name}/{FEATURE}") || *f == format!("{name}?/{FEATURE}"));
            if has && !forwarded {
                problems.insert(format!(
                    "{}: its {FEATURE} feature does not enable \"{name}/{FEATURE}\"",
                    member.name
                ));
            }
        }
    }
    problems.into_iter().collect()
}

#[test]
fn every_failpoints_feature_is_forwarded() {
    let workspace = Workspace::load();
    assert!(
        workspace
            .members
            .iter()
            .filter(|m| m.features.contains_key("failpoints"))
            .count()
            > 1,
        "no member declares failpoints: is cargo metadata read right?"
    );
    let problems = unforwarded_failpoints(&workspace);
    assert!(problems.is_empty(), "{}", problems.join("\n"));
}

#[test]
fn an_unforwarded_failpoints_feature_is_found() {
    let failpoints = |workspace: &mut Workspace, name: &str, enables: &[&str]| {
        let m = workspace
            .members
            .iter_mut()
            .find(|m| m.name == name)
            .unwrap();
        m.features.insert(
            "failpoints".into(),
            enables.iter().map(|e| (*e).to_owned()).collect(),
        );
    };
    let mut forwarded = today();
    failpoints(&mut forwarded, "agentfs-core", &[]);
    failpoints(&mut forwarded, "agentfs-fuse", &["agentfs-core/failpoints"]);
    assert_eq!(unforwarded_failpoints(&forwarded), Vec::<String>::new());
    failpoints(
        &mut forwarded,
        "agentfs-fuse",
        &["agentfs-core?/failpoints"],
    );
    assert_eq!(unforwarded_failpoints(&forwarded), Vec::<String>::new());
    failpoints(&mut forwarded, "agentfs-fuse", &[]);
    assert_eq!(
        unforwarded_failpoints(&forwarded),
        ["agentfs-fuse: its failpoints feature does not enable \"agentfs-core/failpoints\""]
    );
    // A member without the feature forwards nothing.
    let mut csi = today();
    failpoints(&mut csi, "agentfs-core", &[]);
    assert_eq!(unforwarded_failpoints(&csi), Vec::<String>::new());
}

/// What a cargo command selects: its subcommand (the first word that is
/// not an option), the packages (`-p`, `--package`), the features
/// (`--features`, `-F`), the binaries (`--bin`) and the test programs
/// (`--test`). Only values made of
/// `[A-Za-z0-9_-]` are kept: `<crate>` or `$pkg` is no name, and its kind
/// is kept in `variables`.
#[derive(Debug, Default, PartialEq, Eq)]
struct Selection {
    subcommand: Option<String>,
    packages: Vec<String>,
    /// Each kind of selection with a value that is no name, once:
    /// `package`, `feature`, `binary`, `test program`.
    variables: Vec<&'static str>,
    /// `(Some(package), feature)` for `pkg/feat`, `(None, feature)` bare.
    features: Vec<(Option<String>, String)>,
    bins: Vec<String>,
    tests: Vec<String>,
}

fn is_name(value: &str) -> bool {
    !value.is_empty()
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// The words after each `cargo` in `line`, up to the next separator. A
/// word that holds whitespace (a quoted string) is read again as a line.
fn cargo_commands(line: &str, out: &mut Vec<Vec<String>>) {
    let words = lex(line, Comments::Read);
    for word in &words {
        if word.chars().any(char::is_whitespace) {
            cargo_commands(word, out);
        }
    }
    for (i, word) in words.iter().enumerate() {
        if word == "cargo" {
            out.push(
                words[i + 1..]
                    .iter()
                    .take_while(|w| !is_separator(w))
                    .cloned()
                    .collect(),
            );
        }
    }
}

/// What the command whose words follow `cargo` selects.
fn selection(words: &[String]) -> Selection {
    let mut s = Selection::default();
    let variable = |s: &mut Selection, kind: &'static str| {
        if !s.variables.contains(&kind) {
            s.variables.push(kind);
        }
    };
    let package = |s: &mut Selection, value: &str| {
        if is_name(value) {
            s.packages.push(value.to_owned());
        } else {
            variable(s, "package");
        }
    };
    let features = |s: &mut Selection, value: &str| {
        for item in value.split(|c: char| c == ',' || c.is_whitespace()) {
            match item.split_once('/') {
                _ if item.is_empty() => {}
                Some((p, f)) if is_name(p) && is_name(f) => {
                    s.features.push((Some(p.to_owned()), f.to_owned()))
                }
                None if is_name(item) => s.features.push((None, item.to_owned())),
                _ => variable(s, "feature"),
            }
        }
    };
    let bin = |s: &mut Selection, value: &str| {
        if is_name(value) {
            s.bins.push(value.to_owned());
        } else {
            variable(s, "binary");
        }
    };
    let test = |s: &mut Selection, value: &str| {
        if is_name(value) {
            s.tests.push(value.to_owned());
        } else {
            variable(s, "test program");
        }
    };
    let mut i = 0;
    while i < words.len() {
        let word = words[i].as_str();
        let next = words.get(i + 1).map(String::as_str);
        let mut takes_next = false;
        match word {
            "-p" | "--package" => {
                if let Some(v) = next {
                    package(&mut s, v);
                }
                takes_next = true;
            }
            "--features" | "-F" => {
                if let Some(v) = next {
                    features(&mut s, v);
                }
                takes_next = true;
            }
            "--bin" => {
                if let Some(v) = next {
                    bin(&mut s, v);
                }
                takes_next = true;
            }
            "--test" => {
                if let Some(v) = next {
                    test(&mut s, v);
                }
                takes_next = true;
            }
            _ => {
                if let Some(v) = word
                    .strip_prefix("--package=")
                    .or_else(|| word.strip_prefix("-p="))
                    .or_else(|| word.strip_prefix("-p").filter(|v| !v.is_empty()))
                {
                    package(&mut s, v);
                } else if let Some(v) = word.strip_prefix("--features=") {
                    features(&mut s, v);
                } else if let Some(v) = word.strip_prefix("--bin=") {
                    bin(&mut s, v);
                } else if let Some(v) = word.strip_prefix("--test=") {
                    test(&mut s, v);
                } else if s.subcommand.is_none() && !word.starts_with(['-', '+']) {
                    s.subcommand = Some(word.to_owned());
                }
            }
        }
        i += if takes_next { 2 } else { 1 };
    }
    s
}

/// Why `s` names what `workspace` does not have: a package, a package's
/// feature, a feature no selected package declares (cargo's rule, read
/// only when every package is a literal name), a binary of no selected package
/// (of no member when none is selected), a test program of no selected
/// package (of no member when none is selected), or `cargo run` of a
/// package with no binary.
fn selection_problems(workspace: &Workspace, s: &Selection) -> Vec<String> {
    let mut problems = Vec::new();
    let literal = !s.variables.contains(&"package");
    let selected: Vec<&Member> = s
        .packages
        .iter()
        .filter_map(|p| workspace.member(p))
        .collect();
    for p in &s.packages {
        if workspace.member(p).is_none() {
            problems.push(format!(
                "selects package {p}, which the workspace does not have"
            ));
        }
    }
    let has = |m: &Member, kind: &str, name: Option<&str>| {
        m.targets
            .iter()
            .any(|t| t.kinds.iter().any(|k| k == kind) && name.is_none_or(|n| t.name == n))
    };
    let has_bin = |m: &Member, bin: Option<&str>| has(m, "bin", bin);
    for (package, feature) in &s.features {
        match package {
            Some(p) => match workspace.member(p) {
                None => problems.push(format!("{p}/{feature}: the workspace has no package {p}")),
                Some(m) if !m.features.contains_key(feature) => {
                    problems.push(format!("{p}/{feature}: {p} has no feature {feature}"))
                }
                Some(_) => {}
            },
            // cargo's rule: a bare feature is one a selected package declares.
            None if literal
                && !s.packages.is_empty()
                && !selected.iter().any(|m| m.features.contains_key(feature)) =>
            {
                problems.push(format!(
                    "feature {feature}: no selected package ({}) declares it",
                    s.packages.join(", ")
                ))
            }
            None => {}
        }
    }
    if literal {
        for bin in &s.bins {
            let found = if s.packages.is_empty() {
                workspace.members.iter().any(|m| has_bin(m, Some(bin)))
            } else {
                selected.iter().any(|m| has_bin(m, Some(bin)))
            };
            if !found {
                problems.push(format!("--bin {bin}: no selected package has that binary"));
            }
        }
        for test in &s.tests {
            let found = if s.packages.is_empty() {
                workspace.members.iter().any(|m| has(m, "test", Some(test)))
            } else {
                selected.iter().any(|m| has(m, "test", Some(test)))
            };
            if !found {
                problems.push(format!(
                    "--test {test}: no selected package has that test program"
                ));
            }
        }
        if s.subcommand.as_deref() == Some("run") {
            for m in &selected {
                if !has_bin(m, None) {
                    problems.push(format!("cargo run -p {}: it has no binary", m.name));
                }
            }
        }
    }
    problems
}

/// The files whose cargo commands are read: the crates' Rust sources for
/// their doc comments only, and their Python scripts.
fn command_files() -> Vec<PathBuf> {
    let root = root();
    let mut files = Vec::new();
    for dir in ["ci", "examples", "deploy"] {
        files_under(&root.join(dir), &mut files);
    }
    let mut markdown = |dir: &Path| {
        let mut all = Vec::new();
        files_under(dir, &mut all);
        files.extend(
            all.into_iter()
                .filter(|p| p.extension().is_some_and(|e| e == "md")),
        );
    };
    markdown(&root.join("docs"));
    markdown(&root.join(".claude/skills"));
    // All of `.github/`: the workflows, and the pull request template's
    // commands.
    files_under(&root.join(".github"), &mut files);
    for entry in std::fs::read_dir(root.join("crates")).unwrap() {
        let dir = entry.unwrap().path();
        if !dir.is_dir() {
            continue;
        }
        for page in std::fs::read_dir(&dir).unwrap() {
            let page = page.unwrap().path();
            if page.extension().is_some_and(|e| e == "md") {
                files.push(page);
            }
        }
    }
    let mut sources = Vec::new();
    files_under(&root.join("crates"), &mut sources);
    files.extend(
        sources
            .into_iter()
            .filter(|p| p.extension().is_some_and(|e| e == "rs" || e == "py")),
    );
    for file in ["Dockerfile", "AGENTS.md", "README.md", "CONTRIBUTING.md"] {
        files.push(root.join(file));
    }
    files.sort();
    files
}

/// Whether a scanned file runs, so its cargo commands execute: any but
/// markdown, Rust (whose commands are read by people) and the issue
/// templates' forms. [`script_problems`] reads it.
fn runs(path: &Path) -> bool {
    !path.extension().is_some_and(|e| e == "md" || e == "rs")
        && !path.to_string_lossy().contains(".github/ISSUE_TEMPLATE/")
}

/// Why a command `s` of the file at `path`, one that runs, is refused
/// though it may name what the workspace has: a package, feature, binary or
/// test program selected through a variable, which no check can read
/// (markdown keeps placeholders such as `-p <crate>`); and `cargo build` of
/// a package with no binary, since a script builds to run what it built (a
/// stale `-p agentfs`, now the library crate's name).
fn script_problems(workspace: &Workspace, path: &Path, s: &Selection) -> Vec<String> {
    if !runs(path) {
        return Vec::new();
    }
    let mut problems: Vec<String> = s
        .variables
        .iter()
        .map(|kind| {
            format!("selects a {kind} through a variable: name it, so this check can read it")
        })
        .collect();
    if s.subcommand.as_deref() == Some("build") {
        for m in s.packages.iter().filter_map(|p| workspace.member(p)) {
            if !m.targets.iter().any(|t| t.kinds.iter().any(|k| k == "bin")) {
                problems.push(format!(
                    "cargo build -p {}: it has no binary for this file to run",
                    m.name
                ));
            }
        }
    }
    problems
}

/// Every cargo call of a Python `text` written as a list, `["cargo", ...]`,
/// with the line it starts on: its string items as the words after
/// `cargo`, any other item as `$item`, which is no name. A list may run
/// over lines.
fn list_commands(text: &str) -> Vec<(usize, Vec<String>)> {
    let mut out = Vec::new();
    let chars: Vec<char> = text.chars().collect();
    let line_at = |i: usize| chars[..i].iter().filter(|c| **c == '\n').count() + 1;
    for (start, _) in chars.iter().enumerate().filter(|(_, c)| **c == '[') {
        let mut items = Vec::new();
        let mut i = start + 1;
        loop {
            while chars.get(i).is_some_and(|c| c.is_whitespace()) {
                i += 1;
            }
            match chars.get(i) {
                None => break,
                Some(']') => break,
                Some(q @ ('"' | '\'')) => {
                    let q = *q;
                    let mut item = String::new();
                    i += 1;
                    while let Some(&c) = chars.get(i) {
                        i += 1;
                        if c == q {
                            break;
                        }
                        item.push(c);
                    }
                    items.push(item);
                }
                Some(_) => {
                    let mut item = String::from("$");
                    let mut depth = 0usize;
                    while let Some(&c) = chars.get(i) {
                        match c {
                            ',' | ']' if depth == 0 => break,
                            '(' | '[' | '{' => depth += 1,
                            ')' | ']' | '}' => depth = depth.saturating_sub(1),
                            _ => {}
                        }
                        item.push(c);
                        i += 1;
                    }
                    items.push(item.trim_end().to_owned());
                }
            }
            if items.first().is_some_and(|f| f != "cargo") {
                break;
            }
            while chars.get(i).is_some_and(|c| c.is_whitespace()) {
                i += 1;
            }
            match chars.get(i) {
                Some(',') => i += 1,
                _ => break,
            }
        }
        if items.first().is_some_and(|f| f == "cargo") {
            out.push((line_at(start), items.split_off(1)));
        }
    }
    out
}

/// Every cargo command of `text`, with the line it starts on: lines ending
/// in `\` go on.
fn commands_in(text: &str) -> Vec<(usize, Vec<String>)> {
    let mut out = Vec::new();
    let mut logical = String::new();
    let mut first = 0;
    for (n, line) in text.lines().enumerate() {
        if logical.is_empty() {
            first = n + 1;
        }
        let trimmed = line.trim_end();
        if let Some(head) = trimmed.strip_suffix('\\') {
            logical.push_str(head);
            logical.push(' ');
            continue;
        }
        logical.push_str(trimmed);
        let mut commands = Vec::new();
        cargo_commands(&std::mem::take(&mut logical), &mut commands);
        out.extend(commands.into_iter().map(|c| (first, c)));
    }
    if !logical.is_empty() {
        let mut commands = Vec::new();
        cargo_commands(&logical, &mut commands);
        out.extend(commands.into_iter().map(|c| (first, c)));
    }
    out
}

/// Every cargo command of the doc comments (`//!`, `///`) of a Rust
/// `source`, with the line its paragraph starts on: a code span may run
/// over lines, as rustdoc reads it; a fenced block is read line by line.
fn doc_commands(source: &str) -> Vec<(usize, Vec<String>)> {
    let mut out = Vec::new();
    let mut prose = String::new();
    let mut first = 0;
    let mut fenced = false;
    let flush = |prose: &mut String, first: usize, out: &mut Vec<(usize, Vec<String>)>| {
        for span in prose.split('`').skip(1).step_by(2) {
            let mut commands = Vec::new();
            cargo_commands(span, &mut commands);
            out.extend(commands.into_iter().map(|c| (first, c)));
        }
        prose.clear();
    };
    for (n, line) in source.lines().enumerate() {
        let t = line.trim_start();
        let doc = t
            .strip_prefix("//!")
            .or_else(|| t.strip_prefix("///").filter(|_| !t.starts_with("////")));
        let Some(text) = doc else {
            flush(&mut prose, first, &mut out);
            fenced = false;
            continue;
        };
        if text.trim_start().starts_with("```") {
            flush(&mut prose, first, &mut out);
            fenced = !fenced;
        } else if fenced {
            out.extend(commands_in(text).into_iter().map(|(_, c)| (n + 1, c)));
        } else if text.trim().is_empty() {
            flush(&mut prose, first, &mut out);
        } else {
            if prose.is_empty() {
                first = n + 1;
            }
            prose.push_str(text);
            prose.push(' ');
        }
    }
    flush(&mut prose, first, &mut out);
    out
}

/// Every cargo command in the workflows and the rest of `.github/`, the CI
/// scripts, the Dockerfile, the docs, the skills, the examples, the crates'
/// doc comments and Python scripts (a call written as a list too) selects
/// packages, features, binaries and test programs the workspace has: chiefly
/// a reference left to a renamed package. The binary `agentfs` is package
/// `agentfs-cli`'s; the package `agentfs` is the library crate. In a file
/// that runs, every selection is a literal name, so this check can read it,
/// and `cargo build` selects only packages with a binary, so a stale
/// `-p agentfs` building the library crate is refused there. Not read: cargo
/// run through a variable (`$CARGO`), a cargo call built by other code than
/// one list, and a command in a Rust comment that is not a doc comment.
#[test]
fn cargo_commands_name_what_the_workspace_has() {
    let workspace = Workspace::load();
    let root = root();
    let mut problems = Vec::new();
    let mut commands = 0;
    for path in command_files() {
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let shown = path
            .strip_prefix(&root)
            .unwrap_or(&path)
            .display()
            .to_string();
        let mut found = if path.extension().is_some_and(|e| e == "rs") {
            doc_commands(&text)
        } else {
            commands_in(&text)
        };
        if path.extension().is_some_and(|e| e == "py") {
            found.extend(list_commands(&text));
        }
        for (line, words) in found {
            commands += 1;
            let s = selection(&words);
            let problems_here = selection_problems(&workspace, &s)
                .into_iter()
                .chain(script_problems(&workspace, &path, &s));
            for why in problems_here {
                problems.push(format!("{shown}:{line}: cargo {}: {why}", words.join(" ")));
            }
        }
    }
    assert!(
        commands > 100,
        "only {commands} cargo commands found: is the scan broken?"
    );
    assert!(problems.is_empty(), "{}", problems.join("\n"));
}

/// A fixture member of `package` with a binary `bin` and features.
fn binary(package: &str, bin: &str, features: &[&str]) -> Member {
    let mut m = member(package, "filesystem", &[]);
    m.targets = vec![Target {
        name: bin.to_owned(),
        kinds: vec!["bin".into()],
    }];
    m.features = features
        .iter()
        .map(|f| ((*f).to_owned(), Vec::new()))
        .collect();
    m
}

/// Today's fixture lines, each with the selection it yields.
const LINES: &[(&str, &[&str], &[&str])] = &[
    (
        "      - run: cargo build --locked -p agentfs-cli --features otlp-tls",
        &["agentfs-cli"],
        &["otlp-tls"],
    ),
    (
        "`cargo build -p agentfs-cli --features ssh`",
        &["agentfs-cli"],
        &["ssh"],
    ),
    (
        "| `cargo test -p agentfs-cli`, its build and run |",
        &["agentfs-cli"],
        &[],
    ),
    (
        r#"    fail "$bin is missing: cargo build --release -p agentfs-cli -p agentfs-csi""#,
        &["agentfs-cli", "agentfs-csi"],
        &[],
    ),
    (
        " && cargo build --release --locked -p agentfs-cli -p agentfs-csi --features agentfs-cli/https,agentfs-cli/ssh,agentfs-cli/otlp-tls \\\n && touch x",
        &["agentfs-cli", "agentfs-csi"],
        &[
            "agentfs-cli/https",
            "agentfs-cli/ssh",
            "agentfs-cli/otlp-tls",
        ],
    ),
];

fn one_selection(line: &str) -> Selection {
    let commands = commands_in(line);
    assert_eq!(commands.len(), 1, "{line}: {commands:?}");
    selection(&commands[0].1)
}

#[test]
fn cargo_commands_in_doc_comments_are_read_across_lines() {
    let source = [
        "//! Needs the feature: `cargo test -p agentfs-core",
        "//! --features failpoints --test it tree_verify::`. It's",
        "//! `cargo nextest run -p a`'s too.",
        "//!",
        "//! ```sh",
        "//! cargo build -p b",
        "//! cargo build -p c",
        "//! ```",
        "    /// `cargo run -p d`",
        "fn f() {} // `cargo run -p e`",
        "//// `cargo run -p f`",
    ]
    .join("\n");
    let found = doc_commands(&source);
    let packages: Vec<(usize, Vec<String>)> = found
        .iter()
        .map(|(n, words)| (*n, selection(words).packages))
        .collect();
    let p = |n: usize, s: &str| (n, vec![s.to_owned()]);
    assert_eq!(
        packages,
        [
            p(1, "agentfs-core"),
            p(1, "a"),
            p(6, "b"),
            p(7, "c"),
            p(9, "d")
        ]
    );
    assert_eq!(selection(&found[0].1).tests, ["it"]);
}

#[test]
fn cargo_commands_are_read_as_sh_reads_them() {
    for (line, packages, features) in LINES {
        let s = one_selection(line);
        assert_eq!(s.packages, *packages, "{line}");
        let named: Vec<String> = s
            .features
            .iter()
            .map(|(p, f)| p.as_ref().map_or(f.clone(), |p| format!("{p}/{f}")))
            .collect();
        assert_eq!(named, *features, "{line}");
    }
    let s = one_selection(
        "cargo nextest run -pagentfs-core --package=agentfs-fuse -p=x -p <crate> -F a,b --bin=y \
         --test it --test=z",
    );
    assert_eq!(s.subcommand.as_deref(), Some("nextest"));
    assert_eq!(s.packages, ["agentfs-core", "agentfs-fuse", "x"]);
    assert_eq!(s.variables, ["package"]);
    assert_eq!(s.features, [(None, "a".into()), (None, "b".into())]);
    assert_eq!(s.bins, ["y"]);
    assert_eq!(s.tests, ["it", "z"]);
    assert_eq!(commands_in("cargo fmt; cargo test -p a | cargo x").len(), 3);
    assert!(commands_in("~/.cargo/bin and cargo-nextest").is_empty());
}

#[test]
fn cargo_commands_are_refused_where_the_workspace_lacks_their_names() {
    let refused = |workspace: &Workspace, line: &str| {
        !selection_problems(workspace, &one_selection(line)).is_empty()
    };
    // Before the library crate: the binary `agentfs` is package
    // `agentfs-cli`'s, with a test program; no package is named `agentfs`.
    let mut cli = binary("agentfs-cli", "agentfs", &["https", "ssh", "otlp-tls"]);
    cli.targets.push(Target {
        name: "csi_trees".into(),
        kinds: vec!["test".into()],
    });
    let now = Workspace {
        ranks: ranks(),
        members: vec![cli, binary("agentfs-csi", "agentfs-csi", &[])],
    };
    // Once the library crate is `agentfs`: a library with no features.
    let mut front = Workspace {
        ranks: ranks(),
        members: now.members.clone(),
    };
    front.members.push(member("agentfs", "umbrella", &[]));
    for (line, ..) in LINES {
        assert!(!refused(&now, line), "{line}");
        assert!(!refused(&front, line), "{line}");
    }
    // A reference left to the old package: refused before the library
    // crate; once it has the name, only where it asks for what it lacks.
    for (line, on_front) in [
        ("cargo build -p agentfs", false),
        ("cargo test --doc -p agentfs", false),
        ("cargo build --features agentfs/https", true),
        ("cargo run -p agentfs -- init", true),
        (
            "cargo nextest run -p agentfs --bin agentfs reference_tests",
            true,
        ),
        ("cargo nextest run -p agentfs --test csi_trees", true),
    ] {
        assert!(refused(&now, line), "{line}");
        assert_eq!(refused(&front, line), on_front, "{line}");
    }
    for line in [
        "cargo build --features agentfs/tls",
        "cargo build -p agentfs-csi --features otlp-tls",
    ] {
        assert!(refused(&front, line), "{line}");
    }
    assert!(!refused(
        &now,
        "cargo nextest run -p agentfs-cli --bin agentfs reference_tests"
    ));
    assert!(!refused(&now, "cargo run -p agentfs-cli -- init"));
    assert!(!refused(
        &now,
        "cargo nextest run -p agentfs-cli --test csi_trees"
    ));
    assert!(!refused(&now, "cargo test --test=csi_trees"));
    assert!(refused(
        &now,
        "cargo nextest run -p agentfs-csi --test csi_trees"
    ));
    assert!(refused(
        &now,
        "cargo nextest run -p agentfs-cli --test=tree_stage"
    ));
    assert!(refused(&now, "cargo test --test nowhere"));
    assert!(!refused(&now, "cargo build --bin agentfs-csi"));
    // A selection through a variable: no name to judge, but a file that
    // runs must name it.
    let script = |workspace: &Workspace, path: &str, line: &str| {
        script_problems(workspace, Path::new(path), &one_selection(line))
    };
    for (line, kind) in [
        (r#"cargo test --locked -p "$p" --no-run"#, "package"),
        (
            r#"cargo test --locked -p agentfs-cli --test "$1" --no-run"#,
            "test program",
        ),
        ("cargo build -p agentfs-cli --bin=$bin", "binary"),
        ("cargo build -p agentfs-cli --features ssh,$f", "feature"),
    ] {
        let variable = one_selection(line);
        assert_eq!(variable.variables, [kind], "{line}");
        assert!(selection_problems(&now, &variable).is_empty(), "{line}");
        assert_eq!(script(&now, "ci/x.sh", line).len(), 1, "{line}");
        assert_eq!(script(&now, "ci/x.py", line).len(), 1, "{line}");
        assert!(script(&now, "docs/x.md", line).is_empty(), "{line}");
        assert!(
            script(&now, "crates/x/src/lib.rs", line).is_empty(),
            "{line}"
        );
        assert!(
            script(&now, ".github/ISSUE_TEMPLATE/bug_report.yml", line).is_empty(),
            "{line}"
        );
    }
    assert!(
        script(
            &now,
            "ci/x.sh",
            "cargo test -p agentfs-cli --test csi_trees"
        )
        .is_empty()
    );
    // A script's `cargo build` builds something to run: a stale
    // `-p agentfs` building the library front is refused there.
    assert!(script(&now, "ci/x.sh", "cargo build -p agentfs-cli -p agentfs-csi").is_empty());
    for line in [
        "cargo build -p agentfs",
        "cargo build -p agentfs -p agentfs-csi",
    ] {
        assert_eq!(script(&front, "ci/x.sh", line).len(), 1, "{line}");
        assert!(script(&front, "docs/x.md", line).is_empty(), "{line}");
    }
    assert!(script(&front, "ci/x.sh", "cargo test -p agentfs").is_empty());
}

#[test]
fn cargo_calls_written_as_python_lists_are_read() {
    let text = [
        "CARGO = [\"cargo\", \"test\", \"--locked\", \"-p\", \"agentfs-model\", \"--test\", \"scenarios\",",
        "         \"--test\", \"interleavings\"]",
        "run(['cargo', 'build', '-p', pkg, \"--bin=x\"], cwd=ROOT)",
        "subprocess.run([\"git\", \"cargo\"], [f(a, b)])",
        "x = [\"cargo\"]",
    ]
    .join("\n");
    let found = list_commands(&text);
    let words = |w: &[&str]| w.iter().map(|w| (*w).to_owned()).collect::<Vec<_>>();
    assert_eq!(
        found,
        [
            (
                1,
                words(&[
                    "test",
                    "--locked",
                    "-p",
                    "agentfs-model",
                    "--test",
                    "scenarios",
                    "--test",
                    "interleavings"
                ])
            ),
            (3, words(&["build", "-p", "$pkg", "--bin=x"])),
            (5, Vec::new()),
        ]
    );
    let s = selection(&found[1].1);
    assert_eq!(s.variables, ["package"]);
    assert_eq!(s.bins, ["x"]);
    assert_eq!(selection(&found[0].1).tests, ["scenarios", "interleavings"]);
}

/// Every `.rs` file under the `src` of each member, but `except`'s, with its
/// path from the root and its text.
fn member_sources(workspace: &Workspace, except: &str) -> Vec<(String, String)> {
    let root = root();
    let mut sources = Vec::new();
    for member in workspace.members.iter().filter(|m| m.name != except) {
        let src = root.join(&member.dir).join("src");
        if !src.is_dir() {
            continue;
        }
        let mut files = Vec::new();
        files_under(&src, &mut files);
        files.sort();
        for path in files
            .into_iter()
            .filter(|p| p.extension().is_some_and(|e| e == "rs"))
        {
            let text = std::fs::read_to_string(&path).unwrap();
            let shown = path
                .strip_prefix(&root)
                .unwrap_or(&path)
                .display()
                .to_string();
            sources.push((shown, text));
        }
    }
    sources
}

/// Where the failpoint calls in `sources` break the registry's rules: a
/// call to `hit` with a literal name not written `agentfs_failpoints::hit(`
/// (so the scan cannot miss one imported or renamed), a name `points` does
/// not list, a listed point no source hits, and a point listed twice.
fn hit_problems(sources: &[(String, String)], points: &[&str]) -> Vec<String> {
    const CALL: &str = "hit(\"";
    const PATH: &str = "agentfs_failpoints::";
    let mut problems = Vec::new();
    let mut found = BTreeSet::new();
    for (shown, text) in sources {
        for (at, _) in text.match_indices(CALL) {
            let before = &text[..at];
            // Another function or a method ending in `hit`.
            if before
                .chars()
                .next_back()
                .is_some_and(|c| c.is_alphanumeric() || c == '_' || c == '.')
            {
                continue;
            }
            let line = before.matches('\n').count() + 1;
            if !before.ends_with(PATH) {
                problems.push(format!(
                    "{shown}:{line}: a failpoint is passed as `hit(`: write `{PATH}hit(`"
                ));
            }
            let rest = &text[at + CALL.len()..];
            let name = &rest[..rest.find('"').unwrap_or(rest.len())];
            if !points.contains(&name) {
                problems.push(format!(
                    "{shown}:{line}: failpoint {name:?} is not in agentfs_failpoints::POINTS"
                ));
            }
            found.insert(name.to_owned());
        }
    }
    let mut listed = BTreeSet::new();
    for point in points {
        if !listed.insert(*point) {
            problems.push(format!("failpoint {point:?} is listed twice in POINTS"));
        }
        if !found.contains(*point) {
            problems.push(format!(
                "failpoint {point:?} is listed in POINTS but never hit"
            ));
        }
    }
    problems
}

/// `agentfs_failpoints::POINTS` is exactly the set of failpoint calls in
/// the workspace's sources, each written `agentfs_failpoints::hit("name")`,
/// so a renamed, removed or unlisted point fails here.
#[test]
fn every_hit_is_listed() {
    let workspace = Workspace::load();
    let sources = member_sources(&workspace, "agentfs-failpoints");
    assert!(
        sources
            .iter()
            .any(|(_, text)| text.contains("agentfs_failpoints::hit(\"")),
        "no failpoint call found: is the scan broken?"
    );
    let problems = hit_problems(&sources, agentfs_failpoints::POINTS);
    assert!(problems.is_empty(), "{}", problems.join("\n"));
}

#[test]
fn a_failpoint_off_the_list_is_found() {
    let source = |text: &str| vec![("a.rs".to_owned(), text.to_owned())];
    let listed =
        source("agentfs_failpoints::hit(\"tree.a\")?; cache.hit(\"x\"); cache_hit(\"y\");");
    assert!(hit_problems(&listed, &["tree.a"]).is_empty());
    assert_eq!(
        hit_problems(&source("agentfs_failpoints::hit(\"os.x\")?;"), &["tree.a"]),
        [
            "a.rs:1: failpoint \"os.x\" is not in agentfs_failpoints::POINTS",
            "failpoint \"tree.a\" is listed in POINTS but never hit",
        ]
    );
    assert_eq!(
        hit_problems(
            &source("use agentfs_failpoints::hit;\nhit(\"tree.a\")?;"),
            &["tree.a"]
        ),
        ["a.rs:2: a failpoint is passed as `hit(`: write `agentfs_failpoints::hit(`"]
    );
    assert_eq!(
        hit_problems(&source("crate::failpoints::hit(\"tree.a\")?;"), &["tree.a"]),
        ["a.rs:1: a failpoint is passed as `hit(`: write `agentfs_failpoints::hit(`"]
    );
    assert_eq!(
        hit_problems(&listed, &["tree.a", "tree.a"]),
        ["failpoint \"tree.a\" is listed twice in POINTS"]
    );
}

/// A source's production text: up to its first test module, a `mod`
/// under `#[cfg(test)]` or `#[cfg(all(test, ...))]` (other attributes may
/// come between). A test-only item that is not a module is read: the scan
/// stops at the module, not at the first test attribute.
fn production(source: &str) -> &str {
    let mut end = source.len();
    for marker in ["#[cfg(test)]", "#[cfg(all(test"] {
        for (at, _) in source.match_indices(marker) {
            let item = source[at..]
                .lines()
                .skip(1)
                .map(str::trim_start)
                .find(|line| !line.is_empty() && !line.starts_with("#["));
            let item = item.map(|line| {
                line.strip_prefix("pub(crate) ")
                    .or_else(|| line.strip_prefix("pub "))
                    .unwrap_or(line)
            });
            if item.is_some_and(|line| line.starts_with("mod ")) {
                end = end.min(at);
                break;
            }
        }
    }
    &source[..end]
}

/// The ways a source starts a process: `Command`'s methods take no
/// argument (a thread scope's `spawn` takes one), and the same methods
/// passed as functions (a spawner handed to `agentfs_lines`' `_with`
/// calls). `agentfs_align::histogram`, which starts its own `git`, is
/// refused by clippy (`clippy.toml`), which resolves imports and aliases.
const STARTS: [&str; 6] = [
    ".spawn()",
    ".output()",
    ".status()",
    "Command::spawn",
    "Command::output",
    "Command::status",
];

/// How `source`'s production text starts a process itself.
fn starts_in(source: &str) -> Vec<&'static str> {
    let text = production(source);
    STARTS.into_iter().filter(|s| text.contains(s)).collect()
}

/// The tree engine starts each `git` child through
/// `agentfs_git::git_cli::counted`, the one site that counts it: none of
/// its production sources (agentfs-engine's `src`), nor the line maps'
/// (`agentfs-lines`, which take the spawner from their caller), starts a
/// process itself, but the history's repack task, whose `git repack` is not
/// on the actor's path and stays uncounted, so each operation's count of
/// Git children stays exact. `counted` is git_cli's one `.spawn()`.
#[test]
fn the_tree_engine_starts_git_only_through_counted() {
    let workspace = Workspace::load();
    let root = root();
    let dir = |name: &str| root.join(&workspace.member(name).unwrap().dir);
    let src = dir("agentfs-engine").join("src");
    let mut files = Vec::new();
    files_under(&src, &mut files);
    files_under(&dir("agentfs-lines").join("src"), &mut files);
    let repack = src.join("tree_actor/history/repack.rs");
    let mut problems = Vec::new();
    for file in files.iter().filter(|f| **f != repack) {
        let source = std::fs::read_to_string(file).unwrap();
        for start in starts_in(&source) {
            problems.push(format!(
                "{} starts a process with `{start}` outside git_cli::counted",
                file.strip_prefix(&root).unwrap_or(file).display()
            ));
        }
    }
    assert!(
        files.len() > 20,
        "only {} files read: is the scan broken?",
        files.len()
    );
    assert!(problems.is_empty(), "{}", problems.join("\n"));
    let git_cli = std::fs::read_to_string(dir("agentfs-git").join("src/git_cli.rs")).unwrap();
    assert_eq!(
        production(&git_cli).matches(".spawn()").count(),
        1,
        "git_cli::counted is the one site"
    );
}

#[test]
fn a_process_started_outside_counted_is_found() {
    assert!(starts_in("agentfs_git::git_cli::counted(&mut cmd)?; s.spawn(|| ());").is_empty());
    assert_eq!(starts_in("cmd.output()?;"), [".output()"]);
    assert_eq!(
        starts_in("let child = std::process::Command::spawn(&mut cmd)?;"),
        ["Command::spawn"]
    );
    assert_eq!(
        starts_in("histogram_files_with(&a, &b, Command::spawn)"),
        ["Command::spawn"]
    );
    assert!(starts_in("#[cfg(test)]\nmod tests { fn f() { cmd.status(); } }").is_empty());
    assert!(
        starts_in("#[cfg(all(test, feature = \"x\"))]\n#[allow(x)]\nmod t { s.output() }")
            .is_empty()
    );
    assert_eq!(
        starts_in("#[cfg(test)]\nconst X: u8 = 0;\nfn f() { cmd.output(); }"),
        [".output()"]
    );
}
