//! What must hold in the real system, whatever the model said (git-model.md
//! §15, §18). Read from `git`, the crate's own reader of each commit's line
//! maps and name histories ([`crate::git_history`]), and a byte oracle of its
//! own: never the
//! library's bookkeeping. `check::save_aligned` is shared with the model, over the
//! model's `Content`. What a replay compares (each tree's files, the collisions, a
//! take's head) is the replay's, not this file's (`agentfs_model::calls::replay`,
//! [`crate::git_history`]).

use crate::engine_names::EngineNames;
use crate::git_history::{Commit, Git, Version, content, decisions, descends};
use agentfs_model::calls::{Caller, Recorded, RecordedCall, Took, TreeCalls, TreeId};
use agentfs_model::check::save_aligned;
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

/// The harness's own record of a tree: names and bytes, by Linux's rules.
#[derive(Clone, Debug, Default)]
struct OracleTree {
    names: BTreeMap<String, u64>,
    bytes: BTreeMap<u64, Vec<u8>>,
    /// Each path's file identity at the tree's head after the last call: how
    /// an open follows its file through a take, by Linux's rules and the
    /// engine's (git-model §7): a file that stays keeps its object, and one a
    /// take brings is on an object no open holds.
    identities: BTreeMap<String, u64>,
}

#[derive(Clone, Debug)]
enum OpenCall {
    At(u64, Vec<u8>),
    End(Vec<u8>),
    Size(u64),
}

#[derive(Clone, Debug)]
struct OracleOpen {
    tree: TreeId,
    object: u64,
    caller: Caller,
    calls: Vec<OpenCall>,
}

fn apply(bytes: &mut Vec<u8>, calls: &[OpenCall]) {
    for c in calls {
        match c {
            OpenCall::At(offset, data) => {
                let end = *offset as usize + data.len();
                if bytes.len() < end {
                    bytes.resize(end, 0);
                }
                bytes[*offset as usize..end].copy_from_slice(data);
            }
            OpenCall::End(data) => bytes.extend_from_slice(data),
            OpenCall::Size(n) => bytes.resize(*n as usize, 0),
        }
    }
}

/// The real system's invariants after every call of a replay.
pub struct Invariants {
    git: Git,
    /// The engine's name of each tree.
    names: EngineNames,
    trees: BTreeMap<TreeId, OracleTree>,
    opens: BTreeMap<u64, OracleOpen>,
    next_object: u64,
    commits: BTreeMap<String, Commit>,
    opens_saved: BTreeMap<String, String>,
    refs: String,
}

impl Invariants {
    /// Over the bare repository at `repo`, its trees named by `names`, before the
    /// trace's first call.
    pub fn new(repo: &Path, names: EngineNames) -> Result<Self, String> {
        let mut me = Self {
            git: Git::new(repo)?,
            names,
            trees: BTreeMap::new(),
            opens: BTreeMap::new(),
            next_object: 0,
            commits: BTreeMap::new(),
            opens_saved: BTreeMap::new(),
            refs: String::new(),
        };
        me.trees.insert(TreeId::ROOT, OracleTree::default());
        me.reload(TreeId::ROOT)?;
        Ok(me)
    }

    fn head(&mut self, t: TreeId) -> Result<Commit, String> {
        let branch = self.names.branch(t).map_err(|e| e.to_string())?;
        let oid = self.git.run(&["rev-parse", &branch])?;
        self.commit(oid.trim())
    }

    fn commit(&mut self, oid: &str) -> Result<Commit, String> {
        if let Some(c) = self.commits.get(oid) {
            return Ok(c.clone());
        }
        let c = self.git.commit(oid)?;
        self.commits.insert(oid.to_owned(), c.clone());
        Ok(c)
    }

    /// The oracle takes tree `t` from git: only a take changes a tree by rules
    /// this oracle does not know. A file whose identity the tree held keeps
    /// this oracle's object; any other gets a new one.
    fn reload(&mut self, t: TreeId) -> Result<(), String> {
        let head = self.head(t)?;
        let tree = self.trees.entry(t).or_default();
        let held: BTreeMap<u64, u64> = tree
            .names
            .iter()
            .filter_map(|(path, own)| Some((*tree.identities.get(path)?, *own)))
            .collect();
        tree.names.clear();
        for map in head.maps.values() {
            let own = match held.get(&map.identity) {
                Some(own) => *own,
                None => {
                    self.next_object += 1;
                    self.next_object
                }
            };
            tree.names.insert(map.path.clone(), own);
            tree.bytes
                .insert(own, head.files.get(&map.path).cloned().unwrap_or_default());
        }
        tree.identities = Self::identities(&head);
        Ok(())
    }

