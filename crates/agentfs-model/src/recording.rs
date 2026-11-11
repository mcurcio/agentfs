//! Recording: what a program's calls on a tree's files become (MODEL.md rules 1, 6, 8, 10
//! and 11). It keeps the opens and their byte calls, the object each open holds, the
//! orphans, and each close's save, its line identities derived from the bytes then. It reads
//! files through a port ([`Files`]) and never commits, judges or merges: the world commits a
//! [`Save`]. The module-cut page (`docs/target-architecture/future/module-cut.md`) draws its
//! boundary.

use std::collections::{BTreeMap, BTreeSet};

use crate::calls::{Client, HandleId, TreeId, TxnId};
use crate::content::{Content, Edit, Line, PATHS, State, UNBORN, split_lines};

/// The port: a model file at a commit or tree. The world's git repository and its one
/// object cache implement it; a test may use a map.
pub(crate) trait Files {
    /// File `p` (its identity) at a commit or tree id.
    fn file(&self, at: &str, p: &str) -> Content;
}

/// The identity of the file named `p` at `at`, if one is (rule 11: a path is a name line).
pub(crate) fn lookup(git: &impl Files, at: &str, p: &str) -> Option<String> {
    PATHS
        .iter()
        .find(|f| git.file(at, f).path() == Some(p))
        .map(|f| f.to_string())
}

/// Rule 8: one write call on an open file, as POSIX gives it: bytes, never
/// lines. Every byte is ASCII in the model, so an offset is a byte offset
/// and a char boundary at once.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Write {
    /// `write` on an `O_APPEND` open: the kernel seeks to the end and writes
    /// in one step, so these bytes go after the file's last byte as it is
    /// at the save, another writer's concurrent append included. The save
    /// knows it (rule 8).
    Append(String),
    /// `pwrite(fd, bytes, offset)`, or `lseek` and `write`: these bytes
    /// from `offset` on, overwriting what is there and extending the file
    /// past its end; a gap past the end reads as NUL bytes (a hole).
    Pwrite { offset: usize, bytes: String },
    /// `ftruncate(fd, len)`, and an `O_TRUNC` open (`Truncate(0)` at open):
    /// the file cut to `len` bytes, or extended to it with NUL bytes.
    Truncate(usize),
}

impl Write {
    /// The calls applied to `bytes`, in order: what the file holds after
    /// them. The model's own account (the harness keeps another).
    pub fn apply_all(bytes: &str, calls: &[Write]) -> String {
        let mut b = bytes.as_bytes().to_vec();
        for c in calls {
            c.apply(&mut b);
        }
        String::from_utf8(b).expect("ASCII bytes")
    }

    fn apply(&self, b: &mut Vec<u8>) {
        match self {
            Write::Append(x) => b.extend(x.bytes()),
            Write::Pwrite { offset, bytes } => {
                let end = offset + bytes.len();
                if b.len() < end {
                    b.resize(end, 0);
                }
                b[*offset..end].copy_from_slice(bytes.as_bytes());
            }
            Write::Truncate(n) => b.resize(*n, 0),
        }
    }
}

/// One line of a save's replay: a head line kept, or a new version (of a
/// head line when `line` is some, else a new line), whose id is given at
/// close.
#[derive(Clone, Debug)]
enum Replayed {
    Kept(Line),
    New {
        line: Option<String>,
        over: Option<String>,
        state: State,
    },
}

impl Replayed {
    /// This line written again: a new version holding `state`.
    fn version(self, state: State) -> Self {
        match self {
            Replayed::Kept(l) => Replayed::New {
                line: Some(l.line),
                over: Some(l.id),
                state,
            },
            Replayed::New { line, over, .. } => Replayed::New { line, over, state },
        }
    }

    fn live(&self) -> bool {
        match self {
            Replayed::Kept(l) => l.live(),
            Replayed::New { state, .. } => matches!(state, State::Live(_)),
        }
    }

    fn text(&self) -> String {
        match self {
            Replayed::Kept(Line {
                state: State::Live(t),
                ..
            })
            | Replayed::New {
                state: State::Live(t),
                ..
            } => t.clone(),
            _ => String::new(),
        }
    }

    /// This line deleted: a deleted version of a head line; nothing for a
    /// line new in this save.
    fn deleted(self) -> Option<Self> {
        match self {
            Replayed::New { line: None, .. } => None,
            r => Some(r.version(State::Gone)),
        }
    }
}

