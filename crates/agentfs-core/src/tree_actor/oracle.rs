//! Tests only, until the module cut's step 5.1 ends (docs/target-architecture/future/module-cut.md):
//! the old referee, sync and line rules, verbatim but for their paths, compared with the new on
//! seeded trees.

/// The rules as they stood before the referee was cut into units. No line here changes.
mod old {
    /// `tree_reconcile`'s rules.
    pub(super) mod reconcile {
        use std::collections::{BTreeMap, BTreeSet};

        use crate::tree_reconcile::{Error, Id, Line, LineMap};

        /// The parent's decision for one incoming version. The caller supplies the
        /// version graph, including ancestry obtained from the writer's read view.
        #[derive(Clone, Debug, PartialEq, Eq)]
        pub enum Verdict<V = String> {
            /// The version can be applied (including agreeing deletes).
            Apply,
            /// The parent has this version or a later version.
            Drop,
            /// Concurrent versions: the parent wins.
            Lose { winner: V },
        }

        /// Compare versions of one identity. `parent` is absent for a new line.
        /// Both input maps' ancestry must have been combined before calling this.
        pub fn verdict<V: Id>(
            parent: Option<&Line<V>>,
            incoming: &Line<V>,
            ancestry: &BTreeMap<V, Option<V>>,
        ) -> Result<Verdict<V>, Error> {
            let Some(parent) = parent else {
                return Ok(Verdict::Apply);
            };
            if parent.identity != incoming.identity {
                return Err(Error::InvalidMap("different line identities"));
            }
            fn descends<V: Id>(
                start: &V,
                wanted: &V,
                graph: &BTreeMap<V, Option<V>>,
            ) -> Result<bool, Error> {
                let mut next = Some(start);
                let mut seen = BTreeSet::new();
                while let Some(id) = next {
                    if id == wanted {
                        return Ok(true);
                    }
                    if !seen.insert(id) {
                        return Err(Error::InvalidMap("cyclic ancestry"));
                    }
                    next = graph
                        .get(id)
                        .ok_or(Error::InvalidMap("missing ancestor"))?
                        .as_ref();
                }
                Ok(false)
            }
            if parent.version == incoming.version {
                return Ok(Verdict::Drop);
            }
            if descends(&incoming.version, &parent.version, ancestry)? {
                return Ok(Verdict::Apply);
            }
            if descends(&parent.version, &incoming.version, ancestry)? {
                return Ok(Verdict::Drop);
            }
            if parent.bytes.is_none() && incoming.bytes.is_none() {
                return Ok(Verdict::Apply);
            }
            Ok(Verdict::Lose {
                winner: parent.version.clone(),
            })
        }

        /// Why a version was lost when a whole save was judged.
        #[derive(Clone, Debug, PartialEq, Eq)]
        pub enum CollisionKind {
            /// This version collided with the parent's version.
            Line,
            /// This version would have landed, but another version lost the save.
            Whole,
        }

        /// A lost version and the parent version that decided against it.
        #[derive(Clone, Debug, PartialEq, Eq)]
        pub struct Collision<V = String> {
            pub identity: V,
            pub lost: V,
            pub winner: V,
            pub kind: CollisionKind,
        }

        /// The result of one whole save. A losing save leaves `content` exactly the
        /// parent's map; the caller keeps the incoming commit reachable in history.
        #[derive(Clone, Debug, PartialEq, Eq)]
        pub struct Landing<V = String> {
            pub content: LineMap<V>,
            pub collisions: Vec<Collision<V>>,
        }

        fn combined_graph<V: Id>(
            a: &LineMap<V>,
            b: &LineMap<V>,
        ) -> Result<BTreeMap<V, Option<V>>, Error> {
            let mut graph = a.ancestors.clone();
            for (version, over) in &b.ancestors {
                if graph.get(version).is_some_and(|existing| existing != over) {
                    return Err(Error::InvalidMap("conflicting version ancestry"));
                }
                graph.insert(version.clone(), over.clone());
            }
            Ok(graph)
        }

        fn unfinished<V>(line: &Line<V>) -> bool {
            line.bytes.as_ref().is_some_and(|b| !b.ends_with(b"\n"))
        }

        fn parents_unfinished_tail<V>(parent: &LineMap<V>) -> Option<&Line<V>> {
            parent
                .lines
                .iter()
                .rev()
                .find(|line| line.bytes.is_some())
                .filter(|line| unfinished(line))
        }

        // R10.8: the first selected unfinished line with a live line after it, as its
        // identity and the next live line's version.
        fn unfinished_before_kept<V: Id>(
            placed: &[Line<V>],
            selected: &BTreeSet<V>,
        ) -> Option<(V, V)> {
            let mut live = placed.iter().filter(|line| line.bytes.is_some()).peekable();
            while let Some(line) = live.next() {
                if unfinished(line)
                    && selected.contains(&line.identity)
                    && let Some(next) = live.peek()
                {
                    return Some((line.identity.clone(), next.version.clone()));
                }
            }
            None
        }

