//! What must hold in the real system, whatever the model said (git-model.md
//! §15, §18). Read from `git`, this file's own parse of each commit's
//! `metadata.json` and `namespace.json`, and a byte oracle of its own: never the
//! library's bookkeeping. `check::save_aligned` is shared with the model, over the
//! model's `Content`.

use agentfs_model::calls::{
    Caller, Recorded, RecordedCall, Shown, ShownCollision, Took, TreeCalls, TreeId,
    gather_collisions,
};
use agentfs_model::check::save_aligned;
use agentfs_model::content::{Content, Line, State};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

/// A line version in a commit: the file's identity, the line's, the bytes.
#[derive(Clone, Debug)]
struct FileMap {
    path: String,
    object: u64,
    identity: u64,
    lines: Vec<(String, String, Option<Vec<u8>>)>,
    ancestors: BTreeMap<String, Option<String>>,
}

/// One commit, parsed.
#[derive(Clone, Debug, Default)]
struct Commit {
    parents: Vec<String>,
    message: String,
    files: BTreeMap<String, Vec<u8>>,
    maps: BTreeMap<u64, FileMap>,
    /// Each file identity's name history: head, versions.
    names: BTreeMap<u64, (u64, BTreeMap<u64, NameVersion>)>,
}

#[derive(Clone, Debug)]
struct NameVersion {
    path: Option<String>,
    over: Option<u64>,
}

impl Commit {
    fn trailers(&self, key: &str) -> Vec<&str> {
        let prefix = format!("{key}: ");
        self.message
            .lines()
            .filter_map(|l| l.strip_prefix(prefix.as_str()))
            .collect()
    }

    fn holds_line(&self, file: u64, version: &str) -> bool {
        self.maps
            .get(&file)
            .is_some_and(|m| m.lines.iter().any(|l| l.0 == version))
    }

    fn name_path(&self, file: u64) -> Option<&str> {
        let (head, versions) = self.names.get(&file)?;
        versions.get(head)?.path.as_deref()
    }

    fn holds_name(&self, file: u64, version: u64) -> bool {
        self.names.get(&file).is_some_and(|(h, _)| *h == version)
    }