/// What a handle is: an open file, or a path call (rule 11) deferred behind
/// a transaction (R5.3).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Kind {
    Write,
    Unlink,
    /// To that path; `over`: the file there when called (rename-over).
    Rename {
        to: String,
        over: Option<String>,
    },
}

/// Where a write-open writes: its tree, or its client's open transaction.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Target {
    Tree(TreeId),
    Txn(TreeId, TxnId),
}

/// A file a client has open for writing (rule 8), or a path call deferred
/// (rule 11, R5.3). Its write calls stay private to it until close
/// (close-to-open: the owner's decision, git-model.md §3).
#[derive(Clone, Debug)]
pub struct Handle {
    pub id: HandleId,
    pub client: Client,
    pub target: Target,
    /// The file's identity: an open follows its file through a rename.
    /// After an atomic save over the file it was opened on, the file its
    /// object is now named by ([`Recording::named`] says which, at close).
    pub file: String,
    /// The object it holds (the inode, on Linux; git-model.md §7): fixed
    /// at open, never changed. Its close saves to the file whose object
    /// this is in its target then; if no path names it, nothing.
    pub object: u64,
    /// The path it was opened by.
    pub path: String,
    /// R5.3: the call was made and returned while a transaction held its
    /// file: a close whose save, or a path call that, is on
    /// its fork's deferred list (R5.3). Its file counts as open (rule 9) until then.
    pub deferred: bool,
    pub kind: Kind,
    /// Its write calls, in order, applied to the head at close.
    calls: Vec<Write>,
    /// Version ids its edits reserved ([`Recording::reserve`]), each the text of the line
    /// it wrote: a new version holding exactly that text takes the id, so a
    /// test can name it. A label only: which line is new or changed comes
    /// from the bytes alone.
    reserved: Vec<String>,
}

impl Handle {
    /// Its write calls so far, in the order made.
    pub fn calls(&self) -> &[Write] {
        &self.calls
    }

    /// The files it locks or waits for: its file, and a rename's target.
    pub fn files(&self) -> Vec<String> {
        let mut v = vec![self.file.clone()];
        if let Kind::Rename { over: Some(g), .. } = &self.kind {
            v.push(g.clone());
        }
        v
    }
}

/// An `OrphanDropped` event (git-model.md §7, a choice): an open whose
/// object no path named at its close (its file unlinked, or replaced by a
/// rename-over or an atomic save, in its target). Its write calls were
/// recorded as made (the journal, git-model.md §3); they are dropped, never
/// committed, and this event names them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Orphan {
    pub handle: HandleId,
    pub writer: String,
    /// The path it was opened by.
    pub path: String,
    /// The bytes its write calls carried.
    pub bytes: usize,
}

/// Whether file `c` is new since its create: an [`UNBORN`] identity whose
/// name version is still the one its create wrote (never renamed since).
/// A rename-over from such a file is an atomic save (git-model.md §7).
/// Read from git alone, so it is the same whenever the save runs and
/// whether or not the upstream has taken the create yet.
pub fn fresh(c: &Content) -> bool {
    UNBORN.contains(&c.name.line.as_str()) && c.name.over.as_deref() == Some(c.name.line.as_str())
}

/// A save ready to commit over `at`: the files it writes, by identity, and the versions it
/// set. The world commits it on `target` (`World::commit_save`).
pub(crate) struct Save {
    pub(crate) target: Target,
    pub(crate) writer: String,
    pub(crate) at: String,
    pub(crate) files: BTreeMap<String, Content>,
    pub(crate) set: Vec<Edit>,
    /// The open it saves, named by the commit's `Agentfs-Open` trailer.
    pub(crate) open: Option<HandleId>,
}

/// What a close gives: a save, no change (R8.2), or an orphan (R11.3), whose
/// `OrphanDropped` event is recorded with the bytes it dropped (none: it made no call).
pub(crate) enum Saved {
    Save(Save),
    NoChange,
    Orphaned(Option<usize>),
}