        // Replace existing identities in place; insert new identities after their
        // nearest predecessor in the source, past parent-only inserts at that point.
        fn place<V: Id>(
            parent: &[Line<V>],
            source: &[Line<V>],
            selected: &BTreeSet<V>,
        ) -> Vec<Line<V>> {
            let mut result = parent.to_vec();
            let source_ids: BTreeSet<&V> = source.iter().map(|l| &l.identity).collect();
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

        /// Judge a single file save whole at the parent's queue.
        ///
        /// `base` is the file map at the save's first parent, `incoming` is that
        /// save's map, and `parent` is the current parent map. Each changed version
        /// in the incoming map is judged; one loss rejects every applicable version.
        /// The actor must also account for file/name collisions, preserve the losing
        /// commit, and commit the result with both histories as parents. An incoming
        /// unfinished line placed in front of a line the parent keeps loses to that
        /// line (R10.8), so no result holds bytes neither writer wrote.
        pub fn land_save<V: Id>(
            parent: &LineMap<V>,
            base: &LineMap<V>,
            incoming: &LineMap<V>,
        ) -> Result<Landing<V>, Error> {
            let graph = combined_graph(parent, incoming)?;
            let base_versions: BTreeMap<&V, &V> = base
                .lines
                .iter()
                .map(|l| (&l.identity, &l.version))
                .collect();
            let current: BTreeMap<&V, &Line<V>> =
                parent.lines.iter().map(|l| (&l.identity, l)).collect();
            let changed: Vec<&Line<V>> = incoming
                .lines
                .iter()
                .filter(|line| base_versions.get(&line.identity) != Some(&&line.version))
                .collect();
            let mut decisions = BTreeMap::new();
            for line in &changed {
                decisions.insert(
                    line.identity.clone(),
                    verdict(current.get(&line.identity).copied(), line, &graph)?,
                );
            }
            let selected: BTreeSet<V> = decisions
                .iter()
                .filter_map(|(id, verdict)| (*verdict == Verdict::Apply).then_some(id.clone()))
                .collect();
            let placed = place(&parent.lines, &incoming.lines, &selected);
            // R10.4: an incoming line placed after a parent's unfinished last line
            // loses to that line. An edited existing line stays in its existing place.
            if let Some(partial) = parents_unfinished_tail(parent)
                && let Some(pos) = placed
                    .iter()
                    .position(|line| line.version == partial.version)
            {
                for line in placed[pos + 1..]
                    .iter()
                    .filter(|line| line.bytes.is_some() && selected.contains(&line.identity))
                {
                    decisions.insert(
                        line.identity.clone(),
                        Verdict::Lose {
                            winner: partial.version.clone(),
                        },
                    );
                }
            }
            // R10.8: judged on what would still apply after R10.4.
            let applying: BTreeSet<V> = selected
                .iter()
                .filter(|id| decisions[*id] == Verdict::Apply)
                .cloned()
                .collect();
            let kept = place(&parent.lines, &incoming.lines, &applying);
            if let Some((identity, winner)) = unfinished_before_kept(&kept, &applying) {
                decisions.insert(identity, Verdict::Lose { winner });
            }
            let decider = changed
                .iter()
                .find_map(|line| match &decisions[&line.identity] {
                    Verdict::Lose { winner } => Some(winner.clone()),
                    _ => None,
                });
            if let Some(winner) = decider {
                let collisions = changed
                    .iter()
                    .filter_map(|line| match &decisions[&line.identity] {
                        Verdict::Drop => None,
                        Verdict::Apply => Some(Collision {
                            identity: line.identity.clone(),
                            lost: line.version.clone(),
                            winner: winner.clone(),
                            kind: CollisionKind::Whole,
                        }),
                        Verdict::Lose { winner } => Some(Collision {
                            identity: line.identity.clone(),
                            lost: line.version.clone(),
                            winner: winner.clone(),
                            kind: CollisionKind::Line,
                        }),
                    })
                    .collect();
                return Ok(Landing {
                    content: parent.clone(),
                    collisions,
                });
            }
            Ok(Landing {
                content: LineMap {
                    lines: placed,
                    ancestors: graph,
                },
                collisions: Vec::new(),
            })
        }

        /// Sync the current parent with exactly the sandbox versions still awaiting a
        /// decision. `undecided` contains version IDs, determined by commit ancestry;
        /// this function makes no collision decision. The actor must only sync a file
        /// when its writer-lock protocol permits it.
        pub fn sync<V: Id>(
            parent: &LineMap<V>,
            sandbox: &LineMap<V>,
            undecided: &BTreeSet<V>,
        ) -> Result<LineMap<V>, Error> {
            let graph = combined_graph(parent, sandbox)?;
            let selected: BTreeSet<V> = sandbox
                .lines
                .iter()
                .filter(|line| undecided.contains(&line.version))
                .map(|line| line.identity.clone())
                .collect();
            let mut lines = place(&parent.lines, &sandbox.lines, &selected);
            // R10.6: keep the undecided sandbox line visible without inventing a
            // newline after an unseen unfinished parent tail.
            if let Some(partial) = parents_unfinished_tail(parent)
                && let Some(pos) = lines
                    .iter()
                    .position(|line| line.version == partial.version)
                && lines[pos + 1..]
                    .iter()
                    .any(|line| line.bytes.is_some() && selected.contains(&line.identity))
            {
                lines.remove(pos);
            }
            // R10.9, the mirror: keep an undecided unfinished line visible without
            // joining the parent's later lines to it. They arrive with the decision.
            if let Some(pos) = lines
                .iter()
                .position(|line| selected.contains(&line.identity) && unfinished(line))
            {
                let later = lines.split_off(pos + 1);
                lines.extend(
                    later
                        .into_iter()
                        .filter(|line| line.bytes.is_none() || selected.contains(&line.identity)),
                );
            }
            Ok(LineMap {
                lines,
                ancestors: graph,
            })
        }
    }

    /// `referee.rs`'s production part.
    pub(super) mod referee {
        use std::collections::{BTreeMap, BTreeSet};

        use super::reconcile::{self as tree_reconcile, Verdict};
        use crate::tree_actor::referee::{
            Collision, CollisionKind, Decision, File, Snapshot, Version,
        };
        use crate::tree_actor::tree::{EventNumber, LineVersion, Lines};
        use crate::tree_actor::{Error, MergeFileIdentity, NameHistory, NameVersion, Result};