    /// Each path's file identity in `head`.
    fn identities(head: &Commit) -> BTreeMap<String, u64> {
        head.maps
            .values()
            .map(|map| (map.path.clone(), map.identity))
            .collect()
    }

    fn close_in_oracle(&mut self, h: u64) -> Result<(), String> {
        let Some(open) = self.opens.remove(&h) else {
            return Ok(());
        };
        let tree = self.trees.entry(open.tree).or_default();
        if tree.names.values().any(|o| *o == open.object) {
            let bytes = tree.bytes.entry(open.object).or_default();
            apply(bytes, &open.calls);
        }
        Ok(())
    }

    fn tree_of(c: &Caller) -> TreeId {
        match c {
            Caller::Root(_) => TreeId::ROOT,
            Caller::Fork(f) => *f,
        }
    }

    fn opened(
        &mut self,
        c: &Caller,
        path: &str,
        h: u64,
        first: Vec<OpenCall>,
    ) -> Result<(), String> {
        let tree = Self::tree_of(c);
        let object = *self
            .trees
            .entry(tree)
            .or_default()
            .names
            .get(path)
            .ok_or_else(|| format!("the oracle has no {path} in {tree:?}"))?;
        self.opens.insert(
            h,
            OracleOpen {
                tree,
                object,
                caller: c.clone(),
                calls: first,
            },
        );
        Ok(())
    }

    fn note_call(&mut self, r: &Recorded) -> Result<(), String> {
        match &r.call {
            RecordedCall::Fork { id } => {
                let listed = self.git.run(&EngineNames::LIST_FORKS)?;
                self.names.read_forks(&listed).map_err(|e| e.to_string())?;
                // The system's names, made in order, agree with these, read from
                // git, only if git holds exactly the forks the trace made.
                if self.names.forks() != id.number() as usize {
                    return Err(format!(
                        "fork_names: git lists {} forks after the trace's fork {id}",
                        self.names.forks()
                    ));
                }
                let root = self.trees.get(&TreeId::ROOT).cloned().unwrap_or_default();
                self.trees.insert(*id, root);
            }
            RecordedCall::OpenWrite { caller, path, h } => {
                self.opened(caller, path, *h, Vec::new())?;
            }
            RecordedCall::OpenTrunc { caller, path, h } => {
                self.opened(caller, path, *h, vec![OpenCall::Size(0)])?;
            }
            RecordedCall::Create { caller, path, h } => {
                let tree = Self::tree_of(caller);
                self.next_object += 1;
                let own = self.next_object;
                let t = self.trees.entry(tree).or_default();
                t.names.insert(path.clone(), own);
                t.bytes.insert(own, Vec::new());
                self.opened(caller, path, *h, Vec::new())?;
            }
            RecordedCall::OpenGone { .. } => {}
            RecordedCall::Pwrite { h, offset, bytes } => {
                self.push(*h, OpenCall::At(*offset, bytes.clone()));
            }
            RecordedCall::Append { h, bytes } => self.push(*h, OpenCall::End(bytes.clone())),
            RecordedCall::Truncate { h, size } => self.push(*h, OpenCall::Size(*size)),
            RecordedCall::Close { h } => self.close_in_oracle(*h)?,
            RecordedCall::Unlink { caller, path } => {
                self.trees
                    .entry(Self::tree_of(caller))
                    .or_default()
                    .names
                    .remove(path);
            }
            RecordedCall::Rename { caller, from, to } => {
                let t = self.trees.entry(Self::tree_of(caller)).or_default();
                if let Some(own) = t.names.remove(from) {
                    t.names.insert(to.clone(), own);
                }
            }
            RecordedCall::EndClient { caller } => {
                let mine: Vec<u64> = self
                    .opens
                    .iter()
                    .filter(|(_, o)| o.caller == *caller)
                    .map(|(h, _)| *h)
                    .collect();
                for h in mine {
                    self.close_in_oracle(h)?;
                }
            }
            RecordedCall::Take { tree, took } => {
                if *took == Took::Taken {
                    self.reload(*tree)?;
                }
            }
            RecordedCall::Crash => {
                let all: Vec<u64> = self.opens.keys().copied().collect();
                for h in all {
                    self.close_in_oracle(h)?;
                }
            }
            RecordedCall::OutOfScope(why) => return Err(format!("out of scope: {why}")),
        }
        Ok(())
    }

