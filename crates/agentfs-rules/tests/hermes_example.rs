//! The Hermes example's rules (deploy/examples/hermes) on a tree volume:
//! secrets, databases, sessions, logs and checkpoints stay out; the agent's
//! definitions are recorded.

use agentfs_rules::{Nested, NewKind, PathFacts, Placement, RuleSet, RuleTexts};

fn hermes() -> RuleSet {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../deploy/examples/hermes");
    let texts = RuleTexts {
        exclude: std::fs::read(dir.join("exclude")).unwrap(),
        attributes: std::fs::read(dir.join("attributes")).unwrap(),
    };
    RuleSet::build(&texts, Nested::Exclude).unwrap()
}

/// Where `path` lands when each directory above it is made first, as a
/// workload makes them: a directory the rules leave out takes its children
/// with it.
fn place_new(rules: &RuleSet, path: &str) -> Placement {
    let mut parent_untracked = false;
    let parts: Vec<&str> = path.split('/').collect();
    for i in 1..parts.len() {
        let dir = parts[..i].join("/");
        let facts = PathFacts {
            parent_untracked,
            inside_repository: false,
        };
        parent_untracked = rules.place(&dir, NewKind::Dir, facts) != Placement::Tracked;
    }
    let facts = PathFacts {
        parent_untracked,
        inside_repository: false,
    };
    rules.place(path, NewKind::File, facts)
}

#[test]
fn secrets_databases_sessions_and_logs_stay_out() {
    let rules = hermes();
    for path in [
        ".env",
        "auth.json",
        "state.db",
        "state.db-wal",
        "sessions/x.json",
        "logs/a.log",
        "checkpoints/x",
        "memories/MEMORY.md",
        "profiles/p/sessions/x",
    ] {
        assert!(
            matches!(place_new(&rules, path), Placement::Untracked(_)),
            "{path} must not be recorded"
        );
    }
}

#[test]
fn the_agents_definitions_are_recorded() {
    let rules = hermes();
    for path in [
        "SOUL.md",
        "config.yaml",
        "skills/s/SKILL.md",
        "profiles/p/SOUL.md",
        "profiles/p/config.yaml",
        "profiles/p/skills/s/x",
    ] {
        assert_eq!(place_new(&rules, path), Placement::Tracked, "{path}");
    }
}

#[test]
fn secrets_are_marked_never_record_even_where_the_allowlist_would_take_them() {
    let rules = hermes();
    for path in [
        ".env",
        "skills/s/.env",
        "skills/auth.json",
        "profiles/p/x.db",
    ] {
        assert!(rules.never_records(path), "{path}");
    }
}