        fn history(snapshot: &Snapshot, id: MergeFileIdentity) -> Option<&NameHistory> {
            snapshot.names.get(&id)
        }

        fn head(history: &NameHistory) -> Result<&NameVersion> {
            history.versions.get(&history.head).ok_or(Error::Format)
        }

        fn path(history: &NameHistory) -> Result<Option<&str>> {
            Ok(head(history)?.path.as_deref())
        }

        fn name_descends(
            history: &NameHistory,
            mut current: EventNumber,
            ancestor: EventNumber,
        ) -> Result<bool> {
            let mut seen = BTreeSet::new();
            loop {
                if current == ancestor {
                    return Ok(true);
                }
                if !seen.insert(current) {
                    return Err(Error::Format);
                }
                match history.versions.get(&current).ok_or(Error::Format)?.over {
                    Some(previous) => current = previous,
                    None => return Ok(false),
                }
            }
        }

        fn combine_names(a: &NameHistory, b: &NameHistory) -> Result<NameHistory> {
            let mut combined = a.clone();
            for (key, value) in &b.versions {
                if let Some(existing) = combined.versions.get(key) {
                    if existing.path != value.path
                        || existing.over != value.over
                        || existing.replaced_by != value.replaced_by
                    {
                        return Err(Error::Format);
                    }
                } else {
                    combined.versions.insert(*key, value.clone());
                }
            }
            Ok(combined)
        }

        fn name_verdict(
            parent: Option<&NameHistory>,
            incoming: &NameHistory,
        ) -> Result<(Verdict<EventNumber>, NameHistory)> {
            let Some(parent) = parent else {
                return Ok((Verdict::Apply, incoming.clone()));
            };
            let combined = combine_names(parent, incoming)?;
            if parent.head == incoming.head {
                return Ok((Verdict::Drop, combined));
            }
            if name_descends(&combined, incoming.head, parent.head)? {
                return Ok((Verdict::Apply, combined));
            }
            if name_descends(&combined, parent.head, incoming.head)? {
                return Ok((Verdict::Drop, combined));
            }
            if path(parent)?.is_none() && path(incoming)?.is_none() {
                return Ok((Verdict::Apply, combined));
            }
            Ok((
                Verdict::Lose {
                    winner: parent.head,
                },
                combined,
            ))
        }

        fn line_descends<'a>(
            map: &'a Lines,
            mut current: &'a LineVersion,
            ancestor: &LineVersion,
        ) -> Result<bool> {
            let mut seen = BTreeSet::new();
            loop {
                if current == ancestor {
                    return Ok(true);
                }
                if !seen.insert(current) {
                    return Err(Error::Format);
                }
                match map.ancestors.get(current).ok_or(Error::Format)? {
                    Some(previous) => current = previous,
                    None => return Ok(false),
                }
            }
        }

        // An unlink can remove only versions its writer could have seen on its first
        // parent. A line in that snapshot may itself descend from the parent's line.
        fn unseen_parent_line(
            base: Option<&File>,
            parent: Option<&File>,
        ) -> Result<Option<LineVersion>> {
            let Some(parent) = parent else {
                return Ok(None);
            };
            let Some(base) = base else {
                return Ok(parent
                    .lines
                    .lines
                    .iter()
                    .find(|line| line.bytes.is_some())
                    .map(|line| line.version));
            };
            let mut graph = base.lines.clone();
            for (version, over) in &parent.lines.ancestors {
                if graph.ancestors.get(version).is_some_and(|old| old != over) {
                    return Err(Error::Format);
                }
                graph.ancestors.insert(*version, *over);
            }
            let previous: BTreeMap<_, _> =
                base.lines.lines.iter().map(|l| (&l.identity, l)).collect();
            for line in parent
                .lines
                .lines
                .iter()
                .filter(|line| line.bytes.is_some())
            {
                match previous.get(&line.identity) {
                    Some(old) if line_descends(&graph, &old.version, &line.version)? => {}
                    _ => return Ok(Some(line.version)),
                }
            }
            Ok(None)
        }

