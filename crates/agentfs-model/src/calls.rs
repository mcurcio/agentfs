//! The calls a test makes of a tree system, shared by the model and the real
//! system (VERIFYING.md, "The model as the conformance suite"; git-model.md §18, the first row).
//!
//! The scenario bodies assert on the model's internals, so they are not
//! ported. The model runs them unchanged and records each call with what each
//! tree showed after it ([`Recorded`]). [`replay`] makes the same calls of any
//! [`TreeCalls`], and that system must show the same.
//!
//! [`replay`] is also a source for the recorder: it gives each call, with what the
//! system answered and showed, to a [`Sink`], and [`Recorder`] keeps them as a
//! trace. So what the real system did is a trace the model replays (VERIFYING.md,
//! "A regression found on the engine"). A failed call is kept with its errno.
//!
//! A take is one atomic step in the model, as in the real system (the owner,
//! 2026-11-08, finding 49: "B"): [`TreeCalls::take_next`] chooses its entry,
//! locks its files and writes them, and no call runs in between. So the trace
//! is the order the calls ran in, and the trees are compared after every call.
//!
//! This crate has no dependencies, so [`CallError`] is written by hand and the
//! trace format is [`render`] and [`parse`], one line per call with hex bytes.

use std::collections::BTreeMap;
use std::fmt;

/// Who makes a call: one of the root's programs, or a fork's.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Caller {
    Root(String),
    Fork(TreeId),
}

/// A program writing through a view: one of the root's clients (by writer
/// name), a fork's client, or another program in that fork. Only
/// [`Client::Fork`] begins a transaction and writes in it; [`Client::Outside`]
/// always writes in the fork's tree, and so meets a transaction's files
/// (R5.3). Both name the fork's one writer (rule 6).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Client {
    Root(String),
    Fork(TreeId),
    Outside(TreeId),
}

/// A transaction's number in its fork.
pub type TxnId = u32;

/// An open file's, or a deferred path call's, number.
pub type HandleId = u32;

/// A tree: its number. 0 is the root, the one tree with no upstream; each fork
/// takes the next number from 1, in the order made, never reused. Ordered by
/// number: the root first, forks ascending. Which tree a fork syncs from is the
/// system's answer, not the identity's.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TreeId(u32);

impl TreeId {
    /// The root.
    pub const ROOT: TreeId = TreeId(0);

    /// The tree numbered `n` (0 is the root).
    pub const fn new(n: u32) -> TreeId {
        TreeId(n)
    }

    /// Its number; the digest labels it `T<number>`.
    pub const fn number(self) -> u32 {
        self.0
    }
}

/// The bare number: every name built from a tree (`fork/<n>`, the writer
/// `c<n>`) reads as it did when forks were numbers.
impl fmt::Display for TreeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// The digest's label, `T<number>`, so logs and digests read alike.
impl fmt::Debug for TreeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "T{}", self.0)
    }
}

/// What a queue step did: it took an entry, every entry waits, or none is queued.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Took {
    Taken,
    Waiting,
    Empty,
}

/// A call that failed. `errno` is what a program would read, where there is one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CallError {
    pub errno: Option<i32>,
    pub message: String,
}

impl CallError {
    /// An error with no errno.
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            errno: None,
            message: message.into(),
        }
    }
}

impl fmt::Display for CallError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.errno {
            Some(n) => write!(f, "{} (errno {n})", self.message),
            None => f.write_str(&self.message),
        }
    }
}

impl std::error::Error for CallError {}

/// The calls, as plain named methods. The model implements it, and so do the
/// real library and a real mount (`crates/agentfs-conformance`).
pub trait TreeCalls {
    type Open: Copy + Ord + std::fmt::Debug;
    fn fork(&mut self) -> Result<TreeId, CallError>;
    fn open_write(&mut self, c: &Caller, path: &str) -> Result<Self::Open, CallError>;
    fn open_trunc(&mut self, c: &Caller, path: &str) -> Result<Self::Open, CallError>;
    fn create(&mut self, c: &Caller, path: &str) -> Result<Self::Open, CallError>;
    fn pwrite(&mut self, h: Self::Open, offset: u64, bytes: &[u8]) -> Result<(), CallError>;
    fn append(&mut self, h: Self::Open, bytes: &[u8]) -> Result<(), CallError>;
    fn truncate(&mut self, h: Self::Open, size: u64) -> Result<(), CallError>;
    fn close(&mut self, h: Self::Open) -> Result<(), CallError>;
    fn unlink(&mut self, c: &Caller, path: &str) -> Result<(), CallError>;
    fn rename(&mut self, c: &Caller, from: &str, to: &str) -> Result<(), CallError>;
    fn end_client(&mut self, c: &Caller) -> Result<(), CallError>;
    /// One take, whole: the first entry the tree's queue lets go, written.
    fn take_next(&mut self, t: TreeId) -> Result<Took, CallError>;
    fn crash(&mut self) -> Result<(), CallError>;
    fn read(&self, t: TreeId, path: &str) -> Result<Option<Vec<u8>>, CallError>;
    fn paths(&self, t: TreeId) -> Result<Vec<String>, CallError>;
}

