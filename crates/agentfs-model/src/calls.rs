//! The calls a test makes of a tree system, shared by the model and the real
//! system (VERIFYING.md, "Phase 1"; git-model.md §18, the first row).
//!
//! The scenario bodies assert on the model's internals, so they are not
//! ported. The model runs them unchanged and records each call with what each
//! tree showed after it ([`Recorded`]). [`replay`] makes the same calls of any
//! [`TreeCalls`], and that system must show the same.
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

/// Who makes a call: one of main's programs, or a fork's.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Caller {
    Main(String),
    Fork(usize),
}

/// A tree.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum TreeId {
    Main,
    Fork(usize),
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
    fn fork(&mut self) -> Result<usize, CallError>;
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
    /// R1.3: an open's own read.
    fn view(&self, h: Self::Open) -> Result<Vec<u8>, CallError>;
    /// R1.1: another reader.
    fn client_read(&self, c: &Caller, path: &str) -> Result<Option<Vec<u8>>, CallError>;
    fn paths(&self, t: TreeId) -> Result<Vec<String>, CallError>;
}

/// One call. `h` is the model's number for an open: [`replay`] keeps the map.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RecordedCall {
    Fork {
        id: usize,
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

/// One recorded call with what the trees showed after it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Recorded {
    pub call: RecordedCall,
    pub shown: Option<Shown>,
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
        Caller::Main(n) => format!("m:{}", hex(n.as_bytes())),
        Caller::Fork(f) => format!("f:{f}"),
    }
}

fn parse_caller(s: &str) -> Result<Caller, CallError> {
    if let Some(n) = s.strip_prefix("m:") {
        return String::from_utf8(unhex(n)?)
            .map(Caller::Main)
            .map_err(|e| CallError::new(e.to_string()));
    }
    s.strip_prefix("f:")
        .ok_or_else(|| CallError::new(format!("bad caller {s:?}")))
        .and_then(|f| number(f).map(|f| Caller::Fork(f as usize)))
}

fn tree(t: TreeId) -> String {
    match t {
        TreeId::Main => "main".into(),
        TreeId::Fork(f) => format!("f{f}"),
    }
}

fn parse_tree(s: &str) -> Result<TreeId, CallError> {
    if s == "main" {
        return Ok(TreeId::Main);
    }
    s.strip_prefix('f')
        .ok_or_else(|| CallError::new(format!("bad tree {s:?}")))
        .and_then(|f| number(f).map(|f| TreeId::Fork(f as usize)))
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
        RecordedCall::Fork { id } => format!("fork {id}"),
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
        ["fork", id] => RecordedCall::Fork {
            id: number(id)? as usize,
        },
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

/// One line per call, hex bytes, no serde: `<call> |` then, when the call has
/// one, what the trees showed.
pub fn render(trace: &[Recorded]) -> String {
    let mut out = String::new();
    for r in trace {
        out.push_str(&render_call(&r.call));
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
            Ok(Recorded {
                call: parse_call(call)?,
                shown,
            })
        })
        .collect()
}

fn compare_trees<T: TreeCalls>(t: &T, shown: &Shown) -> Result<(), String> {
    for (id, files) in &shown.trees {
        let paths = t.paths(*id).map_err(|e| e.to_string())?;
        let want: Vec<&String> = files.keys().collect();
        if paths.iter().collect::<Vec<_>>() != want {
            return Err(format!("{id:?} shows paths {paths:?}, the trace {want:?}"));
        }
        for (p, want) in files {
            let got = t.read(*id, p).map_err(|e| e.to_string())?;
            if got.as_deref() != Some(want.as_slice()) {
                return Err(format!(
                    "{id:?}:{p} shows {:?}, the trace {:?}",
                    got.as_deref().map(String::from_utf8_lossy),
                    String::from_utf8_lossy(want)
                ));
            }
        }
    }
    Ok(())
}