        /// Judge all versions in one save together. A losing save returns the parent
        /// snapshot unchanged, while every applicable incoming version is reported.
        pub(in crate::tree_actor) fn judge(
            parent: &Snapshot,
            base: &Snapshot,
            incoming: &Snapshot,
            prior_decisions: &BTreeMap<Version, Version>,
        ) -> Result<Decision> {
            let mut result = parent.clone();
            let mut candidates: Vec<(Version, Option<(Version, CollisionKind)>)> = Vec::new();
            let ids: BTreeSet<_> = base
                .names
                .keys()
                .chain(incoming.names.keys())
                .chain(base.files.keys())
                .chain(incoming.files.keys())
                .copied()
                .collect();
            let mut applied_names = BTreeSet::new();
            for id in &ids {
                let Some(new_name) = history(incoming, *id) else {
                    continue;
                };
                if history(base, *id).is_some_and(|old| old.head == new_name.head) {
                    continue;
                }
                let (verdict, mut combined) = name_verdict(history(parent, *id), new_name)?;
                let lost = Version::Name {
                    file: *id,
                    version: new_name.head,
                };
                let mut loss = match verdict {
                    Verdict::Drop => continue,
                    Verdict::Apply => None,
                    Verdict::Lose { .. } => Some((
                        Version::Name {
                            file: *id,
                            version: parent.names[id].head,
                        },
                        CollisionKind::Name,
                    )),
                };
                if path(new_name)?.is_none()
                    && loss.is_none()
                    && let Some(line) =
                        unseen_parent_line(base.files.get(id), parent.files.get(id))?
                {
                    loss = Some((
                        Version::Line {
                            file: *id,
                            version: line,
                        },
                        CollisionKind::Name,
                    ));
                }
                candidates.push((lost, loss.clone()));
                if loss.is_none() {
                    combined.head = new_name.head;
                    result.names.insert(*id, combined);
                    applied_names.insert(*id);
                    if path(new_name)?.is_none() {
                        result.files.remove(id);
                    } else if let std::collections::btree_map::Entry::Vacant(entry) =
                        result.files.entry(*id)
                    {
                        // Accepting a live name for an unborn identity creates an
                        // empty file. Only content versions in this save are applied;
                        // content from an earlier losing save remains absent.
                        let object = incoming.files.get(id).ok_or(Error::Format)?.object;
                        entry.insert(File {
                            object,
                            lines: Lines::default(),
                        });
                    }
                }
            }
            // A name cannot displace another live file unless this same save deletes
            // that identity (rename-over). Name decisions are considered together.
            for id in &applied_names {
                let Some(new_path) = path(&incoming.names[id])? else {
                    continue;
                };
                for (other, parent_name) in &parent.names {
                    if other == id || path(parent_name)? != Some(new_path) {
                        continue;
                    }
                    // A rename-over's own unlink frees the destination for its name
                    // claim even when that unlink loses the whole change.
                    let deleted = match incoming.names.get(other) {
                        Some(name) => {
                            path(name)?.is_none()
                                && head(name)?.replaced_by == Some(*id)
                                && base
                                    .names
                                    .get(other)
                                    .is_none_or(|old| old.head != name.head)
                        }
                        None => false,
                    };
                    if !deleted
                        && let Some((_, loss)) = candidates
                            .iter_mut()
                            .find(|(v, _)| matches!(v, Version::Name { file, .. } if file == id))
                    {
                        *loss = Some((
                            Version::Name {
                                file: *other,
                                version: parent_name.head,
                            },
                            CollisionKind::Name,
                        ));
                    }
                }
            }
            for id in &ids {
                let Some(new_file) = incoming.files.get(id) else {
                    continue;
                };
                let old_file = base.files.get(id);
                if old_file.is_some_and(|old| old.lines == new_file.lines) {
                    continue;
                }
                let original = parent.files.get(id);
                let rejected_name = if original.is_none() && !parent.names.contains_key(id) {
                    if let Some(name) = base.names.get(id) {
                        let mut current = Some(name.head);
                        let mut winner = None;
                        while let Some(version) = current {
                            if let Some(decider) =
                                prior_decisions.get(&Version::Name { file: *id, version })
                            {
                                winner = Some(decider.clone());
                                break;
                            }
                            current = name.versions.get(&version).ok_or(Error::Format)?.over;
                        }
                        winner
                    } else {
                        None
                    }
                } else {
                    None
                };
                let parent_deleted = match parent.names.get(id) {
                    Some(name) => path(name)?.is_none(),
                    None => rejected_name.is_some(),
                };
                if original.is_none() && parent_deleted {
                    let winner = match rejected_name {
                        Some(winner) => winner,
                        None => Version::Name {
                            file: *id,
                            version: parent.names[id].head,
                        },
                    };
                    let old_versions: BTreeMap<_, _> = old_file
                        .into_iter()
                        .flat_map(|f| f.lines.lines.iter())
                        .map(|l| (&l.identity, &l.version))
                        .collect();
                    for line in &new_file.lines.lines {
                        if old_versions.get(&line.identity) != Some(&&line.version) {
                            candidates.push((
                                Version::Line {
                                    file: *id,
                                    version: line.version,
                                },
                                Some((winner.clone(), CollisionKind::Name)),
                            ));
                        }
                    }
                    continue;
                }
                let empty = Lines::default();
                let parent_map = original.map_or(&empty, |f| &f.lines);
                let base_map = old_file.map_or(&empty, |f| &f.lines);
                let landing = tree_reconcile::land_save(parent_map, base_map, &new_file.lines)?;
                let mut graph = parent_map.ancestors.clone();
                graph.extend(new_file.lines.ancestors.clone());
                let old_versions: BTreeMap<_, _> = base_map
                    .lines
                    .iter()
                    .map(|l| (&l.identity, &l.version))
                    .collect();
                let parent_lines: BTreeMap<_, _> =
                    parent_map.lines.iter().map(|l| (&l.identity, l)).collect();
                for line in &new_file.lines.lines {
                    if old_versions.get(&line.identity) == Some(&&line.version) {
                        continue;
                    }
                    let own_collision = landing.collisions.iter().find(|c| c.lost == line.version);
                    let loss = own_collision.and_then(|c| match c.kind {
                        tree_reconcile::CollisionKind::Line => Some((
                            Version::Line {
                                file: *id,
                                version: c.winner,
                            },
                            CollisionKind::Line,
                        )),
                        tree_reconcile::CollisionKind::Whole => None,
                    });
                    if own_collision.is_some()
                        || tree_reconcile::verdict(
                            parent_lines.get(&line.identity).copied(),
                            line,
                            &graph,
                        )? != Verdict::Drop
                    {
                        candidates.push((
                            Version::Line {
                                file: *id,
                                version: line.version,
                            },
                            loss,
                        ));
                    }
                }
                if landing.collisions.is_empty() {
                    result.files.insert(
                        *id,
                        File {
                            object: original.map_or(new_file.object, |f| f.object),
                            lines: landing.content,
                        },
                    );
                }
            }
            let winner = candidates
                .iter()
                .find_map(|(_, loss)| loss.as_ref().map(|(v, _)| v.clone()));
            if let Some(winner) = winner {
                let collisions = candidates
                    .into_iter()
                    .map(|(lost, loss)| {
                        let (decider, kind) =
                            loss.unwrap_or_else(|| (winner.clone(), CollisionKind::Whole));
                        Collision {
                            lost,
                            winner: decider,
                            kind,
                        }
                    })
                    .collect();
                Ok(Decision {
                    snapshot: parent.clone(),
                    collisions,
                })
            } else {
                Ok(Decision {
                    snapshot: result,
                    collisions: Vec::new(),
                })
            }
        }
    }