/// One call. `h` is the model's number for an open: [`replay`] keeps the map.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RecordedCall {
    Fork {
        id: TreeId,
    },
    OpenWrite {
        caller: Caller,
        path: String,
        h: u64,
    },
    OpenTrunc {
        caller: Caller,
        path: String,
        h: u64,
    },
    Create {
        caller: Caller,
        path: String,
        h: u64,
    },
    /// An open whose file no path names in its tree when it is recorded (a
    /// transaction's new file): its calls reach no path there.
    OpenGone {
        h: u64,
    },
    Pwrite {
        h: u64,
        offset: u64,
        bytes: Vec<u8>,
    },
    Append {
        h: u64,
        bytes: Vec<u8>,
    },
    Truncate {
        h: u64,
        size: u64,
    },
    Close {
        h: u64,
    },
    Unlink {
        caller: Caller,
        path: String,
    },
    Rename {
        caller: Caller,
        from: String,
        to: String,
    },
    EndClient {
        caller: Caller,
    },
    Take {
        tree: TreeId,
        took: Took,
    },
    Crash,
    /// Something phase 1 does not build happened in this trace.
    OutOfScope(&'static str),
}

/// A call the system failed, as a trace keeps it: the errno a program reads, or
/// none. The message is the system's own and is not kept. Rendered after the call
/// as ` !<errno>`, or ` !-` without one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Failed {
    pub errno: Option<i32>,
}

impl From<&CallError> for Failed {
    fn from(e: &CallError) -> Failed {
        Failed { errno: e.errno }
    }
}

impl fmt::Display for Failed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.errno {
            Some(n) => write!(f, "errno {n}"),
            None => f.write_str("no errno"),
        }
    }
}

/// One recorded call with what the trees showed after it. `failed`: the call
/// failed; a failed open holds no open, so a later call on its `h` is a mismatch.
/// `head`, for a take that took an entry: the moved tree's head's `Agentfs-Cause`
/// and its number of parents, so a replay also compares the commit graph, a merge
/// against a fast-forward (R6.1).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Recorded {
    pub call: RecordedCall,
    pub failed: Option<Failed>,
    pub shown: Option<Shown>,
    pub head: Option<(String, usize)>,
}

/// The recorder's port: the one entry every source feeds, each call as it
/// returns, a failed call included, with what the system showed after it. A
/// sink keeps what it is given; it never calls a system and never judges.
/// Today's source is the harness ([`replay`]); the engine's own events feed the
/// same entry later (`docs/target-architecture/future/module-cut.md`).
pub trait Sink {
    fn record(&mut self, r: Recorded);
}

/// The recorder: the one sink. Keeps each record in the order it arrived;
/// [`render`] writes it as a trace.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Recorder {
    trace: Vec<Recorded>,
}

impl Recorder {
    /// The records so far, in order.
    pub fn trace(&self) -> &[Recorded] {
        &self.trace
    }

    pub fn into_trace(self) -> Vec<Recorded> {
        self.trace
    }
}

impl Sink for Recorder {
    fn record(&mut self, r: Recorded) {
        self.trace.push(r);
    }
}

/// What a system's history tells that its trees do not, read by a source after
/// each call. `system` is given so the model answers from itself; a real
/// system's history reads git and ignores it.
pub trait History<T: ?Sized> {
    /// Every collision so far, as [`gather_collisions`] gives them.
    fn collisions(&mut self, system: &T) -> Result<Vec<ShownCollision>, CallError>;
    /// Tree `t`'s head: its `Agentfs-Cause` trailer and its number of parents
    /// (R6.1, R6.2).
    fn head(&mut self, system: &T, t: TreeId) -> Result<(String, usize), CallError>;
}

/// What the trees show: each tree's files by path, and every collision so far.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Shown {
    pub trees: BTreeMap<TreeId, BTreeMap<String, Vec<u8>>>,
    pub collisions: Vec<ShownCollision>,
}

/// One file's lost lines in one tree: the lines that lost to a collision, or
/// (`whole`) the ones that lost only because their change lost whole (R3e).
/// `lost_bytes` holds live lines' bytes, sorted. Lost deletes and lost names
/// hold no bytes, so a real system and the model compare only through the
/// file's commit history, by each one's own invariants.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct ShownCollision {
    pub tree: TreeId,
    pub path: String,
    pub lost_bytes: Vec<Vec<u8>>,
    pub whole: bool,
}

/// Gathers per-line records into the form [`Shown`] compares: one entry per
/// (tree, path, whole), its bytes sorted, none with no bytes.
pub fn gather_collisions(lost: impl IntoIterator<Item = ShownCollision>) -> Vec<ShownCollision> {
    let mut by: BTreeMap<(TreeId, String, bool), Vec<Vec<u8>>> = BTreeMap::new();
    for c in lost {
        by.entry((c.tree, c.path, c.whole))
            .or_default()
            .extend(c.lost_bytes);
    }
    by.into_iter()
        .filter(|(_, bytes)| !bytes.is_empty())
        .map(|((tree, path, whole), mut lost_bytes)| {
            lost_bytes.sort();
            ShownCollision {
                tree,
                path,
                lost_bytes,
                whole,
            }
        })
        .collect()
}