    fn push(&mut self, h: u64, call: OpenCall) {
        if let Some(open) = self.opens.get_mut(&h) {
            open.calls.push(call);
        }
    }

    /// The commits no earlier check saw: each invariant of a single commit.
    fn check_new_commits(&mut self) -> Result<(), String> {
        let all = self.git.run(
            &[
                crate::git_history::FILE_COMMITS.as_slice(),
                &["--reverse", "--topo-order"],
            ]
            .concat(),
        )?;
        for oid in all.lines() {
            if self.commits.contains_key(oid) && self.checked(oid) {
                continue;
            }
            let c = self.commit(oid)?;
            one_writer(oid, &c, self)?;
            landing_names_its_writer(oid, &c, self)?;
            self.no_duplicate_opens(oid, &c)?;
            self.save_aligned(oid, &c)?;
            self.landing(oid, &c)?;
            self.mark_checked(oid);
        }
        Ok(())
    }

    fn checked(&self, oid: &str) -> bool {
        self.opens_saved.contains_key(&format!("checked {oid}"))
    }

    fn mark_checked(&mut self, oid: &str) {
        self.opens_saved
            .insert(format!("checked {oid}"), String::new());
    }

    /// R12: no open is saved twice, a crash's replay included.
    fn no_duplicate_opens(&mut self, oid: &str, c: &Commit) -> Result<(), String> {
        for open in c.trailers("Agentfs-Open") {
            if let Some(other) = self.opens_saved.insert(open.to_owned(), oid.to_owned()) {
                return Err(format!(
                    "no_duplicate_opens: open {open} saved by {other:.7} and {oid:.7}"
                ));
            }
        }
        Ok(())
    }

    /// Rule 10, from the two line maps alone.
    fn save_aligned(&mut self, oid: &str, c: &Commit) -> Result<(), String> {
        if c.trailers("Agentfs-Cause") != ["Direct"] || c.parents.len() != 1 {
            return Ok(());
        }
        let parent = self.commit(&c.parents[0])?;
        for file in c.maps.keys() {
            if let (Some(base), Some(now)) = (content(&parent, *file), content(c, *file)) {
                save_aligned(&base, &now)
                    .map_err(|e| format!("save_aligned: {oid:.7} file {file}: {e}"))?;
            }
        }
        Ok(())
    }

    /// R3e, R3b, R7: a landing, read from the collisions it records and its two
    /// parents.
    fn landing(&mut self, oid: &str, c: &Commit) -> Result<(), String> {
        if c.trailers("Agentfs-Cause") != ["Landing"] {
            return Ok(());
        }
        let [before, incoming] = c.parents.as_slice() else {
            return Err(format!("{oid:.7}: a landing without two parents"));
        };
        let (before, incoming_id) = (self.commit(before)?, incoming.clone());
        let incoming = self.commit(&incoming_id)?;
        let base = self.commit(incoming.parents.first().ok_or("a landing of a root")?)?;
        let lost = decisions(c)?;
        let tree = c.trailers("Agentfs-Tree").join("");
        // What the incoming change itself wrote: versions its base lacks.
        let mut written = Vec::new();
        for (file, map) in &incoming.maps {
            for (version, identity, _) in &map.lines {
                if !base.holds_line(*file, version) {
                    written.push((*file, version.clone(), identity.clone()));
                }
            }
        }
        if lost.is_empty() {
            for (file, version, identity) in &written {
                let kept = c.maps.get(file).is_some_and(|m| {
                    m.lines
                        .iter()
                        .any(|l| l.1 == *identity && descends(m, &l.0, version))
                });
                if !kept {
                    return Err(format!(
                        "changes_whole: {oid:.7} won but lacks {version} of file {file}: a partial change"
                    ));
                }
            }
            return Ok(());
        }
        if c.files != before.files {
            return Err(format!(
                "changes_whole: {oid:.7} lost but its files are not the parent's: a partial change"
            ));
        }
        let names: BTreeSet<&Version> = lost.iter().map(|d| &d.lost).collect();
        for (file, version, identity) in &written {
            let line = Version::Line {
                file: *file,
                version: version.clone(),
            };
            let held_before = before.maps.get(file).is_some_and(|m| {
                m.lines
                    .iter()
                    .any(|l| l.1 == *identity && descends(m, &l.0, version))
            });
            if !names.contains(&line) && !held_before {
                return Err(format!(
                    "changes_whole: {oid:.7} lost, no collision names {version} of file {file}"
                ));
            }
        }
        for d in &lost {
            if c.holds(&d.lost) {
                return Err(format!(
                    "upstream_kept: {oid:.7} still holds lost {:?}",
                    d.lost
                ));
            }
            if !c.holds(&d.winner) && !d.whole {
                return Err(format!(
                    "upstream_kept: {oid:.7} lacks the winner {:?}",
                    d.winner
                ));
            }
            if !incoming.holds(&d.lost) {
                return Err(format!(
                    "loser_reachable: {:?} is not in the losing commit {incoming_id:.7}",
                    d.lost
                ));
            }
        }
        let reference = format!("refs/collisions/{tree}/{incoming_id}");
        let at = self.git.run(&["rev-parse", &reference])?;
        if at.trim() != incoming_id {
            return Err(format!("loser_reachable: {reference} names {}", at.trim()));
        }
        Ok(())
    }

