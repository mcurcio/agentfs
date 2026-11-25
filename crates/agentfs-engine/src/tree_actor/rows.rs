//! Rows: the domains' records as event commits beside the trees they concern
//! (docs/trees.md, "Rows and event commits"; git-model §4). The actor knows no
//! domain: the store adapter (`crate::store`) names the chains and writes the
//! rows' text. A row step is one journal record, its commit point; the builder
//! makes each row's commit behind it, as it does a save's. Rows are read from
//! the actor's saved rows, never Git, so they answer while history lags.

use std::collections::BTreeMap;

use agentfs_contracts::pod::PodContext;
use agentfs_contracts::tree::{ForkId, TreeId};

use super::Result;
use super::history::refs;
use super::journal::RowRecord;

/// Which domain's rows a chain holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum RowDomain {
    Volume,
    Trees,
    Clients,
}

/// One commit chain of rows: `refs/agentfs/volume`, `refs/agentfs/trees/fork/<n>`
/// (kept after the fork's drop) or `refs/agentfs/clients/<tree>`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Chain {
    Volume,
    Trees(ForkId),
    Clients(TreeId),
}

impl Chain {
    /// The domain whose rows the chain holds.
    pub fn domain(self) -> RowDomain {
        match self {
            Self::Volume => RowDomain::Volume,
            Self::Trees(_) => RowDomain::Trees,
            Self::Clients(_) => RowDomain::Clients,
        }
    }

    /// The tree an event on this chain concerns, its `Agentfs-Tree`: none for
    /// the volume's chain.
    pub(in crate::tree_actor) fn tree(self) -> Option<TreeId> {
        match self {
            Self::Volume => None,
            Self::Trees(fork) => Some(fork.into()),
            Self::Clients(tree) => Some(tree),
        }
    }
}

/// One row a step writes: the chain's new `row.json` and the event that moved it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RowWrite {
    pub chain: Chain,
    pub row: String,
    pub event: RowEvent,
}

/// What an event commit's message says beyond the shared trailers: its kind
/// (`ForkCreated`), the subject's detail, who caused it (`Agentfs-By`) and
/// where that principal ran.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RowEvent {
    pub kind: &'static str,
    pub detail: String,
    pub by: String,
    pub at: PodContext,
}

impl RowWrite {
    /// The record the step's journal entry holds.
    pub(in crate::tree_actor) fn record(self) -> RowRecord {
        RowRecord {
            chain: refs::row_chain(self.chain),
            row: self.row,
            kind: self.event.kind.to_owned(),
            detail: self.event.detail,
            by: self.event.by,
            at: self.event.at,
        }
    }
}

/// Each chain's current row, as the journal's records leave them: the one
/// apply the call and the replay share.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(in crate::tree_actor) struct SavedRows(BTreeMap<Chain, String>);

impl SavedRows {
    /// The rows of one step, in order: each chain's last is its row.
    ///
    /// # Errors
    ///
    /// [`super::Error::Format`] for a record naming no row chain.
    pub fn apply(&mut self, rows: &[RowRecord]) -> Result<()> {
        for row in rows {
            let chain = refs::chain_of_row(&row.chain).ok_or(super::Error::Format)?;
            self.0.insert(chain, row.row.clone());
        }
        Ok(())
    }

    /// One chain's row as built: what a start reads from each built chain's tip.
    pub fn insert(&mut self, chain: Chain, row: String) {
        self.0.insert(chain, row);
    }

    pub fn get(&self, chain: Chain) -> Option<String> {
        self.0.get(&chain).cloned()
    }

    /// Every chain of `domain` with its row, in chain order.
    pub fn of(&self, domain: RowDomain) -> Vec<(Chain, String)> {
        self.0
            .iter()
            .filter(|(chain, _)| chain.domain() == domain)
            .map(|(chain, row)| (*chain, row.clone()))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(chain: Chain, row: &str) -> RowRecord {
        RowWrite {
            chain,
            row: row.to_owned(),
            event: RowEvent {
                kind: "ForkCreated",
                detail: String::new(),
                by: "agentfs".to_owned(),
                at: PodContext::default(),
            },
        }
        .record()
    }

    #[test]
    fn a_chains_last_row_in_a_step_is_its_row() {
        let mut rows = SavedRows::default();
        let fork = Chain::Trees(ForkId(3));
        rows.apply(&[
            record(fork, "one"),
            record(Chain::Volume, "v"),
            record(fork, "two"),
        ])
        .unwrap();
        assert_eq!(rows.get(fork).as_deref(), Some("two"));
        assert_eq!(rows.of(RowDomain::Trees), [(fork, "two".to_owned())]);
        assert_eq!(
            rows.of(RowDomain::Volume),
            [(Chain::Volume, "v".to_owned())]
        );
        assert!(rows.of(RowDomain::Clients).is_empty());
    }

    #[test]
    fn a_record_naming_no_chain_is_damaged() {
        let mut bad = record(Chain::Volume, "v");
        bad.chain = "refs/heads/root".to_owned();
        assert!(matches!(
            SavedRows::default().apply(&[bad]),
            Err(super::super::Error::Format)
        ));
    }
}