/// Whether a trace stays inside what phase 1 builds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scope {
    Phase1,
    OutOfScope(&'static str),
}

/// A trace is out of phase 1 if it used transactions, `Client::Outside`,
/// nested forks, `Step::Drop` or a policy other than the default.
pub fn scope_of(trace: &[Recorded]) -> Scope {
    trace
        .iter()
        .find_map(|r| match r.call {
            RecordedCall::OutOfScope(why) => Some(Scope::OutOfScope(why)),
            _ => None,
        })
        .unwrap_or(Scope::Phase1)
}

const REASONS: [&str; 5] = ["transaction", "outside", "nested-fork", "drop", "policy"];

/// The reason a world marks its trace out of scope: one of the list [`parse`]
/// reads back.
pub fn out_of_scope(why: &'static str) -> RecordedCall {
    debug_assert!(REASONS.contains(&why));
    RecordedCall::OutOfScope(why)
}

fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2 + 1);
    s.push('x');
    for b in bytes {
        s.push(char::from_digit(u32::from(b >> 4), 16).unwrap_or('0'));
        s.push(char::from_digit(u32::from(b & 15), 16).unwrap_or('0'));
    }
    s
}

fn unhex(s: &str) -> Result<Vec<u8>, CallError> {
    let bad = || CallError::new(format!("bad hex {s:?}"));
    let digits = s.strip_prefix('x').ok_or_else(bad)?.as_bytes();
    if digits.len() % 2 != 0 {
        return Err(bad());
    }
    digits
        .chunks(2)
        .map(|p| {
            let d = |c: u8| char::from(c).to_digit(16).ok_or_else(bad);
            Ok((d(p[0])? * 16 + d(p[1])?) as u8)
        })
        .collect()
}

fn caller(c: &Caller) -> String {
    match c {
        Caller::Root(n) => format!("r:{}", hex(n.as_bytes())),
        Caller::Fork(f) => format!("f:{}", f.number()),
    }
}

fn parse_caller(s: &str) -> Result<Caller, CallError> {
    if let Some(n) = s.strip_prefix("r:") {
        return String::from_utf8(unhex(n)?)
            .map(Caller::Root)
            .map_err(|e| CallError::new(e.to_string()));
    }
    s.strip_prefix("f:")
        .ok_or_else(|| CallError::new(format!("bad caller {s:?}")))
        .and_then(fork_id)
        .map(Caller::Fork)
}

fn tree(t: TreeId) -> String {
    format!("t{}", t.number())
}

fn parse_tree(s: &str) -> Result<TreeId, CallError> {
    s.strip_prefix('t')
        .ok_or_else(|| CallError::new(format!("bad tree {s:?}")))
        .and_then(tree_id)
}

/// A tree's number as the trace writes it; one past `u32` is an error, never a wrap.
fn tree_id(s: &str) -> Result<TreeId, CallError> {
    let n = number(s)?;
    u32::try_from(n)
        .map(TreeId::new)
        .map_err(|_| CallError::new(format!("bad tree number {s:?}")))
}

/// A fork's number: a tree's, but never the root's.
fn fork_id(s: &str) -> Result<TreeId, CallError> {
    let t = tree_id(s)?;
    if t == TreeId::ROOT {
        return Err(CallError::new(format!("{s:?} names the root, not a fork")));
    }
    Ok(t)
}

fn number(s: &str) -> Result<u64, CallError> {
    s.parse()
        .map_err(|_| CallError::new(format!("bad number {s:?}")))
}

fn path_text(p: &str) -> String {
    hex(p.as_bytes())
}

fn parse_path(s: &str) -> Result<String, CallError> {
    String::from_utf8(unhex(s)?).map_err(|e| CallError::new(e.to_string()))
}

fn render_call(c: &RecordedCall) -> String {
    match c {
        RecordedCall::Fork { id } => format!("fork {}", id.number()),
        RecordedCall::OpenWrite { caller: c, path, h } => {
            format!("open {} {} {h}", caller(c), path_text(path))
        }
        RecordedCall::OpenTrunc { caller: c, path, h } => {
            format!("trunc-open {} {} {h}", caller(c), path_text(path))
        }
        RecordedCall::Create { caller: c, path, h } => {
            format!("create {} {} {h}", caller(c), path_text(path))
        }
        RecordedCall::OpenGone { h } => format!("open-gone {h}"),
        RecordedCall::Pwrite { h, offset, bytes } => format!("pwrite {h} {offset} {}", hex(bytes)),
        RecordedCall::Append { h, bytes } => format!("append {h} {}", hex(bytes)),
        RecordedCall::Truncate { h, size } => format!("truncate {h} {size}"),
        RecordedCall::Close { h } => format!("close {h}"),
        RecordedCall::Unlink { caller: c, path } => {
            format!("unlink {} {}", caller(c), path_text(path))
        }
        RecordedCall::Rename {
            caller: c,
            from,
            to,
        } => format!("rename {} {} {}", caller(c), path_text(from), path_text(to)),
        RecordedCall::EndClient { caller: c } => format!("end {}", caller(c)),
        RecordedCall::Take { tree: t, took } => {
            let took = match took {
                Took::Taken => "taken",
                Took::Waiting => "waiting",
                Took::Empty => "empty",
            };
            format!("take {} {took}", tree(*t))
        }
        RecordedCall::Crash => "crash".into(),
        RecordedCall::OutOfScope(why) => format!("out {why}"),
    }
}