/// Makes the trace's calls of `t`. A failed call, a different [`Took`] or
/// different trees is an `Err` naming the call. `each` runs after every call,
/// for what the trait does not expose (the collisions, the invariants).
pub fn replay<T: TreeCalls>(
    trace: &[Recorded],
    t: &mut T,
    each: &mut dyn FnMut(&T, &Recorded) -> Result<(), String>,
) -> Result<(), String> {
    let mut opens: BTreeMap<u64, Option<T::Open>> = BTreeMap::new();
    for (i, r) in trace.iter().enumerate() {
        let at = |e: &dyn fmt::Display| format!("call {i} ({}): {e}", render_call(&r.call));
        let open = |opens: &BTreeMap<u64, Option<T::Open>>, h: u64| {
            opens
                .get(&h)
                .copied()
                .ok_or_else(|| at(&format!("no open {h}")))
        };
        match &r.call {
            RecordedCall::Fork { id } => {
                let got = t.fork().map_err(|e| at(&e))?;
                if got != *id {
                    return Err(at(&format!("the system's fork is {got}")));
                }
            }
            RecordedCall::OpenWrite { caller, path, h } => {
                let o = t.open_write(caller, path).map_err(|e| at(&e))?;
                opens.insert(*h, Some(o));
            }
            RecordedCall::OpenTrunc { caller, path, h } => {
                let o = t.open_trunc(caller, path).map_err(|e| at(&e))?;
                opens.insert(*h, Some(o));
            }
            RecordedCall::Create { caller, path, h } => {
                let o = t.create(caller, path).map_err(|e| at(&e))?;
                opens.insert(*h, Some(o));
            }
            RecordedCall::OpenGone { h } => {
                opens.insert(*h, None);
            }
            RecordedCall::Pwrite { h, offset, bytes } => {
                if let Some(o) = open(&opens, *h)? {
                    t.pwrite(o, *offset, bytes).map_err(|e| at(&e))?;
                }
            }
            RecordedCall::Append { h, bytes } => {
                if let Some(o) = open(&opens, *h)? {
                    t.append(o, bytes).map_err(|e| at(&e))?;
                }
            }
            RecordedCall::Truncate { h, size } => {
                if let Some(o) = open(&opens, *h)? {
                    t.truncate(o, *size).map_err(|e| at(&e))?;
                }
            }
            RecordedCall::Close { h } => {
                if let Some(o) = open(&opens, *h)? {
                    t.close(o).map_err(|e| at(&e))?;
                }
                opens.remove(h);
            }
            RecordedCall::Unlink { caller, path } => {
                t.unlink(caller, path).map_err(|e| at(&e))?;
            }
            RecordedCall::Rename { caller, from, to } => {
                t.rename(caller, from, to).map_err(|e| at(&e))?;
            }
            RecordedCall::EndClient { caller } => {
                t.end_client(caller).map_err(|e| at(&e))?;
            }
            RecordedCall::Take { tree, took } => {
                let got = t.take_next(*tree).map_err(|e| at(&e))?;
                if got != *took {
                    return Err(at(&format!("the system answered {got:?}")));
                }
            }
            RecordedCall::Crash => {
                t.crash().map_err(|e| at(&e))?;
                opens.clear();
            }
            RecordedCall::OutOfScope(why) => return Err(at(&format!("out of scope: {why}"))),
        }
        if let Some(shown) = &r.shown {
            compare_trees(t, shown).map_err(|e| at(&e))?;
        }
        each(t, r).map_err(|e| at(&e))?;
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
                    TreeId::Main,
                    BTreeMap::from([("a b".into(), b"-\n".to_vec())]),
                ),
                (TreeId::Fork(2), BTreeMap::from([("c".into(), Vec::new())])),
            ]),
            collisions: vec![ShownCollision {
                tree: TreeId::Main,
                path: "a".into(),
                lost_bytes: vec![b"x\n".to_vec(), Vec::new()],
                whole: true,
            }],
        };
        let calls = vec![
            RecordedCall::Fork { id: 1 },
            RecordedCall::OpenTrunc {
                caller: Caller::Main("m 1".into()),
                path: "a".into(),
                h: 7,
            },
            RecordedCall::Pwrite {
                h: 7,
                offset: 3,
                bytes: vec![0, 255, b'\n'],
            },
            RecordedCall::Take {
                tree: TreeId::Fork(1),
                took: Took::Waiting,
            },
            RecordedCall::Crash,
            RecordedCall::OutOfScope("drop"),
        ];
        let trace: Vec<Recorded> = calls
            .into_iter()
            .map(|call| Recorded {
                call,
                shown: Some(shown.clone()),
            })
            .chain([Recorded {
                call: RecordedCall::Take {
                    tree: TreeId::Main,
                    took: Took::Taken,
                },
                shown: None,
            }])
            .collect();
        assert_eq!(parse(&render(&trace)).unwrap(), trace);
        assert_eq!(scope_of(&trace), Scope::OutOfScope("drop"));
    }
}
