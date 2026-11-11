//! Queue and locks: each tree's landing queue (MODEL.md rule 2), the order it keeps per
//! source (P1), the files a take locks and the opens it waits for (rule 9), the prefix a
//! sync may deliver (P2, its [`Cut`]), and the order of calls deferred behind a transaction
//! (R5.3). It reads the other modules through one port ([`Keys`]) and never reads line
//! content. The module-cut page (`docs/target-architecture/future/module-cut.md`) draws its
//! boundary.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use crate::calls::TreeId;
use crate::content::Content;
use crate::recording::HandleId;

/// The port: what queue and locks reads of the other modules. The world implements it.
pub(crate) trait Keys {
    /// A landing's plan, built by the world from the referee's.
    type Plan: Clone;
    /// Tree `t`'s head.
    fn head(&self, t: TreeId) -> String;
    /// The tree `t` syncs from and lands into; none for the root.
    fn upstream_of(&self, t: TreeId) -> Option<TreeId>;
    /// Lock keys of what clients of `t` have open for writing, deferred calls included
    /// (recording; R9.1).
    fn write_open(&self, t: TreeId) -> BTreeSet<String>;
    /// Lock keys commit `c`'s own change touches, against its first parent (R9.1).
    fn touches_of(&self, c: &str) -> BTreeSet<String>;
    /// Lock keys of going from commit `ours` to tree `tree` (R9, R9.1).
    fn changed_keys(&self, ours: &str, tree: &str) -> BTreeSet<String>;
    /// Whether commit `a` is `b` or an ancestor of it.
    fn is_ancestor(&self, a: &str, b: &str) -> bool;
    /// Commit `c`'s first parent.
    fn first_parent(&self, c: &str) -> String;
    /// The tree a sync of upstream commit `up` into a fork at `ours` makes; none if the
    /// fork has `up`.
    fn sync_tree(&self, ours: &str, up: &str) -> Option<String>;
    /// Lock keys delivering upstream commit `c` (its sync's tree `tree`) into a fork at
    /// `ours` writes, finding 38's changed line maps included: the world reads line
    /// content so queue and locks never does.
    fn delivery_keys(&self, ours: &str, c: &str, tree: Option<&str>) -> BTreeSet<String>;
    /// The plan of landing commit `c` on `ours`, and the keys it writes.
    fn plan(&self, ours: &str, c: &str) -> (Self::Plan, BTreeSet<String>);
}

/// A queued change into a tree (rule 2).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Entry {
    /// A child fork's commit, to land in this tree.
    Landing { from: TreeId, commit: String },
    /// The upstream moved: bring its current decided version into this fork
    /// (a sync carries the upstream's current version, never a stale one).
    Sync,
}

/// Where a queue entry comes from: its order is kept per source (P1).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Source {
    /// A sync: the tree's upstream.
    Upstream,
    /// A landing of a child fork's commit.
    Child(TreeId),
}

impl Entry {
    pub fn source(&self) -> Source {
        match self {
            Entry::Landing { from, .. } => Source::Child(*from),
            Entry::Sync => Source::Upstream,
        }
    }
}

/// R9.3: the last upstream commit a sync delivers, and the tree it makes (none: the fork
/// has it). Only this module makes one; the world's sync accepts nothing else.
#[derive(Clone, Debug)]
pub struct Cut {
    up: String,
    tree: Option<String>,
}

impl Cut {
    /// The upstream's commit the sync goes up to.
    pub fn up(&self) -> &str {
        &self.up
    }

    /// The tree the sync makes; none if the fork has [`Cut::up`].
    pub fn tree(&self) -> Option<&str> {
        self.tree.as_deref()
    }
}

/// What a tree's take writes (rule 9).
#[derive(Clone, Debug)]
pub enum Take<P> {
    /// The upstream's commit it delivers and the tree it makes, and whether
    /// a commit of the fork was then on its way to the upstream.
    Sync {
        cut: Cut,
        landing: bool,
        /// The upstream's head the take read: the cut's commit is it, or under P2
        /// the last of its first parents the take may deliver.
        head: String,
    },
    /// A child's commit and the planned merge.
    Landing {
        from: TreeId,
        commit: String,
        plan: P,
    },
}

