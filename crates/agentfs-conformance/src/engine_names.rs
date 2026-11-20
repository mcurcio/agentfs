//! How the engine names a trace's trees (VERIFYING.md, "The model as the
//! conformance suite"). The root is the engine's `root`; fork n is the n-th fork
//! the engine made, `fork/<event>`, never reused. The one type that maps a model
//! [`TreeId`] to an engine [`EngineTree`] in this crate, apart from the model's
//! own `world::TreeNames`: the engine numbers its forks by event, the model by
//! order.
//!
//! A replay holds three of them: the system's ([`crate::Library`] or
//! [`crate::Mounted`]), filled as it makes forks ([`EngineNames::forked`]), and
//! [`crate::EngineHistory`]'s and [`crate::invariants::Invariants`]'s, filled from
//! git ([`EngineNames::read_forks`]). They agree because `forked` refuses a fork
//! whose event is not past the last fork's, so the order made is the order of
//! events, and `Invariants` checks after each fork that git lists exactly the
//! forks the trace made.

use agentfs_core::tree_actor::TreeId as EngineTree;
use agentfs_model::calls::{CallError, TreeId};

/// The engine's tree for each tree of a trace.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct EngineNames {
    forks: Vec<EngineTree>,
}

/// The engine fork `name` names; an error for the root or no tree.
fn fork_of(name: &str) -> Result<EngineTree, CallError> {
    match name.parse() {
        Ok(fork @ EngineTree::Fork(_)) => Ok(fork),
        _ => Err(CallError::new(format!("{name:?} is no fork's name"))),
    }
}

impl EngineNames {
    /// The `git` arguments that list every fork, one name a line, for
    /// [`Self::read_forks`].
    pub const LIST_FORKS: [&str; 3] = [
        "for-each-ref",
        "--format=%(refname:lstrip=2)",
        "refs/heads/fork/",
    ];

    /// How many forks are made.
    pub fn forks(&self) -> usize {
        self.forks.len()
    }

    /// The root, and no fork yet.
    pub fn new() -> EngineNames {
        EngineNames::default()
    }

    /// The engine just made `fork`, the next in order: its tree. An error for
    /// the root, or a fork whose event is not past the last fork's, since then
    /// the order made would not be the order [`Self::read_forks`] reads.
    pub fn forked(&mut self, fork: EngineTree) -> Result<TreeId, CallError> {
        if fork.is_root() {
            return Err(CallError::new("the root is no fork"));
        }
        if let Some(last) = self.forks.last()
            && *last >= fork
        {
            return Err(CallError::new(format!(
                "fork {fork} made after {last}: events out of order"
            )));
        }
        self.forks.push(fork);
        Ok(TreeId::new(self.forks.len() as u32))
    }

    /// Every fork made, from [`Self::LIST_FORKS`]'s output, in the order of
    /// their event numbers.
    pub fn read_forks(&mut self, listed: &str) -> Result<(), CallError> {
        let mut forks = listed
            .lines()
            .map(fork_of)
            .collect::<Result<Vec<_>, CallError>>()?;
        forks.sort();
        self.forks = forks;
        Ok(())
    }

    /// Tree `t`'s engine tree. An error for a fork not made.
    pub fn name(&self, t: TreeId) -> Result<EngineTree, CallError> {
        match t {
            TreeId::ROOT => Ok(EngineTree::Root),
            f => self
                .forks
                .get(f.number() as usize - 1)
                .copied()
                .ok_or_else(|| CallError::new(format!("no fork {f}"))),
        }
    }

    /// Tree `t`'s branch, `refs/heads/<tree>`.
    pub fn branch(&self, t: TreeId) -> Result<String, CallError> {
        Ok(format!("refs/heads/{}", self.name(t)?))
    }

    /// The tree an `Agentfs-Tree` trailer names; none for a tree never made.
    pub fn tree(&self, name: &str) -> Option<TreeId> {
        match name.parse().ok()? {
            EngineTree::Root => Some(TreeId::ROOT),
            fork => self
                .forks
                .iter()
                .position(|f| *f == fork)
                .map(|i| TreeId::new(i as u32 + 1)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn forks_are_ordered_by_event_and_named_back() {
        let mut names = EngineNames::new();
        names.read_forks("fork/10\nfork/9\n").unwrap();
        assert_eq!(names.name(TreeId::new(1)).unwrap(), EngineTree::Fork(9));
        assert_eq!(names.name(TreeId::new(2)).unwrap(), EngineTree::Fork(10));
        assert_eq!(names.branch(TreeId::ROOT).unwrap(), "refs/heads/root");
        assert!(names.name(TreeId::new(3)).is_err(), "a fork not made");
        for t in [TreeId::ROOT, TreeId::new(1), TreeId::new(2)] {
            assert_eq!(names.tree(&names.name(t).unwrap().to_string()), Some(t));
        }
        assert_eq!(names.tree("fork/11"), None);
        assert_eq!(names.tree("main"), None);
        assert_eq!(names.forks(), 2);
        assert!(
            names.forked(EngineTree::Fork(10)).is_err(),
            "not past fork/10"
        );
        assert!(names.forked(EngineTree::Root).is_err());
        assert_eq!(names.forked(EngineTree::Fork(12)).unwrap(), TreeId::new(3));
        assert!(names.read_forks("main\n").is_err());
        assert!(names.read_forks("root\n").is_err());
    }
}
