//! The ref layout of a tree volume's history (docs/target-architecture/git-model.md §4), its
//! one home. Prefixes end in '/', so `fork/7/` never matches `fork/70/`.

use std::collections::BTreeMap;

use git2::Oid;

use crate::tree_actor::pointers::Pointers;
use crate::tree_actor::rows::Chain;
use crate::tree_actor::tree::EventNumber;
use crate::tree_actor::{Error, Result, TreeId};
use agentfs_contracts::tree::ForkId;

/// Every pointer: what `check_objects` names when a lost object is no one pointer's.
pub(in crate::tree_actor) const ALL: &str = "refs/";
/// Every queue entry of every tree: the queued metric.
pub(in crate::tree_actor) const QUEUES: &str = "refs/queue/";
const BRANCHES: &str = "refs/heads/";
const COLLISIONS: &str = "refs/collisions/";
/// Every row chain: the domains' event commits, beside the trees they concern.
pub(in crate::tree_actor) const ROWS: &str = "refs/agentfs/";
const VOLUME_ROW: &str = "refs/agentfs/volume";
const TREES_ROWS: &str = "refs/agentfs/trees/";
const CLIENTS_ROWS: &str = "refs/agentfs/clients/";

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

/// The tree whose queue entry `name` is, if it names one.
pub(in crate::tree_actor) fn tree_of_queue_entry(name: &str) -> Option<TreeId> {
    let (tree, _) = name.strip_prefix(QUEUES)?.rsplit_once('/')?;
    tree.parse().ok()
}

/// The ref that keeps a landing that collided in `tree`: `refs/collisions/<tree>/<loser>`.
pub(in crate::tree_actor) fn collision(tree: TreeId, loser: Oid) -> String {
    format!("{COLLISIONS}{tree}/{loser}")
}

/// A row chain's ref: `refs/agentfs/volume`, `refs/agentfs/trees/fork/<n>` or
/// `refs/agentfs/clients/<tree>`.
pub(in crate::tree_actor) fn row_chain(chain: Chain) -> String {
    match chain {
        Chain::Volume => VOLUME_ROW.to_owned(),
        Chain::Trees(fork) => format!("{TREES_ROWS}{}", TreeId::from(fork)),
        Chain::Clients(tree) => format!("{CLIENTS_ROWS}{tree}"),
    }
}

/// The chain `name` is the ref of, if it names one.
pub(in crate::tree_actor) fn chain_of_row(name: &str) -> Option<Chain> {
    if name == VOLUME_ROW {
        return Some(Chain::Volume);
    }
    if let Some(tree) = name.strip_prefix(TREES_ROWS) {
        let fork: ForkId = tree.parse::<TreeId>().ok()?.fork()?;
        return Some(Chain::Trees(fork));
    }
    Some(Chain::Clients(
        name.strip_prefix(CLIENTS_ROWS)?.parse().ok()?,
    ))
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
        assert_eq!(ROWS, "refs/agentfs/");
        assert_eq!(row_chain(Chain::Volume), "refs/agentfs/volume");
        assert_eq!(
            row_chain(Chain::Trees(ForkId(7))),
            "refs/agentfs/trees/fork/7"
        );
        assert_eq!(
            row_chain(Chain::Clients(TreeId::Root)),
            "refs/agentfs/clients/root"
        );
        assert_eq!(
            row_chain(Chain::Clients(FORK_70)),
            "refs/agentfs/clients/fork/70"
        );
    }

    #[test]
    fn a_row_chain_reads_back_and_nothing_else_is_one() {
        for chain in [
            Chain::Volume,
            Chain::Trees(ForkId(7)),
            Chain::Clients(TreeId::Root),
            Chain::Clients(FORK_70),
        ] {
            assert_eq!(chain_of_row(&row_chain(chain)), Some(chain));
        }
        for name in [
            "refs/heads/root",
            "refs/agentfs/trees/root",
            "refs/agentfs/trees/main",
            "refs/agentfs/clients/main",
            "refs/agentfs/volume/x",
            "refs/agentfs/objects/root",
        ] {
            assert_eq!(chain_of_row(name), None, "{name}");
        }
    }

    #[test]
    fn a_queue_entry_names_its_tree() {
        for tree in [TreeId::Root, FORK_7, FORK_70] {
            assert_eq!(
                tree_of_queue_entry(&queue_entry(tree, EventNumber::new(5))),
                Some(tree)
            );
        }
        assert_eq!(tree_of_queue_entry("refs/heads/root"), None);
        assert_eq!(tree_of_queue_entry("refs/queue/main/1"), None);
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
            ]
            .into_iter()
            .collect(),
        );
        assert_eq!(owner_tips(&pointers), [oid(1), oid(2), oid(4)]);
    }
}