    /// Only a file's last live line may lack a newline (R10.4, R10.6, R10.8), and
    /// a commit's files are its line maps' bytes.
    fn lines_whole(t: TreeId, c: &Commit) -> Result<(), String> {
        for map in c.maps.values() {
            let live: Vec<&Vec<u8>> = map.lines.iter().filter_map(|l| l.2.as_ref()).collect();
            if let Some(n) = live.iter().rev().skip(1).find(|b| b.last() != Some(&b'\n')) {
                return Err(format!(
                    "lines_whole: {t:?}:{} holds {:?} before another line",
                    map.path,
                    String::from_utf8_lossy(n)
                ));
            }
            let joined: Vec<u8> = live.into_iter().flatten().copied().collect();
            if c.files.get(&map.path).map(Vec::as_slice) != Some(joined.as_slice()) {
                return Err(format!(
                    "lines_whole: {t:?}:{}: the line map's bytes are not the file's",
                    map.path
                ));
            }
        }
        Ok(())
    }

    fn bytes_match<T: TreeCalls>(
        &mut self,
        t: &T,
        id: TreeId,
        head: &Commit,
    ) -> Result<(), String> {
        let tree = self.trees.entry(id).or_default();
        let oracle: BTreeMap<String, Vec<u8>> = tree
            .names
            .iter()
            .map(|(p, o)| (p.clone(), tree.bytes.get(o).cloned().unwrap_or_default()))
            .collect();
        let mut view = BTreeMap::new();
        for p in t.paths(id).map_err(|e| e.to_string())? {
            let b = t.read(id, &p).map_err(|e| e.to_string())?;
            view.insert(p, b.unwrap_or_default());
        }
        if view != oracle {
            return Err(format!(
                "bytes_match_harness: {id:?} shows {}, the oracle {}",
                print(&view),
                print(&oracle)
            ));
        }
        if head.files != view {
            return Err(format!(
                "commit_files_match_view: {id:?} shows {}, its commit {}",
                print(&view),
                print(&head.files)
            ));
        }
        Ok(())
    }

    /// Queues empty, every tree holds the root's files, and every save of every tree
    /// is in the root's history (R7).
    fn drained<T: TreeCalls>(&mut self, t: &T) -> Result<(), String> {
        let queued = self
            .git
            .run(&["for-each-ref", "--format=%(refname)", "refs/queue/"])?;
        if !queued.trim().is_empty() {
            return Ok(());
        }
        let root_paths = t.paths(TreeId::ROOT).map_err(|e| e.to_string())?;
        let ids: Vec<TreeId> = self.trees.keys().copied().collect();
        for id in ids {
            let paths = t.paths(id).map_err(|e| e.to_string())?;
            if paths != root_paths {
                return Err(format!(
                    "converged: {id:?} has {paths:?}, the root {root_paths:?}"
                ));
            }
            for p in &paths {
                let (a, b) = (
                    t.read(id, p).map_err(|e| e.to_string())?,
                    t.read(TreeId::ROOT, p).map_err(|e| e.to_string())?,
                );
                if a != b {
                    return Err(format!("converged: {id:?}:{p} differs from the root's"));
                }
            }
        }
        let reached: BTreeSet<String> = self
            .git
            .run(&[
                "rev-list",
                &self.names.branch(TreeId::ROOT).map_err(|e| e.to_string())?,
            ])?
            .lines()
            .map(str::to_owned)
            .collect();
        for (oid, c) in &self.commits {
            if c.trailers("Agentfs-Cause") == ["Direct"] && !reached.contains(oid) {
                return Err(format!(
                    "full_lineage: {oid:.7} is not in the root's history"
                ));
            }
        }
        Ok(())
    }

