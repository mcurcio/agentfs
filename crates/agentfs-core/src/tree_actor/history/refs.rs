//! The ref layout of a tree volume's history (docs/target-architecture/git-model.md §4), its
//! one home. Prefixes end in '/', so `fork/7/` never matches `fork/70/`.

use std::collections::BTreeMap;

use git2::Oid;

use crate::tree_actor::pointers::Pointers;
use crate::tree_actor::tree::EventNumber;
use crate::tree_actor::{Error, Result, TreeId};

/// Every pointer: what `check_objects` names when a lost object is no one pointer's.
pub(in crate::tree_actor) const ALL: &str = "refs/";
/// Every queue entry of every tree: the queued metric.
pub(in crate::tree_actor) const QUEUES: &str = "refs/queue/";
const BRANCHES: &str = "refs/heads/";
const COLLISIONS: &str = "refs/collisions/";

/// A tree's branch: `refs/heads/<tree>`.
pub(in crate::tree_actor) fn branch(tree: TreeId) -> String {
    format!("{BRANCHES}{tree}")
}

/// HEAD's ref: the root's branch.
pub(in crate::tree_actor) fn head() -> String {
    branch(TreeId::Root)
}

/// Every branch's tree and head; [`Error::Format`] for a `refs/heads/` name that is no
/// [`TreeId`].
pub(in crate::tree_actor) fn branches(pointers: &Pointers) -> Result<BTreeMap<TreeId, Oid>> {
    pointers
        .under(BRANCHES)
        .into_iter()
        .map(|(name, head)| {
            let tree = name.strip_prefix(BRANCHES).ok_or(Error::Format)?.parse()?;
            Ok((tree, head))
        })
        .collect()
}

/// The tree whose branch `name` is, if it names one.
pub(in crate::tree_actor) fn tree_of_branch(name: &str) -> Option<TreeId> {
    name.strip_prefix(BRANCHES)?.parse().ok()
}

/// A tree's queue: the prefix of its entries, `refs/queue/<tree>/`.
pub(in crate::tree_actor) fn queue(tree: TreeId) -> String {
    format!("{QUEUES}{tree}/")
}

/// One queue entry, numbered by the step that caused it and zero-padded to 20 digits, so
/// name order is queue order.
pub(in crate::tree_actor) fn queue_entry(tree: TreeId, event: EventNumber) -> String {
    format!("{}{event:020}", queue(tree))
}

/// The ref that keeps a landing that collided in `tree`: `refs/collisions/<tree>/<loser>`.
pub(in crate::tree_actor) fn collision(tree: TreeId, loser: Oid) -> String {
    format!("{COLLISIONS}{tree}/{loser}")
}

/// A tree's object table: `refs/agentfs/objects/<tree>`.
pub(in crate::tree_actor) fn object_table(tree: TreeId) -> String {
    format!("refs/agentfs/objects/{tree}")
}

/// The tips that reach every save's commit: branches, the root's queue (only it holds
/// landings; syncs are blobs), collision refs.
pub(in crate::tree_actor) fn owner_tips(pointers: &Pointers) -> Vec<Oid> {
    let mut tips = Vec::new();
    for prefix in [BRANCHES, &queue(TreeId::Root), COLLISIONS] {
        tips.extend(pointers.under(prefix).into_iter().map(|(_, oid)| oid));
    }
    tips
}

#[cfg(test)]
mod tests {
    use super::*;

    const FORK_7: TreeId = TreeId::Fork(7);
    const FORK_70: TreeId = TreeId::Fork(70);

    fn oid(n: u8) -> Oid {
        Oid::from_bytes(&[n; 20]).unwrap()
    }

    #[test]
    fn each_ref_is_the_layouts_literal_text() {
        assert_eq!(ALL, "refs/");
        assert_eq!(QUEUES, "refs/queue/");
        assert_eq!(branch(TreeId::Root), "refs/heads/root");
        assert_eq!(branch(FORK_7), "refs/heads/fork/7");
        assert_eq!(branch(FORK_70), "refs/heads/fork/70");
        assert_eq!(head(), "refs/heads/root");
        assert_eq!(queue(TreeId::Root), "refs/queue/root/");
        assert_eq!(queue(FORK_7), "refs/queue/fork/7/");
        assert_eq!(queue(FORK_70), "refs/queue/fork/70/");
        assert_eq!(
            queue_entry(TreeId::Root, EventNumber::new(12)),
            "refs/queue/root/00000000000000000012"
        );
        assert_eq!(
            queue_entry(FORK_7, EventNumber::new(3)),
            "refs/queue/fork/7/00000000000000000003"
        );
        assert_eq!(
            queue_entry(FORK_70, EventNumber::new(u64::MAX)),
            "refs/queue/fork/70/18446744073709551615"
        );
        let loser = oid(0xab);
        assert_eq!(
            collision(TreeId::Root, loser),
            format!("refs/collisions/root/{}", "ab".repeat(20))
        );
        assert_eq!(
            collision(FORK_7, loser),
            format!("refs/collisions/fork/7/{}", "ab".repeat(20))
        );
        assert_eq!(object_table(TreeId::Root), "refs/agentfs/objects/root");
        assert_eq!(object_table(FORK_7), "refs/agentfs/objects/fork/7");
        assert_eq!(object_table(FORK_70), "refs/agentfs/objects/fork/70");
    }

    #[test]
    fn a_forks_queue_is_no_prefix_of_another_forks_entries() {
        let entry = queue_entry(FORK_70, EventNumber::new(1));
        assert!(!entry.starts_with(&queue(FORK_7)));
        assert!(entry.starts_with(&queue(FORK_70)));
        let pointers = Pointers::new([(entry, oid(1))].into_iter().collect());
        assert!(pointers.under(&queue(FORK_7)).is_empty());
    }

    #[test]
    fn branches_parses_each_tree_and_refuses_a_name_that_is_none() {
        let pointers = Pointers::new(
            [
                (branch(TreeId::Root), oid(1)),
                (branch(FORK_7), oid(2)),
                (queue_entry(FORK_7, EventNumber::new(4)), oid(3)),
            ]
            .into_iter()
            .collect(),
        );
        assert_eq!(
            branches(&pointers).unwrap(),
            [(TreeId::Root, oid(1)), (FORK_7, oid(2))]
                .into_iter()
                .collect()
        );
        let bad = Pointers::new(
            [("refs/heads/main".to_owned(), oid(1))]
                .into_iter()
                .collect(),
        );
        assert!(matches!(branches(&bad), Err(Error::Format)));
    }

    #[test]
    fn owner_tips_are_branches_the_roots_queue_and_collisions() {
        let pointers = Pointers::new(
            [
                (branch(TreeId::Root), oid(1)),
                (queue_entry(TreeId::Root, EventNumber::new(5)), oid(2)),
                (queue_entry(FORK_7, EventNumber::new(5)), oid(3)),
                (collision(TreeId::Root, oid(4)), oid(4)),
                (object_table(TreeId::Root), oid(5)),
            ]
            .into_iter()
            .collect(),
        );
        assert_eq!(owner_tips(&pointers), [oid(1), oid(2), oid(4)]);
    }
}
