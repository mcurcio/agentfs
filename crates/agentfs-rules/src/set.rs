//! A volume's rule set: where each new name of a tree lives, recorded or in
//! the tree's untracked area (docs/trees.md, "Rules and the untracked
//! area"). Rules decide only for new names, as git's do for untracked
//! paths: a tracked path stays recorded whatever they say, so the caller
//! asks only when it makes a name.

use std::fmt;

use crate::operator::{AttrState, OperatorRules};

/// The operator's rule files as read: `info/exclude` and `info/attributes`.
/// A missing file is empty.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RuleTexts {
    pub exclude: Vec<u8>,
    pub attributes: Vec<u8>,
}

/// What a nested repository's files are: never recorded (`Exclude`, the
/// default), or the tree's like any other (`WorkingTree`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Nested {
    #[default]
    Exclude,
    WorkingTree,
}

/// The kind of entry a call makes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NewKind {
    File,
    Dir,
    Symlink,
    Fifo,
    Socket,
}

/// What the caller observed around the new name.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PathFacts {
    /// The directory the name is made in is itself untracked.
    pub parent_untracked: bool,
    /// A directory above the name, other than the tree's root, holds a
    /// `.git` entry.
    pub inside_repository: bool,
}

/// Where a new name lives.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Placement {
    /// In the tree: recorded.
    Tracked,
    /// In the tree's untracked area, for this reason.
    Untracked(Untracked),
}

/// Why a new name is not recorded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Untracked {
    /// Its directory is untracked.
    UnderUntrackedDir,
    /// A `.git` component: a repository's own directory or file.
    GitDir,
    /// A FIFO or a socket.
    Special,
    /// `-agentfs` in `info/attributes`.
    NeverRecord,
    /// Inside a nested repository, with [`Nested::Exclude`].
    InsideRepository,
    /// Excluded by `info/exclude`, by this pattern as written.
    Ignored { pattern: String },
}

impl fmt::Display for Untracked {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnderUntrackedDir => f.write_str("its directory is not recorded"),
            Self::GitDir => f.write_str("a .git entry is never recorded"),
            Self::Special => f.write_str("FIFOs and sockets are never recorded"),
            Self::NeverRecord => f.write_str("marked -agentfs in info/attributes"),
            Self::InsideRepository => f.write_str("inside a nested repository"),
            Self::Ignored { pattern } => write!(f, "ignored by info/exclude ({pattern})"),
        }
    }
}

/// A rule the tree views do not build, refused when the volume opens: a
/// rule silently ignored would record what the operator meant to keep out.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RulesError {
    #[error("info/attributes line {line}: {attribute} is not built for tree volumes")]
    NotBuilt { line: u32, attribute: String },
    #[error("info/attributes line {line}: {attribute}: {reason}")]
    BadValue {
        line: u32,
        attribute: String,
        reason: String,
    },
}

/// The record attribute: `-agentfs` keeps a path out of history.
const RECORD: &str = "agentfs";

/// One volume's rules, built once when it opens. `Send + Sync`.
#[derive(Debug)]
pub struct RuleSet {
    operator: OperatorRules,
    nested: Nested,
}

impl RuleSet {
    /// Builds the rule set from the operator's files.
    ///
    /// # Errors
    ///
    /// [`RulesError::NotBuilt`] for any `agentfs.<name>` attribute and for
    /// `filter=lfs`; [`RulesError::BadValue`] for `agentfs=<value>`. Other
    /// git attributes are accepted and unused.
    pub fn build(texts: &RuleTexts, nested: Nested) -> Result<Self, RulesError> {
        check_attributes(&texts.attributes)?;
        Ok(Self {
            operator: OperatorRules::parse(&texts.exclude, &texts.attributes),
            nested,
        })
    }

    /// No operator rule: only the built-ins place a name untracked.
    pub fn empty() -> Self {
        Self {
            operator: OperatorRules::parse(b"", b""),
            nested: Nested::Exclude,
        }
    }

