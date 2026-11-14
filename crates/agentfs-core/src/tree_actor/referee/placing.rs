//! Where versions go in a file's lines: a landing's applying lines placed over the upstream's
//! (R3e.1) with its joins (R10.4, R10.8), and a sync's undecided lines kept over the upstream's
//! (R3d, R10.6, R10.9) (docs/target-architecture/git-model.md §7, §8).

use std::collections::{BTreeMap, BTreeSet};

use super::{ByLine as Snapshot, Expanded as Lines, Verdict, ancestry};
use crate::tree_actor::tree::{LineVersion, MergeFileIdentity};
use crate::tree_actor::{Error, Result};
use crate::tree_reconcile::Line;

/// One file's lines placed, and the joins that lose: each as the losing version and the
/// upstream's version it loses to.
pub(super) struct Placed {
    pub joins: Vec<(LineVersion, LineVersion)>,
    pub lines: Lines,
}

fn unfinished(line: &Line<LineVersion>) -> bool {
    line.live && line.open.is_some()
}

fn unfinished_tail(lines: &Lines) -> Option<&Line<LineVersion>> {
    lines
        .lines
        .iter()
        .rev()
        .find(|line| line.live)
        .filter(|line| unfinished(line))
}

// R10.8: the first selected unfinished line with a live line after it, as its version and the
// next live line's version.
fn unfinished_before_kept(
    placed: &[Line<LineVersion>],
    selected: &BTreeSet<LineVersion>,
) -> Option<(LineVersion, LineVersion)> {
    let mut live = placed.iter().filter(|line| line.live).peekable();
    while let Some(line) = live.next() {
        if unfinished(line)
            && selected.contains(&line.identity)
            && let Some(next) = live.peek()
        {
            return Some((line.version, next.version));
        }
    }
    None
}

// Replace existing identities in place; insert new identities after their nearest predecessor
// in the source, past upstream-only inserts at that point.
fn lay(
    upstream: &[Line<LineVersion>],
    source: &[Line<LineVersion>],
    selected: &BTreeSet<LineVersion>,
) -> Vec<Line<LineVersion>> {
    let mut result = upstream.to_vec();
    let source_ids: BTreeSet<&LineVersion> = source.iter().map(|l| &l.identity).collect();
    for (index, line) in source.iter().enumerate() {
        if !selected.contains(&line.identity) {
            continue;
        }
        if let Some(pos) = result.iter().position(|p| p.identity == line.identity) {
            result[pos] = line.clone();
            continue;
        }
        let anchor = source[..index]
            .iter()
            .rev()
            .find_map(|p| result.iter().position(|r| r.identity == p.identity));
        let mut pos = anchor.map_or(0, |p| p + 1);
        while pos < result.len() && !source_ids.contains(&result[pos].identity) {
            pos += 1;
        }
        result.insert(pos, line.clone());
    }
    result
}

/// One file's applying lines placed over the upstream's (R3e.1), and its joins: an applying line
/// after the upstream's unfinished last line (R10.4), then an unfinished applying line with a kept
/// line after it (R10.8). `verdicts` holds each changed line's verdict by its identity.
pub(super) fn place(
    upstream: &Lines,
    incoming: &Lines,
    verdicts: &BTreeMap<LineVersion, Verdict<LineVersion>>,
) -> Result<Placed> {
    let ancestors = ancestry::union(&upstream.ancestors, &incoming.ancestors)?;
    let selected: BTreeSet<LineVersion> = verdicts
        .iter()
        .filter_map(|(id, verdict)| matches!(verdict, Verdict::Apply).then_some(*id))
        .collect();
    let placed = lay(&upstream.lines, &incoming.lines, &selected);
    let mut joins = Vec::new();
    let mut applying = selected.clone();
    // R10.4: an incoming line placed after the upstream's unfinished last line loses to that
    // line. An edited existing line stays in its existing place.
    if let Some(partial) = unfinished_tail(upstream)
        && let Some(pos) = placed
            .iter()
            .position(|line| line.version == partial.version)
    {
        for line in placed[pos + 1..]
            .iter()
            .filter(|line| line.live && selected.contains(&line.identity))
        {
            joins.push((line.version, partial.version));
            applying.remove(&line.identity);
        }
    }
    // R10.8: judged on what would still apply after R10.4.
    let kept = lay(&upstream.lines, &incoming.lines, &applying);
    if let Some(join) = unfinished_before_kept(&kept, &applying) {
        joins.push(join);
    }
    Ok(Placed {
        joins,
        lines: Lines {
            lines: placed,
            ancestors,
        },
    })
}