    /// One object, one file: no two names of a tree share an object (the
    /// real system's numbering, never history's, which names none).
    fn one_object_one_file<T: crate::Objects>(t: &T, id: TreeId) -> Result<(), String> {
        let mut held: BTreeMap<u64, String> = BTreeMap::new();
        for (path, object) in t.objects(id).map_err(|e| e.to_string())? {
            if let Some(other) = held.insert(object, path.clone()) {
                return Err(format!(
                    "one_object_one_file: {id:?}'s {other} and {path} are one object {object}"
                ));
            }
        }
        Ok(())
    }

    /// After one recorded call. A failed call changed nothing this oracle follows.
    pub fn after<T: TreeCalls + crate::Objects>(
        &mut self,
        t: &T,
        r: &Recorded,
    ) -> Result<(), String> {
        if r.failed.is_none() {
            self.note_call(r)?;
        }
        let refs = self.git.run(&["for-each-ref"])?;
        if refs != self.refs {
            self.refs = refs;
            self.check_new_commits()?;
        }
        let ids: Vec<TreeId> = self.trees.keys().copied().collect();
        for id in ids {
            let head = self.head(id)?;
            Self::lines_whole(id, &head)?;
            self.bytes_match(t, id, &head)?;
            Self::one_object_one_file(t, id)?;
            if let Some(tree) = self.trees.get_mut(&id) {
                tree.identities = Self::identities(&head);
            }
        }
        if matches!(r.call, RecordedCall::Take { .. } | RecordedCall::Crash) {
            self.drained(t)?;
        }
        Ok(())
    }
}

/// The writer trailers a landing merge copies from the commit it lands.
const WRITER_TRAILERS: [&str; 5] = [
    "Agentfs-Writer",
    "Agentfs-Namespace",
    "Agentfs-Pod",
    "Agentfs-Pod-Uid",
    "Agentfs-Node",
];

/// git-model §3: a landing merge names the writer whose save it lands. Its writer and
/// pod trailers are its second parent's, each one alike.
fn landing_names_its_writer(oid: &str, c: &Commit, inv: &mut Invariants) -> Result<(), String> {
    if c.trailers("Agentfs-Cause") != ["Landing"] {
        return Ok(());
    }
    let [_, landed] = c.parents.as_slice() else {
        return Err(format!("landing_names_its_writer: {oid:.7} is no merge"));
    };
    let landed = inv.commit(landed)?;
    for key in WRITER_TRAILERS {
        if c.trailers(key) != landed.trailers(key) {
            return Err(format!(
                "landing_names_its_writer: {oid:.7} has {key} {:?}, the landed commit {:?}",
                c.trailers(key),
                landed.trailers(key)
            ));
        }
    }
    Ok(())
}

/// R6: every commit names one writer, and a merge originates nothing: it holds only
/// lines one of its parents holds. One exception: a sync (R11.5, the owner's "A" of
/// 2026-11-08, finding 48) may hold a line of the upstream's last version of a file the
/// upstream has ended, because a fork keeping that file shows the upstream's last edit
/// and an upstream that ended the file stores no map of it. Nothing wider.
fn one_writer(oid: &str, c: &Commit, inv: &mut Invariants) -> Result<(), String> {
    let writers = c.trailers("Agentfs-Writer");
    if writers.len() != 1 {
        return Err(format!("one_writer: {oid:.7} names {writers:?}"));
    }
    if c.parents.len() < 2 {
        return Ok(());
    }
    let parents: Vec<Commit> = c
        .parents
        .clone()
        .iter()
        .map(|p| inv.commit(p))
        .collect::<Result<_, _>>()?;
    if merge_originates(c, &parents, &BTreeMap::new()).is_none() {
        return Ok(());
    }
    let mut last = BTreeMap::new();
    if let ([_, upstream], ["Sync"]) = (parents.as_slice(), c.trailers("Agentfs-Cause").as_slice())
    {
        let ended: BTreeSet<u64> = c
            .maps
            .keys()
            .filter(|file| !upstream.maps.contains_key(*file))
            .copied()
            .collect();
        if !ended.is_empty() {
            // The upstream parent's first parents are that tree's own history.
            for commit in inv
                .git
                .run(&["rev-list", "--first-parent", &c.parents[1]])?
                .lines()
            {
                let commit = inv.commit(commit)?;
                for file in &ended {
                    if !last.contains_key(file) && commit.maps.contains_key(file) {
                        last.insert(*file, commit.clone());
                    }
                }
            }
        }
    }
    match merge_originates(c, &parents, &last) {
        Some((file, version)) => Err(format!(
            "one_writer: merge {oid:.7} originates {version} of file {file}"
        )),
        None => Ok(()),
    }
}