    /// Where a new name lives. Judged in this order, first match wins: an
    /// untracked parent; a `.git` component; a FIFO or socket; `-agentfs`;
    /// inside a nested repository with [`Nested::Exclude`]; `info/exclude`;
    /// else tracked. Pure and total.
    pub fn place(&self, path: &str, kind: NewKind, facts: PathFacts) -> Placement {
        let untracked = if facts.parent_untracked {
            Untracked::UnderUntrackedDir
        } else if path.split('/').any(|component| component == ".git") {
            Untracked::GitDir
        } else if matches!(kind, NewKind::Fifo | NewKind::Socket) {
            Untracked::Special
        } else if self.never_records(path) {
            Untracked::NeverRecord
        } else if facts.inside_repository && self.nested == Nested::Exclude {
            Untracked::InsideRepository
        } else if let Some(pattern) = self.operator.ignored(path, kind == NewKind::Dir) {
            Untracked::Ignored { pattern }
        } else {
            return Placement::Tracked;
        };
        Placement::Untracked(untracked)
    }

    /// Whether `-agentfs` marks `path`.
    pub fn never_records(&self, path: &str) -> bool {
        self.operator.attributes(path).get(RECORD) == Some(&Some(AttrState::Unset))
    }
}

/// Refuses every attribute assignment the tree views do not build, line by
/// line, macro definitions included.
fn check_attributes(text: &[u8]) -> Result<(), RulesError> {
    for (i, line) in text.split(|b| *b == b'\n').enumerate() {
        let line_no = u32::try_from(i + 1).unwrap_or(u32::MAX);
        let line = String::from_utf8_lossy(line);
        let line = line.trim_start();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some(rest) = after_pattern(line) else {
            continue;
        };
        for token in rest.split_whitespace() {
            let bare = token.trim_start_matches(['-', '!']);
            let (name, value) = match bare.split_once('=') {
                Some((name, value)) => (name, Some(value)),
                None => (bare, None),
            };
            if name.starts_with("agentfs.") {
                return Err(RulesError::NotBuilt {
                    line: line_no,
                    attribute: token.to_owned(),
                });
            }
            if name == "filter" && value == Some("lfs") {
                return Err(RulesError::NotBuilt {
                    line: line_no,
                    attribute: token.to_owned(),
                });
            }
            if name == RECORD && value.is_some() {
                return Err(RulesError::BadValue {
                    line: line_no,
                    attribute: token.to_owned(),
                    reason: "agentfs takes no value: use -agentfs".to_owned(),
                });
            }
        }
    }
    Ok(())
}

