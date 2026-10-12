//! Path rules: decides whether a change to a path is recorded or skipped,
//! and why (design §4, §5).
//!
//! Rules follow git's model: ignore rules and nested-repository exclusion
//! only affect paths that are not tracked. The caller says whether a path is
//! tracked ([`crate::tracked`]); this module is pure policy over the rule
//! files and the backing tree.
//!
//! Sources (design §4.1):
//!
//! - **Operator rules**, `$GIT_DIR/info/exclude` and `$GIT_DIR/info/attributes`,
//!   always apply. They are read once, when the volume is opened; changing
//!   them takes a remount.
//! - **Tree rules**, `.gitignore` and `.gitattributes` files in the backing
//!   tree, apply only with `agentfs.respectTreeRules = true`. They are read
//!   on demand, never through a symlink and never past 1 MiB
//!   ([`crate::probe`]), and can never set `agentfs.*` attributes.
//! - System and user-global git config, excludes and attributes are never
//!   read: rule files are parsed from buffers this module reads itself, with
//!   the pure-Rust `gix-ignore` and `gix-attributes` parsers (design §4.4).
//!
//! Operator and tree ignore rules are separate layers and either one
//! excludes, so a tree `!pattern` cannot re-include an operator exclude.
//! Matching is always case-sensitive; `core.ignoreCase` is never consulted.

use std::collections::BTreeMap;
use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use bstr::ByteSlice;
use gix_attributes::StateRef;
use gix_attributes::search::{MetadataCollection, Outcome};
use gix_ignore::glob::pattern::Case;
use gix_ignore::search::Ignore;

use crate::checks::Refusal;
use crate::config::{Nested, Settings};
use crate::fsync_class::{self, FsyncClass};
use crate::nested::{self, NestedKind};
use crate::paths::RepoPath;
use crate::probe;

#[derive(Debug, thiserror::Error)]
pub enum RulesError {
    #[error("reading {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    /// An `agentfs.*` value in `info/attributes` that is not valid (design
    /// §4.2): the volume refuses to open rather than guess.
    #[error("info/attributes line {line}: {name}={value}: {reason}")]
    BadAttribute {
        line: usize,
        name: String,
        value: String,
        reason: String,
    },
}

fn io_error(path: impl Into<PathBuf>) -> impl FnOnce(io::Error) -> RulesError {
    let path = path.into();
    move |source| RulesError::Io { path, source }
}

/// The settings that shape rule evaluation (design §4.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RuleOptions {
    pub respect_tree_rules: bool,
    pub nested: Nested,
}

impl Default for RuleOptions {
    fn default() -> Self {
        Self {
            respect_tree_rules: false,
            nested: Nested::Exclude,
        }
    }
}

impl From<&Settings> for RuleOptions {
    fn from(s: &Settings) -> Self {
        Self {
            respect_tree_rules: s.respect_tree_rules,
            nested: s.nested,
        }
    }
}

/// Where an ignore rule came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RuleSource {
    /// `$GIT_DIR/info/exclude`.
    OperatorExclude,
    /// A `.gitignore` in the backing tree (the path of that file).
    TreeIgnore(RepoPath),
}

impl fmt::Display for RuleSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::OperatorExclude => f.write_str("info/exclude"),
            Self::TreeIgnore(file) => write!(f, "{file}"),
        }
    }
}

/// Why a change is not recorded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkipReason {
    /// The operator marked the path `-agentfs` (design §4.2).
    NeverRecord,
    /// An ignore rule matches the untracked path or one of its directories.
    Ignored { source: RuleSource, pattern: String },
    /// The untracked path is inside a nested repository (design §5).
    Nested { repo: RepoPath, kind: NestedKind },
    /// A removal of a path with nothing tracked at or below it.
    Untracked,
    /// git's fsck or a remote's limit would refuse the path (design §3.4,
    /// [`crate::checks`]).
    Refused(Refusal),
    /// A SQLite shared-memory file (`<stem>-shm`) of an fsync-class
    /// database: SQLite rebuilds it, so it is never recorded (design §8).
    SharedMemory,
    /// Another member of the path's fsync-class group was refused, and a
    /// group is recorded whole or not at all (design §8).
    GroupRefused { member: RepoPath },
}

impl fmt::Display for SkipReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NeverRecord => f.write_str("marked -agentfs in info/attributes"),
            Self::Ignored { source, pattern } => write!(f, "ignored by {source} ({pattern})"),
            Self::Nested { repo, kind } => {
                write!(f, "inside nested repository {repo} ({})", kind.describe())
            }
            Self::Untracked => f.write_str("nothing tracked at this path"),
            Self::Refused(refusal) => write!(f, "refused: {refusal}"),
            Self::SharedMemory => f.write_str("SQLite shared memory of an fsync-class database"),
            Self::GroupRefused { member } => {
                write!(
                    f,
                    "group member {member} was refused, so the group is not recorded"
                )
            }
        }
    }
}

