//! Which store a recorded file's content goes to (design §7, §4.2).
//!
//! In order:
//!
//! 1. Always a git blob: empty files, git's special files
//!    (`.gitattributes`, `.gitmodules`, `.gitignore`, `.mailmap`, in any
//!    spelling git treats as one) and `.lfsconfig`, and a path no
//!    `.gitattributes` line can name exactly
//!    ([`super::attributes::pattern_for`]). Symlinks are never LFS.
//! 2. `filter=lfs`: LFS, even when `agentfs.store=blob` is also set
//!    (`blob` may not override `filter=lfs`). Operator `info/attributes`
//!    always counts; a tracked `.gitattributes` only with
//!    `respectTreeRules`, which [`crate::rules::Rules::attributes`] already
//!    applies.
//! 3. `agentfs.store=lfs` or `agentfs.store=blob` (operator attributes
//!    only).
//! 4. `agentfs.lfsThreshold`, if set: content strictly larger is LFS.
//! 5. Otherwise a git blob.
//!
//! The decision is made when the file is recorded, on the frozen copy's
//! size, and saved in the journal with the change.

use serde::{Deserialize, Serialize};

use super::attributes::pattern_for;
use crate::paths::{RepoPath, special_file};
use crate::rules::{AttrState, Attributes};

/// Where a recorded file's content is stored.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Store {
    /// An ordinary git blob.
    #[default]
    Blob,
    /// An LFS object, with a pointer blob in history.
    Lfs,
}

impl Store {
    pub fn is_blob(&self) -> bool {
        *self == Self::Blob
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Blob => "blob",
            Self::Lfs => "lfs",
        }
    }
}

/// The store selection rules of one volume.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct StorePolicy {
    /// `agentfs.lfsThreshold`.
    pub threshold: Option<u64>,
}

impl StorePolicy {
    /// The store for a regular file at `path` holding `size` bytes, with
    /// `attrs` its attributes (see the module documentation).
    pub fn decide(&self, path: &RepoPath, size: u64, attrs: &Attributes) -> Store {
        if size == 0 || always_blob(path) || pattern_for(path).is_none() {
            return Store::Blob;
        }
        if attrs.get("filter") == Some(&AttrState::Value("lfs".into())) {
            return Store::Lfs;
        }
        match attrs.get("agentfs.store") {
            Some(AttrState::Value(v)) if v == "lfs" => return Store::Lfs,
            Some(AttrState::Value(v)) if v == "blob" => return Store::Blob,
            _ => {}
        }
        match self.threshold {
            Some(threshold) if size > threshold => Store::Lfs,
            _ => Store::Blob,
        }
    }

    /// Whether this policy can store anything as LFS without an attribute:
    /// a threshold is set.
    pub fn has_threshold(&self) -> bool {
        self.threshold.is_some()
    }
}

/// Files whose content git or git-lfs itself reads, which must stay
/// readable as blobs.
fn always_blob(path: &RepoPath) -> bool {
    let name = path.file_name();
    special_file(name).is_some() || name.eq_ignore_ascii_case(".lfsconfig")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rules::{RuleOptions, Rules};

    fn p(s: &str) -> RepoPath {
        RepoPath::new(s).unwrap()
    }

    /// Attributes of `path` from operator `info/attributes` and, with
    /// `respect`, the tree's `.gitattributes`.
    fn attrs(info: &str, tree_attrs: &str, respect: bool, path: &str) -> Attributes {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(".gitattributes"), tree_attrs).unwrap();
        let options = RuleOptions {
            respect_tree_rules: respect,
            ..RuleOptions::default()
        };
        let rules = Rules::new(dir.path(), options, b"", info.as_bytes());
        rules.attributes(&p(path)).unwrap()
    }

    #[test]
    fn selection_order() {
        let none = Attributes::default();
        let threshold = StorePolicy {
            threshold: Some(100),
        };
        let off = StorePolicy::default();
        // The threshold is strict.
        assert_eq!(threshold.decide(&p("a"), 100, &none), Store::Blob);
        assert_eq!(threshold.decide(&p("a"), 101, &none), Store::Lfs);
        // No threshold, no size rule.
        assert_eq!(off.decide(&p("a"), u64::MAX, &none), Store::Blob);
        // Attributes.
        let lfs = attrs("*.psd filter=lfs\n", "", false, "x.psd");
        assert_eq!(off.decide(&p("x.psd"), 1, &lfs), Store::Lfs);
        let forced = attrs("big agentfs.store=lfs\n", "", false, "big");
        assert_eq!(off.decide(&p("big"), 1, &forced), Store::Lfs);
        let blob = attrs("keep agentfs.store=blob\n", "", false, "keep");
        assert_eq!(threshold.decide(&p("keep"), 1000, &blob), Store::Blob);
        // `blob` may not override `filter=lfs`.
        let both = attrs("x filter=lfs agentfs.store=blob\n", "", false, "x");
        assert_eq!(off.decide(&p("x"), 1, &both), Store::Lfs);
        // An unset or other filter is not LFS.
        let other = attrs("x filter=crypt\n", "", false, "x");
        assert_eq!(off.decide(&p("x"), 1, &other), Store::Blob);
    }

    #[test]
    fn some_files_are_always_blobs() {
        let forced = attrs("* agentfs.store=lfs filter=lfs\n", "", false, "x");
        let policy = StorePolicy { threshold: Some(1) };
        for path in [
            ".gitattributes",
            "sub/.gitignore",
            ".GITMODULES",
            "gitmod~1",
            ".mailmap",
            ".lfsconfig",
            "d/.LfsConfig",
            "tab\there",
        ] {
            assert_eq!(
                policy.decide(&p(path), 1000, &forced),
                Store::Blob,
                "{path}"
            );
        }
        // Empty files.
        assert_eq!(policy.decide(&p("x"), 0, &forced), Store::Blob);
    }

    #[test]
    fn tree_filter_lfs_counts_only_with_respect_tree_rules() {
        let off = StorePolicy::default();
        let ignored = attrs("", "*.bin filter=lfs\n", false, "a.bin");
        assert_eq!(off.decide(&p("a.bin"), 10, &ignored), Store::Blob);
        let respected = attrs("", "*.bin filter=lfs\n", true, "a.bin");
        assert_eq!(off.decide(&p("a.bin"), 10, &respected), Store::Lfs);
        // A tree file never sets agentfs.store.
        let tree_store = attrs("", "*.bin agentfs.store=lfs\n", true, "a.bin");
        assert_eq!(off.decide(&p("a.bin"), 10, &tree_store), Store::Blob);
        // The operator's `blob` cannot beat a tree `filter=lfs` either.
        let mixed = attrs(
            "*.bin agentfs.store=blob\n",
            "*.bin filter=lfs\n",
            true,
            "a.bin",
        );
        assert_eq!(off.decide(&p("a.bin"), 10, &mixed), Store::Lfs);
    }
}