fn parse_call(s: &str) -> Result<RecordedCall, CallError> {
    let f: Vec<&str> = s.split(' ').collect();
    let bad = || CallError::new(format!("bad call {s:?}"));
    Ok(match f.as_slice() {
        ["fork", id] => RecordedCall::Fork { id: fork_id(id)? },
        ["open", c, p, h] => RecordedCall::OpenWrite {
            caller: parse_caller(c)?,
            path: parse_path(p)?,
            h: number(h)?,
        },
        ["trunc-open", c, p, h] => RecordedCall::OpenTrunc {
            caller: parse_caller(c)?,
            path: parse_path(p)?,
            h: number(h)?,
        },
        ["create", c, p, h] => RecordedCall::Create {
            caller: parse_caller(c)?,
            path: parse_path(p)?,
            h: number(h)?,
        },
        ["open-gone", h] => RecordedCall::OpenGone { h: number(h)? },
        ["pwrite", h, o, b] => RecordedCall::Pwrite {
            h: number(h)?,
            offset: number(o)?,
            bytes: unhex(b)?,
        },
        ["append", h, b] => RecordedCall::Append {
            h: number(h)?,
            bytes: unhex(b)?,
        },
        ["truncate", h, n] => RecordedCall::Truncate {
            h: number(h)?,
            size: number(n)?,
        },
        ["close", h] => RecordedCall::Close { h: number(h)? },
        ["unlink", c, p] => RecordedCall::Unlink {
            caller: parse_caller(c)?,
            path: parse_path(p)?,
        },
        ["rename", c, a, b] => RecordedCall::Rename {
            caller: parse_caller(c)?,
            from: parse_path(a)?,
            to: parse_path(b)?,
        },
        ["end", c] => RecordedCall::EndClient {
            caller: parse_caller(c)?,
        },
        ["take", t, took] => RecordedCall::Take {
            tree: parse_tree(t)?,
            took: match *took {
                "taken" => Took::Taken,
                "waiting" => Took::Waiting,
                "empty" => Took::Empty,
                _ => return Err(bad()),
            },
        },
        ["crash"] => RecordedCall::Crash,
        ["out", why] => {
            RecordedCall::OutOfScope(REASONS.iter().copied().find(|r| r == why).ok_or_else(bad)?)
        }
        _ => return Err(bad()),
    })
}

fn render_shown(s: &Shown) -> String {
    let mut out = String::new();
    for (t, files) in &s.trees {
        out.push_str(" t ");
        out.push_str(&tree(*t));
        for (p, b) in files {
            out.push_str(&format!(" {}={}", path_text(p), hex(b)));
        }
    }
    for c in &s.collisions {
        out.push_str(&format!(
            " c {} {} {}",
            tree(c.tree),
            path_text(&c.path),
            u8::from(c.whole)
        ));
        for b in &c.lost_bytes {
            out.push(' ');
            out.push_str(&hex(b));
        }
    }
    out
}

fn parse_shown(s: &str) -> Result<Shown, CallError> {
    let mut shown = Shown::default();
    let mut f = s.split_whitespace().peekable();
    while let Some(kind) = f.next() {
        let bad = || CallError::new(format!("bad shown {s:?}"));
        match kind {
            "t" => {
                let t = parse_tree(f.next().ok_or_else(bad)?)?;
                let files = shown.trees.entry(t).or_default();
                while let Some(x) = f.next_if(|x| x.contains('=')) {
                    let (p, b) = x.split_once('=').ok_or_else(bad)?;
                    files.insert(parse_path(p)?, unhex(b)?);
                }
            }
            "c" => {
                let t = parse_tree(f.next().ok_or_else(bad)?)?;
                let path = parse_path(f.next().ok_or_else(bad)?)?;
                let whole = f.next().ok_or_else(bad)? == "1";
                let mut lost_bytes = Vec::new();
                while let Some(x) = f.next_if(|x| x.starts_with('x')) {
                    lost_bytes.push(unhex(x)?);
                }
                shown.collisions.push(ShownCollision {
                    tree: t,
                    path,
                    lost_bytes,
                    whole,
                });
            }
            _ => return Err(bad()),
        }
    }
    Ok(shown)
}