    /// `sync.rs`'s production part.
    pub(super) mod sync {
        use std::collections::{BTreeMap, BTreeSet};

        use super::reconcile as tree_reconcile;
        use crate::tree_actor::referee::{File, Snapshot};
        use crate::tree_actor::tree::{EventNumber, LineVersion, Lines};
        use crate::tree_actor::{Error, MergeFileIdentity, NameHistory, Result};

        fn path(history: &NameHistory) -> Result<Option<&str>> {
            Ok(history
                .versions
                .get(&history.head)
                .ok_or(Error::Format)?
                .path
                .as_deref())
        }

        fn combine(parent: &NameHistory, fork: &NameHistory) -> Result<NameHistory> {
            let mut result = parent.clone();
            for (id, version) in &fork.versions {
                if let Some(old) = result.versions.get(id) {
                    if old.path != version.path
                        || old.over != version.over
                        || old.replaced_by != version.replaced_by
                    {
                        return Err(Error::Format);
                    }
                } else {
                    result.versions.insert(*id, version.clone());
                }
            }
            Ok(result)
        }

        /// Whether the parent ended this file: it has the file's name history and no file.
        pub(in crate::tree_actor) fn ended(
            parent: &Snapshot,
            id: MergeFileIdentity,
        ) -> Result<bool> {
            Ok(!parent.files.contains_key(&id)
                && match parent.names.get(&id) {
                    Some(name) => path(name)?.is_none(),
                    None => false,
                })
        }

        /// Bring the parent's current snapshot into the fork without deciding any
        /// fork version. Name keys are scoped to the file; line IDs are volume-unique.
        /// `last_lines` holds the parent's last lines of each file it ended that the
        /// fork keeps under an undecided name: until the parent decides, that file reads
        /// as any other, the parent's decided lines and the fork's undecided ones (R3a,
        /// R3d, finding 44).
        pub(in crate::tree_actor) fn sync(
            parent: &Snapshot,
            fork: &Snapshot,
            undecided_lines: &BTreeSet<LineVersion>,
            undecided_names: &BTreeSet<(MergeFileIdentity, EventNumber)>,
            last_lines: &BTreeMap<MergeFileIdentity, Lines>,
        ) -> Result<Snapshot> {
            let mut names = BTreeMap::new();
            let ids: BTreeSet<_> = parent
                .names
                .keys()
                .chain(fork.names.keys())
                .copied()
                .collect();
            for id in ids {
                let p = parent.names.get(&id);
                let f = fork.names.get(&id);
                if p.is_none() && f.is_some_and(|f| !undecided_names.contains(&(id, f.head))) {
                    continue;
                }
                let mut name = match (p, f) {
                    (Some(p), Some(f)) => combine(p, f)?,
                    (Some(p), None) => p.clone(),
                    (None, Some(f)) => f.clone(),
                    (None, None) => unreachable!(),
                };
                if let Some(f) = f
                    && (p.is_none() || undecided_names.contains(&(id, f.head)))
                {
                    name.head = f.head;
                }
                path(&name)?;
                names.insert(id, name);
            }

            // A parent name that clashes with an undecided fork name falls back to
            // the name that file had in the fork. Repeat for chains of displaced names.
            loop {
                let mut occupied: BTreeMap<String, Vec<MergeFileIdentity>> = BTreeMap::new();
                for (&id, name) in &names {
                    if let Some(p) = path(name)? {
                        occupied.entry(p.to_owned()).or_default().push(id);
                    }
                }
                let Some(ids) = occupied.values().find(|ids| ids.len() > 1) else {
                    break;
                };
                // Both already show their fork names: the input fork is invalid.
                let id = ids
                    .iter()
                    .find(|id| fork.names.get(id).map(|h| h.head) != Some(names[id].head))
                    .copied()
                    .ok_or(Error::Format)?;
                if let Some(f) = fork.names.get(&id) {
                    names.get_mut(&id).ok_or(Error::Format)?.head = f.head;
                } else {
                    // The fork never had this file; it is hidden until the clash ends.
                    names.remove(&id);
                }
            }

            let mut files = BTreeMap::new();
            for (&id, name) in &names {
                if path(name)?.is_none() {
                    continue;
                }
                let p = parent.files.get(&id);
                let f = fork.files.get(&id);
                let file = match (p, f) {
                    (Some(p), Some(f)) => File {
                        object: f.object,
                        lines: tree_reconcile::sync(&p.lines, &f.lines, undecided_lines)?,
                    },
                    // A file this tree ended by an unlink or a rename-over that lost comes
                    // back as a new object, numbered by the event that ended it, which
                    // numbers nothing else. Its old object stays with whoever holds it: an
                    // old writer reaches no file (R11.3), and after an atomic save that
                    // object is now the replaced file's (finding 39).
                    (Some(p), None) => File {
                        object: fork
                            .names
                            .get(&id)
                            .map_or(p.object, |ended| ended.head.object()),
                        lines: tree_reconcile::sync(&p.lines, &Lines::default(), undecided_lines)?,
                    },
                    (None, Some(f)) => File {
                        object: f.object,
                        lines: tree_reconcile::sync(
                            last_lines.get(&id).unwrap_or(&Lines::default()),
                            &f.lines,
                            undecided_lines,
                        )?,
                    },
                    (None, None) => return Err(Error::Format),
                };
                files.insert(id, file);
            }
            Ok(Snapshot { files, names })
        }
    }
}