/// The outcome for one path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    Record,
    Skip(SkipReason),
}

/// An attribute's state for a path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AttrState {
    Set,
    Unset,
    Value(String),
}

/// The attributes that apply to a path, by name. Unspecified attributes are
/// absent.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Attributes(BTreeMap<String, AttrState>);

impl Attributes {
    /// Attributes from a map, as tests and policy code build them.
    pub fn from_map(map: BTreeMap<String, AttrState>) -> Self {
        Self(map)
    }

    pub fn get(&self, name: &str) -> Option<&AttrState> {
        self.0.get(name)
    }

    pub fn iter(&self) -> impl Iterator<Item = (&str, &AttrState)> {
        self.0.iter().map(|(k, v)| (k.as_str(), v))
    }
}

/// True for attribute names only operator rules may set.
pub fn is_agentfs_attr(name: &str) -> bool {
    name == "agentfs" || name.starts_with("agentfs.")
}

/// The rules engine for one volume. `Send + Sync`; share it behind an `Arc`.
pub struct Rules {
    tree: PathBuf,
    options: RuleOptions,
    operator_ignore: gix_ignore::Search,
    operator_attrs: gix_attributes::Search,
    operator_macros: MetadataCollection,
    /// Whether any operator line mentions `agentfs.commit=fsync`, so callers
    /// can skip class lookups on volumes without databases.
    fsync_class: bool,
}

impl fmt::Debug for Rules {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Rules")
            .field("tree", &self.tree)
            .field("options", &self.options)
            .finish_non_exhaustive()
    }
}

/// Every attribute a search set for a path. `None` is an explicit
/// unspecify (`!name`), which still counts as the source deciding.
type RawAttrs = BTreeMap<String, Option<AttrState>>;

impl Rules {
    /// Builds the engine for the backing tree at `tree` from the contents of
    /// the operator's `info/exclude` and `info/attributes`.
    pub fn new(
        tree: impl Into<PathBuf>,
        options: RuleOptions,
        info_exclude: &[u8],
        info_attributes: &[u8],
    ) -> Self {
        let mut operator_ignore = gix_ignore::Search::default();
        // With no root the list has no base: patterns are relative to the
        // root of the tree, as info/exclude's are in git.
        operator_ignore
            .add_patterns_buffer(info_exclude, "info/exclude", None, Ignore::default())
            .expect("a list without a root has no base to convert");
        let mut operator_macros = MetadataCollection::default();
        let mut operator_attrs = gix_attributes::Search::default();
        operator_attrs
            .add_patterns_buffer(
                info_attributes,
                "info/attributes".into(),
                None,
                &mut operator_macros,
                true,
            )
            .expect("a list without a root has no base to convert");
        Self {
            tree: tree.into(),
            options,
            operator_ignore,
            operator_attrs,
            operator_macros,
            fsync_class: info_attributes
                .windows(FSYNC_LINE.len())
                .any(|w| w == FSYNC_LINE),
        }
    }

