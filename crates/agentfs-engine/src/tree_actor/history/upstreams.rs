//! Each fork's upstream: the shape of a volume's trees (docs/trees.md). The root has none.

use std::collections::BTreeMap;

use super::refs;
use crate::tree_actor::{Error, Result, TreeId};

/// Each fork's upstream.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(in crate::tree_actor) struct Upstreams(BTreeMap<TreeId, TreeId>);

impl Upstreams {
    /// Fork number `fork` syncs from and lands into `upstream`. It takes a fork's number, so the
    /// root cannot be a fork.
    pub(in crate::tree_actor) fn add(&mut self, fork: u64, upstream: TreeId) {
        self.0.insert(TreeId::Fork(fork), upstream);
    }

    /// A dropped fork leaves the shape: it syncs from and lands into nothing.
    pub(in crate::tree_actor) fn remove(&mut self, fork: TreeId) {
        self.0.remove(&fork);
    }

    /// Recovery's check: [`Error::Format`] unless every fork has a branch (`has_branch`) and
    /// its upstream is the root. Forks are made only of the root, and only the root's queue
    /// holds landings (`take_next`, `refs::owner_tips`).
    pub(in crate::tree_actor) fn check_recovered(
        &self,
        has_branch: impl Fn(TreeId) -> bool,
    ) -> Result<()> {
        if self
            .0
            .iter()
            .all(|(fork, upstream)| upstream.is_root() && has_branch(*fork))
        {
            Ok(())
        } else {
            Err(Error::Format)
        }
    }

    pub(in crate::tree_actor) fn upstream_of(&self, tree: TreeId) -> Option<TreeId> {
        self.0.get(&tree).copied()
    }

    /// The trees `tree` is the upstream of, by number.
    pub(in crate::tree_actor) fn forks_of(
        &self,
        tree: TreeId,
    ) -> impl Iterator<Item = TreeId> + '_ {
        self.0
            .iter()
            .filter(move |(_, upstream)| **upstream == tree)
            .map(|(fork, _)| *fork)
    }

    /// Each fork's text and its upstream's: the journal's checkpoint.
    pub(in crate::tree_actor) fn to_text(&self) -> BTreeMap<String, String> {
        self.0
            .iter()
            .map(|(fork, upstream)| (fork.to_string(), upstream.to_string()))
            .collect()
    }

    /// Each fork's text and the full ref it merges: Git's config.
    pub(in crate::tree_actor) fn merges(&self) -> BTreeMap<String, String> {
        self.0
            .iter()
            .map(|(fork, upstream)| (fork.to_string(), refs::branch(*upstream)))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn of(pairs: &[(u64, TreeId)]) -> Upstreams {
        let mut upstreams = Upstreams::default();
        for (fork, upstream) in pairs {
            upstreams.add(*fork, *upstream);
        }
        upstreams
    }

    #[test]
    fn check_recovered_accepts_forks_of_the_root_with_branches() {
        let upstreams = of(&[(1, TreeId::Root), (4, TreeId::Root)]);
        upstreams.check_recovered(|_| true).unwrap();
        Upstreams::default().check_recovered(|_| false).unwrap();
    }

    #[test]
    fn check_recovered_refuses_a_fork_of_a_fork() {
        let upstreams = of(&[(1, TreeId::Root), (2, TreeId::Fork(1))]);
        assert!(matches!(
            upstreams.check_recovered(|_| true),
            Err(Error::Format)
        ));
    }

    #[test]
    fn check_recovered_refuses_a_fork_with_no_branch() {
        let upstreams = of(&[(1, TreeId::Root), (5, TreeId::Root)]);
        assert!(matches!(
            upstreams.check_recovered(|tree| tree != TreeId::Fork(5)),
            Err(Error::Format)
        ));
    }

    #[test]
    fn upstream_of_and_forks_of_read_the_map() {
        let upstreams = of(&[(10, TreeId::Root), (2, TreeId::Root), (3, TreeId::Fork(2))]);
        assert_eq!(upstreams.upstream_of(TreeId::Fork(2)), Some(TreeId::Root));
        assert_eq!(upstreams.upstream_of(TreeId::Root), None);
        assert_eq!(
            upstreams.forks_of(TreeId::Root).collect::<Vec<_>>(),
            [TreeId::Fork(2), TreeId::Fork(10)]
        );
        assert_eq!(
            upstreams.forks_of(TreeId::Fork(2)).collect::<Vec<_>>(),
            [TreeId::Fork(3)]
        );
    }

    #[test]
    fn a_removed_fork_leaves_the_shape() {
        let mut upstreams = of(&[(1, TreeId::Root), (2, TreeId::Root)]);
        upstreams.remove(TreeId::Fork(1));
        assert_eq!(upstreams.upstream_of(TreeId::Fork(1)), None);
        assert_eq!(
            upstreams.forks_of(TreeId::Root).collect::<Vec<_>>(),
            [TreeId::Fork(2)]
        );
        upstreams
            .check_recovered(|tree| tree != TreeId::Fork(1))
            .unwrap();
    }

    #[test]
    fn its_texts_are_the_checkpoints_and_the_configs() {
        let upstreams = of(&[(7, TreeId::Root)]);
        assert_eq!(
            upstreams.to_text(),
            [("fork/7".to_owned(), "root".to_owned())]
                .into_iter()
                .collect()
        );
        assert_eq!(
            upstreams.merges(),
            [("fork/7".to_owned(), "refs/heads/root".to_owned())]
                .into_iter()
                .collect()
        );
    }
}