/// One file of a sync (R3d): the upstream's lines with the fork's undecided ones placed, never
/// bytes nobody wrote (R10.6, R10.9).
pub(super) fn sync_file(
    upstream: &Lines,
    fork: &Lines,
    undecided: &BTreeSet<LineVersion>,
) -> Result<Lines> {
    let ancestors = ancestry::union(&upstream.ancestors, &fork.ancestors)?;
    let selected: BTreeSet<LineVersion> = fork
        .lines
        .iter()
        .filter(|line| undecided.contains(&line.version))
        .map(|line| line.identity)
        .collect();
    let mut lines = lay(&upstream.lines, &fork.lines, &selected);
    // R10.6: keep the undecided fork line visible without inventing a newline after an unseen
    // unfinished upstream tail.
    if let Some(partial) = unfinished_tail(upstream)
        && let Some(pos) = lines
            .iter()
            .position(|line| line.version == partial.version)
        && lines[pos + 1..]
            .iter()
            .any(|line| line.live && selected.contains(&line.identity))
    {
        lines.remove(pos);
    }
    // R10.9, the mirror: keep an undecided unfinished line visible without joining the
    // upstream's later lines to it. They arrive with the decision.
    if let Some(pos) = lines
        .iter()
        .position(|line| selected.contains(&line.identity) && unfinished(line))
    {
        let later = lines.split_off(pos + 1);
        lines.extend(
            later
                .into_iter()
                .filter(|line| !line.live || selected.contains(&line.identity)),
        );
    }
    Ok(Lines { lines, ancestors })
}

/// Every file either side holds, synced. A file the fork ended comes back as a new object
/// numbered by the event that ended it (R11.3, `EventNumber::object`); one only the fork holds is
/// synced over its `last_lines`.
pub(super) fn sync_files(
    upstream: &Snapshot,
    fork: &Snapshot,
    undecided: &BTreeSet<LineVersion>,
    last_lines: &BTreeMap<MergeFileIdentity, Lines>,
) -> Result<BTreeMap<MergeFileIdentity, super::File<Lines>>> {
    let empty = Lines::default();
    let ids: BTreeSet<_> = upstream
        .files
        .keys()
        .chain(fork.files.keys())
        .copied()
        .collect();
    let mut files = BTreeMap::new();
    for id in ids {
        let file = match (upstream.files.get(&id), fork.files.get(&id)) {
            (Some(theirs), Some(own)) => super::File {
                object: own.object,
                lines: sync_file(&theirs.lines, &own.lines, undecided)?,
            },
            // A file this tree ended by an unlink or a rename-over that lost comes back as a new
            // object, numbered by the event that ended it, which numbers nothing else. Its old
            // object stays with whoever holds it: an old writer reaches no file (R11.3), and after
            // an atomic save that object is now the replaced file's (finding 39).
            (Some(theirs), None) => super::File {
                object: fork
                    .names
                    .get(&id)
                    .map_or(theirs.object, |ended| ended.head.object()),
                lines: sync_file(&theirs.lines, &empty, undecided)?,
            },
            (None, Some(own)) => super::File {
                object: own.object,
                lines: sync_file(last_lines.get(&id).unwrap_or(&empty), &own.lines, undecided)?,
            },
            (None, None) => return Err(Error::Format),
        };
        files.insert(id, file);
    }
    Ok(files)
}

#[cfg(test)]
mod tests {
    use super::super::{CollisionKind, File, Version, land, test_bytes};
    use super::*;
    use crate::tree_actor::tree::{EventNumber, NameHistory};

    const FILE: MergeFileIdentity = MergeFileIdentity(100);

    fn save(base: &Lines, bytes: &[u8], event: u64) -> Lines {
        test_bytes::save_lines(base, bytes, event)
    }

    fn snapshot(lines: &Lines) -> super::super::Snapshot {
        super::super::Snapshot {
            files: [(
                FILE,
                File {
                    object: 100,
                    lines: super::super::Lines::from_expanded(lines).unwrap(),
                },
            )]
            .into_iter()
            .collect(),
            names: [(
                FILE,
                NameHistory::created(EventNumber::new(100), "f".into()),
            )]
            .into_iter()
            .collect(),
        }
    }

    /// One file's save landed whole: the file's lines after, and each collision as the lost
    /// version, the winner and the kind.
    fn land_save(
        parent: &Lines,
        base: &Lines,
        incoming: &Lines,
    ) -> (Lines, Vec<(LineVersion, LineVersion, CollisionKind)>) {
        let decision = land(
            &snapshot(parent),
            &snapshot(base),
            &snapshot(incoming),
            &BTreeMap::new(),
        )
        .unwrap();
        let line = |version: &Version| match version {
            Version::Line { file, version } if *file == FILE => *version,
            other => panic!("not a line of the file: {other:?}"),
        };
        let collisions = decision
            .collisions
            .iter()
            .map(|c| (line(&c.lost), line(&c.winner), c.kind.clone()))
            .collect();
        (decision.snapshot.files[&FILE].lines.expand(), collisions)
    }

    #[test]
    fn one_collision_loses_the_entire_save_and_names_clean_edits() {
        let base = save(&Lines::default(), b"a\nb\n", 1);
        let parent = save(&base, b"parent\nb\n", 2);
        let child = save(&base, b"child\nclean\n", 3);
        let (content, collisions) = land_save(&parent, &base, &child);
        assert_eq!(content, parent);
        assert_eq!(collisions.len(), 2);
        assert_eq!(collisions[0].2, CollisionKind::Line);
        assert_eq!(collisions[1].2, CollisionKind::Whole);
        assert_eq!(collisions[1].0, child.lines[1].version);
    }

