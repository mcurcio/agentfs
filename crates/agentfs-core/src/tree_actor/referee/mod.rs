//! The referee: every line and name rule of one pair of trees, a fork and its upstream
//! (docs/target-architecture/git-model.md §7, §8).
//! Pure: it reads history only through [`Landed`], writes nothing, and gives back plans: `land` a
//! [`Decision`], `sync` a fork's snapshot. The actor owns queue order and durability.

mod ancestry;
mod names;
mod placing;
mod undecided;
mod verdicts;
mod whole;

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use super::queue;
use super::tree::{EventNumber, LineVersion, Lines, MergeFileIdentity, NameHistory};
use super::{Error, Result};
pub(super) use ancestry::Broken;
pub(super) use undecided::{Landed, Undecided, ended_files, undecided};

/// A tree's files and name histories, each file's lines as stored (runs) or,
/// for the rules, line by line ([`ByLine`]).
#[derive(Clone, Debug, Default)]
pub(super) struct Snapshot<L = Lines> {
    pub files: BTreeMap<MergeFileIdentity, File<L>>,
    pub names: BTreeMap<MergeFileIdentity, NameHistory>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct File<L = Lines> {
    pub object: u64,
    pub lines: L,
}

/// A file's lines one by one, every version with its ancestor: what the rules read.
pub(super) type Expanded = crate::tree_reconcile::Expanded<LineVersion>;

/// A snapshot line by line.
pub(super) type ByLine = Snapshot<Expanded>;

impl Snapshot {
    /// The snapshot line by line: a take's cost, its files' line counts.
    pub(super) fn by_line(&self) -> ByLine {
        Snapshot {
            files: self
                .files
                .iter()
                .map(|(id, f)| {
                    (
                        *id,
                        File {
                            object: f.object,
                            lines: f.lines.expand(),
                        },
                    )
                })
                .collect(),
            names: self.names.clone(),
        }
    }
}

impl ByLine {
    /// The snapshot as stored.
    pub(super) fn stored(self) -> Result<Snapshot> {
        Ok(Snapshot {
            files: self
                .files
                .into_iter()
                .map(|(id, f)| {
                    Ok((
                        id,
                        File {
                            object: f.object,
                            lines: Lines::from_expanded(&f.lines)?,
                        },
                    ))
                })
                .collect::<Result<_>>()?,
            names: self.names,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub(super) enum Version {
    Name {
        file: MergeFileIdentity,
        version: EventNumber,
    },
    Line {
        file: MergeFileIdentity,
        version: LineVersion,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(super) enum CollisionKind {
    Name,
    Line,
    Whole,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct Collision {
    pub lost: Version,
    pub winner: Version,
    pub kind: CollisionKind,
}

#[derive(Clone, Debug)]
pub(super) struct Decision<L = Lines> {
    pub snapshot: Snapshot<L>,
    pub collisions: Vec<Collision>,
}

/// Rule 3's answer for one incoming version against the upstream's.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Verdict<V> {
    /// The version applies (agreeing deletes included).
    Apply,
    /// The upstream holds this version or one written over it.
    Drop,
    /// Concurrent versions: the upstream's wins.
    Lose { winner: V },
}

/// Earlier decisions in the upstream's history: each lost version and the version it lost to.
pub(super) type PriorDecisions = BTreeMap<Version, Version>;

/// Rule 3 at the upstream's queue: one change (`incoming`, made on `base`) into the upstream's
/// head. Verdicts, names (R11.2), placing (R3e.1, R10.4, R10.8), whole (R3e). A transaction's
/// commit and a landing alike.
pub(super) fn land(
    upstream: &Snapshot,
    base: &Snapshot,
    incoming: &Snapshot,
    prior: &PriorDecisions,
) -> Result<Decision> {
    let decision = land_by_line(
        &upstream.by_line(),
        &base.by_line(),
        &incoming.by_line(),
        prior,
    )?;
    Ok(Decision {
        snapshot: decision.snapshot.stored()?,
        collisions: decision.collisions,
    })
}

fn land_by_line(
    upstream: &ByLine,
    base: &ByLine,
    incoming: &ByLine,
    prior: &PriorDecisions,
) -> Result<Decision<Expanded>> {
    let verdicts::Verdicts {
        mut candidates,
        applied_names,
        files,
    } = verdicts::verdicts(upstream, base, incoming, prior)?;
    names::displace(upstream, base, incoming, &applied_names, &mut candidates)?;
    let mut applied = upstream.clone();
    for (id, name) in applied_names {
        if name.is_ended()? {
            applied.files.remove(&id);
        } else if let std::collections::btree_map::Entry::Vacant(entry) = applied.files.entry(id) {
            // A live name for an identity the upstream lacks makes an empty file. Only lines of
            // this change apply; lines of an earlier change that lost stay absent.
            let object = incoming.files.get(&id).ok_or(Error::Format)?.object;
            entry.insert(File {
                object,
                lines: Expanded::default(),
            });
        }
        applied.names.insert(id, name);
    }
    let empty = Expanded::default();
    for (id, verdicts) in files {
        let file = incoming.files.get(&id).ok_or(Error::Format)?;
        let theirs = upstream.files.get(&id);
        let placed = placing::place(theirs.map_or(&empty, |f| &f.lines), &file.lines, &verdicts)?;
        for (lost, winner) in placed.joins {
            let lost = Version::Line {
                file: id,
                version: lost,
            };
            if let Some(candidate) = candidates.iter_mut().find(|c| c.lost == lost) {
                candidate.loss = Some((
                    Version::Line {
                        file: id,
                        version: winner,
                    },
                    CollisionKind::Line,
                ));
            }
        }
        applied.files.insert(
            id,
            File {
                object: theirs.map_or(file.object, |f| f.object),
                lines: placed.lines,
            },
        );
    }
    Ok(whole::whole(upstream, applied, candidates))
}

/// A sync never decides (R3a): the upstream's snapshot with the fork's undecided versions kept
/// (R3d). Placing over every file either side holds, then names; a live name with no placed file
/// is [`Error::Format`]. `last_lines` holds the upstream's last lines of each file it ended that
/// the fork keeps under an undecided name: until the upstream decides, that file reads as any
/// other (R3a, R3d, finding 44).
pub(super) fn sync(
    upstream: &Snapshot,
    fork: &Snapshot,
    undecided: &Undecided,
    last_lines: &BTreeMap<MergeFileIdentity, Lines>,
) -> Result<Snapshot> {
    let (upstream, fork) = (upstream.by_line(), fork.by_line());
    let last_lines = last_lines
        .iter()
        .map(|(id, lines)| (*id, lines.expand()))
        .collect();
    let mut placed = placing::sync_files(&upstream, &fork, &undecided.lines, &last_lines)?;
    let names = names::sync_names(&upstream, &fork, &undecided.names)?;
    let mut files = BTreeMap::new();
    for (id, name) in &names {
        if !name.is_ended()? {
            files.insert(*id, placed.remove(id).ok_or(Error::Format)?);
        }
    }
    Snapshot { files, names }.stored()
}

/// A step's lock keys (R9, R9.1): every file whose name head or lines differ, and each path it
/// had or has.
pub(super) fn touches(before: &Snapshot, after: &Snapshot) -> BTreeSet<queue::Key> {
    let ids: BTreeSet<_> = before
        .names
        .keys()
        .chain(after.names.keys())
        .chain(before.files.keys())
        .chain(after.files.keys())
        .copied()
        .collect();
    let mut keys = BTreeSet::new();
    for id in ids {
        let old = before.names.get(&id);
        let new = after.names.get(&id);
        if old.map(|h| h.head) == new.map(|h| h.head)
            && before.files.get(&id).map(|file| &file.lines)
                == after.files.get(&id).map(|file| &file.lines)
        {
            continue;
        }
        keys.insert(queue::Key::File(id));
        for history in [old, new].into_iter().flatten() {
            // A history with no head version names no path.
            if let Ok(Some(path)) = history.path() {
                keys.insert(queue::Key::Name(path.to_owned()));
            }
        }
    }
    keys
}

/// The rules' tests' bytes: a map holds none, so each test notes every new
/// line's bytes by its version (a version is written once) and renders a map
/// from them.
#[cfg(test)]
pub(super) mod test_bytes {
    use std::cell::RefCell;
    use std::collections::HashMap;

    use super::Expanded;
    use crate::tree_actor::tree::{EventNumber, LineVersion, Lines, Versions};
    use crate::tree_reconcile::Line;

    thread_local! {
        static NOTED: RefCell<HashMap<LineVersion, Vec<u8>>> = RefCell::new(HashMap::new());
    }

    fn note_live(lines: &Expanded, image: &[u8]) {
        let parts = image.split_inclusive(|b| *b == b'\n');
        NOTED.with(|noted| {
            let mut noted = noted.borrow_mut();
            for (line, part) in lines.lines.iter().filter(|l| l.live).zip(parts) {
                noted.entry(line.version).or_insert_with(|| part.to_vec());
            }
        });
    }

    /// The bytes of `lines`' live lines.
    pub(in crate::tree_actor) fn of(lines: &Expanded) -> Vec<u8> {
        NOTED.with(|noted| {
            let noted = noted.borrow();
            lines
                .lines
                .iter()
                .filter(|l| l.live)
                .flat_map(|l| noted[&l.version].clone())
                .collect()
        })
    }

    /// The bytes of a stored map's live lines.
    pub(in crate::tree_actor) fn of_map(lines: &Lines) -> Vec<u8> {
        of(&lines.expand())
    }

    /// `bytes` saved over `base` with `event`'s new versions.
    pub(in crate::tree_actor) fn save(base: &Lines, bytes: &[u8], event: u64) -> Lines {
        let old = of_map(base);
        let mut versions = Versions::of(EventNumber::new(event));
        let map = base.align_bytes(&old, bytes, |n| versions.take(n)).unwrap();
        note_live(&map.expand(), bytes);
        map
    }

    /// [`save`], line by line.
    pub(in crate::tree_actor) fn save_lines(base: &Expanded, bytes: &[u8], event: u64) -> Expanded {
        save(&Lines::from_expanded(base).unwrap(), bytes, event).expand()
    }

    /// `line` now of these bytes.
    pub(in crate::tree_actor) fn set(line: &mut Line<LineVersion>, bytes: &[u8]) {
        *line = self::line(line.identity, line.version, bytes);
    }

    /// A live line of these bytes.
    pub(in crate::tree_actor) fn line(
        identity: LineVersion,
        version: LineVersion,
        bytes: &[u8],
    ) -> Line<LineVersion> {
        NOTED.with(|noted| noted.borrow_mut().insert(version, bytes.to_vec()));
        Line {
            identity,
            version,
            live: true,
            open: (!bytes.ends_with(b"\n")).then_some(bytes.len() as u64),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The landing's rules, as the referee's one entry runs them.
    fn judge(
        upstream: &Snapshot,
        base: &Snapshot,
        incoming: &Snapshot,
        prior: &PriorDecisions,
    ) -> Result<Decision> {
        land(upstream, base, incoming, prior)
    }

    fn path(history: &NameHistory) -> Result<Option<&str>> {
        history.path()
    }

    fn file(snapshot: &mut Snapshot, id: u64, path: &str, bytes: &[u8]) {
        let key = MergeFileIdentity(id);
        let lines = test_bytes::save(&Lines::default(), bytes, id);
        snapshot
            .names
            .insert(key, NameHistory::created(EventNumber::new(id), path.into()));
        snapshot.files.insert(key, File { object: id, lines });
    }
    fn edit(snapshot: &mut Snapshot, id: u64, bytes: &[u8], seq: u64) {
        let file = snapshot.files.get_mut(&MergeFileIdentity(id)).unwrap();
        file.lines = test_bytes::save(&file.lines, bytes, seq);
    }
    fn rename(snapshot: &mut Snapshot, id: u64, path: &str, seq: u64) {
        snapshot
            .names
            .get_mut(&MergeFileIdentity(id))
            .unwrap()
            .advance(EventNumber::new(seq), Some(path.into()), None);
    }
    fn unlink(snapshot: &mut Snapshot, id: u64, seq: u64, replaced_by: Option<u64>) {
        snapshot
            .names
            .get_mut(&MergeFileIdentity(id))
            .unwrap()
            .advance(
                EventNumber::new(seq),
                None,
                replaced_by.map(MergeFileIdentity),
            );
        snapshot.files.remove(&MergeFileIdentity(id));
    }
    fn base() -> Snapshot {
        let mut snapshot = Snapshot::default();
        file(&mut snapshot, 1, "a", b"one\ntwo\n");
        file(&mut snapshot, 2, "b", b"other\n");
        snapshot
    }

    #[test]
    fn rename_and_concurrent_edit_follow_the_identity() {
        let base = base();
        let mut parent = base.clone();
        let mut child = base.clone();
        edit(&mut parent, 1, b"ONE\ntwo\n", 10);
        rename(&mut child, 1, "renamed", 11);
        let decision = judge(&parent, &base, &child, &BTreeMap::new()).unwrap();
        assert!(decision.collisions.is_empty());
        assert_eq!(
            path(&decision.snapshot.names[&MergeFileIdentity(1)]).unwrap(),
            Some("renamed")
        );
        assert_eq!(
            test_bytes::of_map(&decision.snapshot.files[&MergeFileIdentity(1)].lines),
            b"ONE\ntwo\n"
        );
    }

    #[test]
    fn competing_names_keep_parent_and_reject_the_whole_save() {
        let base = base();
        let mut parent = base.clone();
        let mut child = base.clone();
        rename(&mut parent, 1, "shared", 10);
        rename(&mut child, 2, "shared", 11);
        let decision = judge(&parent, &base, &child, &BTreeMap::new()).unwrap();
        assert!(!decision.collisions.is_empty());
        assert_eq!(
            path(&decision.snapshot.names[&MergeFileIdentity(1)]).unwrap(),
            Some("shared")
        );
        assert_eq!(
            path(&decision.snapshot.names[&MergeFileIdentity(2)]).unwrap(),
            Some("b")
        );
    }

    #[test]
    fn unlink_loses_against_an_unseen_parent_edit() {
        let base = base();
        let mut parent = base.clone();
        let mut child = base.clone();
        edit(&mut parent, 1, b"one\nPARENT\n", 10);
        unlink(&mut child, 1, 11, None);
        let decision = judge(&parent, &base, &child, &BTreeMap::new()).unwrap();
        assert!(!decision.collisions.is_empty());
        assert_eq!(
            test_bytes::of_map(&decision.snapshot.files[&MergeFileIdentity(1)].lines),
            b"one\nPARENT\n"
        );
    }

    #[test]
    fn unlink_can_apply_when_parent_only_removed_an_unseen_line() {
        let base = base();
        let mut parent = base.clone();
        let mut child = base.clone();
        edit(&mut parent, 1, b"one\n", 10);
        unlink(&mut child, 1, 11, None);
        let decision = judge(&parent, &base, &child, &BTreeMap::new()).unwrap();
        assert!(decision.collisions.is_empty());
        assert!(!decision.snapshot.files.contains_key(&MergeFileIdentity(1)));
    }

    #[test]
    fn parent_unlink_beats_an_incoming_edit() {
        let base = base();
        let mut parent = base.clone();
        let mut child = base.clone();
        unlink(&mut parent, 1, 10, None);
        edit(&mut child, 1, b"one\nCHILD\n", 11);
        let decision = judge(&parent, &base, &child, &BTreeMap::new()).unwrap();
        assert!(!decision.collisions.is_empty());
        assert!(!decision.snapshot.files.contains_key(&MergeFileIdentity(1)));
    }

    #[test]
    fn an_edit_does_not_create_an_identity_whose_create_lost() {
        let mut base = base();
        file(&mut base, 3, "contested", b"first\n");
        let mut parent = base.clone();
        parent.files.remove(&MergeFileIdentity(3));
        parent.names.remove(&MergeFileIdentity(3));
        let mut child = base.clone();
        edit(&mut child, 3, b"later\n", 11);
        let lost = Version::Name {
            file: MergeFileIdentity(3),
            version: EventNumber::new(3),
        };
        let winner = Version::Name {
            file: MergeFileIdentity(2),
            version: EventNumber::new(2),
        };
        let prior = [(lost, winner.clone())].into_iter().collect();
        let decision = judge(&parent, &base, &child, &prior).unwrap();
        assert!(!decision.snapshot.files.contains_key(&MergeFileIdentity(3)));
        assert!(decision.collisions.iter().any(|c| c.winner == winner));
    }

    #[test]
    fn later_rename_of_rejected_create_has_no_rejected_content() {
        let mut base = base();
        file(&mut base, 3, "contested", b"rejected content\n");
        let mut parent = base.clone();
        parent.files.remove(&MergeFileIdentity(3));
        parent.names.remove(&MergeFileIdentity(3));
        rename(&mut parent, 2, "contested", 10);
        let mut child = base.clone();
        rename(&mut child, 3, "available", 11);
        let lost = Version::Name {
            file: MergeFileIdentity(3),
            version: EventNumber::new(3),
        };
        let winner = Version::Name {
            file: MergeFileIdentity(2),
            version: EventNumber::new(10),
        };
        let prior = [(lost, winner)].into_iter().collect();
        let decision = judge(&parent, &base, &child, &prior).unwrap();
        assert!(decision.collisions.is_empty());
        let renamed = MergeFileIdentity(3);
        assert_eq!(
            path(&decision.snapshot.names[&renamed]).unwrap(),
            Some("available")
        );
        assert_eq!(decision.snapshot.files[&renamed].object, 3);
        assert_eq!(
            test_bytes::of_map(&decision.snapshot.files[&renamed].lines),
            b""
        );
        assert_eq!(
            path(&decision.snapshot.names[&MergeFileIdentity(2)]).unwrap(),
            Some("contested")
        );
    }

    #[test]
    fn rename_over_loses_as_one_when_replaced_file_changed() {
        let base = base();
        let mut parent = base.clone();
        let mut child = base.clone();
        edit(&mut parent, 2, b"parent\n", 10);
        unlink(&mut child, 2, 11, Some(1));
        rename(&mut child, 1, "b", 11);
        let decision = judge(&parent, &base, &child, &BTreeMap::new()).unwrap();
        assert_eq!(decision.collisions.len(), 2);
        assert_eq!(
            path(&decision.snapshot.names[&MergeFileIdentity(1)]).unwrap(),
            Some("a")
        );
        assert_eq!(
            test_bytes::of_map(&decision.snapshot.files[&MergeFileIdentity(2)].lines),
            b"parent\n"
        );
        assert!(
            decision
                .collisions
                .iter()
                .any(|c| c.kind == CollisionKind::Whole)
        );
    }

    // R11.2: only a rename-over onto the new name frees the path a live upstream file holds; the
    // same change's plain unlink of that file does not, and the new name loses whole.
    #[test]
    fn only_a_rename_over_onto_the_name_frees_its_path() {
        let base = base();
        let mut plain = base.clone();
        unlink(&mut plain, 2, 11, None);
        rename(&mut plain, 1, "b", 11);
        let decision = judge(&base, &base, &plain, &BTreeMap::new()).unwrap();
        assert!(
            decision
                .collisions
                .iter()
                .any(|c| c.kind == CollisionKind::Name)
        );
        assert_eq!(
            path(&decision.snapshot.names[&MergeFileIdentity(1)]).unwrap(),
            Some("a")
        );
        assert_eq!(
            path(&decision.snapshot.names[&MergeFileIdentity(2)]).unwrap(),
            Some("b")
        );

        let mut over = base.clone();
        unlink(&mut over, 2, 11, Some(1));
        rename(&mut over, 1, "b", 11);
        let decision = judge(&base, &base, &over, &BTreeMap::new()).unwrap();
        assert!(decision.collisions.is_empty());
        assert_eq!(
            path(&decision.snapshot.names[&MergeFileIdentity(1)]).unwrap(),
            Some("b")
        );
        assert!(!decision.snapshot.files.contains_key(&MergeFileIdentity(2)));
    }

    // R10.3, finding 31: an agreeing delete of a file a rename-over named.
    #[test]
    fn rename_over_then_unlink_agrees_with_a_sandbox_unlink() {
        let base = base();
        let mut parent = base.clone();
        let mut child = base.clone();
        unlink(&mut parent, 2, 10, Some(1));
        rename(&mut parent, 1, "b", 10);
        unlink(&mut parent, 1, 11, None);
        unlink(&mut child, 1, 12, None);
        let decision = judge(&parent, &base, &child, &BTreeMap::new()).unwrap();
        assert!(decision.collisions.is_empty());
        let name = &decision.snapshot.names[&MergeFileIdentity(1)];
        assert_eq!(path(name).unwrap(), None);
        assert_eq!(name.head, EventNumber::new(12));
        assert!(!decision.snapshot.files.contains_key(&MergeFileIdentity(1)));
    }

    // R10.8 through the whole-save judge: the later save, written over a save
    // that lost, loses whole to the parent's next line, by a line collision.
    #[test]
    fn unfinished_line_before_a_kept_line_is_a_line_collision() {
        let base = base();
        let mut first = base.clone();
        edit(&mut first, 1, b"y", 11);
        let mut parent = base.clone();
        edit(&mut parent, 1, b"one\nPARENT\n", 10);
        assert!(
            !judge(&parent, &base, &first, &BTreeMap::new())
                .unwrap()
                .collisions
                .is_empty()
        );
        let mut later = first.clone();
        edit(&mut later, 1, b"n6", 12);
        let decision = judge(&parent, &first, &later, &BTreeMap::new()).unwrap();
        assert_eq!(decision.collisions.len(), 1);
        assert_eq!(decision.collisions[0].kind, CollisionKind::Line);
        assert_eq!(
            test_bytes::of_map(&decision.snapshot.files[&MergeFileIdentity(1)].lines),
            b"one\nPARENT\n"
        );
    }

    #[test]
    fn atomic_rewrite_keeps_target_identity_and_merges_clean_lines() {
        let mut base = base();
        file(&mut base, 3, "temp", b"one\nCHILD\n");
        let mut parent = base.clone();
        let mut child = base.clone();
        edit(&mut parent, 1, b"PARENT\ntwo\n", 10);
        unlink(&mut child, 3, 11, None);
        edit(&mut child, 1, b"one\nCHILD\n", 12);
        child.files.get_mut(&MergeFileIdentity(1)).unwrap().object = 3;
        let decision = judge(&parent, &base, &child, &BTreeMap::new()).unwrap();
        assert!(decision.collisions.is_empty());
        assert_eq!(decision.snapshot.files[&MergeFileIdentity(1)].object, 1);
        assert_eq!(
            test_bytes::of_map(&decision.snapshot.files[&MergeFileIdentity(1)].lines),
            b"PARENT\nCHILD\n"
        );
        assert!(!decision.snapshot.files.contains_key(&MergeFileIdentity(3)));
    }
}

#[cfg(test)]
mod sync_tests {
    use super::*;

    /// A snapshot as the sync takes it.
    trait Stored {
        fn stored_snapshot(&self) -> Snapshot;
    }

    impl Stored for Snapshot {
        fn stored_snapshot(&self) -> Snapshot {
            self.clone()
        }
    }

    impl Stored for ByLine {
        fn stored_snapshot(&self) -> Snapshot {
            self.clone().stored().unwrap()
        }
    }

    /// A sync with these undecided versions.
    fn sync(
        upstream: &impl Stored,
        fork: &impl Stored,
        lines: &BTreeSet<LineVersion>,
        names: &BTreeSet<(MergeFileIdentity, EventNumber)>,
        last_lines: &BTreeMap<MergeFileIdentity, Lines>,
    ) -> Result<Snapshot> {
        let undecided = Undecided {
            lines: lines.clone(),
            names: names.clone(),
        };
        super::sync(
            &upstream.stored_snapshot(),
            &fork.stored_snapshot(),
            &undecided,
            last_lines,
        )
    }

    fn path(history: &NameHistory) -> Result<Option<&str>> {
        history.path()
    }

    /// A line version for each name the cases use.
    fn v(name: &str) -> LineVersion {
        let event = [
            "line1", "line2", "line3", "base", "parent", "fork", "insert", "later", "own", "one",
            "two", "three",
        ]
        .iter()
        .position(|known| *known == name)
        .expect("a known name");
        format!("{}:1", event + 1).parse().unwrap()
    }

    fn file(id: u64, path: &str, version: &str, bytes: &[u8]) -> ByLine {
        let identity = MergeFileIdentity(id);
        let line = test_bytes::line(v(&format!("line{id}")), v(version), bytes);
        Snapshot {
            names: [(
                identity,
                NameHistory::created(EventNumber::new(id), path.into()),
            )]
            .into_iter()
            .collect(),
            files: [(
                identity,
                File {
                    object: id,
                    lines: Expanded {
                        lines: vec![line],
                        ancestors: [(v(version), None)].into_iter().collect(),
                    },
                },
            )]
            .into_iter()
            .collect(),
        }
    }

    #[test]
    fn undecided_line_is_kept_then_parent_decision_arrives() {
        let mut parent = file(1, "a", "base", b"old\n");
        let mut fork = parent.clone();
        parent
            .files
            .get_mut(&MergeFileIdentity(1))
            .unwrap()
            .lines
            .lines[0]
            .version = v("parent");
        test_bytes::set(
            &mut parent
                .files
                .get_mut(&MergeFileIdentity(1))
                .unwrap()
                .lines
                .lines[0],
            b"parent\n",
        );
        parent
            .files
            .get_mut(&MergeFileIdentity(1))
            .unwrap()
            .lines
            .ancestors
            .insert(v("parent"), Some(v("base")));
        fork.files
            .get_mut(&MergeFileIdentity(1))
            .unwrap()
            .lines
            .lines[0]
            .version = v("fork");
        test_bytes::set(
            &mut fork
                .files
                .get_mut(&MergeFileIdentity(1))
                .unwrap()
                .lines
                .lines[0],
            b"fork\n",
        );
        fork.files
            .get_mut(&MergeFileIdentity(1))
            .unwrap()
            .lines
            .ancestors
            .insert(v("fork"), Some(v("base")));
        let kept = sync(
            &parent,
            &fork,
            &[v("fork")].into_iter().collect(),
            &BTreeSet::new(),
            &BTreeMap::new(),
        )
        .unwrap();
        assert_eq!(
            test_bytes::of_map(&kept.files[&MergeFileIdentity(1)].lines),
            b"fork\n"
        );
        let decided = sync(
            &parent,
            &kept,
            &BTreeSet::new(),
            &BTreeSet::new(),
            &BTreeMap::new(),
        )
        .unwrap();
        assert_eq!(
            test_bytes::of_map(&decided.files[&MergeFileIdentity(1)].lines),
            b"parent\n"
        );
    }

    #[test]
    fn unfinished_parent_tail_does_not_acquire_a_phantom_newline() {
        let parent = file(1, "a", "parent", b"partial");
        let mut fork = parent.clone();
        fork.files
            .get_mut(&MergeFileIdentity(1))
            .unwrap()
            .lines
            .lines
            .push(test_bytes::line(v("insert"), v("insert"), b"own\n"));
        fork.files
            .get_mut(&MergeFileIdentity(1))
            .unwrap()
            .lines
            .ancestors
            .insert(v("insert"), None);
        let result = sync(
            &parent,
            &fork,
            &[v("insert")].into_iter().collect(),
            &BTreeSet::new(),
            &BTreeMap::new(),
        )
        .unwrap();
        assert_eq!(
            test_bytes::of_map(&result.files[&MergeFileIdentity(1)].lines),
            b"own\n"
        );
        let result = sync(
            &parent,
            &result,
            &BTreeSet::new(),
            &BTreeSet::new(),
            &BTreeMap::new(),
        )
        .unwrap();
        assert_eq!(
            test_bytes::of_map(&result.files[&MergeFileIdentity(1)].lines),
            b"partial"
        );
    }

    // R10.9: the parent's later line waits for the decision on the sandbox's
    // unfinished line, then the parent's version arrives whole.
    #[test]
    fn undecided_unfinished_line_hides_the_parents_later_line_until_decided() {
        let key = MergeFileIdentity(1);
        let mut parent = file(1, "a", "base", b"old\n");
        let mut fork = parent.clone();
        let lines = &mut parent.files.get_mut(&key).unwrap().lines;
        lines
            .lines
            .push(test_bytes::line(v("later"), v("later"), b"later\n"));
        lines.ancestors.insert(v("later"), None);
        let lines = &mut fork.files.get_mut(&key).unwrap().lines;
        lines.lines[0].version = v("own");
        test_bytes::set(&mut lines.lines[0], b"own");
        lines.ancestors.insert(v("own"), Some(v("base")));
        // Without the hide the sync would read `ownlater\n`.
        let kept = sync(
            &parent,
            &fork,
            &[v("own")].into_iter().collect(),
            &BTreeSet::new(),
            &BTreeMap::new(),
        )
        .unwrap();
        assert_eq!(test_bytes::of_map(&kept.files[&key].lines), b"own");
        let decided = sync(
            &parent,
            &kept,
            &BTreeSet::new(),
            &BTreeSet::new(),
            &BTreeMap::new(),
        )
        .unwrap();
        assert_eq!(
            test_bytes::of_map(&decided.files[&key].lines),
            b"old\nlater\n"
        );
    }

    #[test]
    fn names_fall_back_through_a_chain_then_take_parent_decision() {
        let mut fork = file(1, "a", "one", b"one\n");
        for snapshot in [
            file(2, "b", "two", b"two\n"),
            file(3, "c", "three", b"three\n"),
        ] {
            fork.names.extend(snapshot.names);
            fork.files.extend(snapshot.files);
        }
        let mut parent = fork.clone();
        fork.names.get_mut(&MergeFileIdentity(1)).unwrap().advance(
            EventNumber::new(11),
            Some("target".into()),
            None,
        );
        parent
            .names
            .get_mut(&MergeFileIdentity(2))
            .unwrap()
            .advance(EventNumber::new(12), Some("target".into()), None);
        parent
            .names
            .get_mut(&MergeFileIdentity(3))
            .unwrap()
            .advance(EventNumber::new(13), Some("b".into()), None);
        let pending = [(MergeFileIdentity(1), EventNumber::new(11))]
            .into_iter()
            .collect();
        let result = sync(&parent, &fork, &BTreeSet::new(), &pending, &BTreeMap::new()).unwrap();
        assert_eq!(
            path(&result.names[&MergeFileIdentity(1)]).unwrap(),
            Some("target")
        );
        assert_eq!(
            path(&result.names[&MergeFileIdentity(2)]).unwrap(),
            Some("b")
        );
        assert_eq!(
            path(&result.names[&MergeFileIdentity(3)]).unwrap(),
            Some("c")
        );
        let result = sync(
            &parent,
            &result,
            &BTreeSet::new(),
            &BTreeSet::new(),
            &BTreeMap::new(),
        )
        .unwrap();
        assert_eq!(
            path(&result.names[&MergeFileIdentity(1)]).unwrap(),
            Some("a")
        );
        assert_eq!(
            path(&result.names[&MergeFileIdentity(2)]).unwrap(),
            Some("target")
        );
        assert_eq!(
            path(&result.names[&MergeFileIdentity(3)]).unwrap(),
            Some("b")
        );
    }
}