    /// Reads `git_dir/info/exclude` and `git_dir/info/attributes` (a missing
    /// file is empty) and builds the engine.
    pub fn load(
        git_dir: &Path,
        tree: impl Into<PathBuf>,
        options: RuleOptions,
    ) -> Result<Self, RulesError> {
        let read = |name: &str| {
            let path = git_dir.join("info").join(name);
            match fs::read(&path) {
                Ok(bytes) => Ok(bytes),
                Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Vec::new()),
                Err(e) => Err(io_error(path)(e)),
            }
        };
        let exclude = read("exclude")?;
        let attributes = read("attributes")?;
        validate_operator_attributes(&attributes)?;
        Ok(Self::new(tree, options, &exclude, &attributes))
    }

    /// Whether any operator line mentions `agentfs.commit=fsync`. When
    /// false, no path is in the fsync class.
    pub fn has_fsync_class(&self) -> bool {
        self.fsync_class
    }

    /// The fsync-class settings for `path` (design §8), from operator rules
    /// only: tree files can never set `agentfs.*`.
    pub fn fsync_class(&self, path: &RepoPath) -> Option<FsyncClass> {
        if !self.fsync_class {
            return None;
        }
        FsyncClass::from_attributes(&self.operator_attrs_of(path))
    }

    /// `agentfs.maxSize` for `path`: the largest content recorded there
    /// (design §4.2), from operator rules only.
    pub fn max_size(&self, path: &RepoPath) -> Option<u64> {
        fsync_class::max_size(&self.operator_attrs_of(path))
    }

    fn operator_attrs_of(&self, path: &RepoPath) -> Attributes {
        Attributes(
            self.operator_attributes(path)
                .into_iter()
                .filter_map(|(name, state)| Some((name, state?)))
                .collect(),
        )
    }

    /// Whether `path` is the `-shm` file of a database in the fsync class:
    /// the path itself has the class, or the file it is the shared memory
    /// of does.
    fn is_class_shm(&self, path: &RepoPath, raw: &RawAttrs) -> bool {
        if !self.fsync_class || !fsync_class::is_shm(path.file_name().as_bytes()) {
            return false;
        }
        let own =
            raw.get(fsync_class::COMMIT_ATTR) == Some(&Some(AttrState::Value("fsync".into())));
        own || {
            let name = path.as_str();
            let stem = &name[..name.len() - "-shm".len()];
            RepoPath::new(stem).is_ok_and(|stem| self.fsync_class(&stem).is_some())
        }
    }

    /// The backing tree the rules read.
    pub fn tree(&self) -> &Path {
        &self.tree
    }

    pub fn options(&self) -> RuleOptions {
        self.options
    }

    /// Whether a write to `path` (a file or symlink) is recorded.
    ///
    /// In order: an operator `-agentfs` always skips, and so does the
    /// `-shm` file of an fsync-class database; otherwise a tracked
    /// path is recorded; otherwise nested repositories and ignore rules may
    /// skip it. Tree `.gitattributes` are never read here, since they cannot
    /// set `agentfs.*`.
    pub fn decide(&self, path: &RepoPath, tracked: bool) -> Result<Decision, RulesError> {
        let raw = self.operator_attributes(path);
        if raw.get("agentfs") == Some(&Some(AttrState::Unset)) {
            return Ok(Decision::Skip(SkipReason::NeverRecord));
        }
        if self.is_class_shm(path, &raw) {
            return Ok(Decision::Skip(SkipReason::SharedMemory));
        }
        if tracked {
            return Ok(Decision::Record);
        }
        if let Some((repo, kind)) = self.nested_ancestor(path)? {
            return Ok(Decision::Skip(SkipReason::Nested { repo, kind }));
        }
        if let Some(reason) = self.ignored(path)? {
            return Ok(Decision::Skip(reason));
        }
        Ok(Decision::Record)
    }

    /// The ignore rule that excludes `path` (a file or symlink), if any.
    ///
    /// As in git, once a directory is excluded nothing beneath it can be
    /// re-included, so each directory on the way down is checked first.
    pub fn ignored(&self, path: &RepoPath) -> Result<Option<SkipReason>, RulesError> {
        let components: Vec<&str> = path.components().collect();
        let mut tree_search = if self.options.respect_tree_rules {
            let mut search = gix_ignore::Search::default();
            self.add_tree_ignore(&mut search, "")?;
            Some(search)
        } else {
            None
        };
        let mut prefix = String::new();
        for (i, component) in components.iter().enumerate() {
            if i > 0 {
                prefix.push('/');
            }
            prefix.push_str(component);
            let is_dir = i + 1 < components.len();
            let rela = prefix.as_bytes().as_bstr();

            if let Some(m) = self.operator_ignore.pattern_matching_relative_path(
                rela,
                Some(is_dir),
                Case::Sensitive,
            ) && !m.pattern.is_negative()
            {
                return Ok(Some(SkipReason::Ignored {
                    source: RuleSource::OperatorExclude,
                    pattern: m.pattern.to_string(),
                }));
            }
            if let Some(search) = tree_search.as_mut() {
                if let Some(m) =
                    search.pattern_matching_relative_path(rela, Some(is_dir), Case::Sensitive)
                    && !m.pattern.is_negative()
                {
                    let file = m
                        .source
                        .and_then(|s| s.to_str())
                        .and_then(|s| RepoPath::new(s).ok())
                        .expect("tree ignore sources are repository paths we built");
                    return Ok(Some(SkipReason::Ignored {
                        source: RuleSource::TreeIgnore(file),
                        pattern: m.pattern.to_string(),
                    }));
                }
                if is_dir {
                    self.add_tree_ignore(search, &prefix)?;
                }
            }
        }
        Ok(None)
    }

    /// The outermost proper ancestor directory of `path` that is a nested
    /// repository under the configured `agentfs.nested` mode.
    ///
    /// The mount root is never nested. Under `working-tree`, only a bare
    /// layout counts, since all of it is git directory; a `.git` directory
    /// beneath a working tree is already refused by [`RepoPath`].
    pub fn nested_ancestor(
        &self,
        path: &RepoPath,
    ) -> Result<Option<(RepoPath, NestedKind)>, RulesError> {
        let components: Vec<&str> = path.components().collect();
        let mut prefix = String::new();
        for component in &components[..components.len() - 1] {
            if !prefix.is_empty() {
                prefix.push('/');
            }
            prefix.push_str(component);
            let dir = self.tree.join(&prefix);
            let Some(kind) = nested::detect(&self.tree, &prefix).map_err(io_error(&dir))? else {
                continue;
            };
            if self.options.nested == Nested::WorkingTree && kind != NestedKind::Bare {
                continue;
            }
            let repo = RepoPath::new(prefix.clone()).expect("a prefix of a valid path is valid");
            return Ok(Some((repo, kind)));
        }
        Ok(None)
    }

    /// The attributes for `path` (a file or symlink): the operator's, plus,
    /// with `respectTreeRules`, the tree's for names the operator leaves
    /// alone. Tree files never contribute `agentfs.*` attributes, even
    /// through macros.
    pub fn attributes(&self, path: &RepoPath) -> Result<Attributes, RulesError> {
        let mut merged = self.operator_attributes(path);
        if self.options.respect_tree_rules {
            for (name, state) in self.tree_attributes(path)? {
                if !is_agentfs_attr(&name) {
                    merged.entry(name).or_insert(state);
                }
            }
        }
        Ok(Attributes(
            merged
                .into_iter()
                .filter_map(|(name, state)| Some((name, state?)))
                .collect(),
        ))
    }

    fn operator_attributes(&self, path: &RepoPath) -> RawAttrs {
        collect_attrs(&self.operator_attrs, &self.operator_macros, path)
    }

    /// Attributes from `.gitattributes` in the root and every ancestor
    /// directory of `path`, deeper files winning. Macros are honoured only
    /// in the root file, as in git, and live in their own collection so they
    /// cannot touch operator lines.
    fn tree_attributes(&self, path: &RepoPath) -> Result<RawAttrs, RulesError> {
        let mut macros = MetadataCollection::default();
        let mut search = gix_attributes::Search::default();
        let components: Vec<&str> = path.components().collect();
        let mut dirs = vec![String::new()];
        for i in 1..components.len() {
            dirs.push(components[..i].join("/"));
        }
        for dir in &dirs {
            let file = join(dir, ".gitattributes");
            if let Some(bytes) = self.read_tree_rule_file(&file)? {
                search
                    .add_patterns_buffer(
                        &bytes,
                        PathBuf::from(&file),
                        Some(Path::new("")),
                        &mut macros,
                        dir.is_empty(),
                    )
                    .map_err(io_error(self.tree.join(&file)))?;
            }
        }
        Ok(collect_attrs(&search, &macros, path))
    }

    /// Adds `dir/.gitignore` from the backing tree, if there is one, as a
    /// list whose patterns are relative to `dir`.
    fn add_tree_ignore(
        &self,
        search: &mut gix_ignore::Search,
        dir: &str,
    ) -> Result<(), RulesError> {
        let file = join(dir, ".gitignore");
        if let Some(bytes) = self.read_tree_rule_file(&file)? {
            // A relative source with an empty root makes the source's parent
            // directory the list's base.
            search
                .add_patterns_buffer(
                    &bytes,
                    PathBuf::from(&file),
                    Some(Path::new("")),
                    Ignore::default(),
                )
                .map_err(io_error(self.tree.join(&file)))?;
        }
        Ok(())
    }

    /// Reads a rule file from the backing tree. Only regular files count:
    /// symlinks are skipped, as git (2.32 and later) does and as
    /// [`crate::paths::must_not_be_symlink`] requires. A file over
    /// [`probe::RULE_FILE`] bytes is ignored, as git ignores oversized
    /// attribute files, so the synchronous step stays bounded.
    fn read_tree_rule_file(&self, rel: &str) -> Result<Option<Vec<u8>>, RulesError> {
        let path = self.tree.join(rel);
        probe::read_regular(&path, probe::RULE_FILE).map_err(io_error(path))
    }
}

