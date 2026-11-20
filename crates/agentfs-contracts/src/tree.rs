//! A tree's identity in its volume (docs/trees.md): `root`, the tree with no
//! upstream, or `fork/<n>`, made at journal event `n`. History records it
//! (`Agentfs-Tree`), and every internal ref and checkout is named by it; it is
//! never a published branch name.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

/// A tree's identity in its volume. Its text (`root`, `fork/<n>`) is what history
/// records (`Agentfs-Tree`) and what every internal ref and checkout is named by;
/// never a published branch name (docs/trees.md).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub enum TreeId {
    /// The tree with no upstream. Sorts before every fork.
    Root,
    /// A tree with one upstream, made at journal event `n` (never reused). Forks
    /// sort by `n`.
    Fork(u64),
}

/// A fork's number in its volume: the journal event that made it, never reused.
/// `TreeId::Fork(n)` and `ForkId(n)` name the same fork.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct ForkId(pub u64);

impl From<ForkId> for TreeId {
    fn from(fork: ForkId) -> Self {
        Self::Fork(fork.0)
    }
}

/// Text that names no tree.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[error("{0:?} names no tree: a tree is `root` or `fork/<n>`")]
pub struct NotATree(pub String);

impl TreeId {
    /// Whether this is the root, the tree with no upstream.
    pub fn is_root(self) -> bool {
        self == Self::Root
    }

    /// The fork this tree is; `None` for the root.
    pub fn fork(self) -> Option<ForkId> {
        match self {
            Self::Root => None,
            Self::Fork(n) => Some(ForkId(n)),
        }
    }
}

impl fmt::Display for TreeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Root => f.write_str("root"),
            Self::Fork(n) => write!(f, "fork/{n}"),
        }
    }
}

impl FromStr for TreeId {
    type Err = NotATree;

    /// The exact inverse of `Display`: `fork/07` and `fork/+7` are refused.
    fn from_str(text: &str) -> Result<Self, NotATree> {
        if text == "root" {
            return Ok(Self::Root);
        }
        text.strip_prefix("fork/")
            .filter(|n| {
                !n.is_empty()
                    && n.bytes().all(|b| b.is_ascii_digit())
                    && (*n == "0" || !n.starts_with('0'))
            })
            .and_then(|n| n.parse().ok())
            .map(Self::Fork)
            .ok_or_else(|| NotATree(text.to_owned()))
    }
}

impl TryFrom<String> for TreeId {
    type Error = NotATree;

    fn try_from(text: String) -> Result<Self, NotATree> {
        text.parse()
    }
}

impl From<TreeId> for String {
    fn from(tree: TreeId) -> Self {
        tree.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn its_text_round_trips_and_nothing_else_parses() {
        for tree in [
            TreeId::Root,
            TreeId::Fork(0),
            TreeId::Fork(9),
            TreeId::Fork(10),
        ] {
            assert_eq!(tree.to_string().parse::<TreeId>(), Ok(tree));
        }
        assert_eq!(TreeId::Fork(10).to_string(), "fork/10");
        for text in [
            "main",
            "fork/",
            "fork/07",
            "fork/+7",
            "fork/-1",
            "fork/1/2",
            "Root",
            "root/",
            "",
            "fork/18446744073709551616",
        ] {
            assert_eq!(text.parse::<TreeId>(), Err(NotATree(text.to_owned())));
        }
    }

    #[test]
    fn the_root_sorts_first_and_forks_by_number() {
        let mut trees = vec![TreeId::Fork(10), TreeId::Root, TreeId::Fork(9)];
        trees.sort();
        assert_eq!(trees, [TreeId::Root, TreeId::Fork(9), TreeId::Fork(10)]);
        assert!(TreeId::Root.is_root() && !TreeId::Fork(1).is_root());
    }
}