/// The opens, the objects they hold, and what their saves recorded.
pub(crate) struct Recording {
    handles: BTreeMap<HandleId, Handle>,
    next_handle: HandleId,
    /// Each file's object in each target (the inode an open holds,
    /// git-model.md §7); none once its file is unlinked or replaced there.
    /// Not in git: a view's state, as an inode table is.
    inodes: BTreeMap<(Target, String), u64>,
    next_object: u64,
    /// How many of [`UNBORN`] creates have taken.
    next_unborn: usize,
    counters: BTreeMap<String, u32>,
    /// Every `OrphanDropped` event.
    orphans: Vec<Orphan>,
    /// Each save's write calls, as the program made them, by the save's
    /// commit: `check` reads rule 8's `O_APPEND` from them and the blobs,
    /// not from the replay.
    save_calls: BTreeMap<String, Vec<Write>>,
    /// Main's clients that ended (rule 9: their end event released their
    /// locks).
    ended_main: BTreeSet<String>,
}

impl Recording {
    pub(crate) fn new() -> Self {
        Self {
            handles: BTreeMap::new(),
            next_handle: 1,
            inodes: BTreeMap::new(),
            next_object: 0,
            next_unborn: 0,
            counters: BTreeMap::new(),
            orphans: Vec::new(),
            save_calls: BTreeMap::new(),
            ended_main: BTreeSet::new(),
        }
    }

    /// A new open (or a path call, rule 11) of `file` at `target`, holding its object there.
    /// No take is ever in progress between calls (a take is one step), so none waits. Panics
    /// for a main client that ended.
    pub(crate) fn open(
        &mut self,
        c: &Client,
        target: Target,
        file: String,
        path: &str,
        kind: Kind,
    ) -> HandleId {
        assert!(
            !matches!(c, Client::Main(n) if self.ended_main.contains(n)),
            "a call by {c:?}, which ended"
        );
        let id = self.next_handle;
        self.next_handle += 1;
        let object = self.inode(target, &file);
        let h = Handle {
            id,
            client: c.clone(),
            target,
            file,
            object,
            path: path.to_string(),
            deferred: false,
            kind,
            calls: Vec::new(),
            reserved: Vec::new(),
        };
        self.handles.insert(id, h);
        id
    }

    pub(crate) fn handle(&self, h: HandleId) -> &Handle {
        &self.handles[&h]
    }

    /// Every open file, and every deferred path call (R5.3), by id.
    pub(crate) fn handles(&self) -> impl Iterator<Item = &Handle> {
        self.handles.values()
    }

    /// Open `h` ends here: its close, a path call that runs, or a crash's replay.
    pub(crate) fn remove(&mut self, h: HandleId) -> Option<Handle> {
        self.handles.remove(&h)
    }

    /// The journal's replay takes open `id` back, to save it as its close would.
    // The binding stays a statement of its own: the planted-bug corpus edits after it.
    #[allow(clippy::let_and_return)]
    pub(crate) fn replayed(&mut self, id: HandleId) -> Handle {
        let h = self.handles.remove(&id).expect("an open to replay");
        h
    }

    /// A rollback's opens in `target` end, unsaved.
    pub(crate) fn drop_opens_of(&mut self, target: Target) {
        self.handles.retain(|_, h| h.target != target);
    }

    /// An `O_TRUNC` open's first call: the file truncated to nothing (applied at close).
    pub(crate) fn truncate_at_open(&mut self, h: HandleId) {
        self.handles
            .get_mut(&h)
            .expect("open")
            .calls
            .push(Write::Truncate(0));
    }

    /// Rule 8: one byte call on open `h`, kept until its close.
    pub(crate) fn write(&mut self, h: HandleId, call: Write) {
        let handle = &self.handles[&h];
        assert!(handle.kind == Kind::Write, "a write needs an open");
        let bytes = match &call {
            Write::Append(b) | Write::Pwrite { bytes: b, .. } => b.as_str(),
            Write::Truncate(_) => "",
        };
        assert!(bytes.is_ascii(), "the model's bytes are ASCII");
        self.handles.get_mut(&h).expect("open").calls.push(call);
    }

    /// A version id for a line open `h` is about to write, reserved now: a new
    /// version holding exactly this text at the save takes it.
    pub(crate) fn reserve(&mut self, h: HandleId, writer: &str, txn: Option<TxnId>) -> String {
        let token = self.token(writer, txn);
        self.handles
            .get_mut(&h)
            .expect("open")
            .reserved
            .push(token.clone());
        token
    }

    /// What the program reads through open `h`: its file at `at` (its target's head now),
    /// with its own calls applied (close-to-open: no other writer's unsaved calls).
    pub(crate) fn view(&self, h: HandleId, at: &str, git: &impl Files) -> String {
        let handle = &self.handles[&h];
        let cur = git.file(at, &handle.file);
        Write::apply_all(&cur.bytes(), &handle.calls)
    }