/// Rule 9: what one take locked in its tree and wrote there, in one step.
#[derive(Clone, Debug)]
pub struct TakeRecord<P> {
    /// The files and names it locked.
    pub files: BTreeSet<String>,
    pub take: Take<P>,
    /// The number of the queue entry it took (none: a prefix sync, whose
    /// entry stays queued for the rest).
    pub seq: Option<u64>,
}

/// Progress rule P1: a queue entry that waits for write-opens of its files
/// to close (rule 9). Entries behind it go ahead unless they share a file
/// or a source with it (git-model.md §6). No call waits behind it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Waiting {
    /// The files its take locks (a sync: every file it may write).
    pub files: BTreeSet<String>,
    /// The files whose writers it waits for: it is woken once none is open
    /// for writing. A landing's are all its `files`; a sync's, the files
    /// the first commit it holds back adds to its write (P2, the prefix
    /// sync).
    pub until: BTreeSet<String>,
}

/// Rule 9, a take's choice in one tree ([`Queues::choose`]).
pub(crate) enum Choice<P> {
    /// The entry waits for opens of its files to close; nothing is taken.
    Wait { seq: u64, waiting: Waiting },
    /// P2: a sync delivers its prefix now; its entry stays queued and waits for the rest.
    Prefix {
        seq: u64,
        waiting: Waiting,
        take: TakeRecord<P>,
    },
    /// The entry at `index` leaves the queue, written this step.
    Take { index: usize, take: TakeRecord<P> },
}

/// What a prefix sync (P2) delivers now: the upstream's commit it goes up
/// to, the tree it makes (none: the fork has it) and the commits it holds
/// back; and the open files whose close it waits for to deliver the rest.
struct Prefix {
    deliver: Option<(String, Option<String>)>,
    until: Option<BTreeSet<String>>,
}

/// A lock key of a name, not a file (R9.1): two files can compete for one name.
pub const NAME_KEY: &str = "name:";

/// The lock key of the name file `c` shows, if it has one.
pub fn name_key(c: &Content) -> Option<String> {
    c.path().map(|p| format!("{NAME_KEY}{p}"))
}

/// Rule 9: a take writes only files it holds; the model stops loudly otherwise.
pub(crate) fn held(t: TreeId, files: &BTreeSet<String>, holds: &BTreeSet<String>) {
    assert!(
        files.is_subset(holds),
        "a take into {t:?} would write {files:?}, holding only {holds:?}"
    );
}

/// Every tree's queue, its waiting entries, and the calls deferred behind each fork's
/// transaction.
#[derive(Default)]
pub(crate) struct Queues {
    /// Each tree's queue: its entries, each numbered when queued.
    queues: BTreeMap<TreeId, VecDeque<(u64, Entry)>>,
    next_seq: u64,
    /// Each tree's queue entries that wait for write-opens of their files
    /// to close (P1: several may wait, each for its own files).
    waits: BTreeMap<TreeId, BTreeMap<u64, Waiting>>,
    /// R5.3: the calls on an open transaction's files, in the order they
    /// were made. Each returned at once; each applies, in this order, after
    /// the commit or rollback.
    deferred: BTreeMap<TreeId, VecDeque<HandleId>>,
}

impl Queues {
    /// Rule 1, both ways: tree `t` moved to `head`. A fork's new commit queues in its
    /// `upstream` when `up`; the new content queues in each child.
    pub(crate) fn moved(
        &mut self,
        t: TreeId,
        head: &str,
        upstream: Option<TreeId>,
        children: &[TreeId],
        up: bool,
    ) {
        if up && let Some(upstream) = upstream {
            self.next_seq += 1;
            let n = self.next_seq;
            self.queues.entry(upstream).or_default().push_back((
                n,
                Entry::Landing {
                    from: t,
                    commit: head.to_string(),
                },
            ));
        }
        for &c in children {
            self.next_seq += 1;
            let n = self.next_seq;
            self.queues
                .entry(c)
                .or_default()
                .push_back((n, Entry::Sync));
        }
    }

    pub(crate) fn len(&self, t: TreeId) -> usize {
        self.queues.get(&t).map_or(0, VecDeque::len)
    }

    pub(crate) fn entries(&self, t: TreeId) -> Vec<Entry> {
        self.queues
            .get(&t)
            .map(|q| q.iter().map(|(_, e)| e.clone()).collect())
            .unwrap_or_default()
    }

