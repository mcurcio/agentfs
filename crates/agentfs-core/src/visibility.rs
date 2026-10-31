//! The visibility gate of the sandbox overlay: which names the overlay
//! shows, and where a name it creates or moves may go (design §1.1, §4).
//!
//! The sandbox overlay is a second mount point of a volume that shows only
//! the paths the operator's rules would record if they were new
//! (*recordable* paths). This module is its one judge. It is a pure
//! decision over the volume's [`Rules`]: it holds no rule set of its own,
//! never asks whether a path is tracked, never looks at reads or writes on
//! open files and knows nothing about mounts, callers or errno values.
//!
//! - **Show.** An existing file or symlink is shown when its path is
//!   recordable; a directory when the rules could record something beneath
//!   it ([`Rules::excludes_dir`] is false); a special file never. A path in
//!   an fsync-class group is shown only when every member of its group is
//!   recordable, so a group is shown whole or not at all.
//! - **Place a new name.** A recordable file goes to the shared tree; a file
//!   name the rules skip goes to the overlay's private tmp layer; a
//!   directory goes to the tree when the rules could record beneath it and
//!   is refused otherwise. Symlinks, hard links and special files are
//!   always refused.
//! - **Place a move.** A directory never moves (the caller answers
//!   "cross-device", so tools copy and delete file by file). A symlink never
//!   moves. A shown tree file moves only to a recordable name. A file in
//!   the tmp layer moves into the tree under a recordable name, and stays
//!   in the tmp layer otherwise.
//!
//! Invariants, which the unit tests pin:
//!
//! - **Fail closed.** A rules error answers [`Visibility::Hidden`] or
//!   [`Placement::Refused`], never [`Visibility::Shown`] or
//!   [`Placement::Tree`].
//! - **Existence-blind.** Every answer is a function of the path and the
//!   kind alone. The gate never asks whether a name exists, so the answer
//!   for a hidden name cannot depend on it. (Nested-repository detection,
//!   which reads the tree, can only hide.)
//! - **Rules are fixed while mounted.** The gate reads the [`Rules`] built
//!   when the volume opened; changing a rule file takes a remount. A volume
//!   that reads rules from its tree (`agentfs.respectTreeRules`) cannot
//!   have a gate ([`Gate::new`]): the sandbox could edit the rules behind
//!   its own view.

use std::sync::Arc;

use crate::fsync_class;
use crate::paths::RepoPath;
use crate::rules::{Decision, Rules, RulesError};

/// What an existing entry is, as a lookup or a listing sees it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExistingKind {
    File,
    /// Shown when its path is recordable: readable and removable, never
    /// moved.
    Symlink,
    /// Shown when the rules could record something beneath it.
    Directory,
    /// A device, FIFO or socket: never shown.
    Special,
}

/// What a create-like operation would make.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NewKind {
    File,
    Directory,
    /// Always refused.
    Symlink,
    /// Always refused, whatever the source: one content under two names.
    Hardlink,
    /// A device, FIFO or socket: always refused.
    Special,
}

/// The source of a rename, as the overlay shows it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source<'a> {
    /// An entry of this kind in the shared tree.
    Tree(&'a RepoPath, ExistingKind),
    /// A file in the overlay's private tmp layer.
    TmpLayer(&'a RepoPath),
}

/// Whether an existing name is shown.
#[must_use]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Visibility {
    Shown,
    Hidden(Hidden),
}

/// Why a name is hidden.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Hidden {
    /// The rules would not record it (or, for a directory, anything
    /// beneath it).
    NotRecordable,
    /// The rules could not decide (an I/O error probing for a nested
    /// repository): hidden, failing closed.
    RulesError,
}

/// Where a new or moved name goes.
#[must_use]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Placement {
    /// A recordable name in the shared tree.
    Tree,
    /// A file name the rules skip: in the overlay's private tmp layer,
    /// never recorded.
    TmpLayer,
    /// A directory move: refused so that tools fall back to copying and
    /// deleting file by file.
    CrossDevice,
    Refused(Denial),
}

/// Why a placement is refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Denial {
    /// The kind is never placed: a symlink, hard link or special file, or a
    /// move of a symlink.
    Kind,
    /// A directory the rules could record nothing beneath, or a move of a
    /// tree file to a name the rules skip.
    NotRecordable,
    /// The source of a move is not shown.
    SourceHidden,
    /// The rules could not decide: refused, failing closed.
    RulesError,
}