    /// The file open `h`'s object is in its target now (at `at`, the target's head): the
    /// live file whose object it is; none if no path names it (an orphan: its file was
    /// unlinked, or replaced by a rename-over or an atomic save, there).
    pub(crate) fn named(&self, h: &Handle, at: &str, git: &impl Files) -> Option<String> {
        self.inodes
            .iter()
            .find(|((t, f), o)| *t == h.target && **o == h.object && git.file(at, f).name.live())
            .map(|((_, f), _)| f.clone())
    }

    /// The files clients writing in tree `t` (at `at`, its head) have open, and the files
    /// their deferred calls (R5.3) will save: identities, not lock keys. An open whose
    /// object no path names (an orphan) has no file open.
    pub(crate) fn open_files(&self, t: TreeId, at: &str, git: &impl Files) -> BTreeSet<String> {
        self.handles
            .values()
            .filter(|h| h.target == Target::Tree(t))
            .flat_map(|h| {
                let mut files: Vec<String> = self.named(h, at, git).into_iter().collect();
                if h.kind != Kind::Write {
                    files.extend(h.files());
                }
                files
            })
            .collect()
    }

    /// File `f`'s object in target `t`, a new one if it has none.
    pub(crate) fn inode(&mut self, t: Target, f: &str) -> u64 {
        if let Some(o) = self.inodes.get(&(t, f.to_string())) {
            return *o;
        }
        self.next_object += 1;
        self.inodes.insert((t, f.to_string()), self.next_object);
        self.next_object
    }

    /// A transaction's open `h` after its commit: it writes in `target`, holding the object
    /// `file` (its file in the transaction) has there; with no such file a new object no
    /// path names, so it is an orphan (R11.3).
    pub(crate) fn retarget(&mut self, h: HandleId, target: Target, file: Option<String>) {
        let object = match &file {
            Some(x) => self.inode(target, x),
            None => {
                self.next_object += 1;
                self.next_object
            }
        };
        let handle = self.handles.get_mut(&h).expect("an open");
        handle.target = target;
        handle.object = object;
        if let Some(x) = file {
            handle.file = x;
        }
    }

    /// R5.3: call `h` was made and returned while a transaction held its file.
    pub(crate) fn set_deferred(&mut self, h: HandleId) {
        self.handles.get_mut(&h).expect("a call").deferred = true;
    }

    fn token(&mut self, writer: &str, txn: Option<TxnId>) -> String {
        let n = self.counters.entry(writer.to_string()).or_default();
        *n += 1;
        match txn {
            Some(t) => format!("{writer}.{n}~t{t}"),
            None => format!("{writer}.{n}"),
        }
    }

    /// Rules 8 and 10, the save at close of open `h`: its write calls applied, in order, to
    /// its file at `at` (the head of its tree or transaction now, [`Recording::replay`]):
    /// every line the program's bytes leave unchanged keeps its version, a line a sync
    /// brought or another writer's append included. No change if nothing changed; an orphan
    /// if no path names its object (as on Linux, its write calls reach no path).
    pub(crate) fn save(
        &mut self,
        h: &Handle,
        writer: &str,
        txn: Option<TxnId>,
        at: &str,
        git: &impl Files,
    ) -> Saved {
        let Some(p) = self.named(h, at, git) else {
            return Saved::Orphaned(self.orphan(h, writer));
        };
        let cur = git.file(at, &p);
        let mut reserved = h.reserved.clone();
        let (lines, set) = self.rewrite(&cur, &p, &h.calls, writer, txn, &mut reserved);
        if set.is_empty() {
            return Saved::NoChange;
        }
        let new = Content {
            name: cur.name.clone(),
            lines,
        };
        Saved::Save(Save {
            target: h.target,
            writer: writer.to_string(),
            at: at.to_string(),
            files: BTreeMap::from([(p, new)]),
            set,
            open: Some(h.id),
        })
    }

    /// An open no path names at its close: its calls are dropped, and an
    /// `OrphanDropped` event names them (none if it made no call): the bytes dropped.
    fn orphan(&mut self, h: &Handle, writer: &str) -> Option<usize> {
        if h.calls.is_empty() {
            return None;
        }
        let bytes = h
            .calls
            .iter()
            .map(|c| match c {
                Write::Append(b) | Write::Pwrite { bytes: b, .. } => b.len(),
                Write::Truncate(_) => 0,
            })
            .sum();
        self.orphans.push(Orphan {
            handle: h.id,
            writer: writer.to_string(),
            path: h.path.clone(),
            bytes,
        });
        Some(bytes)
    }

