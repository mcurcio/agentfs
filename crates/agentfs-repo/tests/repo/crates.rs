//! The workspace's crates: their dependencies against the ranks
//! (docs/architecture.md, "Ranks"), the `failpoints` feature forwarded to
//! every dependency that has it, and every cargo command the repository
//! writes down naming what the workspace has. Each check is a pure function
//! over a [`Workspace`] or a line, with a fixture test beside it.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use agentfs_repo::workspace::{Dependency, DependencyKind, Member, Rank, Target, Workspace};

use crate::{Comments, files_under, is_separator, lex, root};

/// Where `workspace` breaks its ranks: a rank listed twice, a rank using
/// one listed after it, a member naming no rank or an unknown one, and a
/// normal or build dependency on a member in a rank its own rank does not
/// use. Dev-dependencies are exempt.
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
            if dependency.kind == DependencyKind::Dev {
                continue;
            }
            let Some(their) = workspace
                .member(&dependency.name)
                .and_then(|d| d.rank.as_deref())
                .filter(|r| workspace.rank(r).is_some())
            else {
                continue;
            };
            if !rank.uses.iter().any(|u| u == their) {
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
        (
            "filesystem",
            &["leaf", "shared-kernel", "domain", "engine", "filesystem"],
        ),
        (
            "kubernetes",
            &["leaf", "shared-kernel", "engine", "filesystem"],
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
/// shared kernel, two domains, the engine, the FUSE layer, the CSI driver.
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
    // A test may drive anything.
    let mut dev = today();
    dev.members[0].dependencies.push(Dependency {
        name: "agentfs-csi".into(),
        kind: DependencyKind::Dev,
    });
    assert_eq!(rank_problems(&dev), Vec::<String>::new());
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
/// (`--features`, `-F`) and the binaries (`--bin`). Only values made of
/// `[A-Za-z0-9_-]` are kept: `<crate>` or `$pkg` is no name.
#[derive(Debug, Default, PartialEq, Eq)]
struct Selection {
    subcommand: Option<String>,
    packages: Vec<String>,
    /// Whether every package selection was a literal name.
    literal: bool,
    /// `(Some(package), feature)` for `pkg/feat`, `(None, feature)` bare.
    features: Vec<(Option<String>, String)>,
    bins: Vec<String>,
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
    let mut s = Selection {
        literal: true,
        ..Selection::default()
    };
    let package = |s: &mut Selection, value: &str| {
        if is_name(value) {
            s.packages.push(value.to_owned());
        } else {
            s.literal = false;
        }
    };
    let features = |s: &mut Selection, value: &str| {
        for item in value.split(|c: char| c == ',' || c.is_whitespace()) {
            match item.split_once('/') {
                Some((p, f)) if is_name(p) && is_name(f) => {
                    s.features.push((Some(p.to_owned()), f.to_owned()))
                }
                None if is_name(item) => s.features.push((None, item.to_owned())),
                _ => {}
            }
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
                if let Some(v) = next.filter(|v| is_name(v)) {
                    s.bins.push(v.to_owned());
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
                    if is_name(v) {
                        s.bins.push(v.to_owned());
                    }
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
/// only when every selection is literal), a binary of no selected package
/// (of no member when none is selected), or `cargo run` of a package with
/// no binary.
fn selection_problems(workspace: &Workspace, s: &Selection) -> Vec<String> {
    let mut problems = Vec::new();
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
    let has_bin = |m: &Member, bin: Option<&str>| {
        m.targets
            .iter()
            .any(|t| t.kinds.iter().any(|k| k == "bin") && bin.is_none_or(|b| t.name == b))
    };
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
            None if s.literal
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
    if s.literal {
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

/// The files whose cargo commands are read.
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
    let mut workflows = Vec::new();
    files_under(&root.join(".github/workflows"), &mut workflows);
    files.extend(workflows);
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
    for file in ["Dockerfile", "AGENTS.md", "README.md", "CONTRIBUTING.md"] {
        files.push(root.join(file));
    }
    files.sort();
    files
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

/// Every cargo command in the workflows, the CI scripts, the Dockerfile,
/// the docs, the skills and the examples selects packages, features and
/// binaries the workspace has: chiefly a reference meant for the binary
/// once its package is renamed. After the library front takes the name
/// `agentfs`, a bare `cargo build -p agentfs` meant for the binary is, in
/// form, a correct use of the front: this check cannot see it.
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
        for (line, words) in commands_in(&text) {
            commands += 1;
            for why in selection_problems(&workspace, &selection(&words)) {
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

/// A fixture member with a binary and features.
fn binary(name: &str, features: &[&str]) -> Member {
    let mut m = member(name, "filesystem", &[]);
    m.targets = vec![Target {
        name: name.to_owned(),
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
        "      - run: cargo build --locked -p agentfs --features otlp-tls",
        &["agentfs"],
        &["otlp-tls"],
    ),
    (
        "`cargo build -p agentfs --features ssh`",
        &["agentfs"],
        &["ssh"],
    ),
    (
        "| `cargo test -p agentfs`, its build and run |",
        &["agentfs"],
        &[],
    ),
    (
        r#"    fail "$bin is missing: cargo build --release -p agentfs -p agentfs-csi""#,
        &["agentfs", "agentfs-csi"],
        &[],
    ),
    (
        " && cargo build --release --locked -p agentfs -p agentfs-csi --features agentfs/https,agentfs/ssh,agentfs/otlp-tls \\\n && touch x",
        &["agentfs", "agentfs-csi"],
        &["agentfs/https", "agentfs/ssh", "agentfs/otlp-tls"],
    ),
];

fn one_selection(line: &str) -> Selection {
    let commands = commands_in(line);
    assert_eq!(commands.len(), 1, "{line}: {commands:?}");
    selection(&commands[0].1)
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
        "cargo nextest run -pagentfs-core --package=agentfs-fuse -p=x -p <crate> -F a,b --bin=y",
    );
    assert_eq!(s.subcommand.as_deref(), Some("nextest"));
    assert_eq!(s.packages, ["agentfs-core", "agentfs-fuse", "x"]);
    assert!(!s.literal);
    assert_eq!(s.features, [(None, "a".into()), (None, "b".into())]);
    assert_eq!(s.bins, ["y"]);
    assert_eq!(commands_in("cargo fmt; cargo test -p a | cargo x").len(), 3);
    assert!(commands_in("~/.cargo/bin and cargo-nextest").is_empty());
}

#[test]
fn cargo_commands_are_refused_where_the_workspace_lacks_their_names() {
    let refused = |workspace: &Workspace, line: &str| {
        !selection_problems(workspace, &one_selection(line)).is_empty()
    };
    let csi = binary("agentfs-csi", &[]);
    // Today: the binary's package is `agentfs`.
    let now = Workspace {
        ranks: ranks(),
        members: vec![
            binary("agentfs", &["https", "ssh", "otlp-tls"]),
            csi.clone(),
        ],
    };
    // Once the binary's package is renamed: no `agentfs` at all.
    let renamed = Workspace {
        ranks: ranks(),
        members: vec![
            binary("agentfs-cli", &["https", "ssh", "otlp-tls"]),
            csi.clone(),
        ],
    };
    // Once the library front is `agentfs`: a library with no features.
    let front = Workspace {
        ranks: ranks(),
        members: vec![
            member("agentfs", "filesystem", &[]),
            binary("agentfs-cli", &["https", "ssh", "otlp-tls"]),
            csi,
        ],
    };
    for (line, ..) in LINES {
        assert!(!refused(&now, line), "{line}");
        assert!(refused(&renamed, line), "{line}");
    }
    for (line, _, features) in LINES {
        assert_eq!(refused(&front, line), !features.is_empty(), "{line}");
    }
    for line in [
        "cargo nextest run -p agentfs --bin agentfs reference_tests",
        "cargo run -p agentfs -- init",
        "cargo build --features agentfs/tls",
        "cargo build -p agentfs-csi --features otlp-tls",
    ] {
        assert!(refused(&front, line), "{line}");
    }
    assert!(!refused(&front, "cargo test --doc -p agentfs"));
    assert!(!refused(
        &now,
        "cargo nextest run -p agentfs --bin agentfs reference_tests"
    ));
    assert!(!refused(&now, "cargo run -p agentfs -- init"));
    assert!(!refused(&now, "cargo build --bin agentfs-csi"));
    assert!(!refused(
        &renamed,
        "cargo nextest run -p $pkg --features failpoints"
    ));
}