/// What follows a line's pattern: a quoted pattern (C-style escapes), or one
/// up to whitespace. `None` for a line with no attribute.
fn after_pattern(line: &str) -> Option<&str> {
    if let Some(quoted) = line.strip_prefix('"') {
        let mut escaped = false;
        let end = quoted.char_indices().find_map(|(j, c)| {
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == '"' {
                return Some(j + 1);
            }
            None
        })?;
        Some(&quoted[end..])
    } else {
        line.find(char::is_whitespace).map(|end| &line[end..])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(exclude: &str, attributes: &str) -> RuleSet {
        RuleSet::build(
            &RuleTexts {
                exclude: exclude.as_bytes().to_vec(),
                attributes: attributes.as_bytes().to_vec(),
            },
            Nested::Exclude,
        )
        .unwrap()
    }

    fn untracked(why: Untracked) -> Placement {
        Placement::Untracked(why)
    }

    const NONE: PathFacts = PathFacts {
        parent_untracked: false,
        inside_repository: false,
    };

    #[test]
    fn placement_is_judged_in_order() {
        let rules = set("*.db\nlogs/\n", "*.db -agentfs\n");
        let under = PathFacts {
            parent_untracked: true,
            inside_repository: true,
        };
        assert_eq!(
            rules.place("a/.git", NewKind::Fifo, under),
            untracked(Untracked::UnderUntrackedDir)
        );
        assert_eq!(
            rules.place("a/.git", NewKind::Fifo, NONE),
            untracked(Untracked::GitDir)
        );
        assert_eq!(
            rules.place("s.db", NewKind::Socket, NONE),
            untracked(Untracked::Special)
        );
        let inside = PathFacts {
            inside_repository: true,
            ..NONE
        };
        assert_eq!(
            rules.place("r/s.db", NewKind::File, inside),
            untracked(Untracked::NeverRecord)
        );
        assert_eq!(
            rules.place("r/logs", NewKind::Dir, inside),
            untracked(Untracked::InsideRepository)
        );
        assert_eq!(
            rules.place("logs", NewKind::Dir, NONE),
            untracked(Untracked::Ignored {
                pattern: "logs/".into()
            })
        );
        assert_eq!(rules.place("logs", NewKind::File, NONE), Placement::Tracked);
        assert_eq!(
            rules.place("notes.md", NewKind::File, NONE),
            Placement::Tracked
        );
    }

    #[test]
    fn a_git_component_at_any_depth_is_untracked() {
        let rules = RuleSet::empty();
        for path in [".git", ".git/config", "a/b/.git", "a/.git/objects/x"] {
            assert_eq!(
                rules.place(path, NewKind::File, NONE),
                untracked(Untracked::GitDir),
                "{path}"
            );
        }
        for path in [".gitignore", "a/.github/x", "x.git", "git"] {
            assert_eq!(
                rules.place(path, NewKind::File, NONE),
                Placement::Tracked,
                "{path}"
            );
        }
    }

    #[test]
    fn nested_exclude_against_working_tree() {
        let inside = PathFacts {
            inside_repository: true,
            ..NONE
        };
        assert_eq!(
            RuleSet::empty().place("repo/x", NewKind::File, inside),
            untracked(Untracked::InsideRepository)
        );
        let working = RuleSet::build(&RuleTexts::default(), Nested::WorkingTree).unwrap();
        assert_eq!(
            working.place("repo/x", NewKind::File, inside),
            Placement::Tracked
        );
        assert_eq!(
            working.place("repo/.git", NewKind::Dir, inside),
            untracked(Untracked::GitDir),
            ".git stays out under WorkingTree"
        );
    }

    #[test]
    fn a_reinclude_under_an_excluded_directory_stays_excluded() {
        let rules = set("build/\n!build/keep\n", "");
        assert_eq!(
            rules.place("build/keep", NewKind::File, NONE),
            untracked(Untracked::Ignored {
                pattern: "build/".into()
            })
        );
    }

    #[test]
    fn matching_is_case_sensitive() {
        let rules = set("*.LOG\n", ".ENV -agentfs\n");
        assert_eq!(
            rules.place("a.log", NewKind::File, NONE),
            Placement::Tracked
        );
        assert_eq!(rules.place(".env", NewKind::File, NONE), Placement::Tracked);
        assert_eq!(
            rules.place(".ENV", NewKind::File, NONE),
            untracked(Untracked::NeverRecord)
        );
    }

    #[test]
    fn rules_not_built_are_refused_with_their_line() {
        let refuse = |attributes: &str| {
            RuleSet::build(
                &RuleTexts {
                    exclude: Vec::new(),
                    attributes: attributes.as_bytes().to_vec(),
                },
                Nested::Exclude,
            )
            .unwrap_err()
        };
        assert_eq!(
            refuse("*.db agentfs.group=x\n"),
            RulesError::NotBuilt {
                line: 1,
                attribute: "agentfs.group=x".into()
            }
        );
        assert_eq!(
            refuse("# c\n\n*.bin -diff filter=lfs\n"),
            RulesError::NotBuilt {
                line: 3,
                attribute: "filter=lfs".into()
            }
        );
        assert_eq!(
            refuse("*.x -agentfs\n\"a b\" -agentfs.maxSize\n"),
            RulesError::NotBuilt {
                line: 2,
                attribute: "-agentfs.maxSize".into()
            }
        );
        assert!(matches!(
            refuse("[attr]db agentfs.commit=fsync\n"),
            RulesError::NotBuilt { line: 1, .. }
        ));
        assert!(matches!(
            refuse("x\n*.db agentfs=no\n"),
            RulesError::BadValue { line: 2, .. }
        ));
        assert_eq!(
            refuse("*.db agentfs.group=x\n").to_string(),
            "info/attributes line 1: agentfs.group=x is not built for tree volumes"
        );
        // Accepted: -agentfs, !agentfs, agentfs, other attributes.
        set(
            "",
            "*.db -agentfs\n*.md agentfs\n*.x !agentfs text diff=y filter=other\n",
        );
    }

    #[test]
    fn empty_rules_place_only_the_built_ins() {
        let rules = set("", "");
        assert_eq!(
            rules.place("a/b.log", NewKind::File, NONE),
            Placement::Tracked
        );
        assert!(!rules.never_records("a"));
    }

    #[test]
    fn untracked_reasons_read_plainly() {
        assert_eq!(
            Untracked::NeverRecord.to_string(),
            "marked -agentfs in info/attributes"
        );
        assert_eq!(
            Untracked::Ignored {
                pattern: "*.log".into()
            }
            .to_string(),
            "ignored by info/exclude (*.log)"
        );
    }

    #[test]
    fn a_rule_set_is_shared_across_threads() {
        fn shared<T: Send + Sync>() {}
        shared::<RuleSet>();
    }
}