    /// Rule 11, path call `h` run on `at` (its target's head now): one save writing its
    /// file's name line (an unlink a deleted version, a rename a new one, a rename-over also
    /// the replaced file's), and the path the file had when it ran. None when it changes
    /// nothing: its file is gone, or a rename to the path it has.
    pub(crate) fn path_save(
        &mut self,
        h: &Handle,
        writer: &str,
        txn: Option<TxnId>,
        at: &str,
        git: &impl Files,
    ) -> Option<(Save, String)> {
        let cur = git.file(at, &h.file);
        if !cur.name.live() {
            return None;
        }
        // The path it has when it runs: a deferred call runs later (R5.3).
        let from = cur.path().unwrap_or_default().to_string();
        let mut files = BTreeMap::new();
        let mut set = Vec::new();
        let name = |w: &mut Self, c: &Content, state: State| {
            let id = w.token(writer, txn);
            let e = Edit {
                path: c.name.line.clone(),
                line: c.name.line.clone(),
                token: id.clone(),
                gone: !matches!(state, State::Live(_)),
            };
            let mut n = c.clone();
            n.name = Line {
                id,
                line: c.name.line.clone(),
                over: Some(c.name.id.clone()),
                state,
            };
            (n, e)
        };
        fn put(
            files: &mut BTreeMap<String, Content>,
            set: &mut Vec<Edit>,
            (n, e): (Content, Edit),
        ) {
            files.insert(n.name.line.clone(), n);
            set.push(e);
        }
        match &h.kind {
            Kind::Unlink => {
                put(&mut files, &mut set, name(self, &cur, State::Gone));
                self.inodes.remove(&(h.target, h.file.clone()));
            }
            Kind::Rename { to, .. } => {
                if cur.path() == Some(to.as_str()) {
                    return None;
                }
                match lookup(git, at, to) {
                    Some(g) if fresh(&cur) => {
                        // The atomic save (git-model.md §7, a choice): a
                        // rename-over from a file created and never renamed
                        // since is a rewrite of the replaced file with the
                        // new file's bytes: `g` keeps its identity, its
                        // lines aligned with those bytes (rule 10), and the
                        // new file ends. The new file's object is now `g`'s:
                        // its opens follow it; `g`'s old object no path names.
                        let gc = git.file(at, &g);
                        let bytes = cur.bytes();
                        let mut calls = vec![Write::Truncate(0)];
                        if !bytes.is_empty() {
                            calls.push(Write::Pwrite { offset: 0, bytes });
                        }
                        let (lines, edits) =
                            self.rewrite(&gc, &g, &calls, writer, txn, &mut Vec::new());
                        if !edits.is_empty() {
                            files.insert(
                                g.clone(),
                                Content {
                                    name: gc.name.clone(),
                                    lines,
                                },
                            );
                            set.extend(edits);
                        }
                        put(&mut files, &mut set, name(self, &cur, State::Gone));
                        let o = self.inodes.remove(&(h.target, h.file.clone()));
                        self.inodes.remove(&(h.target, g.clone()));
                        if let Some(o) = o {
                            self.inodes.insert((h.target, g.clone()), o);
                        }
                        for x in self.handles.values_mut() {
                            if x.target == h.target && Some(x.object) == o {
                                x.file = g.clone();
                            }
                        }
                    }
                    over => {
                        put(
                            &mut files,
                            &mut set,
                            name(self, &cur, State::Live(to.clone())),
                        );
                        if let Some(g) = over {
                            let gc = git.file(at, &g);
                            put(
                                &mut files,
                                &mut set,
                                name(self, &gc, State::Replaced(h.file.clone())),
                            );
                            self.inodes.remove(&(h.target, g));
                        }
                    }
                }
            }
            Kind::Write => unreachable!("an open is not a path call"),
        }
        let save = Save {
            target: h.target,
            writer: writer.to_string(),
            at: at.to_string(),
            files,
            set,
            open: None,
        };
        Some((save, from))
    }