use std::collections::{BTreeMap, BTreeSet};

use super::referee::{self, Collision, CollisionKind, File, Snapshot, Undecided, Version};
use super::tree::{EventNumber, LineVersion, Lines, Versions};
use super::{MergeFileIdentity, NameHistory};
use crate::tree_reconcile::Line;

/// A seeded generator (SplitMix64): the same seed gives the same case.
struct SplitMix64(u64);

impl SplitMix64 {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    fn below(&mut self, n: usize) -> usize {
        usize::try_from(self.next() % n as u64).unwrap()
    }

    fn chance(&mut self, num: u64, den: u64) -> bool {
        self.next() % den < num
    }
}

const POOL: [&str; 4] = ["a", "b", "c", "d"];

/// One side of a case: its snapshot, the events it made, and the last lines of each file it
/// ended (what `last_lines` reads from history).
#[derive(Clone)]
struct Side {
    snapshot: Snapshot,
    events: BTreeSet<EventNumber>,
    ended: BTreeMap<MergeFileIdentity, Lines>,
    renamed_over: bool,
}

fn path_of(snapshot: &Snapshot, id: MergeFileIdentity) -> Option<&str> {
    let name = snapshot.names.get(&id)?;
    name.versions.get(&name.head)?.path.as_deref()
}

fn holder(snapshot: &Snapshot, path: &str) -> Option<MergeFileIdentity> {
    snapshot
        .files
        .keys()
        .copied()
        .find(|id| path_of(snapshot, *id) == Some(path))
}

fn unfinished(line: &Line<LineVersion>) -> bool {
    line.bytes.as_ref().is_some_and(|b| !b.ends_with(b"\n"))
}

fn live(lines: &Lines) -> Vec<usize> {
    (0..lines.lines.len())
        .filter(|i| lines.lines[*i].bytes.is_some())
        .collect()
}

impl Side {
    fn new(snapshot: Snapshot) -> Self {
        Self {
            snapshot,
            events: BTreeSet::new(),
            ended: BTreeMap::new(),
            renamed_over: false,
        }
    }

    fn event(&mut self, clock: &mut u64) -> EventNumber {
        let event = EventNumber::new(*clock);
        *clock += 1;
        self.events.insert(event);
        event
    }

    fn files(&self) -> Vec<MergeFileIdentity> {
        self.snapshot.files.keys().copied().collect()
    }

    fn free(&self) -> Vec<&'static str> {
        POOL.into_iter()
            .filter(|path| holder(&self.snapshot, path).is_none())
            .collect()
    }

    fn end(
        &mut self,
        id: MergeFileIdentity,
        event: EventNumber,
        replaced_by: Option<MergeFileIdentity>,
    ) {
        if let Some(file) = self.snapshot.files.remove(&id) {
            self.ended.insert(id, file.lines);
        }
        self.snapshot
            .names
            .get_mut(&id)
            .unwrap()
            .advance(event, None, replaced_by);
    }

    /// A new line in `lines` written by `versions`, before an unfinished last line.
    fn insert(rng: &mut SplitMix64, lines: &mut Lines, versions: &mut Versions) {
        let version = versions.next();
        let end = match live(lines).last() {
            Some(last) if unfinished(&lines.lines[*last]) => *last,
            _ => lines.lines.len(),
        };
        let at = rng.below(end + 1);
        lines.ancestors.insert(version, None);
        lines.lines.insert(
            at,
            Line {
                identity: version,
                version,
                bytes: Some(format!("i{version}\n").into_bytes()),
            },
        );
    }

    /// A new version of one live line: an edit, a delete, or the last line left unfinished.
    fn rewrite(rng: &mut SplitMix64, lines: &mut Lines, versions: &mut Versions, kind: u8) -> bool {
        let live = live(lines);
        let Some(&last) = live.last() else {
            return false;
        };
        let at = match kind {
            2 if unfinished(&lines.lines[last]) => return false,
            2 => last,
            _ => live[rng.below(live.len())],
        };
        let version = versions.next();
        let line = &mut lines.lines[at];
        let open = unfinished(line) || kind == 2;
        let text = format!("e{version}{}", if open { "" } else { "\n" });
        lines.ancestors.insert(version, Some(line.version));
        line.version = version;
        line.bytes = (kind != 1).then(|| text.into_bytes());
        true
    }

    fn create(&mut self, rng: &mut SplitMix64, clock: &mut u64) -> bool {
        let free = self.free();
        if free.is_empty() {
            return false;
        }
        let path = free[rng.below(free.len())];
        let event = self.event(clock);
        let id = MergeFileIdentity(event.object());
        let mut lines = Lines::default();
        let mut versions = Versions::of(event);
        for _ in 0..rng.below(3) {
            Self::insert(rng, &mut lines, &mut versions);
        }
        self.snapshot
            .names
            .insert(id, NameHistory::created(event, path.to_owned()));
        self.snapshot.files.insert(
            id,
            File {
                object: event.object(),
                lines,
            },
        );
        true
    }

    /// One operation, one event (an atomic rewrite takes two). False if it had nothing to work on.
    fn operate(&mut self, rng: &mut SplitMix64, clock: &mut u64) -> bool {
        let files = self.files();
        let kind = rng.below(10);
        if files.is_empty() || kind == 0 {
            return self.create(rng, clock);
        }
        let id = files[rng.below(files.len())];
        match kind {
            1..=4 => {
                let event = self.event(clock);
                let mut versions = Versions::of(event);
                let lines = &mut self.snapshot.files.get_mut(&id).unwrap().lines;
                match kind {
                    1 => Self::rewrite(rng, lines, &mut versions, 0),
                    2 => Self::rewrite(rng, lines, &mut versions, 1),
                    3 => {
                        Self::insert(rng, lines, &mut versions);
                        true
                    }
                    _ => Self::rewrite(rng, lines, &mut versions, 2),
                }
            }
            5 => {
                let free = self.free();
                if free.is_empty() {
                    return false;
                }
                let path = free[rng.below(free.len())];
                let event = self.event(clock);
                self.snapshot.names.get_mut(&id).unwrap().advance(
                    event,
                    Some(path.to_owned()),
                    None,
                );
                true
            }
            6 | 7 => {
                let others: Vec<_> = files.iter().copied().filter(|o| *o != id).collect();
                if others.is_empty() {
                    return false;
                }
                let other = others[rng.below(others.len())];
                let path = path_of(&self.snapshot, other).unwrap().to_owned();
                let event = self.event(clock);
                self.end(other, event, Some(id));
                self.snapshot
                    .names
                    .get_mut(&id)
                    .unwrap()
                    .advance(event, Some(path), None);
                self.renamed_over = true;
                true
            }
            8 => {
                let event = self.event(clock);
                self.end(id, event, None);
                true
            }
            _ => {
                // An atomic rewrite: a temporary file renamed over this one. The file keeps its
                // identity and name and takes the temporary's object.
                let temporary = self.event(clock);
                let event = self.event(clock);
                let mut name = NameHistory::created(temporary, format!("tmp{temporary}"));
                name.advance(event, None, None);
                self.snapshot
                    .names
                    .insert(MergeFileIdentity(temporary.object()), name);
                let file = self.snapshot.files.get_mut(&id).unwrap();
                file.object = temporary.object();
                let mut versions = Versions::of(event);
                if !Self::rewrite(rng, &mut file.lines, &mut versions, 0) {
                    Self::insert(rng, &mut file.lines, &mut versions);
                }
                true
            }
        }
    }

    fn operations(&mut self, rng: &mut SplitMix64, clock: &mut u64, at_most: usize) {
        for _ in 0..rng.below(at_most + 1) {
            self.operate(rng, clock);
        }
    }
}