/// Why a gate cannot be built for a volume.
#[derive(Debug, thiserror::Error)]
pub enum GateError {
    /// `agentfs.respectTreeRules = true`: rule files in the tree would be
    /// editable through the overlay.
    #[error(
        "the sandbox overlay needs agentfs.respectTreeRules = false: rule files read from the \
         tree could be edited through the overlay"
    )]
    TreeRules,
}

/// The visibility gate for one volume. `Send + Sync`; cheap to clone.
#[derive(Debug, Clone)]
pub struct Gate {
    rules: Arc<Rules>,
}

impl Gate {
    /// The gate over the volume's `rules`. Refuses a volume whose rules
    /// read tree rule files ([`GateError::TreeRules`]).
    pub fn new(rules: Arc<Rules>) -> Result<Self, GateError> {
        if rules.options().respect_tree_rules {
            return Err(GateError::TreeRules);
        }
        Ok(Self { rules })
    }

    /// Whether any entry at `path` could be shown, whatever its kind:
    /// [`Visibility::Hidden`] means every kind is hidden, so a caller can
    /// answer for the name without looking at the tree.
    ///
    /// A rules error from either predicate answers
    /// [`Hidden::RulesError`], even when the other would show the name:
    /// no path through an error is shown.
    pub fn may_show(&self, path: &RepoPath) -> Visibility {
        either_kind(self.file(path), self.dir(path))
    }

    /// Whether the existing entry at `path`, of kind `kind`, is shown.
    pub fn show(&self, path: &RepoPath, kind: ExistingKind) -> Visibility {
        match kind {
            ExistingKind::File | ExistingKind::Symlink => visibility(self.file(path)),
            ExistingKind::Directory => visibility(self.dir(path)),
            ExistingKind::Special => Visibility::Hidden(Hidden::NotRecordable),
        }
    }

    /// Where a new entry of kind `kind` at `path` goes.
    pub fn place_new(&self, path: &RepoPath, kind: NewKind) -> Placement {
        match kind {
            NewKind::File => file_placement(self.file(path)),
            NewKind::Directory => match self.dir(path) {
                Ok(true) => Placement::Tree,
                Ok(false) => Placement::Refused(Denial::NotRecordable),
                Err(_) => Placement::Refused(Denial::RulesError),
            },
            NewKind::Symlink | NewKind::Hardlink | NewKind::Special => {
                Placement::Refused(Denial::Kind)
            }
        }
    }

    /// Where a rename of `from` to `to` goes. A tree source that is not
    /// shown is refused ([`Denial::SourceHidden`]): the gate never trusts
    /// that its caller checked. An exchange is two moves, each of which
    /// must place in the tree.
    pub fn place_move(&self, from: Source<'_>, to: &RepoPath) -> Placement {
        let (path, kind) = match from {
            Source::TmpLayer(_) => return file_placement(self.file(to)),
            Source::Tree(path, kind) => (path, kind),
        };
        match self.show(path, kind) {
            Visibility::Shown => {}
            Visibility::Hidden(Hidden::NotRecordable) => {
                return Placement::Refused(Denial::SourceHidden);
            }
            Visibility::Hidden(Hidden::RulesError) => {
                return Placement::Refused(Denial::RulesError);
            }
        }
        match kind {
            ExistingKind::Directory => Placement::CrossDevice,
            ExistingKind::Symlink => Placement::Refused(Denial::Kind),
            ExistingKind::File => match self.file(to) {
                Ok(true) => Placement::Tree,
                Ok(false) => Placement::Refused(Denial::NotRecordable),
                Err(_) => Placement::Refused(Denial::RulesError),
            },
            // Never shown, so refused above; kept closed if that changes.
            ExistingKind::Special => Placement::Refused(Denial::SourceHidden),
        }
    }