    #[test]
    fn inserts_keep_parent_first_and_edits_keep_their_place() {
        let base = save(&Lines::default(), b"a\nz\n", 1);
        let parent = save(&base, b"a\nparent\nz\n", 2);
        let child = save(&base, b"a\nchild\nz\n", 3);
        let (content, collisions) = land_save(&parent, &base, &child);
        assert!(collisions.is_empty());
        assert_eq!(test_bytes::of(&content), b"a\nparent\nchild\nz\n");
        let base = save(&Lines::default(), b"line\n", 7);
        let parent = save(&base, b"line\nappend\n", 8);
        let child = save(&base, b"edited\n", 9);
        let (content, collisions) = land_save(&parent, &base, &child);
        assert!(collisions.is_empty());
        assert_eq!(test_bytes::of(&content), b"edited\nappend\n");
    }

    #[test]
    fn unfinished_parent_tail_collides_and_sync_keeps_undecided_child() {
        let base = save(&Lines::default(), b"base\n", 1);
        let parent = save(&base, b"base\nunfinished", 2);
        let child = save(&base, b"base\nchild\n", 3);
        let (content, collisions) = land_save(&parent, &base, &child);
        assert_eq!(content, parent);
        assert_eq!(collisions.len(), 1);
        assert_eq!(collisions[0].1, parent.lines[1].version);
        let undecided = BTreeSet::from([child.lines[1].version]);
        let before = sync_file(&parent, &child, &undecided).unwrap();
        assert_eq!(test_bytes::of(&before), b"base\nchild\n");
        let after = sync_file(&parent, &before, &BTreeSet::new()).unwrap();
        assert_eq!(test_bytes::of(&after), b"base\nunfinished");
    }

    #[test]
    fn agreeing_delete_is_carried_and_clean_changes_land() {
        let base = save(&Lines::default(), b"a\nb\nc\n", 1);
        let parent = save(&base, b"b\nc\n", 2);
        let child = save(&base, b"b\nchanged\n", 3);
        let (content, collisions) = land_save(&parent, &base, &child);
        assert!(collisions.is_empty());
        assert_eq!(test_bytes::of(&content), b"b\nchanged\n");
        assert_eq!(content.lines[0].version, child.lines[0].version);
    }

    #[test]
    fn unfinished_incoming_edit_before_retained_parent_line_loses_whole() {
        let base = save(&Lines::default(), b"first\nsecond\n", 1);
        let parent = save(&base, b"first\nparent\n", 2);
        let first = save(&base, b"partial", 3);
        assert!(!land_save(&parent, &base, &first).1.is_empty());
        let later = save(&first, b"later", 10);
        let (content, collisions) = land_save(&parent, &first, &later);
        assert_eq!(content, parent);
        assert_eq!(collisions.len(), 1);
        assert_eq!(collisions[0].2, CollisionKind::Line);
        assert_eq!(collisions[0].1, parent.lines[1].version);
    }

    #[test]
    fn an_unfinished_save_past_the_parents_lines_lands() {
        let base = save(&Lines::default(), b"a\n", 1);
        let parent = save(&base, b"a\nparent\n", 2);
        let child = save(&base, b"a\nchild", 3);
        let (content, collisions) = land_save(&parent, &base, &child);
        assert!(collisions.is_empty());
        assert_eq!(test_bytes::of(&content), b"a\nparent\nchild");
    }

    #[test]
    fn sync_hides_parent_lines_after_an_undecided_unfinished_line() {
        let base = save(&Lines::default(), b"a\nb\n", 1);
        let parent = save(&base, b"a\nb\nm7\n", 2);
        let changed = |child: &Lines| -> BTreeSet<LineVersion> {
            child
                .lines
                .iter()
                .filter(|l| !base.ancestors.contains_key(&l.version))
                .map(|l| l.version)
                .collect()
        };
        let child = save(&base, b"y", 3);
        let before = sync_file(&parent, &child, &changed(&child)).unwrap();
        assert_eq!(test_bytes::of(&before), b"y");
        let after = sync_file(&parent, &before, &BTreeSet::new()).unwrap();
        assert_eq!(test_bytes::of(&after), b"a\nb\nm7\n");
        // An unfinished line past the parent's lines hides nothing.
        let child = save(&base, b"a\nb\nchild", 10);
        assert_eq!(
            test_bytes::of(&sync_file(&parent, &child, &changed(&child)).unwrap()),
            b"a\nb\nm7\nchild"
        );
        // Nor does a finished line.
        let child = save(&base, b"y\n", 11);
        assert_eq!(
            test_bytes::of(&sync_file(&parent, &child, &changed(&child)).unwrap()),
            b"y\nm7\n"
        );
    }
}