    fn holds(&self, v: &Version) -> bool {
        match v {
            Version::Line { file, version } => self.holds_line(*file, version),
            Version::Name { file, version } => self.holds_name(*file, *version),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Version {
    Line { file: u64, version: String },
    Name { file: u64, version: u64 },
}

impl Version {
    fn parse(v: &Value) -> Result<Self, String> {
        let number = |x: &Value| x.as_u64().ok_or_else(|| format!("bad version {v}"));
        if let Some(line) = v.get("Line") {
            Ok(Self::Line {
                file: number(&line["file"])?,
                version: line["version"]
                    .as_str()
                    .ok_or_else(|| format!("bad version {v}"))?
                    .to_owned(),
            })
        } else if let Some(name) = v.get("Name") {
            Ok(Self::Name {
                file: number(&name["file"])?,
                version: number(&name["version"])?,
            })
        } else {
            Err(format!("bad version {v}"))
        }
    }

    fn file(&self) -> u64 {
        match self {
            Self::Line { file, .. } | Self::Name { file, .. } => *file,
        }
    }
}

#[derive(Clone, Debug)]
struct Decision {
    lost: Version,
    winner: Version,
    whole: bool,
}

fn decisions(c: &Commit) -> Result<Vec<Decision>, String> {
    let mut out = Vec::new();
    for json in c.trailers("Agentfs-Collisions") {
        let parsed: Value = serde_json::from_str(json).map_err(|e| e.to_string())?;
        for d in parsed.as_array().ok_or("collisions are not a list")? {
            out.push(Decision {
                lost: Version::parse(&d["lost"])?,
                winner: Version::parse(&d["winner"])?,
                whole: d["kind"] == "Whole",
            });
        }
    }
    Ok(out)
}

/// One long-lived `git cat-file --batch`, and short `git` commands.
struct Git {
    dir: PathBuf,
    child: Child,
    input: ChildStdin,
    output: BufReader<ChildStdout>,
}

impl Git {
    fn new(dir: &Path) -> Result<Self, String> {
        let mut child = Command::new("git")
            .arg("--git-dir")
            .arg(dir)
            .args(["cat-file", "--batch"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .map_err(|e| e.to_string())?;
        let input = child.stdin.take().ok_or("no stdin")?;
        let output = BufReader::new(child.stdout.take().ok_or("no stdout")?);
        Ok(Self {
            dir: dir.to_owned(),
            child,
            input,
            output,
        })
    }

    fn run(&self, args: &[&str]) -> Result<String, String> {
        let out = Command::new("git")
            .arg("--git-dir")
            .arg(&self.dir)
            .args(args)
            .output()
            .map_err(|e| e.to_string())?;
        if !out.status.success() {
            return Err(format!(
                "git {args:?}: {}",
                String::from_utf8_lossy(&out.stderr)
            ));
        }
        String::from_utf8(out.stdout).map_err(|e| e.to_string())
    }

    fn object(&mut self, name: &str) -> Result<Vec<u8>, String> {
        writeln!(self.input, "{name}").map_err(|e| e.to_string())?;
        self.input.flush().map_err(|e| e.to_string())?;
        let mut header = String::new();
        self.output
            .read_line(&mut header)
            .map_err(|e| e.to_string())?;
        let size: usize = header
            .trim_end()
            .rsplit(' ')
            .next()
            .and_then(|s| s.parse().ok())
            .ok_or_else(|| format!("{name}: {header:?}"))?;
        let mut bytes = vec![0; size + 1];
        self.output
            .read_exact(&mut bytes)
            .map_err(|e| e.to_string())?;
        bytes.pop();
        Ok(bytes)
    }

    fn commit(&mut self, oid: &str) -> Result<Commit, String> {
        let raw = self.object(oid)?;
        let raw = String::from_utf8_lossy(&raw).into_owned();
        let (head, message) = raw.split_once("\n\n").unwrap_or((&raw, ""));
        let parents = head
            .lines()
            .filter_map(|l| l.strip_prefix("parent "))
            .map(str::to_owned)
            .collect();
        let mut files = BTreeMap::new();
        for line in self
            .run(&["ls-tree", "-r", "--full-tree", oid, "files/"])?
            .lines()
        {
            let (meta, path) = line.split_once('\t').ok_or_else(|| line.to_owned())?;
            let blob = meta.rsplit(' ').next().ok_or_else(|| line.to_owned())?;
            let name = path.strip_prefix("files/").unwrap_or(path).to_owned();
            files.insert(name, self.object(blob)?);
        }
        let mut commit = Commit {
            parents,
            message: message.to_owned(),
            files,
            ..Commit::default()
        };
        // The root commit has no envelope.
        if commit.trailers("Agentfs-Format").is_empty() {
            return Ok(commit);
        }
        let metadata: Value =
            serde_json::from_slice(&self.object(&format!("{oid}:metadata.json"))?)
                .map_err(|e| e.to_string())?;
        for (path, entry) in metadata.as_object().ok_or("metadata is not a map")? {
            let map = parse_map(path, entry).map_err(|e| format!("{oid} {path}: {e}"))?;
            commit.maps.insert(map.identity, map);
        }
        let namespace: Value =
            serde_json::from_slice(&self.object(&format!("{oid}:namespace.json"))?)
                .map_err(|e| e.to_string())?;
        for (file, history) in namespace.as_object().ok_or("namespace is not a map")? {
            let file: u64 = file.parse().map_err(|_| "bad file identity")?;
            let head = history["head"].as_u64().ok_or("bad name head")?;
            let mut versions = BTreeMap::new();
            for (n, v) in history["versions"].as_object().ok_or("bad name versions")? {
                versions.insert(
                    n.parse::<u64>().map_err(|_| "bad name version")?,
                    NameVersion {
                        path: v["path"].as_str().map(str::to_owned),
                        over: v["over"].as_u64(),
                    },
                );
            }
            commit.names.insert(file, (head, versions));
        }
        Ok(commit)
    }
}

impl Drop for Git {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn parse_map(path: &str, entry: &Value) -> Result<FileMap, String> {
    let object = entry[0].as_u64().ok_or("bad object")?;
    let identity = entry[1].as_u64().ok_or("bad identity")?;
    let map = &entry[2];
    let mut lines = Vec::new();
    for l in map["lines"].as_array().ok_or("bad lines")? {
        let bytes = match &l["bytes"] {
            Value::Null => None,
            b => Some(
                b.as_array()
                    .ok_or("bad bytes")?
                    .iter()
                    .map(|x| x.as_u64().map(|x| x as u8).ok_or("bad byte"))
                    .collect::<Result<Vec<u8>, _>>()?,
            ),
        };
        lines.push((
            l["version"].as_str().ok_or("bad version")?.to_owned(),
            l["identity"]
                .as_str()
                .ok_or("bad line identity")?
                .to_owned(),
            bytes,
        ));
    }
    let mut ancestors = BTreeMap::new();
    for (v, over) in map["ancestors"].as_object().ok_or("bad ancestors")? {
        ancestors.insert(v.clone(), over.as_str().map(str::to_owned));
    }
    Ok(FileMap {
        path: path.to_owned(),
        object,
        identity,
        lines,
        ancestors,
    })
}

/// A file in the model's terms, so its pure checks apply. Its name is one more
/// line, its ids this file's own.
fn content(c: &Commit, file: u64) -> Option<Content> {
    let map = c.maps.get(&file)?;
    let (head, versions) = c.names.get(&file)?;
    let named = versions.get(head)?;
    let name = Line {
        id: format!("name{head}"),
        line: format!("name-of-{file}"),
        over: named.over.map(|o| format!("name{o}")),
        state: named.path.clone().map_or(State::Gone, State::Live),
    };
    let lines = map
        .lines
        .iter()
        .map(|(version, identity, bytes)| Line {
            id: version.clone(),
            line: identity.clone(),
            over: map.ancestors.get(version).cloned().flatten(),
            state: bytes.as_ref().map_or(State::Gone, |b| {
                State::Live(String::from_utf8_lossy(b).into_owned())
            }),
        })
        .collect();
    Some(Content { name, lines })
}

fn descends<'a>(map: &'a FileMap, mut version: &'a str, from: &str) -> bool {
    loop {
        if version == from {
            return true;
        }
        match map.ancestors.get(version) {
            Some(Some(over)) => version = over,
            _ => return false,
        }
    }
}

/// The harness's own record of a tree: names and bytes, by Linux's rules.
#[derive(Clone, Debug, Default)]
struct OracleTree {
    names: BTreeMap<String, u64>,
    bytes: BTreeMap<u64, Vec<u8>>,
    /// The real object number behind each of this oracle's objects: bound only so
    /// an open follows its file through a reload.
    bound: BTreeMap<u64, u64>,
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
    /// The real fork names, in the order the trace's forks were made.
    forks: Vec<String>,
    trees: BTreeMap<TreeId, OracleTree>,
    opens: BTreeMap<u64, OracleOpen>,
    next_object: u64,
    commits: BTreeMap<String, Commit>,
    opens_saved: BTreeMap<String, String>,
    collisions: Vec<ShownCollision>,
    refs: String,
}

impl Invariants {
    /// Over the bare repository at `repo`, before the trace's first call.
    pub fn new(repo: &Path) -> Result<Self, String> {
        let mut me = Self {
            git: Git::new(repo)?,
            forks: Vec::new(),
            trees: BTreeMap::new(),
            opens: BTreeMap::new(),
            next_object: 0,
            commits: BTreeMap::new(),
            opens_saved: BTreeMap::new(),
            collisions: Vec::new(),
            refs: String::new(),
        };
        me.trees.insert(TreeId::Main, OracleTree::default());
        me.reload(TreeId::Main)?;
        Ok(me)
    }

    fn refname(&self, t: TreeId) -> Result<String, String> {
        match t {
            TreeId::Main => Ok("refs/heads/main".into()),
            TreeId::Fork(f) => self
                .forks
                .get(f.wrapping_sub(1))
                .map(|n| format!("refs/heads/{n}"))
                .ok_or_else(|| format!("no fork {f}")),
        }
    }

    fn head(&mut self, t: TreeId) -> Result<Commit, String> {
        let oid = self.git.run(&["rev-parse", &self.refname(t)?])?;
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
    /// this oracle does not know.
    fn reload(&mut self, t: TreeId) -> Result<(), String> {
        let head = self.head(t)?;
        let mut objects: BTreeMap<u64, u64> = BTreeMap::new();
        let tree = self.trees.entry(t).or_default();
        for (own, real) in &tree.bound {
            objects.insert(*real, *own);
        }
        tree.names.clear();
        for map in head.maps.values() {
            let own = *objects.entry(map.object).or_insert_with(|| {
                self.next_object += 1;
                self.next_object
            });
            tree.names.insert(map.path.clone(), own);
            tree.bytes
                .insert(own, head.files.get(&map.path).cloned().unwrap_or_default());
            tree.bound.insert(own, map.object);
        }
        Ok(())
    }

    /// Bind a file this oracle just made to its real object number.
    fn bind(&mut self, t: TreeId, path: &str, own: u64) -> Result<(), String> {
        let head = self.head(t)?;
        let tree = self.trees.entry(t).or_default();
        if let Some(map) = head.maps.values().find(|m| m.path == path) {
            tree.bound.insert(own, map.object);
        }
        Ok(())
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
            Caller::Main(_) => TreeId::Main,
            Caller::Fork(f) => TreeId::Fork(*f),
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

    fn note_call(&mut self, r: &Recorded, undone: bool) -> Result<(), String> {
        match &r.call {
            RecordedCall::Fork { id } => {
                let listed = self.git.run(&[
                    "for-each-ref",
                    "--format=%(refname:lstrip=2)",
                    "refs/heads/fork/",
                ])?;
                let mut names: Vec<String> = listed.lines().map(str::to_owned).collect();
                names.sort_by_key(|n| {
                    n.strip_prefix("fork/")
                        .and_then(|s| s.parse::<u64>().ok())
                        .unwrap_or(0)
                });
                self.forks = names;
                let main = self.trees.get(&TreeId::Main).cloned().unwrap_or_default();
                self.trees.insert(TreeId::Fork(*id), main);
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
                self.bind(tree, path, own)?;
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
                if *took == Took::Taken && !undone {
                    self.reload(*tree)?;
                }
            }
            RecordedCall::Finish { .. } => {}
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
        let all = self
            .git
            .run(&["rev-list", "--all", "--reverse", "--topo-order"])?;
        for oid in all.lines() {
            if self.commits.contains_key(oid) && self.checked(oid) {
                continue;
            }
            let c = self.commit(oid)?;
            one_writer(oid, &c, self)?;
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
                    "parent_kept: {oid:.7} still holds lost {:?}",
                    d.lost
                ));
            }
            if !c.holds(&d.winner) && !d.whole {
                return Err(format!(
                    "parent_kept: {oid:.7} lacks the winner {:?}",
                    d.winner
                ));
            }
            if !incoming.holds(&d.lost) {
                return Err(format!(
                    "loser_reachable: {:?} is not in the losing commit {incoming_id:.7}",
                    d.lost
                ));
            }
            let path = c
                .name_path(d.lost.file())
                .or(incoming.name_path(d.lost.file()))
                .unwrap_or_default()
                .to_owned();
            let lost_bytes = match &d.lost {
                Version::Line { file, version } => incoming
                    .maps
                    .get(file)
                    .and_then(|m| m.lines.iter().find(|l| l.0 == *version))
                    .and_then(|l| l.2.clone())
                    .into_iter()
                    .collect(),
                Version::Name { .. } => Vec::new(),
            };
            self.collisions.push(ShownCollision {
                tree: self
                    .forks
                    .iter()
                    .position(|f| *f == tree)
                    .map_or(TreeId::Main, |i| TreeId::Fork(i + 1)),
                path,
                lost_bytes,
                whole: d.whole,
            });
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

    /// Queues empty, every tree holds main's files, and every save of every tree is
    /// in main's history (R7).
    fn drained<T: TreeCalls>(&mut self, t: &T) -> Result<(), String> {
        let queued = self
            .git
            .run(&["for-each-ref", "--format=%(refname)", "refs/queue/"])?;
        if !queued.trim().is_empty() {
            return Ok(());
        }
        let main_paths = t.paths(TreeId::Main).map_err(|e| e.to_string())?;
        let ids: Vec<TreeId> = self.trees.keys().copied().collect();
        for id in ids {
            let paths = t.paths(id).map_err(|e| e.to_string())?;
            if paths != main_paths {
                return Err(format!(
                    "converged: {id:?} has {paths:?}, main {main_paths:?}"
                ));
            }
            for p in &paths {
                let (a, b) = (
                    t.read(id, p).map_err(|e| e.to_string())?,
                    t.read(TreeId::Main, p).map_err(|e| e.to_string())?,
                );
                if a != b {
                    return Err(format!("converged: {id:?}:{p} differs from main's"));
                }
            }
        }
        let reached: BTreeSet<String> = self
            .git
            .run(&["rev-list", "refs/heads/main"])?
            .lines()
            .map(str::to_owned)
            .collect();
        for (oid, c) in &self.commits {
            if c.trailers("Agentfs-Cause") == ["Direct"] && !reached.contains(oid) {
                return Err(format!("full_lineage: {oid:.7} is not in main's history"));
            }
        }
        Ok(())
    }

    /// After one recorded call. `undone`: a take a crash ended, which the replay
    /// did not make.
    pub fn after<T: TreeCalls>(&mut self, t: &T, r: &Recorded, undone: bool) -> Result<(), String> {
        self.note_call(r, undone)?;
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
        }
        if matches!(r.call, RecordedCall::Take { .. } | RecordedCall::Crash) {
            self.drained(t)?;
        }
        if let Some(shown) = &r.shown {
            self.collisions_match(shown)?;
        }
        Ok(())
    }

    fn collisions_match(&mut self, shown: &Shown) -> Result<(), String> {
        let real = gather_collisions(self.collisions.iter().cloned());
        if real != shown.collisions {
            return Err(format!(
                "collisions: the system recorded {real:?}, the trace {:?}",
                shown.collisions
            ));
        }
        Ok(())
    }
}

/// R6: every commit names one writer, and a merge originates nothing.
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
    for (file, map) in &c.maps {
        for (version, _, _) in &map.lines {
            if !parents.iter().any(|p| p.holds_line(*file, version)) {
                return Err(format!(
                    "one_writer: merge {oid:.7} originates {version} of file {file}"
                ));
            }
        }
    }
    Ok(())
}

fn print(files: &BTreeMap<String, Vec<u8>>) -> String {
    let shown: Vec<String> = files
        .iter()
        .map(|(p, b)| format!("{p}={:?}", String::from_utf8_lossy(b)))
        .collect();
    shown.join(" ")
}