/// One line per call, hex bytes, no serde: `<call>`, then ` !<errno>` (` !-`
/// without one) when the call failed, then ` ^<cause>:<parents>` when the call
/// moved a tree's head, then ` |` and, when the call has one, what the trees
/// showed.
pub fn render(trace: &[Recorded]) -> String {
    let mut out = String::new();
    for r in trace {
        out.push_str(&render_call(&r.call));
        if let Some(f) = &r.failed {
            match f.errno {
                Some(n) => out.push_str(&format!(" !{n}")),
                None => out.push_str(" !-"),
            }
        }
        if let Some((cause, parents)) = &r.head {
            out.push_str(&format!(" ^{cause}:{parents}"));
        }
        if let Some(s) = &r.shown {
            out.push_str(" |");
            out.push_str(&render_shown(s));
        }
        out.push('\n');
    }
    out
}

/// What [`render`] wrote.
pub fn parse(text: &str) -> Result<Vec<Recorded>, CallError> {
    text.lines()
        .map(|line| {
            let (call, shown) = match line.split_once(" |") {
                Some((c, s)) => (c, Some(parse_shown(s)?)),
                None => (line, None),
            };
            let (call, head) = match call.split_once(" ^") {
                Some((c, h)) => {
                    let (cause, parents) = h
                        .split_once(':')
                        .ok_or_else(|| CallError::new(format!("bad head {h:?}")))?;
                    (c, Some((cause.to_owned(), number(parents)? as usize)))
                }
                None => (call, None),
            };
            let (call, failed) = match call.split_once(" !") {
                Some((c, "-")) => (c, Some(Failed { errno: None })),
                Some((c, n)) => {
                    let errno = n
                        .parse()
                        .map_err(|_| CallError::new(format!("bad errno {n:?}")))?;
                    (c, Some(Failed { errno: Some(errno) }))
                }
                None => (call, None),
            };
            Ok(Recorded {
                call: parse_call(call)?,
                failed,
                shown,
                head,
            })
        })
        .collect()
}

/// Every tree in `trees`, by path, as `t` shows it.
fn read_trees<T: TreeCalls>(
    t: &T,
    trees: &[TreeId],
) -> Result<BTreeMap<TreeId, BTreeMap<String, Vec<u8>>>, CallError> {
    let mut out = BTreeMap::new();
    for id in trees {
        let mut files = BTreeMap::new();
        for p in t.paths(*id)? {
            if let Some(bytes) = t.read(*id, &p)? {
                files.insert(p, bytes);
            }
        }
        out.insert(*id, files);
    }
    Ok(out)
}

/// The first tree the trace lists that `got` shows otherwise.
fn compare_trees(got: &Shown, want: &Shown) -> Result<(), String> {
    for (id, files) in &want.trees {
        let Some(shows) = got.trees.get(id) else {
            return Err(format!("{id:?} is no tree the system made"));
        };
        let (paths, want_paths): (Vec<&String>, Vec<&String>) =
            (shows.keys().collect(), files.keys().collect());
        if paths != want_paths {
            return Err(format!(
                "{id:?} shows paths {paths:?}, the trace {want_paths:?}"
            ));
        }
        for (p, want) in files {
            let got = shows.get(p);
            if got != Some(want) {
                return Err(format!(
                    "{id:?}:{p} shows {:?}, the trace {:?}",
                    got.map(|b| String::from_utf8_lossy(b)),
                    String::from_utf8_lossy(want)
                ));
            }
        }
    }
    if got.collisions != want.collisions {
        return Err(format!(
            "collisions: the system recorded {:?}, the trace {:?}",
            got.collisions, want.collisions
        ));
    }
    Ok(())
}

/// Makes one call of `t`: the call as the system answered it (a fork's number, a
/// take's [`Took`]) and its result. An `Err` is no call the system answered.
fn make_call<T: TreeCalls>(
    want: &RecordedCall,
    t: &mut T,
    opens: &mut BTreeMap<u64, Option<T::Open>>,
    trees: &mut Vec<TreeId>,
) -> Result<(RecordedCall, Result<(), CallError>), String> {
    let open = |opens: &BTreeMap<u64, Option<T::Open>>, h: u64| {
        opens.get(&h).copied().ok_or_else(|| format!("no open {h}"))
    };
    let opened = |opens: &mut BTreeMap<u64, Option<T::Open>>, h: u64, o| match o {
        Ok(o) => {
            opens.insert(h, Some(o));
            Ok(())
        }
        Err(e) => {
            opens.remove(&h);
            Err(e)
        }
    };
    let result = match want {
        RecordedCall::Fork { .. } => {
            return Ok(match t.fork() {
                Ok(id) => {
                    trees.push(id);
                    (RecordedCall::Fork { id }, Ok(()))
                }
                Err(e) => (want.clone(), Err(e)),
            });
        }
        RecordedCall::OpenWrite { caller, path, h } => {
            opened(opens, *h, t.open_write(caller, path))
        }
        RecordedCall::OpenTrunc { caller, path, h } => {
            opened(opens, *h, t.open_trunc(caller, path))
        }
        RecordedCall::Create { caller, path, h } => opened(opens, *h, t.create(caller, path)),
        RecordedCall::OpenGone { h } => {
            opens.insert(*h, None);
            Ok(())
        }
        RecordedCall::Pwrite { h, offset, bytes } => match open(opens, *h)? {
            Some(o) => t.pwrite(o, *offset, bytes),
            None => Ok(()),
        },
        RecordedCall::Append { h, bytes } => match open(opens, *h)? {
            Some(o) => t.append(o, bytes),
            None => Ok(()),
        },
        RecordedCall::Truncate { h, size } => match open(opens, *h)? {
            Some(o) => t.truncate(o, *size),
            None => Ok(()),
        },
        RecordedCall::Close { h } => {
            let closed = match open(opens, *h)? {
                Some(o) => t.close(o),
                None => Ok(()),
            };
            opens.remove(h);
            closed
        }
        RecordedCall::Unlink { caller, path } => t.unlink(caller, path),
        RecordedCall::Rename { caller, from, to } => t.rename(caller, from, to),
        RecordedCall::EndClient { caller } => t.end_client(caller),
        RecordedCall::Take { tree, .. } => {
            return Ok(match t.take_next(*tree) {
                Ok(took) => (RecordedCall::Take { tree: *tree, took }, Ok(())),
                Err(e) => (want.clone(), Err(e)),
            });
        }
        RecordedCall::Crash => {
            let crashed = t.crash();
            opens.clear();
            crashed
        }
        RecordedCall::OutOfScope(why) => return Err(format!("out of scope: {why}")),
    };
    Ok((want.clone(), result))
}