    /// The queue with each entry's number.
    pub(crate) fn numbered(&self, t: TreeId) -> Vec<(u64, Entry)> {
        self.queues
            .get(&t)
            .map(|q| q.iter().cloned().collect())
            .unwrap_or_default()
    }

    /// Trees with an entry queued, in [`TreeId`] order.
    pub(crate) fn queued_trees(&self) -> Vec<TreeId> {
        self.queues
            .iter()
            .filter(|(_, q)| !q.is_empty())
            .map(|(t, _)| *t)
            .collect()
    }

    /// Each tree whose queue has waiting entries, with what each waits for.
    pub(crate) fn waits(&self) -> &BTreeMap<TreeId, BTreeMap<u64, Waiting>> {
        &self.waits
    }

    /// The files tree `t`'s waiting entries wait for: those of their files
    /// open for writing there (`open`, its write locks).
    pub(crate) fn waiting_for(&self, t: TreeId, open: &BTreeSet<String>) -> BTreeSet<String> {
        self.waits
            .get(&t)
            .into_iter()
            .flat_map(|m| m.values())
            .flat_map(|x| x.until.iter())
            .filter(|p| open.contains(*p) && !p.starts_with(NAME_KEY))
            .cloned()
            .collect()
    }

    /// R6.1's flag: whether a commit of fork `f` waits in `upstream`'s queue.
    pub(crate) fn landing_pending(&self, upstream: TreeId, f: TreeId) -> bool {
        self.entries(upstream)
            .iter()
            .any(|e| matches!(e, Entry::Landing { from, .. } if *from == f))
    }

    /// P1 (R9.5): the first entry of `t`'s queue that may be taken now.
    pub(crate) fn scan<K: Keys + ?Sized>(&self, t: TreeId, port: &K) -> Option<usize> {
        Locks { q: self, port }.scan(t)
    }

    /// Rule 9: plan the first entry P1 lets go into `t` and the files it locks. Reads only;
    /// [`Queues::apply`] records the choice. Panics if no entry may go.
    pub(crate) fn choose<K: Keys + ?Sized>(&self, t: TreeId, port: &K) -> Choice<K::Plan> {
        Locks { q: self, port }.choose(t)
    }

    /// Records a choice in `t`: its entry waits, or leaves the queue.
    pub(crate) fn apply<P>(&mut self, t: TreeId, choice: &Choice<P>) {
        match choice {
            Choice::Wait { seq, waiting } | Choice::Prefix { seq, waiting, .. } => {
                self.waits
                    .entry(t)
                    .or_default()
                    .insert(*seq, waiting.clone());
            }
            Choice::Take { index, .. } => {
                let mut qd = self.queues.remove(&t).unwrap_or_default();
                qd.remove(*index);
                self.queues.insert(t, qd);
            }
        }
    }

    /// A close or a client's death in `t`: each waiting entry there none of
    /// whose `until` files is `open` stops waiting; the next queue step on
    /// `t` takes it, or finds it must wait again (P1). Nothing waits behind
    /// it meanwhile (P3 is gone). The numbers of the entries woken.
    pub(crate) fn wake(&mut self, t: TreeId, open: &BTreeSet<String>) -> Vec<u64> {
        let Some(m) = self.waits.get_mut(&t) else {
            return Vec::new();
        };
        let woken: Vec<u64> = m
            .iter()
            .filter(|(_, x)| x.until.is_disjoint(open))
            .map(|(n, _)| *n)
            .collect();
        for n in &woken {
            m.remove(n);
        }
        if m.is_empty() {
            self.waits.remove(&t);
        }
        woken
    }

    /// A crash: every wait is dropped, to be found again at the next take.
    pub(crate) fn clear_waits(&mut self) {
        self.waits.clear();
    }

    /// A dropped fork's queue goes.
    pub(crate) fn forget(&mut self, t: TreeId) {
        self.queues.remove(&t);
    }

    /// R5.3: the calls deferred behind fork `f`'s transaction, in the order made.
    pub(crate) fn deferred(&self, f: TreeId) -> impl Iterator<Item = HandleId> + '_ {
        self.deferred.get(&f).into_iter().flatten().copied()
    }

    /// R5.3: call `h` waits behind fork `f`'s transaction, after those before it.
    pub(crate) fn defer(&mut self, f: TreeId, h: HandleId) {
        self.deferred.entry(f).or_default().push_back(h);
    }

    /// R5.3: the next deferred call of fork `f`, the first made first.
    pub(crate) fn next_deferred(&mut self, f: TreeId) -> Option<HandleId> {
        self.deferred.get_mut(&f)?.pop_front()
    }
}