    /// Rule 11: `open(p, O_CREAT | O_EXCL | O_WRONLY)` of a path no file has at `at`: a new
    /// file, its identity the next of [`UNBORN`] (each taken once in the volume), made by
    /// one save of its name line, written over the unborn version, with no lines. The world
    /// commits it, then opens the file. None once every unborn identity is taken.
    pub(crate) fn create(
        &mut self,
        p: &str,
        writer: &str,
        txn: Option<TxnId>,
        target: Target,
        at: &str,
        git: &impl Files,
    ) -> Option<Save> {
        assert!(lookup(git, at, p).is_none(), "create of {p}: it exists");
        let f = UNBORN.get(self.next_unborn)?.to_string();
        self.next_unborn += 1;
        let cur = git.file(at, &f);
        let id = self.token(writer, txn);
        let mut n = cur.clone();
        n.name = Line {
            id: id.clone(),
            line: f.clone(),
            over: Some(cur.name.id.clone()),
            state: State::Live(p.to_string()),
        };
        let set = vec![Edit {
            path: f.clone(),
            line: f.clone(),
            token: id,
            gone: false,
        }];
        Some(Save {
            target,
            writer: writer.to_string(),
            at: at.to_string(),
            files: BTreeMap::from([(f, n)]),
            set,
            open: None,
        })
    }

    /// The calls of the save committed as `commit`, as the program made them.
    pub(crate) fn record_calls(&mut self, commit: String, calls: Vec<Write>) {
        self.save_calls.insert(commit, calls);
    }

    /// One of main's clients ended: it opens nothing after.
    pub(crate) fn end_main_client(&mut self, writer: &str) {
        self.ended_main.insert(writer.to_string());
    }

    /// Every `OrphanDropped` event.
    pub(crate) fn orphans(&self) -> &[Orphan] {
        &self.orphans
    }

    pub(crate) fn save_calls(&self) -> &BTreeMap<String, Vec<Write>> {
        &self.save_calls
    }

    pub(crate) fn ended_main(&self) -> &BTreeSet<String> {
        &self.ended_main
    }

    /// File `p`'s lines `cur` with byte calls replayed on them (rules 8 and
    /// 10): the new lines, and the versions they set. A new version holding
    /// exactly the text of a `reserved` id takes that id.
    fn rewrite(
        &mut self,
        cur: &Content,
        p: &str,
        calls: &[Write],
        writer: &str,
        txn: Option<TxnId>,
        reserved: &mut Vec<String>,
    ) -> (Vec<Line>, Vec<Edit>) {
        let mut lines = Vec::new();
        let mut set = Vec::new();
        for r in self.replay(cur, calls) {
            match r {
                Replayed::Kept(l) => lines.push(l),
                Replayed::New { line, over, state } => {
                    let text = match &state {
                        State::Live(t) => t.strip_suffix('\n').unwrap_or(t),
                        _ => "",
                    };
                    let id = match reserved.iter().position(|t| t == text) {
                        Some(i) => reserved.remove(i),
                        None => self.token(writer, txn),
                    };
                    let line = line.unwrap_or_else(|| id.clone());
                    set.push(Edit {
                        path: p.to_string(),
                        line: line.clone(),
                        token: id.clone(),
                        gone: !matches!(state, State::Live(_)),
                    });
                    lines.push(Line {
                        id,
                        line,
                        over,
                        state,
                    });
                }
            }
        }
        (lines, set)
    }

    /// Rules 8 and 10: an open's byte calls on `cur` (the head at close),
    /// line identities derived from the bytes. An open of appends only is
    /// exact: the kernel, and so the save, knows its bytes go after the end.
    /// If the file then ends in a partial last line (no final newline), the
    /// appended bytes up to and including their first newline finish that
    /// line: a new version of it, written over it, not a new line ("foo"
    /// and an append of "bar" is one line, "foobar"). Every further line the
    /// append wrote is a new line after the file's last. An open with any
    /// other call is a rewrite whichever call wrote the bytes (finding 37;
    /// the owner, 2026-11-07, "2. B"): the bytes all its calls leave are
    /// aligned with the lines once, at close ([`Recording::align`]). So a
    /// program's insert, delete or move comes out as the alignment reads
    /// its bytes, and a move as a delete plus an insert.
    fn replay(&self, cur: &Content, calls: &[Write]) -> Vec<Replayed> {
        let mut seq: Vec<Replayed> = cur.lines.iter().cloned().map(Replayed::Kept).collect();
        if !calls.iter().all(|c| matches!(c, Write::Append(_))) {
            return self.align(seq, &Write::apply_all(&cur.bytes(), calls));
        }
        for call in calls {
            let Write::Append(x) = call else {
                unreachable!("every call is an append here")
            };
            let mut rest = x.as_str();
            if let Some(k) = seq.iter().rposition(Replayed::live)
                && !seq[k].text().ends_with('\n')
                && !rest.is_empty()
            {
                let cut = rest.find('\n').map_or(rest.len(), |i| i + 1);
                let t = format!("{}{}", seq[k].text(), &rest[..cut]);
                seq[k] = seq[k].clone().version(State::Live(t));
                rest = &rest[cut..];
            }
            for l in split_lines(rest) {
                seq.push(Replayed::New {
                    line: None,
                    over: None,
                    state: State::Live(l),
                });
            }
        }
        seq
    }