/// One landing and one sync to compare.
struct Case {
    upstream: Snapshot,
    base: Snapshot,
    incoming: Snapshot,
    prior: BTreeMap<Version, Version>,
    renamed_over: bool,
    undecided_lines: BTreeSet<LineVersion>,
    undecided_names: BTreeSet<(MergeFileIdentity, EventNumber)>,
    last_lines: BTreeMap<MergeFileIdentity, Lines>,
}

fn case(seed: u64) -> Case {
    let mut rng = SplitMix64(seed);
    let mut clock = [1, 8, 95, 990][rng.below(4)];
    let mut root = Side::new(Snapshot::default());
    for _ in 0..=rng.below(4) {
        root.create(&mut rng, &mut clock);
    }
    let root = root.snapshot;
    let mut upstream = Side::new(root.clone());
    let mut incoming = Side::new(root.clone());
    // One case in four, the upstream already holds the change's first operation (R3e.2): those
    // versions are decided, so not the fork's undecided ones.
    if rng.chance(1, 4) && incoming.operate(&mut rng, &mut clock) {
        incoming.events.clear();
        upstream = incoming.clone();
        upstream.renamed_over = false;
    }
    // The two sides' events interleave, as a volume's do.
    let (mut up, mut inc) = (rng.below(4), rng.below(4));
    while up + inc > 0 {
        if inc == 0 || (up > 0 && rng.chance(1, 2)) {
            upstream.operate(&mut rng, &mut clock);
            up -= 1;
        } else {
            incoming.operate(&mut rng, &mut clock);
            inc -= 1;
        }
    }
    let mut base = root;
    let mut prior = BTreeMap::new();
    if rng.chance(1, 4) {
        let first = old::referee::judge(
            &upstream.snapshot,
            &base,
            &incoming.snapshot,
            &BTreeMap::new(),
        )
        .unwrap();
        if !first.collisions.is_empty() {
            for collision in first.collisions {
                prior.entry(collision.lost).or_insert(collision.winner);
            }
            base = incoming.snapshot.clone();
            incoming.operations(&mut rng, &mut clock, 3);
        }
    }
    let fork = &incoming.snapshot;
    let mut undecided_lines = BTreeSet::new();
    for file in fork.files.values() {
        for line in &file.lines.lines {
            if incoming.events.contains(&line.version.event()) && rng.chance(3, 4) {
                undecided_lines.insert(line.version);
            }
        }
    }
    let mut undecided_names = BTreeSet::new();
    let mut last_lines = BTreeMap::new();
    for (id, name) in &fork.names {
        if incoming.events.contains(&name.head) && rng.chance(3, 4) {
            undecided_names.insert((*id, name.head));
            if old::sync::ended(&upstream.snapshot, *id).unwrap()
                && let Some(lines) = upstream.ended.get(id)
            {
                last_lines.insert(*id, lines.clone());
            }
        }
    }
    Case {
        upstream: upstream.snapshot,
        base,
        incoming: incoming.snapshot,
        prior,
        renamed_over: incoming.renamed_over,
        undecided_lines,
        undecided_names,
        last_lines,
    }
}

/// The bytes `metadata.json` and `namespace.json` are made of.
fn fingerprint(s: &Snapshot) -> String {
    let files: BTreeMap<_, _> = s
        .files
        .iter()
        .map(|(id, file)| (id, (file.object, &file.lines)))
        .collect();
    format!(
        "{}\n{}",
        serde_json::to_string(&files).unwrap(),
        serde_json::to_string(&s.names).unwrap()
    )
}