/// The first line version merge `c` holds that none of its `parents` holds and that
/// `last`, the upstream's last version of each file it ended, does not hold either.
fn merge_originates(
    c: &Commit,
    parents: &[Commit],
    last: &BTreeMap<u64, Commit>,
) -> Option<(u64, String)> {
    c.maps
        .iter()
        .flat_map(|(file, map)| map.lines.iter().map(move |l| (*file, l.0.clone())))
        .find(|(file, version)| {
            !parents.iter().any(|p| p.holds_line(*file, version))
                && !last.get(file).is_some_and(|l| l.holds_line(*file, version))
        })
}

fn print(files: &BTreeMap<String, Vec<u8>>) -> String {
    let shown: Vec<String> = files
        .iter()
        .map(|(p, b)| format!("{p}={:?}", String::from_utf8_lossy(b)))
        .collect();
    shown.join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::EngineNames;
    use crate::git_history::tests::Repo;
    use agentfs_model::calls::CallError;

    /// The root's engine name.
    fn root() -> String {
        EngineNames::new().name(TreeId::ROOT).unwrap().to_string()
    }

    /// [`Invariants`] over `repo` with a root commit on the root's branch.
    fn invariants(repo: &Repo) -> Invariants {
        let root = repo.commit(&[], "Root", Some(&[]));
        let branch = EngineNames::new().branch(TreeId::ROOT).unwrap();
        repo.git(&["update-ref", &branch, &root], b"");
        Invariants::new(repo.path(), EngineNames::new()).unwrap()
    }
    /// The real `one_writer` on `merge`.
    fn check(inv: &mut Invariants, merge: &str) -> Result<(), String> {
        let c = inv.commit(merge).unwrap();
        one_writer(merge, &c, inv)
    }

    /// Upstream: a root holding `9:0`, an edit replacing it with `2:0`, an unlink.
    /// The fork made its own line on the root and never held either.
    fn upstream_ended_a_file() -> (Repo, String, String, String) {
        let repo = Repo::new();
        let root = repo.commit(&[], "Root", Some(&["9:0"]));
        let edit = repo.commit(&[&root], "Direct", Some(&["2:0"]));
        let unlink = repo.commit(&[&edit], "Direct", None);
        let fork = repo.commit(&[&root], "Direct", Some(&["s"]));
        (repo, edit, unlink, fork)
    }

    /// Finding 48 (the owner's "A", 2026-11-08): a sync may hold a line of the
    /// upstream's last version of a file the upstream ended.
    #[test]
    fn one_writer_accepts_a_line_of_the_upstreams_last_version() {
        let (repo, _, unlink, fork) = upstream_ended_a_file();
        let merge = repo.commit(&[&fork, &unlink], "Sync", Some(&["2:0", "s"]));
        let mut inv = invariants(&repo);
        assert_eq!(check(&mut inv, &merge), Ok(()));
    }

    /// And nothing wider: a superseded version, the upstream still holding the file,
    /// a merge that is not a sync, or a line from nowhere all originate it.
    #[test]
    fn one_writer_refuses_anything_wider() {
        let (repo, edit, unlink, fork) = upstream_ended_a_file();
        let stale = repo.commit(&[&fork, &unlink], "Sync", Some(&["9:0", "s"]));
        let kept = repo.commit(&[&edit], "Direct", Some(&["2:0"]));
        let held = repo.commit(&[&fork, &kept], "Sync", Some(&["9:0", "2:0", "s"]));
        let not_a_sync = repo.commit(&[&fork, &unlink], "Landing", Some(&["2:0", "s"]));
        let nowhere = repo.commit(&[&fork, &unlink], "Sync", Some(&["7:7", "s"]));
        let mut inv = invariants(&repo);
        for merge in [stale, held, not_a_sync, nowhere] {
            let error = check(&mut inv, &merge).unwrap_err();
            assert!(error.contains("one_writer"), "{error}");
        }
    }

    /// A landing names the writer and pod of the commit it lands, never its own.
    #[test]
    fn a_landing_names_the_writer_it_lands() {
        let repo = Repo::new();
        let root = repo.commit(&[], "Root", Some(&["b"]));
        let before = repo.commit(&[&root], "Direct", Some(&["u"]));
        let pod = "Agentfs-Pod: p-1\nAgentfs-Node: n1\n";
        let incoming = repo.commit_with(&[&root], "Direct", Some(&["f"]), pod);
        let named = repo.commit_with(&[&before, &incoming], "Landing", Some(&["u", "f"]), pod);
        let unnamed = repo.commit(&[&before, &incoming], "Landing", Some(&["u", "f"]));
        let other_pod = repo.commit_with(
            &[&before, &incoming],
            "Landing",
            Some(&["u", "f"]),
            "Agentfs-Pod: p-2\nAgentfs-Node: n1\n",
        );
        let mut inv = invariants(&repo);
        let mut judge = |oid: &str| {
            let c = inv.commit(oid).unwrap();
            landing_names_its_writer(oid, &c, &mut inv)
        };
        assert_eq!(judge(&named), Ok(()));
        for wrong in [unnamed, other_pod] {
            let error = judge(&wrong).unwrap_err();
            assert!(error.contains("Agentfs-Pod"), "{error}");
        }
    }

    /// Objects by path, for `one_object_one_file`.
    struct Numbered(Vec<(String, u64)>);

    impl crate::Objects for Numbered {
        fn objects(&self, _: TreeId) -> Result<Vec<(String, u64)>, CallError> {
            Ok(self.0.clone())
        }
    }

    #[test]
    fn one_object_one_file_refuses_two_names_of_one_object() {
        let distinct = Numbered(vec![("a".into(), 3), ("b".into(), 4)]);
        assert_eq!(
            Invariants::one_object_one_file(&distinct, TreeId::ROOT),
            Ok(())
        );
        let shared = Numbered(vec![("a".into(), 3), ("b".into(), 3)]);
        let error = Invariants::one_object_one_file(&shared, TreeId::ROOT).unwrap_err();
        assert!(error.contains("one_object_one_file"), "{error}");
    }

    /// What [`Invariants::drained`] reads of the trees; the other calls never run here.
    struct Shows(BTreeMap<TreeId, BTreeMap<String, Vec<u8>>>);

    impl TreeCalls for Shows {
        type Open = u64;
        fn fork(&mut self) -> Result<TreeId, CallError> {
            unreachable!("drained only reads")
        }
        fn open_write(&mut self, _: &Caller, _: &str) -> Result<u64, CallError> {
            unreachable!("drained only reads")
        }
        fn open_trunc(&mut self, _: &Caller, _: &str) -> Result<u64, CallError> {
            unreachable!("drained only reads")
        }
        fn create(&mut self, _: &Caller, _: &str) -> Result<u64, CallError> {
            unreachable!("drained only reads")
        }
        fn pwrite(&mut self, _: u64, _: u64, _: &[u8]) -> Result<(), CallError> {
            unreachable!("drained only reads")
        }
        fn append(&mut self, _: u64, _: &[u8]) -> Result<(), CallError> {
            unreachable!("drained only reads")
        }
        fn truncate(&mut self, _: u64, _: u64) -> Result<(), CallError> {
            unreachable!("drained only reads")
        }
        fn close(&mut self, _: u64) -> Result<(), CallError> {
            unreachable!("drained only reads")
        }
        fn unlink(&mut self, _: &Caller, _: &str) -> Result<(), CallError> {
            unreachable!("drained only reads")
        }
        fn rename(&mut self, _: &Caller, _: &str, _: &str) -> Result<(), CallError> {
            unreachable!("drained only reads")
        }
        fn end_client(&mut self, _: &Caller) -> Result<(), CallError> {
            unreachable!("drained only reads")
        }
        fn take_next(&mut self, _: TreeId) -> Result<Took, CallError> {
            unreachable!("drained only reads")
        }
        fn crash(&mut self) -> Result<(), CallError> {
            unreachable!("drained only reads")
        }
        fn read(&self, t: TreeId, path: &str) -> Result<Option<Vec<u8>>, CallError> {
            Ok(self.0.get(&t).and_then(|f| f.get(path)).cloned())
        }
        fn paths(&self, t: TreeId) -> Result<Vec<String>, CallError> {
            Ok(self
                .0
                .get(&t)
                .map(|f| f.keys().cloned().collect())
                .unwrap_or_default())
        }
    }

    /// The root and fork 1 showing `root` and `fork` at path `a`.
    fn shows(root: &[u8], fork: &[u8]) -> Shows {
        let tree = |bytes: &[u8]| BTreeMap::from([("a".to_owned(), bytes.to_vec())]);
        Shows(BTreeMap::from([
            (TreeId::ROOT, tree(root)),
            (TreeId::new(1), tree(fork)),
        ]))
    }

    /// `Invariants` over a new repository with a fork 1 to check, and its root's tip.
    fn with_fork(repo: &Repo) -> (Invariants, String) {
        let mut inv = invariants(repo);
        inv.trees.insert(TreeId::new(1), OracleTree::default());
        let tip = repo.git(&["rev-parse", &format!("refs/heads/{}", root())], b"");
        (inv, tip)
    }

    /// R7: once the queues are empty every tree holds the root's files.
    #[test]
    fn converged_fails_when_a_tree_differs_from_the_root() {
        let repo = Repo::new();
        let (mut inv, _) = with_fork(&repo);
        assert_eq!(inv.drained(&shows(b"x\n", b"x\n")), Ok(()));
        let error = inv.drained(&shows(b"x\n", b"y\n")).unwrap_err();
        assert!(error.contains("converged:"), "{error}");
    }

    /// R7: once the queues are empty every save of every tree is in the root's history.
    #[test]
    fn full_lineage_fails_when_a_save_is_not_in_the_roots_history() {
        let repo = Repo::new();
        let (mut inv, tip) = with_fork(&repo);
        let on_root = repo.commit(&[&tip], "Direct", Some(&["m"]));
        repo.git(
            &["update-ref", &format!("refs/heads/{}", root()), &on_root],
            b"",
        );
        inv.commit(&on_root).unwrap();
        assert_eq!(inv.drained(&shows(b"x\n", b"x\n")), Ok(()));
        let stray = repo.commit(&[&tip], "Direct", Some(&["s"]));
        inv.commit(&stray).unwrap();
        let error = inv.drained(&shows(b"x\n", b"x\n")).unwrap_err();
        assert!(error.contains("full_lineage:"), "{error}");
    }

    /// R3: the upstream's line wins, so the result of a lost landing never holds
    /// the line that lost. Both the valid landing and the bad one are real commits.
    #[test]
    fn upstream_kept_fails_when_a_landing_keeps_the_lost_line() {
        let repo = Repo::new();
        let base = repo.commit(&[], "Root", Some(&["b"]));
        let before = repo.commit(&[&base], "Direct", Some(&["u", "w"]));
        let incoming = repo.commit(&[&base], "Direct", Some(&["f"]));
        repo.git(
            &[
                "update-ref",
                &format!("refs/collisions/{}/{incoming}", root()),
                &incoming,
            ],
            b"",
        );
        let trailers = format!(
            "Agentfs-Tree: {}\n{}{}\n",
            root(),
            r#"Agentfs-Collisions: [{"lost":{"Line":{"file":1,"version":"f"}},"#,
            r#""winner":{"Line":{"file":1,"version":"u"}},"kind":"Line"}]"#,
        );
        let landing = |lines: &[&str]| {
            repo.commit_with(&[&before, &incoming], "Landing", Some(lines), &trailers)
        };
        let mut inv = invariants(&repo);
        let keeps_upstream = landing(&["u", "w"]);
        let c = inv.commit(&keeps_upstream).unwrap();
        assert_eq!(inv.landing(&keeps_upstream, &c), Ok(()));
        // The winner is still there, so only the lost line tells: every line's bytes
        // match, which the checks before it read.
        let keeps_lost = landing(&["u", "f"]);
        let c = inv.commit(&keeps_lost).unwrap();
        let error = inv.landing(&keeps_lost, &c).unwrap_err();
        assert!(error.contains("upstream_kept:"), "{error}");
    }
}