/// What [`Rules::has_fsync_class`] looks for in `info/attributes`.
const FSYNC_LINE: &[u8] = b"agentfs.commit=fsync";

/// Checks every `agentfs.commit`, `agentfs.coalesce` and `agentfs.maxSize`
/// assignment in the operator's `info/attributes`, macro definitions
/// included ([`fsync_class::validate_value`]). A bad value is an error so a
/// typo never silently records a database on close.
pub fn validate_operator_attributes(bytes: &[u8]) -> Result<(), RulesError> {
    for (i, line) in bytes.split(|b| *b == b'\n').enumerate() {
        let line = String::from_utf8_lossy(line);
        let line = line.trim_start();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        // Skip the pattern: quoted (C-style escapes) or up to whitespace.
        let rest = if let Some(quoted) = line.strip_prefix('"') {
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
            });
            match end {
                Some(end) => &quoted[end..],
                None => continue,
            }
        } else {
            match line.find(char::is_whitespace) {
                Some(end) => &line[end..],
                None => continue,
            }
        };
        for token in rest.split_whitespace() {
            if token.starts_with('-') || token.starts_with('!') {
                continue;
            }
            let (name, value) = match token.split_once('=') {
                Some((name, value)) => (name, Some(value)),
                None => (token, None),
            };
            fsync_class::validate_value(name, value).map_err(|reason| {
                RulesError::BadAttribute {
                    line: i + 1,
                    name: name.to_owned(),
                    value: value.unwrap_or("").to_owned(),
                    reason,
                }
            })?;
        }
    }
    Ok(())
}