    /// Whether a file at `path` would be recorded if it were new, and, in
    /// an fsync-class group, whether every member of its group would be.
    /// The tracked flag is always false: the gate never asks the tracked
    /// set.
    fn file(&self, path: &RepoPath) -> Result<bool, RulesError> {
        if self.rules.decide(path, false)? != Decision::Record {
            return Ok(false);
        }
        let Some(class) = self.rules.fsync_class(path) else {
            return Ok(true);
        };
        let Some(group) = fsync_class::group_members(path, &class, |q| self.rules.fsync_class(q))
        else {
            return Ok(true);
        };
        for member in &group.members {
            if self.rules.decide(member, false)? != Decision::Record {
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// Whether the rules could record something beneath directory `path`.
    fn dir(&self, path: &RepoPath) -> Result<bool, RulesError> {
        Ok(!self.rules.excludes_dir(path)?)
    }
}

/// An existing name's visibility from a predicate: failing closed.
fn visibility(recordable: Result<bool, RulesError>) -> Visibility {
    match recordable {
        Ok(true) => Visibility::Shown,
        Ok(false) => Visibility::Hidden(Hidden::NotRecordable),
        Err(_) => Visibility::Hidden(Hidden::RulesError),
    }
}

/// Whether a name could be shown as a file (`file`) or as a directory
/// (`dir`): shown when either could be, hidden when neither, and a rules
/// error, failing closed, whenever either predicate errs.
fn either_kind<E>(file: Result<bool, E>, dir: Result<bool, E>) -> Visibility {
    match (file, dir) {
        (Err(_), _) | (_, Err(_)) => Visibility::Hidden(Hidden::RulesError),
        (Ok(true), _) | (_, Ok(true)) => Visibility::Shown,
        (Ok(false), Ok(false)) => Visibility::Hidden(Hidden::NotRecordable),
    }
}

/// Where a new file, or a file moved out of the tmp layer, goes.
fn file_placement(recordable: Result<bool, RulesError>) -> Placement {
    match recordable {
        Ok(true) => Placement::Tree,
        Ok(false) => Placement::TmpLayer,
        Err(_) => Placement::Refused(Denial::RulesError),
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;

    use super::*;
    use crate::rules::RuleOptions;

    /// The allowlist the worked examples use.
    const E: &str = "/*\n!/config.yaml\n!/skills/\n";

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

        fn file(&self, rel: &str) -> &Self {
            let path = self.tree().join(rel);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, "x").unwrap();
            self
        }

        fn mkdir(&self, rel: &str) -> &Self {
            fs::create_dir_all(self.tree().join(rel)).unwrap();
            self
        }

        fn rules(&self, exclude: &str, attributes: &str) -> Rules {
            Rules::new(
                self.tree(),
                RuleOptions::default(),
                exclude.as_bytes(),
                attributes.as_bytes(),
            )
        }

        fn gate(&self, exclude: &str, attributes: &str) -> Gate {
            Gate::new(Arc::new(self.rules(exclude, attributes))).unwrap()
        }
    }

    const SHOWN: Visibility = Visibility::Shown;
    const HIDDEN: Visibility = Visibility::Hidden(Hidden::NotRecordable);
    const RULES_ERROR: Visibility = Visibility::Hidden(Hidden::RulesError);

    #[test]
    fn a1_skipped_file_is_hidden_whether_or_not_it_exists() {
        let f = Fixture::new();
        let g = f.gate(E, "");
        let secrets = p("secrets.db");
        assert_eq!(g.show(&secrets, ExistingKind::File), HIDDEN);
        assert_eq!(g.may_show(&secrets), HIDDEN);
        f.file("secrets.db");
        assert_eq!(g.show(&secrets, ExistingKind::File), HIDDEN);
        assert_eq!(g.may_show(&secrets), HIDDEN);
    }

    #[test]
    fn a2_new_skipped_file_goes_to_the_tmp_layer() {
        let g = Fixture::new().gate(E, "");
        assert_eq!(
            g.place_new(&p("notes.txt"), NewKind::File),
            Placement::TmpLayer
        );
    }

    #[test]
    fn a3_save_by_temp_file_and_rename() {
        let g = Fixture::new().gate(E, "");
        assert_eq!(
            g.place_new(&p("config.yaml.tmp"), NewKind::File),
            Placement::TmpLayer
        );
        assert_eq!(
            g.place_move(Source::TmpLayer(&p("config.yaml.tmp")), &p("config.yaml")),
            Placement::Tree
        );
        assert_eq!(
            g.place_move(Source::TmpLayer(&p("a.tmp")), &p("b.tmp")),
            Placement::TmpLayer
        );
    }

    #[test]
    fn a4_recordable_file_is_shown_and_placed_in_the_tree() {
        let g = Fixture::new().gate(E, "");
        assert_eq!(g.show(&p("config.yaml"), ExistingKind::File), SHOWN);
        assert_eq!(g.may_show(&p("config.yaml")), SHOWN);
        assert_eq!(
            g.place_new(&p("config.yaml"), NewKind::File),
            Placement::Tree
        );
    }

    #[test]
    fn a5_tree_file_never_moves_to_a_skipped_name() {
        let g = Fixture::new().gate(E, "");
        assert_eq!(
            g.place_move(
                Source::Tree(&p("config.yaml"), ExistingKind::File),
                &p("config.bak")
            ),
            Placement::Refused(Denial::NotRecordable)
        );
        assert_eq!(
            g.place_move(
                Source::Tree(&p("config.yaml"), ExistingKind::File),
                &p("skills/config.yaml")
            ),
            Placement::Tree
        );
    }

    #[test]
    fn a6_directory_moves_are_cross_device_whatever_the_target() {
        let g = Fixture::new().gate(E, "");
        let from = p("skills/a");
        for to in ["skills/b", "hidden/b"] {
            assert_eq!(
                g.place_move(Source::Tree(&from, ExistingKind::Directory), &p(to)),
                Placement::CrossDevice,
                "{to}"
            );
        }
    }

    #[test]
    fn a7_tree_rules_are_refused_and_rules_stay_fixed() {
        let f = Fixture::new();
        let respect = Rules::new(
            f.tree(),
            RuleOptions {
                respect_tree_rules: true,
                ..RuleOptions::default()
            },
            E.as_bytes(),
            b"",
        );
        assert!(matches!(
            Gate::new(Arc::new(respect)),
            Err(GateError::TreeRules)
        ));

        let git_dir = tempfile::tempdir().unwrap();
        let info = git_dir.path().join("info");
        fs::create_dir_all(&info).unwrap();
        fs::write(info.join("exclude"), E).unwrap();
        let rules = Rules::load(git_dir.path(), f.tree(), RuleOptions::default()).unwrap();
        let g = Gate::new(Arc::new(rules)).unwrap();
        let answers = |g: &Gate| {
            (
                g.show(&p("notes.txt"), ExistingKind::File),
                g.show(&p("config.yaml"), ExistingKind::File),
                g.place_new(&p("notes.txt"), NewKind::File),
                g.place_new(&p("cache"), NewKind::Directory),
            )
        };
        let before = answers(&g);
        assert_eq!(
            before,
            (
                HIDDEN,
                SHOWN,
                Placement::TmpLayer,
                Placement::Refused(Denial::NotRecordable)
            )
        );
        fs::write(info.join("exclude"), "").unwrap();
        assert_eq!(answers(&g), before);
    }

    #[test]
    fn a8_symlinks_are_shown_but_never_placed() {
        let g = Fixture::new().gate(E, "");
        assert_eq!(g.show(&p("skills/link"), ExistingKind::Symlink), SHOWN);
        assert_eq!(g.show(&p("link"), ExistingKind::Symlink), HIDDEN);
        assert_eq!(
            g.place_new(&p("skills/l2"), NewKind::Symlink),
            Placement::Refused(Denial::Kind)
        );
        assert_eq!(
            g.place_move(
                Source::Tree(&p("skills/link"), ExistingKind::Symlink),
                &p("skills/l3")
            ),
            Placement::Refused(Denial::Kind)
        );
    }

    #[test]
    fn a9_hard_links_and_special_files_are_refused() {
        let g = Fixture::new().gate(E, "");
        for path in ["skills/c.yaml", "hidden.yaml", "config.yaml"] {
            assert_eq!(
                g.place_new(&p(path), NewKind::Hardlink),
                Placement::Refused(Denial::Kind),
                "{path}"
            );
            assert_eq!(
                g.place_new(&p(path), NewKind::Special),
                Placement::Refused(Denial::Kind),
                "{path}"
            );
        }
        assert_eq!(g.show(&p("skills/fifo"), ExistingKind::Special), HIDDEN);
        assert_eq!(
            g.place_move(
                Source::Tree(&p("skills/fifo"), ExistingKind::Special),
                &p("skills/f2")
            ),
            Placement::Refused(Denial::SourceHidden)
        );
    }

    #[test]
    fn a10_tracking_is_ignored() {
        let g = Fixture::new().gate("/*\n!/config.yaml\n", "");
        assert_eq!(g.show(&p("old.txt"), ExistingKind::File), HIDDEN);
        assert_eq!(
            g.rules.decide(&p("old.txt"), true).unwrap(),
            Decision::Record
        );
        assert_eq!(
            g.place_move(
                Source::Tree(&p("old.txt"), ExistingKind::File),
                &p("config.yaml")
            ),
            Placement::Refused(Denial::SourceHidden)
        );
    }

    #[test]
    fn a11_directories() {
        let f = Fixture::new();
        f.mkdir("skills/repo/.git");
        let g = f.gate(E, "");
        assert_eq!(g.show(&p("skills"), ExistingKind::Directory), SHOWN);
        assert_eq!(g.may_show(&p("skills")), SHOWN);
        assert_eq!(g.show(&p("secrets"), ExistingKind::Directory), HIDDEN);
        assert_eq!(g.may_show(&p("secrets")), HIDDEN);
        assert_eq!(
            g.place_new(&p("skills/new"), NewKind::Directory),
            Placement::Tree
        );
        assert_eq!(
            g.place_new(&p("cache"), NewKind::Directory),
            Placement::Refused(Denial::NotRecordable)
        );
        assert_eq!(g.show(&p("skills/repo"), ExistingKind::Directory), HIDDEN);
        assert_eq!(g.show(&p("skills/repo/x"), ExistingKind::File), HIDDEN);
    }

    #[test]
    fn a12_never_record_groups_and_shared_memory() {
        let f = Fixture::new();
        let g = f.gate(E, "/skills/*.key -agentfs\n");
        assert_eq!(g.show(&p("skills/a.key"), ExistingKind::File), HIDDEN);
        assert_eq!(
            g.place_new(&p("skills/a.key"), NewKind::File),
            Placement::TmpLayer
        );

        let group = "/skills/db agentfs.commit=fsync agentfs.group=g\n\
                     /skills/db-wal agentfs.commit=fsync agentfs.group=g\n";
        let split = f.gate(&format!("{E}/skills/db-wal\n"), group);
        assert_eq!(split.show(&p("skills/db"), ExistingKind::File), HIDDEN);
        assert_eq!(
            split.place_new(&p("skills/db"), NewKind::File),
            Placement::TmpLayer
        );
        let whole = f.gate(E, group);
        assert_eq!(whole.show(&p("skills/db"), ExistingKind::File), SHOWN);
        assert_eq!(whole.show(&p("skills/db-wal"), ExistingKind::File), SHOWN);
        assert_eq!(whole.show(&p("skills/db-shm"), ExistingKind::File), HIDDEN);
    }

    #[test]
    fn a13_rules_errors_fail_closed() {
        let f = Fixture::new();
        let g = f.gate(E, "");
        // Longer than any file name the host allows, so probing `D/.git`
        // for a nested repository fails with ENAMETOOLONG, which is not
        // "absent".
        let long = "d".repeat(300);
        let x = p(&format!("{long}/x"));
        assert!(g.rules.nested_ancestor(&x).is_err());

        assert_eq!(g.show(&x, ExistingKind::File), RULES_ERROR);
        assert_eq!(g.show(&x, ExistingKind::Symlink), RULES_ERROR);
        assert_eq!(g.show(&x, ExistingKind::Directory), RULES_ERROR);
        assert_eq!(g.may_show(&x), RULES_ERROR);
        assert_eq!(
            g.place_new(&x, NewKind::File),
            Placement::Refused(Denial::RulesError)
        );
        assert_eq!(
            g.place_new(&x, NewKind::Directory),
            Placement::Refused(Denial::RulesError)
        );
        assert_eq!(
            g.place_move(Source::TmpLayer(&p("a.tmp")), &x),
            Placement::Refused(Denial::RulesError)
        );
        assert_eq!(
            g.place_move(Source::Tree(&x, ExistingKind::File), &p("config.yaml")),
            Placement::Refused(Denial::RulesError)
        );
        assert_eq!(
            g.place_move(Source::Tree(&x, ExistingKind::Directory), &p("skills/b")),
            Placement::Refused(Denial::RulesError)
        );
        assert_eq!(
            g.place_move(Source::Tree(&p("config.yaml"), ExistingKind::File), &x),
            Placement::Refused(Denial::RulesError)
        );
    }

    /// I1 for `may_show`: an error from either predicate is a rules
    /// error, even when the other predicate would show the name.
    #[test]
    fn may_show_fails_closed_when_either_predicate_errs() {
        let ok = |b: bool| Ok::<bool, ()>(b);
        assert_eq!(either_kind(ok(true), ok(false)), SHOWN);
        assert_eq!(either_kind(ok(false), ok(true)), SHOWN);
        assert_eq!(either_kind(ok(false), ok(false)), HIDDEN);
        assert_eq!(either_kind(Err(()), ok(true)), RULES_ERROR);
        assert_eq!(either_kind(ok(true), Err(())), RULES_ERROR);
        assert_eq!(either_kind(Err(()), ok(false)), RULES_ERROR);
        assert_eq!(either_kind(ok(false), Err(())), RULES_ERROR);
        assert_eq!(either_kind::<()>(Err(()), Err(())), RULES_ERROR);
    }

    #[test]
    fn a14_gate_is_send_and_sync() {
        fn check<T: Send + Sync>() {}
        check::<Gate>();
    }
}