    /// Rule 10's one alignment: the live lines of `seq` aligned with the
    /// program's bytes `image` (its lines, [`split_lines`]) by git's
    /// histogram diff ([`Recording::hunks`]): unchanged lines keep their
    /// versions, a changed region pairs old and new lines in order (each new
    /// one a version of its old one), the extra new ones are new lines and
    /// the extra old ones deleted (lines at a region's ends whose bytes did
    /// not change keep their versions); deleted versions stay where they
    /// were. A line's bytes include its newline, so adding or removing the
    /// final newline changes the last line.
    fn align(&self, seq: Vec<Replayed>, image: &str) -> Vec<Replayed> {
        let texts = split_lines(image);
        let lives: Vec<usize> = (0..seq.len()).filter(|k| seq[*k].live()).collect();
        let old: Vec<String> = lives.iter().map(|k| seq[*k].text()).collect();
        let mut instead: Vec<Vec<Replayed>> = lives.iter().map(|k| vec![seq[*k].clone()]).collect();
        let mut before: Vec<Vec<Replayed>> = vec![Vec::new(); old.len() + 1];
        for (os, on, ns, nn) in Self::hunks(&old, &texts) {
            // git may put lines whose bytes did not change at either end of
            // a region (among repeated lines): they keep their versions.
            let same = |i: usize, j: usize| old[i] == texts[j];
            let mut head = 0;
            while head < on.min(nn) && same(os + head, ns + head) {
                head += 1;
            }
            let mut tail = 0;
            while tail < (on - head).min(nn - head) && same(os + on - 1 - tail, ns + nn - 1 - tail)
            {
                tail += 1;
            }
            let (os, on, ns, nn) = (os + head, on - head - tail, ns + head, nn - head - tail);
            let n = on.min(nn);
            for j in 0..on {
                let r = seq[lives[os + j]].clone();
                instead[os + j] = if j < n {
                    vec![r.version(State::Live(texts[ns + j].clone()))]
                } else {
                    r.deleted().into_iter().collect()
                };
            }
            for t in &texts[ns + n..ns + nn] {
                before[os + on].push(Replayed::New {
                    line: None,
                    over: None,
                    state: State::Live(t.clone()),
                });
            }
        }
        let mut out = Vec::new();
        let mut k = 0;
        for r in seq {
            if r.live() {
                out.append(&mut before[k]);
                out.append(&mut instead[k]);
                k += 1;
            } else {
                out.push(r);
            }
        }
        out.append(&mut before[k]);
        out
    }

