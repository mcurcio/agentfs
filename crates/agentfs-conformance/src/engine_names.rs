//! How the engine names a trace's trees (VERIFYING.md, "The model as the
//! conformance suite"). The root goes by the name it is given (today
//! [`crate::ENGINE_ROOT`], through `engine_names`); fork n by the name the engine
//! gave the n-th fork made, `fork/<event>`, never reused. The one type that maps a
//! [`TreeId`] to an engine name in this crate, apart from the model's own
//! `world::TreeNames`: the engine numbers its forks by event, the model by order.
//!
//! A replay holds three of them: the system's ([`crate::Library`] or
//! [`crate::Mounted`]), filled as it makes forks ([`EngineNames::forked`]), and
//! [`crate::EngineHistory`]'s and [`crate::invariants::Invariants`]'s, filled from
//! git ([`EngineNames::read_forks`]). They agree because `forked` refuses a fork
//! whose event is not past the last fork's, so the order made is the order of
//! events, and `Invariants` checks after each fork that git lists exactly the
//! forks the trace made.

use agentfs_model::calls::{CallError, TreeId};

/// The engine's name of each tree of a trace.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EngineNames {
    root: String,
    forks: Vec<String>,
}

/// The event number of fork `name`, `fork/<event>`.
fn event_of(name: &str) -> Result<u64, CallError> {
    name.strip_prefix("fork/")
        .and_then(|n| n.parse::<u64>().ok())
        .ok_or_else(|| CallError::new(format!("{name:?} is no fork's name")))
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

    /// The root named `root`, and no fork yet.
    pub fn new(root: &str) -> EngineNames {
        EngineNames {
            root: root.to_owned(),
            forks: Vec::new(),
        }
    }

    /// The engine just made fork `name`, the next in order: its tree. An error
    /// for a name that is no fork's, or whose event is not past the last fork's,
    /// since then the order made would not be the order [`Self::read_forks`] reads.
    pub fn forked(&mut self, name: String) -> Result<TreeId, CallError> {
        let event = event_of(&name)?;
        if let Some(last) = self.forks.last()
            && event_of(last)? >= event
        {
            return Err(CallError::new(format!(
                "fork {name:?} made after {last:?}: events out of order"
            )));
        }
        self.forks.push(name);
        Ok(TreeId::new(self.forks.len() as u32))
    }

    /// Every fork made, from [`Self::LIST_FORKS`]'s output, in the order of
    /// their event numbers.
    pub fn read_forks(&mut self, listed: &str) -> Result<(), CallError> {
        let mut forks = listed
            .lines()
            .map(|name| Ok((event_of(name)?, name.to_owned())))
            .collect::<Result<Vec<_>, CallError>>()?;
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
        assert_eq!(names.forks(), 2);
        assert!(names.forked("fork/10".into()).is_err(), "not past fork/10");
        assert!(names.forked("main".into()).is_err());
        assert_eq!(names.forked("fork/12".into()).unwrap(), TreeId::new(3));
        assert!(names.read_forks("main\n").is_err());
    }
}
