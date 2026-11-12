//! How the engine names a trace's trees (VERIFYING.md, "The model as the
//! conformance suite"). The root goes by the name it is given (today
//! [`crate::ENGINE_ROOT`], through `engine_names`); fork n by the name the engine
//! gave the n-th fork made, `fork/<event>`, never reused. The one map from a
//! [`TreeId`] to an engine name in this crate, apart from the model's own
//! `world::TreeNames`: the engine numbers its forks by event, the model by order.

use agentfs_model::calls::{CallError, TreeId};

/// The engine's name of each tree of a trace.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EngineNames {
    root: String,
    forks: Vec<String>,
}

impl EngineNames {
    /// The root named `root`, and no fork yet.
    pub fn new(root: &str) -> EngineNames {
        EngineNames {
            root: root.to_owned(),
            forks: Vec::new(),
        }
    }

    /// The engine just made fork `name`, the next in order: its tree.
    pub fn forked(&mut self, name: String) -> TreeId {
        self.forks.push(name);
        TreeId::new(self.forks.len() as u32)
    }

    /// Every fork made, from `git for-each-ref --format=%(refname:lstrip=2)
    /// refs/heads/fork/`, in the order of their event numbers.
    pub fn read_forks(&mut self, listed: &str) -> Result<(), CallError> {
        let mut forks = listed
            .lines()
            .map(|name| {
                name.strip_prefix("fork/")
                    .and_then(|n| n.parse::<u64>().ok())
                    .map(|event| (event, name.to_owned()))
                    .ok_or_else(|| CallError::new(format!("{name:?} is no fork's name")))
            })
            .collect::<Result<Vec<_>, _>>()?;
        forks.sort();
        self.forks = forks.into_iter().map(|(_, name)| name).collect();
        Ok(())
    }

    /// Tree `t`'s engine name. An error for a fork not made.
    pub fn name(&self, t: TreeId) -> Result<&str, CallError> {
        match t {
            TreeId::ROOT => Ok(&self.root),
            f => self
                .forks
                .get(f.number() as usize - 1)
                .map(String::as_str)
                .ok_or_else(|| CallError::new(format!("no fork {f}"))),
        }
    }

    /// Tree `t`'s branch, `refs/heads/<name>`.
    pub fn branch(&self, t: TreeId) -> Result<String, CallError> {
        Ok(format!("refs/heads/{}", self.name(t)?))
    }

    /// The tree an `Agentfs-Tree` trailer names; none for a name never made.
    pub fn tree(&self, name: &str) -> Option<TreeId> {
        if name == self.root {
            return Some(TreeId::ROOT);
        }
        self.forks
            .iter()
            .position(|f| f == name)
            .map(|i| TreeId::new(i as u32 + 1))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn forks_are_ordered_by_event_and_named_back() {
        let mut names = EngineNames::new("r");
        names.read_forks("fork/10\nfork/9\n").unwrap();
        assert_eq!(names.name(TreeId::new(1)).unwrap(), "fork/9");
        assert_eq!(names.name(TreeId::new(2)).unwrap(), "fork/10");
        assert_eq!(names.branch(TreeId::ROOT).unwrap(), "refs/heads/r");
        assert!(names.name(TreeId::new(3)).is_err(), "a fork not made");
        for t in [TreeId::ROOT, TreeId::new(1), TreeId::new(2)] {
            assert_eq!(names.tree(names.name(t).unwrap()), Some(t));
        }
        assert_eq!(names.tree("fork/11"), None);
        assert_eq!(names.forked("fork/12".into()), TreeId::new(3));
        assert!(names.read_forks("main\n").is_err());
    }
}