    /// Rule 10's alignment: git's histogram diff of the old lines' bytes
    /// against the new (as text, whatever bytes they hold), through the call the
    /// library shares (finding 41), as (old start, old count, new start, new count)
    /// per changed region, 0-based.
    fn hunks(old: &[String], new: &[String]) -> Vec<(usize, usize, usize, usize)> {
        agentfs_align::histogram(old.concat().as_bytes(), new.concat().as_bytes())
            .expect("the alignment's diff runs")
            .into_iter()
            .map(|h| (h.old_start, h.old_count, h.new_start, h.new_count))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every model file's root content, whatever the commit: no git.
    struct Roots;

    impl Files for Roots {
        fn file(&self, _at: &str, p: &str) -> Content {
            Content::root(p)
        }
    }

    const ROOT: Target = Target::Tree(TreeId::ROOT);

    fn client() -> Client {
        Client::Main("m".into())
    }

    fn line(id: &str, text: &str) -> Line {
        Line {
            id: id.into(),
            line: id.into(),
            over: None,
            state: State::Live(text.into()),
        }
    }

    #[test]
    fn an_append_finishes_a_partial_last_line_then_adds_lines() {
        let mut r = Recording::new();
        let cur = Content {
            name: line("a", "a"),
            lines: vec![line("a0", "foo")],
        };
        let calls = [Write::Append("bar\nbaz\n".into())];
        let (lines, set) = r.rewrite(&cur, "a", &calls, "m", None, &mut Vec::new());
        let texts: Vec<_> = lines.iter().filter_map(Line::text).collect();
        assert_eq!(texts, ["foobar\n", "baz\n"]);
        assert_eq!(lines[0].line, "a0", "the partial line's new version");
        assert_eq!(lines[0].over.as_deref(), Some("a0"));
        assert_eq!(lines[1].line, lines[1].id, "a new line");
        assert_eq!(set.len(), 2);
    }

    #[test]
    fn a_rewrite_is_aligned_with_the_lines_once() {
        let mut r = Recording::new();
        let cur = Content::root("a");
        let calls = [Write::Pwrite {
            offset: 2,
            bytes: "x\n".into(),
        }];
        let mut reserved = vec!["x".to_string()];
        let (lines, set) = r.rewrite(&cur, "a", &calls, "m", Some(3), &mut reserved);
        assert_eq!(lines.len(), cur.lines.len(), "one line changed in place");
        assert_eq!(
            set,
            [Edit {
                path: "a".into(),
                line: "a1".into(),
                token: "x".into(),
                gone: false,
            }],
            "the second line's new version takes the id reserved for its text"
        );
        assert!(reserved.is_empty());
        let (_, set) = r.rewrite(&cur, "a", &calls, "m", Some(3), &mut Vec::new());
        assert_eq!(set[0].token, "m.1~t3", "else the writer's next version");
    }

    #[test]
    fn a_close_saves_its_calls_or_nothing() {
        let mut r = Recording::new();
        let h = r.open(&client(), ROOT, "a".into(), "a", Kind::Write);
        r.write(
            h,
            Write::Pwrite {
                offset: 0,
                bytes: "-\n".into(),
            },
        );
        let same = r.remove(h).expect("open");
        assert!(matches!(
            r.save(&same, "m", None, "c", &Roots),
            Saved::NoChange
        ));
        let h = r.open(&client(), ROOT, "a".into(), "a", Kind::Write);
        r.write(h, Write::Append("new\n".into()));
        assert_eq!(r.view(h, "c", &Roots), format!("{}new\n", "-\n".repeat(6)));
        let open = r.remove(h).expect("open");
        let Saved::Save(save) = r.save(&open, "m", None, "c", &Roots) else {
            panic!("a save")
        };
        assert_eq!(save.open, Some(h));
        assert_eq!(save.files.keys().collect::<Vec<_>>(), ["a"]);
        assert_eq!(save.set.len(), 1);
    }

    #[test]
    fn an_open_of_an_unlinked_file_is_an_orphan() {
        let mut r = Recording::new();
        let h = r.open(&client(), ROOT, "b".into(), "b", Kind::Write);
        r.write(h, Write::Append("lost\n".into()));
        let u = r.open(&client(), ROOT, "b".into(), "b", Kind::Unlink);
        let call = r.remove(u).expect("a path call");
        let (save, from) = r.path_save(&call, "m", None, "c", &Roots).expect("a save");
        assert_eq!(from, "b");
        assert!(!save.files["b"].name.live(), "a deleted name line");
        let open = r.remove(h).expect("open");
        assert!(r.named(&open, "c", &Roots).is_none());
        assert!(matches!(
            r.save(&open, "m", None, "c", &Roots),
            Saved::Orphaned(Some(5))
        ));
        assert_eq!(r.orphans().len(), 1);
    }

    #[test]
    fn a_create_takes_the_next_unborn_identity() {
        let mut r = Recording::new();
        for (p, f) in [("new", "g"), ("other", "h")] {
            let save = r.create(p, "m", None, ROOT, "c", &Roots).expect("one left");
            assert_eq!(save.files.keys().collect::<Vec<_>>(), [f]);
            assert_eq!(save.files[f].path(), Some(p));
            assert_eq!(save.set[0].line, f);
        }
    }

    #[test]
    fn a_path_finds_its_file() {
        assert_eq!(lookup(&Roots, "c", "c").as_deref(), Some("c"));
        assert_eq!(lookup(&Roots, "c", "g"), None, "an unborn file has no path");
    }
}