/// The queues read with the port: the rules' bodies call `self.x(..)` for either.
struct Locks<'a, K: Keys + ?Sized> {
    q: &'a Queues,
    port: &'a K,
}

impl<K: Keys + ?Sized> Locks<'_, K> {
    fn head(&self, t: TreeId) -> String {
        self.port.head(t)
    }

    fn upstream_of(&self, t: TreeId) -> Option<TreeId> {
        self.port.upstream_of(t)
    }

    fn write_open(&self, t: TreeId) -> BTreeSet<String> {
        self.port.write_open(t)
    }

    fn touches_of(&self, c: &str) -> BTreeSet<String> {
        self.port.touches_of(c)
    }

    fn changed_keys(&self, ours: &str, tree: &str) -> BTreeSet<String> {
        self.port.changed_keys(ours, tree)
    }

    fn is_ancestor(&self, a: &str, b: &str) -> bool {
        self.port.is_ancestor(a, b)
    }

    fn first_parent(&self, c: &str) -> String {
        self.port.first_parent(c)
    }

    fn sync_tree(&self, ours: &str, up: &str) -> Option<String> {
        self.port.sync_tree(ours, up)
    }

    fn delivery_keys(&self, ours: &str, c: &str, tree: Option<&str>) -> BTreeSet<String> {
        self.port.delivery_keys(ours, c, tree)
    }

    fn plan(&self, ours: &str, c: &str) -> (K::Plan, BTreeSet<String>) {
        self.port.plan(ours, c)
    }

    fn landing_pending(&self, upstream: TreeId, f: TreeId) -> bool {
        self.q.landing_pending(upstream, f)
    }

    /// Progress rule P1 (git-model.md §6): the first entry of `t`'s queue
    /// that may be taken now. An entry waits behind every
    /// earlier waiting entry it shares a file or a source with; so does
    /// every entry behind one that waits behind, so one source's entries
    /// keep their order and no two entries sharing a file pass each other.
    /// Which files an entry not yet planned touches is read from git: a
    /// landing's own changes (each against its first parent), a sync's
    /// every file its tree and the upstream's head differ in (all it may
    /// write).
    fn scan(&self, t: TreeId) -> Option<usize> {
        let waiting = self.q.waits.get(&t);
        let mut files = BTreeSet::new();
        let mut sources = BTreeSet::new();
        for (i, (n, e)) in self.q.queues.get(&t).into_iter().flatten().enumerate() {
            if let Some(x) = waiting.and_then(|m| m.get(n)) {
                files.extend(x.files.iter().cloned());
                sources.insert(e.source());
                continue;
            }
            let mine = self.hint(t, e);
            if sources.contains(&e.source()) || !mine.is_disjoint(&files) {
                files.extend(mine);
                sources.insert(e.source());
                continue;
            }
            return Some(i);
        }
        None
    }

    /// The files queue entry `e` of `t` may touch, before it is planned.
    fn hint(&self, t: TreeId, e: &Entry) -> BTreeSet<String> {
        match e {
            Entry::Landing { commit, .. } => self.touches_of(commit),
            Entry::Sync => match self.upstream_of(t) {
                Some(up) => self.changed_keys(&self.head(t), &self.head(up)),
                None => BTreeSet::new(),
            },
        }
    }

    /// Rule 9, a take's choice: plan the first entry P1 lets go
    /// ([`Locks::scan`]) into `t` and the files it locks there (a landing:
    /// every file its changes touch, [`Locks::touched`]). If a client of `t`
    /// has one of them open for writing, the entry waits for those to close
    /// and nothing is taken; a new open of one of its files is granted (no call
    /// waits on a waiting take, the owner's decision of 2026-11-04). Else it
    /// leaves the queue, and the world writes it at once. A prefix sync (P2)
    /// delivers its prefix and waits for the rest.
    fn choose(&self, t: TreeId) -> Choice<K::Plan> {
        let q = self.q.numbered(t);
        let Some(i) = self.scan(t) else {
            panic!("{t:?}: no entry can be taken")
        };
        let (seq, head) = q[i].clone();
        let ours = self.head(t);
        let (take, files) = match head {
            Entry::Sync => {
                let f = t;
                let Some(upstream) = self.upstream_of(f) else {
                    unreachable!("the root has no upstream")
                };
                let whole = self.head(upstream);
                let landing = self.landing_pending(upstream, f);
                let p = self.prefix(t, &ours, &whole);
                let Some((up, tree)) = p.deliver else {
                    // Its first commit would overwrite an open file: it
                    // waits for those to close, taking nothing.
                    let until = p.until.expect("a sync with nothing to deliver waits");
                    let mut files = self.hint(t, &Entry::Sync);
                    files.extend(until.iter().cloned());
                    return Choice::Wait {
                        seq,
                        waiting: Waiting { files, until },
                    };
                };
                let files = tree
                    .as_ref()
                    .map(|tr| self.changed_keys(&ours, tr))
                    .unwrap_or_default();
                let cut = Cut { up, tree };
                if let Some(until) = p.until {
                    // P2: it delivers the prefix now; the entry stays queued
                    // and waits for the rest.
                    let mut all = self.hint(t, &Entry::Sync);
                    all.extend(until.iter().cloned());
                    all.extend(files.iter().cloned());
                    return Choice::Prefix {
                        seq,
                        waiting: Waiting { files: all, until },
                        take: TakeRecord {
                            files,
                            take: Take::Sync {
                                cut,
                                landing,
                                head: whole,
                            },
                            seq: None,
                        },
                    };
                }
                (
                    Take::Sync {
                        cut,
                        landing,
                        head: whole,
                    },
                    files,
                )
            }
            Entry::Landing { from, commit } => {
                let (plan, written) = self.plan(&ours, &commit);
                let files = self.touched(&commit, written);
                (Take::Landing { from, commit, plan }, files)
            }
        };
        let open: BTreeSet<String> = self.write_open(t).intersection(&files).cloned().collect();
        if !open.is_empty() {
            // A landing whose files are open waits for them to close; a
            // sync never gets here (`prefix` delivers none of an open file).
            return Choice::Wait {
                seq,
                waiting: Waiting {
                    until: files.clone(),
                    files,
                },
            };
        }
        Choice::Take {
            index: i,
            take: TakeRecord {
                files,
                take,
                seq: Some(seq),
            },
        }
    }

    /// Rule 9, the files a landing locks: every file its change touches (read
    /// from git, against its first parent), whether the plan says it wins or
    /// loses, and every file the plan writes (`written`, R9.1). So a landing waits
    /// for an open writer of a file its change touches even when that change loses.
    fn touched(&self, c: &str, written: BTreeSet<String>) -> BTreeSet<String> {
        let mut files = written;
        files.extend(self.touches_of(c));
        files
    }

    /// Progress rule P2, the prefix sync (the owner, 2026-11-04: "agreed",
    /// on "a sync delivers the parent's changes in order up to the first
    /// change that touches a file the sandbox has open for writing"): the
    /// upstream's commits fork `t` (at `ours`) lacks are `whole`'s first
    /// parents back to the first the fork has, each one change as the
    /// upstream took it (a save, a transaction's commit, a landing). It delivers them
    /// in order up to, not including, the first that changes the line map of
    /// a file open for writing in `t`, even where the fork's file would
    /// read the same because it keeps its own undecided line there (finding
    /// 38; the owner, 2026-11-07, "3. B"), and holds back that one and every
    /// later one, the decisions they carry included, until those files close.
    fn prefix(&self, t: TreeId, ours: &str, whole: &str) -> Prefix {
        let open = self.write_open(t);
        let mut chain = Vec::new();
        let mut c = whole.to_string();
        while !self.is_ancestor(&c, ours) {
            chain.push(c.clone());
            c = self.first_parent(&c);
        }
        chain.reverse();
        if open.is_empty() || chain.is_empty() {
            return Prefix {
                deliver: Some((whole.to_string(), self.sync_tree(ours, whole))),
                until: None,
            };
        }
        let mut deliver = None;
        let mut before = BTreeSet::new();
        for c in &chain {
            let tree = self.sync_tree(ours, c);
            let mut now = self.delivery_keys(ours, c, tree.as_deref());
            now.extend(before.iter().cloned());
            if !now.is_disjoint(&open) {
                // It waits until no file this commit adds to the sync's
                // write is open (its own change's files, as for a landing).
                return Prefix {
                    deliver,
                    until: Some(now.difference(&before).cloned().collect()),
                };
            }
            deliver = Some((c.clone(), tree));
            before = now;
        }
        Prefix {
            deliver,
            until: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ROOT: TreeId = TreeId::ROOT;
    const T1: TreeId = TreeId::new(1);
    const T2: TreeId = TreeId::new(2);
    const T3: TreeId = TreeId::new(3);

    /// Commits by name, each with its first parent and the keys its own change touches; a
    /// tree is named by the commit it is. No git, and no line content.
    #[derive(Default)]
    struct Fake {
        heads: BTreeMap<TreeId, String>,
        upstream: BTreeMap<TreeId, TreeId>,
        open: BTreeMap<TreeId, BTreeSet<String>>,
        parent: BTreeMap<String, String>,
        touches: BTreeMap<String, BTreeSet<String>>,
    }

    impl Fake {
        fn commit(&mut self, c: &str, parent: &str, keys: &[&str]) {
            self.parent.insert(c.into(), parent.into());
            self.touches
                .insert(c.into(), keys.iter().map(|k| k.to_string()).collect());
        }
    }

    impl Keys for Fake {
        type Plan = ();

        fn head(&self, t: TreeId) -> String {
            self.heads[&t].clone()
        }

        fn upstream_of(&self, t: TreeId) -> Option<TreeId> {
            self.upstream.get(&t).copied()
        }

        fn write_open(&self, t: TreeId) -> BTreeSet<String> {
            self.open.get(&t).cloned().unwrap_or_default()
        }

        fn touches_of(&self, c: &str) -> BTreeSet<String> {
            self.touches.get(c).cloned().unwrap_or_default()
        }

        fn changed_keys(&self, ours: &str, tree: &str) -> BTreeSet<String> {
            let mut keys = BTreeSet::new();
            let mut c = tree.to_string();
            while !self.is_ancestor(&c, ours) {
                keys.extend(self.touches_of(&c));
                c = self.first_parent(&c);
            }
            keys
        }

        fn is_ancestor(&self, a: &str, b: &str) -> bool {
            let mut c = Some(b.to_string());
            while let Some(x) = c {
                if x == a {
                    return true;
                }
                c = self.parent.get(&x).cloned();
            }
            false
        }

        fn first_parent(&self, c: &str) -> String {
            self.parent[c].clone()
        }

        fn sync_tree(&self, ours: &str, up: &str) -> Option<String> {
            (!self.is_ancestor(up, ours)).then(|| up.to_string())
        }

        fn delivery_keys(&self, _ours: &str, c: &str, _tree: Option<&str>) -> BTreeSet<String> {
            self.touches_of(c)
        }

        fn plan(&self, _ours: &str, c: &str) -> ((), BTreeSet<String>) {
            ((), self.touches_of(c))
        }
    }

    fn keys(k: &[&str]) -> BTreeSet<String> {
        k.iter().map(|k| k.to_string()).collect()
    }

    #[test]
    fn a_move_queues_a_landing_upstream_and_a_sync_in_each_child() {
        let mut q = Queues::default();
        q.moved(T1, "c", Some(ROOT), &[T2, T3], true);
        q.moved(T1, "d", Some(ROOT), &[], false);
        assert_eq!(
            q.numbered(ROOT),
            [(
                1,
                Entry::Landing {
                    from: T1,
                    commit: "c".into()
                }
            )]
        );
        assert_eq!(q.numbered(T2), [(2, Entry::Sync)]);
        assert_eq!(q.numbered(T3), [(3, Entry::Sync)]);
        assert_eq!(q.queued_trees(), [ROOT, T2, T3]);
        assert!(q.landing_pending(ROOT, T1));
        assert!(!q.landing_pending(ROOT, T2));
    }

    /// The root's queue: landings of `a` from T1 and T2, then of `b` from T3.
    fn three_landings() -> (Queues, Fake) {
        let mut k = Fake::default();
        k.heads.insert(ROOT, "base".into());
        for (c, t, file) in [("c1", T1, "a"), ("c2", T2, "a"), ("c3", T3, "b")] {
            k.commit(c, "base", &[file]);
            k.upstream.insert(t, ROOT);
        }
        let mut q = Queues::default();
        for (c, t) in [("c1", T1), ("c2", T2), ("c3", T3)] {
            q.moved(t, c, Some(ROOT), &[], true);
        }
        (q, k)
    }

    #[test]
    fn an_entry_waits_behind_a_waiting_entry_of_its_file_and_another_goes() {
        let (mut q, mut k) = three_landings();
        assert_eq!(q.scan(ROOT, &k), Some(0));
        k.open.insert(ROOT, keys(&["a"]));
        let choice = q.choose(ROOT, &k);
        let Choice::Wait { seq: 1, waiting } = &choice else {
            panic!("the first landing waits for its open file")
        };
        assert_eq!(waiting.until, keys(&["a"]));
        q.apply(ROOT, &choice);
        assert_eq!(q.scan(ROOT, &k), Some(2), "T2's landing of a waits too");
        assert_eq!(q.waiting_for(ROOT, &keys(&["a"])), keys(&["a"]));
        assert_eq!(q.wake(ROOT, &keys(&["a"])), Vec::<u64>::new());
        assert_eq!(q.wake(ROOT, &BTreeSet::new()), [1]);
        assert!(q.waits().is_empty());
        let choice = q.choose(ROOT, &k);
        assert!(matches!(choice, Choice::Wait { seq: 1, .. }));
        k.open.clear();
        let Choice::Take { index: 0, take } = q.choose(ROOT, &k) else {
            panic!("it goes once its file closes")
        };
        assert_eq!(take.seq, Some(1));
        assert_eq!(take.files, keys(&["a"]));
    }

    /// Fork T1 at `base`; its upstream, the root, at `u3`: `u1` touches a, `u2` b, `u3` c.
    fn a_fork_behind() -> (Queues, Fake) {
        let mut k = Fake::default();
        k.heads.insert(ROOT, "u3".into());
        k.heads.insert(T1, "base".into());
        k.upstream.insert(T1, ROOT);
        k.commit("u1", "base", &["a"]);
        k.commit("u2", "u1", &["b"]);
        k.commit("u3", "u2", &["c"]);
        let mut q = Queues::default();
        q.moved(ROOT, "u3", None, &[T1], true);
        (q, k)
    }

    #[test]
    fn a_sync_cuts_before_the_first_commit_of_an_open_file() {
        let (mut q, mut k) = a_fork_behind();
        k.open.insert(T1, keys(&["b"]));
        let choice = q.choose(T1, &k);
        let Choice::Prefix { waiting, take, .. } = &choice else {
            panic!("a prefix sync")
        };
        let Take::Sync { cut, head, .. } = &take.take else {
            panic!("a sync")
        };
        assert_eq!(
            (cut.up(), cut.tree(), head.as_str()),
            ("u1", Some("u1"), "u3")
        );
        assert_eq!(take.files, keys(&["a"]));
        assert_eq!(waiting.until, keys(&["b"]));
        q.apply(T1, &choice);
        assert_eq!(q.len(T1), 1, "the entry stays for the rest");
    }

    #[test]
    fn a_sync_delivers_all_or_waits_for_its_first_commit() {
        let (q, mut k) = a_fork_behind();
        let Choice::Take { take, .. } = q.choose(T1, &k) else {
            panic!("nothing open: all of it")
        };
        let Take::Sync { cut, .. } = &take.take else {
            panic!("a sync")
        };
        assert_eq!(cut.up(), "u3");
        k.open.insert(T1, keys(&["a"]));
        let Choice::Wait { waiting, .. } = q.choose(T1, &k) else {
            panic!("its first commit touches an open file")
        };
        assert_eq!(waiting.until, keys(&["a"]));
    }

    #[test]
    fn deferred_calls_keep_their_order() {
        let mut q = Queues::default();
        q.defer(T1, 5);
        q.defer(T1, 7);
        q.defer(T2, 6);
        assert_eq!(q.deferred(T1).collect::<Vec<_>>(), [5, 7]);
        assert_eq!(q.next_deferred(T1), Some(5));
        assert_eq!(q.next_deferred(T1), Some(7));
        assert_eq!(q.next_deferred(T1), None);
        assert_eq!(q.next_deferred(T3), None);
    }
}