/// How the system's record differs from the trace's, before what it showed.
fn compare_call(got: &Recorded, want: &Recorded, error: Option<&CallError>) -> Result<(), String> {
    match (&want.failed, error) {
        (None, Some(e)) => return Err(e.to_string()),
        (Some(f), None) => return Err(format!("the system did it, the trace failed ({f})")),
        (Some(f), Some(e)) if *f != Failed::from(e) => {
            return Err(format!("{e}, the trace failed ({f})"));
        }
        _ => {}
    }
    match (&got.call, &want.call) {
        (RecordedCall::Fork { id }, RecordedCall::Fork { id: want }) if id != want => {
            Err(format!("the system's fork is {id:?}"))
        }
        (RecordedCall::Take { took, .. }, RecordedCall::Take { took: want, .. })
            if took != want =>
        {
            Err(format!("the system answered {took:?}"))
        }
        _ => Ok(()),
    }
}

/// Source A, the harness: makes the trace's calls of `system` and gives `sink`
/// each one with what the system returned and then showed: every tree made so
/// far by path, `history`'s collisions, and for a take that took, the tree's
/// head. A fork's `id` and a take's `took` are the system's answers; open
/// numbers `h` are the trace's.
///
/// Stops at the first record that differs from the trace's, after giving it to
/// `sink`: failed or not and the errno, a take's `took`, a fork's number, a
/// listed tree's paths or bytes, the collisions (both only when the trace's
/// record has a shown part), the head (when the trace has one). Then `each`
/// runs, for the real system's own invariants. Errors name the call, "call {i}
/// ({call}): {difference}". `OpenGone` makes no call; `OutOfScope` is an error,
/// and so is a call the trace cannot make (an open number it never opened): the
/// sink gets neither, since no system answered.
///
/// After every call it reads every tree made so far and the collisions, whether
/// or not the trace lists them, so the record is whole: a read that fails fails
/// the call, and each call costs a read of every tree.
pub fn replay<T: TreeCalls, H: History<T>, S: Sink + ?Sized>(
    trace: &[Recorded],
    system: &mut T,
    history: &mut H,
    sink: &mut S,
    each: &mut dyn FnMut(&T, &Recorded) -> Result<(), String>,
) -> Result<(), String> {
    let mut opens: BTreeMap<u64, Option<T::Open>> = BTreeMap::new();
    let mut trees = vec![TreeId::ROOT];
    for (i, want) in trace.iter().enumerate() {
        let at = |e: &dyn fmt::Display| format!("call {i} ({}): {e}", render_call(&want.call));
        let (call, result) =
            make_call(&want.call, system, &mut opens, &mut trees).map_err(|e| at(&e))?;
        let shown = read_trees(system, &trees).and_then(|trees| {
            Ok(Shown {
                trees,
                collisions: history.collisions(system)?,
            })
        });
        let head = match call {
            RecordedCall::Take {
                tree,
                took: Took::Taken,
            } => history.head(system, tree).map(Some),
            _ => Ok(None),
        };
        let got = Recorded {
            call,
            failed: result.as_ref().err().map(Failed::from),
            shown: shown.as_ref().ok().cloned(),
            head: head.as_ref().ok().cloned().flatten(),
        };
        sink.record(got.clone());
        compare_call(&got, want, result.as_ref().err()).map_err(|e| at(&e))?;
        let shown = shown.map_err(|e| at(&e))?;
        if let Some(want) = &want.shown {
            compare_trees(&shown, want).map_err(|e| at(&e))?;
        }
        let head = head.map_err(|e| at(&e))?;
        if let (Some(want), RecordedCall::Take { tree, .. }) = (&want.head, &got.call)
            && head.as_ref() != Some(want)
        {
            let head = head.map_or_else(|| "none".to_owned(), |h| format!("{h:?}"));
            return Err(at(&format!(
                "history_shape: {tree:?}'s head is {head}, the trace {want:?}"
            )));
        }
        each(system, want).map_err(|e| at(&e))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_trace_reads_back_as_it_was_written() {
        let shown = Shown {
            trees: BTreeMap::from([
                (
                    TreeId::ROOT,
                    BTreeMap::from([("a b".into(), b"-\n".to_vec())]),
                ),
                (TreeId::new(2), BTreeMap::from([("c".into(), Vec::new())])),
            ]),
            collisions: vec![ShownCollision {
                tree: TreeId::ROOT,
                path: "a".into(),
                lost_bytes: vec![b"x\n".to_vec(), Vec::new()],
                whole: true,
            }],
        };
        let calls = vec![
            RecordedCall::Fork { id: TreeId::new(1) },
            RecordedCall::OpenTrunc {
                caller: Caller::Root("m 1".into()),
                path: "a".into(),
                h: 7,
            },
            RecordedCall::Pwrite {
                h: 7,
                offset: 3,
                bytes: vec![0, 255, b'\n'],
            },
            RecordedCall::Take {
                tree: TreeId::new(1),
                took: Took::Waiting,
            },
            RecordedCall::Crash,
            RecordedCall::OutOfScope("drop"),
        ];
        let trace: Vec<Recorded> = calls
            .into_iter()
            .map(|call| Recorded {
                call,
                failed: None,
                shown: Some(shown.clone()),
                head: None,
            })
            .chain([
                Recorded {
                    call: RecordedCall::Take {
                        tree: TreeId::ROOT,
                        took: Took::Taken,
                    },
                    failed: None,
                    shown: None,
                    head: Some(("Sync".into(), 2)),
                },
                Recorded {
                    call: RecordedCall::Unlink {
                        caller: Caller::Fork(TreeId::new(1)),
                        path: "a".into(),
                    },
                    failed: Some(Failed { errno: Some(2) }),
                    shown: Some(shown.clone()),
                    head: None,
                },
                Recorded {
                    call: RecordedCall::Crash,
                    failed: Some(Failed { errno: None }),
                    shown: None,
                    head: None,
                },
            ])
            .collect();
        let text = render(&trace);
        assert!(text.contains(" !2 |"), "{text}");
        assert!(text.contains("crash !-\n"), "{text}");
        assert_eq!(parse(&render(&trace)).unwrap(), trace);
        assert_eq!(scope_of(&trace), Scope::OutOfScope("drop"));
    }

    #[test]
    fn a_tree_number_past_u32_or_a_root_fork_is_an_error() {
        assert_eq!(parse_tree("t0").unwrap(), TreeId::ROOT);
        assert!(
            parse_tree("t4294967296").is_err(),
            "it would wrap to the root"
        );
        assert!(parse_caller("f:0").is_err(), "the root is no fork");
        assert!(parse_caller("f:4294967297").is_err());
        assert_eq!(parse_caller("f:3").unwrap(), Caller::Fork(TreeId::new(3)));
        assert!(parse_call("fork 0").is_err());
    }

    /// A system the replay's tests script: the root shows `a`, an open of any
    /// other path fails with `ENOENT`, and takes answer in turn from `takes`.
    struct Scripted {
        forks: u32,
        takes: Vec<Took>,
        opens: u64,
    }

    fn scripted(takes: &[Took]) -> Scripted {
        Scripted {
            forks: 0,
            takes: takes.iter().rev().copied().collect(),
            opens: 0,
        }
    }

    const ENOENT: i32 = 2;

    impl TreeCalls for Scripted {
        type Open = u64;
        fn fork(&mut self) -> Result<TreeId, CallError> {
            self.forks += 1;
            Ok(TreeId::new(self.forks))
        }
        fn open_write(&mut self, _: &Caller, path: &str) -> Result<u64, CallError> {
            if path != "a" {
                return Err(CallError {
                    errno: Some(ENOENT),
                    message: format!("open of {path}"),
                });
            }
            self.opens += 1;
            Ok(self.opens)
        }
        fn open_trunc(&mut self, c: &Caller, path: &str) -> Result<u64, CallError> {
            self.open_write(c, path)
        }
        fn create(&mut self, c: &Caller, path: &str) -> Result<u64, CallError> {
            self.open_write(c, path)
        }
        fn pwrite(&mut self, _: u64, _: u64, _: &[u8]) -> Result<(), CallError> {
            Ok(())
        }
        fn append(&mut self, _: u64, _: &[u8]) -> Result<(), CallError> {
            Ok(())
        }
        fn truncate(&mut self, _: u64, _: u64) -> Result<(), CallError> {
            Ok(())
        }
        fn close(&mut self, _: u64) -> Result<(), CallError> {
            Ok(())
        }
        fn unlink(&mut self, _: &Caller, _: &str) -> Result<(), CallError> {
            Ok(())
        }
        fn rename(&mut self, _: &Caller, _: &str, _: &str) -> Result<(), CallError> {
            Ok(())
        }
        fn end_client(&mut self, _: &Caller) -> Result<(), CallError> {
            Ok(())
        }
        fn take_next(&mut self, _: TreeId) -> Result<Took, CallError> {
            Ok(self.takes.pop().unwrap_or(Took::Empty))
        }
        fn crash(&mut self) -> Result<(), CallError> {
            Ok(())
        }
        fn read(&self, _: TreeId, path: &str) -> Result<Option<Vec<u8>>, CallError> {
            Ok((path == "a").then(|| b"-\n".to_vec()))
        }
        fn paths(&self, _: TreeId) -> Result<Vec<String>, CallError> {
            Ok(vec!["a".into()])
        }
    }

    /// No collisions; every head a sync's merge.
    struct Merges;

    impl History<Scripted> for Merges {
        fn collisions(&mut self, _: &Scripted) -> Result<Vec<ShownCollision>, CallError> {
            Ok(Vec::new())
        }
        fn head(&mut self, _: &Scripted, _: TreeId) -> Result<(String, usize), CallError> {
            Ok(("Sync".into(), 2))
        }
    }

    fn call(call: RecordedCall) -> Recorded {
        Recorded {
            call,
            failed: None,
            shown: None,
            head: None,
        }
    }

    fn take(tree: TreeId, took: Took) -> RecordedCall {
        RecordedCall::Take { tree, took }
    }

    fn open(path: &str, h: u64) -> RecordedCall {
        RecordedCall::OpenWrite {
            caller: Caller::Root("w".into()),
            path: path.into(),
            h,
        }
    }

    fn enoent(r: Recorded) -> Recorded {
        Recorded {
            failed: Some(Failed {
                errno: Some(ENOENT),
            }),
            ..r
        }
    }

    fn run(trace: &[Recorded], takes: &[Took]) -> (Result<(), String>, Vec<Recorded>) {
        let mut sink = Recorder::default();
        let result = replay(
            trace,
            &mut scripted(takes),
            &mut Merges,
            &mut sink,
            &mut |_, _| Ok(()),
        );
        (result, sink.into_trace())
    }

    /// The sink gets every call with the system's answers and what it showed: the
    /// fork's number, a failed call, a head only for a take that took.
    #[test]
    fn the_sink_gets_each_call_as_the_system_answered_it() {
        let t1 = TreeId::new(1);
        let trace = [
            call(RecordedCall::Fork { id: t1 }),
            enoent(call(open("gone", 1))),
            call(take(t1, Took::Taken)),
            call(take(TreeId::ROOT, Took::Waiting)),
        ];
        let (result, got) = run(&trace, &[Took::Taken, Took::Waiting]);
        assert_eq!(result, Ok(()));
        let files = BTreeMap::from([("a".to_owned(), b"-\n".to_vec())]);
        let shown = Shown {
            trees: BTreeMap::from([(TreeId::ROOT, files.clone()), (t1, files)]),
            collisions: Vec::new(),
        };
        let want: Vec<Recorded> = trace
            .iter()
            .map(|r| Recorded {
                shown: Some(shown.clone()),
                head: (r.call == take(t1, Took::Taken)).then(|| ("Sync".into(), 2)),
                ..r.clone()
            })
            .collect();
        assert_eq!(got, want);
    }

    /// The replay stops at the first difference, after the sink has it.
    #[test]
    fn the_replay_stops_after_the_first_difference() {
        let trace = [
            call(take(TreeId::ROOT, Took::Waiting)),
            call(take(TreeId::ROOT, Took::Empty)),
        ];
        let (result, got) = run(&trace, &[Took::Taken, Took::Empty]);
        assert_eq!(
            result,
            Err("call 0 (take t0 waiting): the system answered Taken".into())
        );
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].call, take(TreeId::ROOT, Took::Taken));
        assert_eq!(got[0].head, Some(("Sync".into(), 2)));
    }

    /// A failed call passes only with the trace's errno, and a later call on a
    /// failed open is a mismatch.
    #[test]
    fn a_failed_call_must_fail_with_the_traces_errno() {
        let other = Recorded {
            failed: Some(Failed { errno: Some(13) }),
            ..call(open("gone", 1))
        };
        let (result, _) = run(&[other], &[]);
        assert!(result.unwrap_err().contains("the trace failed (errno 13)"));
        let (result, _) = run(&[call(open("gone", 1))], &[]);
        assert!(result.unwrap_err().contains("(errno 2)"));
        let (result, got) = run(&[enoent(call(open("a", 1)))], &[]);
        assert!(result.unwrap_err().contains("the system did it"));
        assert_eq!(got[0].failed, None);
        let after = [
            enoent(call(open("gone", 1))),
            call(RecordedCall::Close { h: 1 }),
        ];
        let (result, _) = run(&after, &[]);
        assert_eq!(result, Err("call 1 (close 1): no open 1".into()));
    }
}
