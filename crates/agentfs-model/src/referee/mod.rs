//! The referee: every line and name rule of one pair of trees, a fork and its
//! upstream (`docs/target-architecture/future/module-cut.md`). It reads git only
//! through its port ([`Lineage`]), writes nothing, and gives back a plan: land
//! ([`land`]) the files a change writes into its upstream and the records of what
//! lost; sync ([`sync`]) the files a sync writes into the fork. The world builds
//! the trees, the commits and the decisions.
//!
//! Private units. A landing runs them in a fixed order over one take's per-line
//! verdicts ([`Lines`]): undecided, verdicts, names (to a fixed point), placing
//! (its joins), whole. A sync runs undecided, then placing, then names. The
//! ancestry questions are asked through the port as the units need them.

use std::collections::{BTreeMap, BTreeSet};

use crate::content::{Content, Edit, Line};
use crate::world::{By, Direction};

mod ancestry;
mod names;
mod placing;
mod undecided;
mod verdicts;
mod whole;

pub(crate) use ancestry::Lineage;
use undecided::Own;

/// The referee over one port: each unit is a method of it.
struct Referee<'a, L: Lineage + ?Sized> {
    git: &'a L,
}

/// One take's per-line verdicts (land): the change's versions by file, a verdict
/// per (file, line identity), and the files that lost by a file rule.
struct Lines<'i> {
    dir: Direction,
    by_path: BTreeMap<&'i str, Vec<&'i Incoming>>,
    verdicts: BTreeMap<(String, String), Verdict>,
    by_file: BTreeSet<String>,
}

/// What a landing plans: the files it writes over the upstream's head, by
/// identity, and what of the change lost.
pub(crate) struct Plan {
    pub(crate) files: BTreeMap<String, Content>,
    pub(crate) collisions: Vec<Lost>,
}

fn key(i: &Incoming) -> (String, String) {
    (i.path.clone(), i.version.line.clone())
}

/// Rules 3, 10 and 11 at the upstream's queue: one change (`incoming`, from
/// commit `tip`) into the upstream's head `ours`.
pub(crate) fn land<L: Lineage + ?Sized>(
    git: &L,
    dir: Direction,
    ours: &str,
    incoming: &[Incoming],
    tip: &str,
) -> Plan {
    let r = Referee { git };
    let mut lines = Lines::undecided(dir, incoming);
    r.verdicts(ours, tip, &mut lines);
    r.names(ours, incoming, &mut lines);
    r.joins(ours, tip, &mut lines);
    r.whole(ours, tip, &lines)
}

/// [`crate::world::MergeRule::PerLine`]'s sync of the upstream's `up` into the
/// fork at `ours`: the files it writes over `up`, by identity. `changed`: the
/// files `up` changes from `ours`.
pub(crate) fn sync<L: Lineage + ?Sized>(
    git: &L,
    ours: &str,
    up: &str,
    changed: BTreeSet<String>,
) -> BTreeMap<String, Content> {
    let r = Referee { git };
    let own: Vec<Own> = r.undecided(ours, up, changed);
    let mut files = placing::kept(own);
    r.keep_own_names(ours, up, &mut files);
    files
}

/// One file's incoming versions of one change lost in a landing, for one
/// cause: their line identities, the versions, and the tree's versions
/// that won, one for each.
#[derive(Clone, Debug)]
pub(crate) struct Lost {
    pub(crate) path: String,
    pub(crate) lines: Vec<String>,
    pub(crate) lost: Vec<String>,
    pub(crate) won: Vec<String>,
    pub(crate) by: By,
    /// The change that lost.
    pub(crate) loser: String,
}

/// One line version a landing carries.
#[derive(Clone, Debug)]
pub(crate) struct Incoming {
    pub(crate) path: String,
    pub(crate) version: Line,
    /// The commit that wrote it (`git blame`).
    pub(crate) origin: String,
}

impl Incoming {
    pub(crate) fn edit(&self) -> Edit {
        Edit {
            path: self.path.clone(),
            line: self.version.line.clone(),
            token: self.version.id.clone(),
            gone: !self.version.live(),
        }
    }
}

/// The referee's verdict on one incoming version (rule 3).
#[derive(Clone, Debug, PartialEq, Eq)]
enum Verdict {
    Apply,
    /// The same version, or a stale one.
    Drop,
    /// The upstream's version (or the line that decided a whole loss) wins.
    Lose(String),
}