/// How often each kind of case came up, across all seeds.
#[derive(Debug, Default)]
struct Seen {
    collided: u32,
    clean: u32,
    name: u32,
    line: u32,
    whole: u32,
    rename_overs: u32,
    ended_edits: u32,
    joins: u32,
    syncs_differ: u32,
    fallbacks: u32,
    held: u32,
}

impl Seen {
    fn add(&mut self, case: &Case, collisions: &[Collision], synced: &Snapshot) {
        if collisions.is_empty() {
            self.clean += 1;
        } else {
            self.collided += 1;
        }
        let has = |kind: CollisionKind| collisions.iter().any(|c| c.kind == kind);
        self.name += u32::from(has(CollisionKind::Name));
        self.line += u32::from(has(CollisionKind::Line));
        self.whole += u32::from(has(CollisionKind::Whole));
        self.rename_overs += u32::from(case.renamed_over);
        let ended_edit = case.incoming.files.iter().any(|(id, file)| {
            !case.upstream.files.contains_key(id)
                && case
                    .base
                    .files
                    .get(id)
                    .is_some_and(|b| b.lines != file.lines)
        });
        self.ended_edits += u32::from(ended_edit);
        let join = collisions.iter().any(|c| match (&c.kind, &c.winner) {
            (CollisionKind::Line, Version::Line { file, version }) => {
                case.upstream.files.get(file).is_some_and(|f| {
                    f.lines
                        .lines
                        .iter()
                        .any(|l| l.version == *version && unfinished(l))
                })
            }
            _ => false,
        });
        self.joins += u32::from(join);
        let result = fingerprint(synced);
        self.syncs_differ += u32::from(
            result != fingerprint(&case.upstream) && result != fingerprint(&case.incoming),
        );
        let fallback = case
            .upstream
            .names
            .iter()
            .any(|(id, name)| match synced.names.get(id) {
                None => true,
                Some(kept) => {
                    kept.head != name.head && !case.undecided_names.contains(&(*id, kept.head))
                }
            });
        self.fallbacks += u32::from(fallback);
        let held_line = case.incoming.files.iter().any(|(id, file)| {
            let seen: BTreeSet<_> = case
                .base
                .files
                .get(id)
                .map(|b| b.lines.lines.iter().map(|l| l.version).collect())
                .unwrap_or_default();
            file.lines.lines.iter().any(|line| {
                !seen.contains(&line.version)
                    && case.upstream.files.get(id).is_some_and(|theirs| {
                        theirs.lines.lines.iter().any(|l| l.version == line.version)
                    })
            })
        });
        let held_name = case.incoming.names.iter().any(|(id, name)| {
            case.base.names.get(id).is_none_or(|b| b.head != name.head)
                && case
                    .upstream
                    .names
                    .get(id)
                    .is_some_and(|u| u.head == name.head)
        });
        self.held += u32::from(held_line || held_name);
    }

    fn floors(&self) -> [(&'static str, u32); 11] {
        [
            ("decisions with collisions", self.collided),
            ("decisions without collisions", self.clean),
            ("Name collisions", self.name),
            ("Line collisions", self.line),
            ("Whole collisions", self.whole),
            ("rename-overs", self.rename_overs),
            ("edits of a file the upstream ended", self.ended_edits),
            ("joins", self.joins),
            ("syncs unlike both inputs", self.syncs_differ),
            ("name fallbacks", self.fallbacks),
            ("versions the upstream already holds", self.held),
        ]
    }
}

/// For seeds 0..4096: the referee's landing and sync equal the old rules': Ok on both, with equal
/// fingerprints and equal `Agentfs-Collisions` text, order included. Any `Err` fails: the
/// generator makes valid trees only. Fails unless every kind of case came up at least 50 times.
#[test]
fn land_and_sync_equal_the_oracle() {
    let mut seen = Seen::default();
    for seed in 0..4096 {
        let case = case(seed);
        let old = old::referee::judge(&case.upstream, &case.base, &case.incoming, &case.prior)
            .unwrap_or_else(|e| panic!("seed {seed}: old judge: {e}"));
        let new = referee::land(&case.upstream, &case.base, &case.incoming, &case.prior)
            .unwrap_or_else(|e| panic!("seed {seed}: land: {e}"));
        assert_eq!(
            fingerprint(&new.snapshot),
            fingerprint(&old.snapshot),
            "seed {seed}: landing"
        );
        assert_eq!(
            serde_json::to_string(&new.collisions).unwrap(),
            serde_json::to_string(&old.collisions).unwrap(),
            "seed {seed}: collisions"
        );
        let old_sync = old::sync::sync(
            &case.upstream,
            &case.incoming,
            &case.undecided_lines,
            &case.undecided_names,
            &case.last_lines,
        )
        .unwrap_or_else(|e| panic!("seed {seed}: old sync: {e}"));
        let undecided = Undecided {
            lines: case.undecided_lines.clone(),
            names: case.undecided_names.clone(),
        };
        let new_sync = referee::sync(&case.upstream, &case.incoming, &undecided, &case.last_lines)
            .unwrap_or_else(|e| panic!("seed {seed}: sync: {e}"));
        assert_eq!(
            fingerprint(&new_sync),
            fingerprint(&old_sync),
            "seed {seed}: sync"
        );
        seen.add(&case, &old.collisions, &old_sync);
    }
    eprintln!("{seen:?}");
    for (what, count) in seen.floors() {
        assert!(count >= 50, "only {count} {what} in 4096 seeds: {seen:?}");
    }
}