fn join(dir: &str, name: &str) -> String {
    if dir.is_empty() {
        name.to_owned()
    } else {
        format!("{dir}/{name}")
    }
}

fn collect_attrs(
    search: &gix_attributes::Search,
    macros: &MetadataCollection,
    path: &RepoPath,
) -> RawAttrs {
    let mut out = Outcome::default();
    out.initialize(macros);
    search.pattern_matching_relative_path(
        path.as_str().as_bytes().as_bstr(),
        Case::Sensitive,
        Some(false),
        &mut out,
    );
    out.iter()
        .map(|m| {
            let state = match m.assignment.state {
                StateRef::Set => Some(AttrState::Set),
                StateRef::Unset => Some(AttrState::Unset),
                StateRef::Value(v) => Some(AttrState::Value(v.as_bstr().to_string())),
                StateRef::Unspecified => None,
            };
            (m.assignment.name.as_str().to_owned(), state)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::symlink;

    use super::*;

    fn p(s: &str) -> RepoPath {
        RepoPath::new(s).unwrap()
    }

    struct Fixture {
        dir: tempfile::TempDir,
    }

    impl Fixture {
        fn new() -> Self {
            Self {
                dir: tempfile::tempdir().unwrap(),
            }
        }

        fn tree(&self) -> &Path {
            self.dir.path()
        }

        fn file(&self, rel: &str, content: &str) -> &Self {
            let path = self.tree().join(rel);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, content).unwrap();
            self
        }

        fn mkdir(&self, rel: &str) -> &Self {
            fs::create_dir_all(self.tree().join(rel)).unwrap();
            self
        }

        fn bare(&self, rel: &str) -> &Self {
            self.mkdir(&format!("{rel}/objects"))
                .mkdir(&format!("{rel}/refs"))
                .file(&format!("{rel}/HEAD"), "ref: refs/heads/main\n")
        }

        fn rules(&self, respect: bool, exclude: &str, attributes: &str) -> Rules {
            self.rules_with(
                RuleOptions {
                    respect_tree_rules: respect,
                    nested: Nested::Exclude,
                },
                exclude,
                attributes,
            )
        }

        fn rules_with(&self, options: RuleOptions, exclude: &str, attributes: &str) -> Rules {
            Rules::new(
                self.tree(),
                options,
                exclude.as_bytes(),
                attributes.as_bytes(),
            )
        }
    }

    fn decide(r: &Rules, path: &str) -> Decision {
        r.decide(&p(path), false).unwrap()
    }

    fn operator_ignored(pattern: &str) -> Decision {
        Decision::Skip(SkipReason::Ignored {
            source: RuleSource::OperatorExclude,
            pattern: pattern.into(),
        })
    }

    fn tree_ignored(file: &str, pattern: &str) -> Decision {
        Decision::Skip(SkipReason::Ignored {
            source: RuleSource::TreeIgnore(p(file)),
            pattern: pattern.into(),
        })
    }

    #[test]
    fn rules_are_send_and_sync() {
        fn check<T: Send + Sync>() {}
        check::<Rules>();
    }

    #[test]
    fn operator_excludes_only_untracked_paths() {
        let f = Fixture::new();
        let r = f.rules(false, "*.log\n", "");
        assert_eq!(decide(&r, "a/x.log"), operator_ignored("*.log"));
        assert_eq!(r.decide(&p("a/x.log"), true).unwrap(), Decision::Record);
        assert_eq!(decide(&r, "a/x.txt"), Decision::Record);
    }

    #[test]
    fn excluded_directory_cannot_be_reincluded() {
        let f = Fixture::new();
        let r = f.rules(false, "build/\n!build/keep\n", "");
        assert_eq!(decide(&r, "build/a/b.o"), operator_ignored("build/"));
        assert_eq!(decide(&r, "build/keep"), operator_ignored("build/"));
        // `build/` only matches directories.
        assert_eq!(decide(&r, "x/build"), Decision::Record);
    }

    #[test]
    fn negation_and_anchoring() {
        let f = Fixture::new();
        let r = f.rules(false, "*.log\n!keep.log\n/top.txt\n", "");
        assert_eq!(decide(&r, "keep.log"), Decision::Record);
        assert_eq!(decide(&r, "x.log"), operator_ignored("*.log"));
        assert_eq!(decide(&r, "top.txt"), operator_ignored("/top.txt"));
        assert_eq!(decide(&r, "a/top.txt"), Decision::Record);
    }

    #[test]
    fn case_is_never_folded() {
        let f = Fixture::new();
        let r = f.rules(false, "*.LOG\n", "");
        assert_eq!(decide(&r, "a.log"), Decision::Record);
        assert_eq!(decide(&r, "a.LOG"), operator_ignored("*.LOG"));
    }

    #[test]
    fn tree_gitignore_counts_only_when_respected() {
        let f = Fixture::new();
        f.file(".gitignore", "*.tmp\n");
        assert_eq!(decide(&f.rules(false, "", ""), "x.tmp"), Decision::Record);
        assert_eq!(
            decide(&f.rules(true, "", ""), "x.tmp"),
            tree_ignored(".gitignore", "*.tmp")
        );
    }

    #[test]
    fn nested_gitignore_is_relative_to_its_directory() {
        let f = Fixture::new();
        f.file("sub/.gitignore", "/x\n!keep.tmp\n")
            .file(".gitignore", "*.tmp\n");
        let r = f.rules(true, "", "");
        assert_eq!(decide(&r, "sub/x"), tree_ignored("sub/.gitignore", "/x"));
        assert_eq!(decide(&r, "x"), Decision::Record);
        assert_eq!(decide(&r, "sub/y/x"), Decision::Record);
        // The deeper file wins.
        assert_eq!(decide(&r, "sub/keep.tmp"), Decision::Record);
        assert_eq!(
            decide(&r, "sub/other.tmp"),
            tree_ignored(".gitignore", "*.tmp")
        );
    }

    #[test]
    fn tree_negation_cannot_reinclude_operator_exclude() {
        let f = Fixture::new();
        f.file(".gitignore", "!secret\n");
        let r = f.rules(true, "secret\n", "");
        assert_eq!(decide(&r, "secret"), operator_ignored("secret"));
    }

    #[test]
    fn symlinked_gitignore_is_not_read() {
        let f = Fixture::new();
        f.file("rules", "*\n");
        symlink(f.tree().join("rules"), f.tree().join(".gitignore")).unwrap();
        let r = f.rules(true, "", "");
        assert_eq!(decide(&r, "a"), Decision::Record);
        // A directory named .gitignore is not a rule file either.
        f.mkdir("d/.gitignore");
        assert_eq!(decide(&r, "d/x"), Decision::Record);
    }

    #[test]
    fn never_record_applies_even_when_tracked() {
        let f = Fixture::new();
        let r = f.rules(false, "", "secret.txt -agentfs\nsecrets/** -agentfs\n");
        let skip = Decision::Skip(SkipReason::NeverRecord);
        assert_eq!(decide(&r, "secret.txt"), skip);
        assert_eq!(r.decide(&p("secret.txt"), true).unwrap(), skip);
        assert_eq!(decide(&r, "secrets/a/b"), skip);
        assert_eq!(decide(&r, "other.txt"), Decision::Record);
        assert_eq!(
            r.attributes(&p("secret.txt")).unwrap().get("agentfs"),
            Some(&AttrState::Unset)
        );
    }

    #[test]
    fn directory_attribute_patterns_do_not_cover_contents() {
        // As in git: `secrets/` matches the directory, not its files.
        let f = Fixture::new();
        let r = f.rules(false, "", "secrets/ -agentfs\n");
        assert_eq!(decide(&r, "secrets/a"), Decision::Record);
    }

    #[test]
    fn tree_attributes_cannot_set_agentfs() {
        let f = Fixture::new();
        f.file(
            ".gitattributes",
            "[attr]evil -agentfs\n* -agentfs\n*.txt evil\n*.db agentfs.commit=fsync\n",
        );
        let r = f.rules(true, "", "");
        assert_eq!(decide(&r, "a.txt"), Decision::Record);
        assert_eq!(decide(&r, "a.db"), Decision::Record);
        let attrs = r.attributes(&p("a.txt")).unwrap();
        assert_eq!(attrs.get("agentfs"), None);
        assert_eq!(attrs.get("evil"), Some(&AttrState::Set));
        assert_eq!(
            r.attributes(&p("a.db")).unwrap().get("agentfs.commit"),
            None
        );

        let r = f.rules(true, "", "*.db agentfs.commit=fsync\n");
        assert_eq!(
            r.attributes(&p("a.db")).unwrap().get("agentfs.commit"),
            Some(&AttrState::Value("fsync".into()))
        );
    }

    #[test]
    fn tree_macros_cannot_change_operator_lines() {
        let f = Fixture::new();
        f.file(".gitattributes", "[attr]keep -agentfs\n");
        let r = f.rules(true, "", "*.txt keep\n");
        assert_eq!(decide(&r, "a.txt"), Decision::Record);
        assert_eq!(
            r.attributes(&p("a.txt")).unwrap().get("keep"),
            Some(&AttrState::Set)
        );
        // Operator macros do expand.
        let r = f.rules(true, "", "[attr]private -agentfs\n*.key private\n");
        assert_eq!(decide(&r, "a.key"), Decision::Skip(SkipReason::NeverRecord));
    }

    #[test]
    fn tree_attributes_for_other_names() {
        let f = Fixture::new();
        f.file(".gitattributes", "*.txt foo=bar text\n")
            .file("sub/.gitattributes", "*.txt foo=deep\n");
        assert_eq!(
            f.rules(false, "", "")
                .attributes(&p("a.txt"))
                .unwrap()
                .get("foo"),
            None
        );
        let r = f.rules(true, "", "");
        assert_eq!(
            r.attributes(&p("a.txt")).unwrap().get("foo"),
            Some(&AttrState::Value("bar".into()))
        );
        let sub = r.attributes(&p("sub/a.txt")).unwrap();
        assert_eq!(sub.get("foo"), Some(&AttrState::Value("deep".into())));
        assert_eq!(sub.get("text"), Some(&AttrState::Set));

        let r = f.rules(true, "", "*.txt foo=op\n");
        assert_eq!(
            r.attributes(&p("sub/a.txt")).unwrap().get("foo"),
            Some(&AttrState::Value("op".into()))
        );
        // An explicit operator unspecify also wins.
        let r = f.rules(true, "", "*.txt !foo\n");
        assert_eq!(r.attributes(&p("a.txt")).unwrap().get("foo"), None);
    }

    #[test]
    fn nested_repositories_are_excluded_by_default() {
        let f = Fixture::new();
        f.mkdir("sub/.git").file("sub/file", "x");
        let r = f.rules(false, "", "");
        let nested = |repo: &str, kind| {
            Decision::Skip(SkipReason::Nested {
                repo: p(repo),
                kind,
            })
        };
        assert_eq!(decide(&r, "sub/file"), nested("sub", NestedKind::GitDir));
        assert_eq!(r.decide(&p("sub/file"), true).unwrap(), Decision::Record);
        assert_eq!(
            decide(&r, "sub"),
            Decision::Record,
            "the leaf is not probed"
        );

        f.mkdir("a/.git").mkdir("a/b/.git");
        assert_eq!(decide(&r, "a/b/c"), nested("a", NestedKind::GitDir));

        f.bare("store");
        assert_eq!(
            decide(&r, "store/objects/ab/cd"),
            nested("store", NestedKind::Bare)
        );

        f.bare("modules/m").mkdir("w");
        f.file("w/.git", "gitdir: ../modules/m\n");
        assert_eq!(decide(&r, "w/f"), nested("w", NestedKind::GitFile));
    }

    #[test]
    fn working_tree_mode_records_working_files_only() {
        let f = Fixture::new();
        f.mkdir("sub/.git")
            .bare("store")
            .bare("modules/sub")
            .mkdir("w");
        f.file("w/.git", "gitdir: ../modules/sub\n");
        let r = f.rules_with(
            RuleOptions {
                respect_tree_rules: false,
                nested: Nested::WorkingTree,
            },
            "",
            "",
        );
        assert_eq!(decide(&r, "sub/file"), Decision::Record);
        assert_eq!(decide(&r, "w/file"), Decision::Record);
        assert_eq!(
            decide(&r, "store/objects/ab/cd"),
            Decision::Skip(SkipReason::Nested {
                repo: p("store"),
                kind: NestedKind::Bare
            })
        );
        assert_eq!(
            decide(&r, "modules/sub/HEAD"),
            Decision::Skip(SkipReason::Nested {
                repo: p("modules/sub"),
                kind: NestedKind::Bare
            })
        );
    }

    #[test]
    fn the_root_is_never_nested() {
        let f = Fixture::new();
        f.mkdir(".git")
            .file("HEAD", "ref: refs/heads/main\n")
            .file("objects/x", "")
            .file("refs/y", "");
        let r = f.rules(false, "", "");
        for path in ["HEAD", "objects/x", "refs/y"] {
            assert_eq!(decide(&r, path), Decision::Record, "{path}");
        }
    }

    #[test]
    fn load_reads_operator_files() {
        let f = Fixture::new();
        let git = f.tree().join("git");
        let tree = f.tree().join("tree");
        fs::create_dir_all(&tree).unwrap();
        // Missing info files are empty.
        let r = Rules::load(&git, &tree, RuleOptions::default()).unwrap();
        assert_eq!(decide(&r, "a.log"), Decision::Record);

        fs::create_dir_all(git.join("info")).unwrap();
        fs::write(git.join("info/exclude"), "*.log\n").unwrap();
        fs::write(git.join("info/attributes"), "*.key -agentfs\n").unwrap();
        let r = Rules::load(&git, &tree, RuleOptions::default()).unwrap();
        assert_eq!(decide(&r, "a.log"), operator_ignored("*.log"));
        assert_eq!(decide(&r, "a.key"), Decision::Skip(SkipReason::NeverRecord));
    }

    #[test]
    fn fsync_class_comes_from_operator_rules_only() {
        let f = Fixture::new();
        f.file(".gitattributes", "*.db agentfs.commit=fsync\n");
        assert!(!f.rules(true, "", "").has_fsync_class());
        assert_eq!(f.rules(true, "", "").fsync_class(&p("a.db")), None);
        let r = f.rules(
            true,
            "",
            "[attr]sqlite agentfs.commit=fsync agentfs.group=sqlite agentfs.coalesce=5s\n\
             *.db sqlite\n*.db-wal sqlite\n*.db-shm sqlite\nbig/* agentfs.maxSize=1k\n",
        );
        assert!(r.has_fsync_class());
        assert_eq!(
            r.fsync_class(&p("d/a.db-wal")),
            Some(FsyncClass {
                coalesce: std::time::Duration::from_secs(5),
                group: Some("sqlite".into())
            })
        );
        assert_eq!(r.fsync_class(&p("a.txt")), None);
        assert_eq!(r.max_size(&p("big/x")), Some(1024));
        assert_eq!(r.max_size(&p("x")), None);
    }

    #[test]
    fn shared_memory_of_a_class_database_is_never_recorded() {
        let f = Fixture::new();
        let shm = Decision::Skip(SkipReason::SharedMemory);
        // The -shm itself has the class.
        let r = f.rules(false, "", "*.db* agentfs.commit=fsync\n");
        assert_eq!(decide(&r, "a.db-shm"), shm);
        assert_eq!(r.decide(&p("a.db-shm"), true).unwrap(), shm);
        assert_eq!(decide(&r, "a.db-wal"), Decision::Record);
        // Only the database has it.
        let r = f.rules(false, "", "*.db agentfs.commit=fsync\n");
        assert_eq!(decide(&r, "d/a.db-shm"), shm);
        // Without the class a -shm file is just a file.
        let r = f.rules(false, "", "*.db agentfs.commit=close\n");
        assert_eq!(decide(&r, "a.db-shm"), Decision::Record);
        // -agentfs still reads as itself.
        let r = f.rules(false, "", "*.db agentfs.commit=fsync\n*-shm -agentfs\n");
        assert_eq!(
            decide(&r, "a.db-shm"),
            Decision::Skip(SkipReason::NeverRecord)
        );
    }

    #[test]
    fn load_refuses_bad_class_values() {
        let f = Fixture::new();
        let git = f.tree().join("git");
        fs::create_dir_all(git.join("info")).unwrap();
        for (text, line, name) in [
            ("*.db agentfs.commit=always\n", 1, "agentfs.commit"),
            (
                "# c\n*.db agentfs.commit=fsync agentfs.coalesce=10ms\n",
                2,
                "agentfs.coalesce",
            ),
            ("[attr]db agentfs.commit\n", 1, "agentfs.commit"),
            ("\"a b\" agentfs.maxSize=lots\n", 1, "agentfs.maxSize"),
        ] {
            fs::write(git.join("info/attributes"), text).unwrap();
            match Rules::load(&git, f.tree(), RuleOptions::default()) {
                Err(RulesError::BadAttribute {
                    line: l, name: n, ..
                }) => {
                    assert_eq!((l, n.as_str()), (line, name), "{text}");
                }
                other => panic!("{text}: {other:?}"),
            }
        }
        fs::write(
            git.join("info/attributes"),
            "*.db agentfs.commit=fsync agentfs.coalesce=2s agentfs.maxSize=64m\n\
             x -agentfs.commit !agentfs.coalesce\n\"q\\\" r\" agentfs.commit=close\n",
        )
        .unwrap();
        Rules::load(&git, f.tree(), RuleOptions::default()).unwrap();
        let err = validate_operator_attributes(b"*.db agentfs.coalesce=0s\n").unwrap_err();
        assert_eq!(
            err.to_string(),
            "info/attributes line 1: agentfs.coalesce=0s: expected a duration of at least 1s (e.g. 5s, 30s, 2m)"
        );
    }

    #[test]
    fn reasons_read_well() {
        let cases = [
            (
                SkipReason::Ignored {
                    source: RuleSource::OperatorExclude,
                    pattern: "*.log".into(),
                },
                "ignored by info/exclude (*.log)",
            ),
            (
                SkipReason::Ignored {
                    source: RuleSource::TreeIgnore(p("sub/.gitignore")),
                    pattern: "/x".into(),
                },
                "ignored by sub/.gitignore (/x)",
            ),
            (
                SkipReason::NeverRecord,
                "marked -agentfs in info/attributes",
            ),
            (
                SkipReason::Nested {
                    repo: p("sub"),
                    kind: NestedKind::GitDir,
                },
                "inside nested repository sub (.git directory)",
            ),
            (SkipReason::Untracked, "nothing tracked at this path"),
            (
                SkipReason::SharedMemory,
                "SQLite shared memory of an fsync-class database",
            ),
            (
                SkipReason::GroupRefused {
                    member: p("a.db-wal"),
                },
                "group member a.db-wal was refused, so the group is not recorded",
            ),
        ];
        for (reason, text) in cases {
            assert_eq!(reason.to_string(), text);
        }
    }
}
