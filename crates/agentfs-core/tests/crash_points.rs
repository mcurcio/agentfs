//! Crash injection at every engine failpoint (design §18 test 3; the crash
//! model of docs/architecture.md). Built only with the `failpoints`
//! feature: `cargo test -p agentfs-core --features failpoints --test
//! crash_points`. No FUSE and no root.
//!
//! A scripted workload (writes, an overwrite, a removal, a directory move,
//! LFS content, a commit after every few records, a push after every
//! commit to a local remote `origin`, and a remote `side` that someone
//! else advances, so its pushes take the divergence path of §11.3) runs
//! in a child process: this test binary again, with `CRASH_CHILD` set.
//! For every engine failpoint the child is:
//!
//! - **killed** (SIGKILL to itself) at the point's 1st and 3rd hit; or
//! - **failed** (the step returns an I/O error) there, and carries on as
//!   the daemon would: a failed record is made again by the next
//!   boundary, a failed push is retried, and a failed commit is retried
//!   by the next commit tick, after the records in between; or
//! - for the steps after a commit is in history (ref, sync, truncation,
//!   removal of copies, push state), **failed once and then killed** in
//!   the next commit, because a failure that in-process state papers over
//!   (a journal still appending to an unlinked file) only shows after a
//!   restart.
//!
//! The child appends a promise to an fsynced log after each call that
//! returned success. The parent then recovers as the next `agentfs`
//! process would (open the volume, commit, push `origin`) and checks the
//! oracle (`support/crash_oracle.rs`): nothing promised is lost, the step
//! in flight landed whole or not at all, nothing is committed twice or out
//! of order, the journal and `pending/` are empty, `origin` holds exactly
//! the head, `side` holds at most one saved head, and both repositories
//! pass `git fsck --full --strict`. A baseline run with no failure checks
//! that every engine point is reached, so a renamed or unreachable point
//! fails here.

#![cfg(feature = "failpoints")]

#[path = "support/crash_oracle.rs"]
mod crash_oracle;

use std::collections::BTreeMap;
use std::fs;
use std::io::Read as _;
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use agentfs_core::events::NullSink;
use agentfs_core::failpoints::POINTS;
use agentfs_core::identity::Context;
use agentfs_core::paths::RepoPath;
use agentfs_core::push::{PushOutcome, RemotePusher};
use agentfs_core::record::{Caller, Trigger};
use agentfs_core::telemetry::Telemetry;
use agentfs_core::volume::{self, Committer, Input, MoveOutcome, Recorder, StateDir};
use git2::{Oid, Repository, Signature};

use crash_oracle::{Mode, Model, PromiseLog};

const BRANCH: &str = "refs/heads/agentfs/vol";
const VOLUME: &str = "vol";
/// Content over this many bytes is stored with LFS (design §7).
const LFS_THRESHOLD: usize = 64;

/// The failpoints this workload passes. The FUSE, history service,
/// patch and repack points have their own harnesses (crates/agentfs/tests).
const ENGINE_POINTS: &[&str] = &[
    "pending.after_freeze",
    "journal.before_write",
    "journal.before_fsync",
    "journal.after_fsync",
    "record.after_journal",
    "lfs.after_cache_insert",
    "commit.after_tree",
    "commit.sync_objects",
    "commit.after_object",
    "commit.after_ref",
    "commit.after_sync",
    "journal.truncate.after_tmp",
    "journal.truncate.after_rename",
    "commit.before_pending_remove",
    "pending.remove.between",
    "push.after_lfs_upload",
    "push.after_remote_update",
    "push.diverged.after_pending",
    "push.diverged.after_ref",
    "state.json.before_rename",
];

#[derive(Debug, Clone)]
enum Op {
    Write(&'static str, Vec<u8>),
    Remove(&'static str),
    /// A directory rename, recorded as one move (design §3.1).
    Move(&'static str, &'static str),
    Commit,
    Push,
    /// Someone else advances `side`'s branch.
    Diverge,
}

fn small(text: &str) -> Vec<u8> {
    text.as_bytes().to_vec()
}

fn large(version: u8) -> Vec<u8> {
    let mut v = vec![b'L'; LFS_THRESHOLD * 3];
    v.push(b'0' + version);
    v
}

fn workload() -> Vec<Op> {
    use Op::*;
    vec![
        Write("a.txt", small("a1")),
        Write("d/x", small("x1")),
        Write("d/y", small("y1")),
        Commit,
        Push,
        Write("a.txt", small("a2")),
        Write("big.bin", large(1)),
        Remove("d/x"),
        Commit,
        Push,
        Move("d", "e"),
        Write("e/z", small("z1")),
        Write("a.txt", small("a3")),
        Commit,
        Push,
        Diverge,
        Write("b.txt", small("b1")),
        Write("big.bin", large(2)),
        Remove("a.txt"),
        Commit,
        Push,
        Write("c.txt", small("c1")),
        Write("e/y", small("y2")),
        Write("a.txt", small("a4")),
        Commit,
        Push,
    ]
}

fn model(ops: &[Op]) -> Model {
    let mut model = Model::new();
    for op in ops {
        match op {
            Op::Write(path, content) => model.step(|t| {
                t.insert((*path).to_owned(), content.clone());
            }),
            Op::Remove(path) => model.step(|t| {
                t.remove(*path);
            }),
            Op::Move(from, to) => model.step(|t| {
                let prefix = format!("{from}/");
                let moved: Vec<String> = t
                    .keys()
                    .filter(|k| k.starts_with(&prefix))
                    .cloned()
                    .collect();
                for k in moved {
                    let v = t.remove(&k).unwrap();
                    t.insert(format!("{to}/{}", &k[prefix.len()..]), v);
                }
            }),
            Op::Commit | Op::Push | Op::Diverge => {}
        }
    }
    model
}

struct Env {
    dir: PathBuf,
}

impl Env {
    fn new(dir: &Path) -> Self {
        let env = Self {
            dir: dir.to_owned(),
        };
        fs::create_dir_all(env.tree()).unwrap();
        volume::init(&env.state_dir(), VOLUME, None).unwrap();
        for remote in [env.origin(), env.side()] {
            Repository::init_bare(remote).unwrap();
        }
        let mut config = git2::Config::open(&env.state().join("git/config")).unwrap();
        config
            .set_str("remote.origin.url", env.origin().to_str().unwrap())
            .unwrap();
        config
            .set_str("remote.side.url", env.side().to_str().unwrap())
            .unwrap();
        config
            .set_str("agentfs.lfsThreshold", &LFS_THRESHOLD.to_string())
            .unwrap();
        env
    }

    fn state(&self) -> PathBuf {
        self.dir.join("state")
    }

    fn state_dir(&self) -> StateDir {
        StateDir::new(self.state())
    }

    fn tree(&self) -> PathBuf {
        self.dir.join("tree")
    }

    fn origin(&self) -> PathBuf {
        self.dir.join("origin.git")
    }

    fn side(&self) -> PathBuf {
        self.dir.join("side.git")
    }

    fn promises(&self) -> PromiseLog {
        PromiseLog::new(self.dir.join("promises"))
    }

    fn open(&self) -> (Recorder, Committer) {
        let context = Context {
            volume: VOLUME.into(),
            replica: "crash".into(),
            ..Context::default()
        };
        let (recorder, committer, _) = volume::open(
            &self.state_dir(),
            &self.tree(),
            context,
            Arc::new(NullSink),
            &Telemetry::noop(),
        )
        .expect("opening the volume");
        (recorder, committer)
    }

    fn pusher(&self, committer: &Committer, name: &str) -> RemotePusher {
        let settings = committer.history().settings();
        let remote = settings
            .remotes
            .iter()
            .find(|r| r.name == name)
            .expect("remote configured");
        RemotePusher::open(
            &self.state_dir(),
            settings,
            remote,
            Arc::new(NullSink),
            &Telemetry::noop(),
        )
        .expect("opening the pusher")
    }
}

fn caller() -> Caller {
    Caller {
        uid: rustix::process::getuid().as_raw(),
        gid: rustix::process::getgid().as_raw(),
        pid: std::process::id(),
    }
}

/// Retries `f` a few times, as the next boundary or the commit loop would.
fn retry<T, E: std::fmt::Display>(what: &str, mut f: impl FnMut() -> Result<T, E>) -> T {
    let mut last = String::new();
    for _ in 0..5 {
        match f() {
            Ok(v) => return v,
            Err(e) => {
                eprintln!("crash child: {what} failed, retrying: {e}");
                last = e.to_string();
            }
        }
    }
    panic!("{what} kept failing: {last}");
}

fn push_until_ok(pusher: &mut RemotePusher) -> Option<Oid> {
    for _ in 0..5 {
        match pusher.push_once() {
            PushOutcome::Pushed { to, .. } => return Some(to),
            PushOutcome::UpToDate { head } => return Some(head),
            PushOutcome::NothingToPush => return None,
            other => eprintln!(
                "crash child: push {} failed, retrying: {other:?}",
                pusher.name()
            ),
        }
    }
    panic!("pushing {} kept failing", pusher.name());
}

/// Someone else commits on `side`'s copy of the branch.
fn diverge(side: &Path) {
    let repo = Repository::open_bare(side).unwrap();
    let theirs = repo
        .find_reference(BRANCH)
        .unwrap()
        .peel_to_commit()
        .unwrap();
    let sig = Signature::now("someone else", "else@example.invalid").unwrap();
    let new = repo
        .commit(
            None,
            &sig,
            &sig,
            "out of band",
            &theirs.tree().unwrap(),
            &[&theirs],
        )
        .unwrap();
    repo.reference(BRANCH, new, true, "out of band").unwrap();
}

/// The workload, run by the child.
fn run_workload(env: &Env) {
    let (recorder, committer) = env.open();
    let mut origin = env.pusher(&committer, "origin");
    let mut side = env.pusher(&committer, "side");
    let promises = env.promises();
    let tree = env.tree();
    let mut step = 0;
    for op in workload() {
        match op {
            Op::Write(path, content) => {
                step += 1;
                let backing = tree.join(path);
                fs::create_dir_all(backing.parent().unwrap()).unwrap();
                fs::write(&backing, &content).unwrap();
                retry("record", || {
                    recorder.record(
                        Trigger::Close,
                        caller(),
                        vec![Input::Write {
                            path: RepoPath::new(path).unwrap(),
                            backing: backing.clone(),
                            executable: false,
                        }],
                    )
                });
                promises.promise(&format!("R {step}"));
            }
            Op::Remove(path) => {
                step += 1;
                let _ = fs::remove_file(tree.join(path));
                retry("record", || {
                    recorder.record(
                        Trigger::Unlink,
                        caller(),
                        vec![Input::Remove {
                            path: RepoPath::new(path).unwrap(),
                        }],
                    )
                });
                promises.promise(&format!("R {step}"));
            }
            Op::Move(from, to) => {
                step += 1;
                if tree.join(from).exists() {
                    fs::rename(tree.join(from), tree.join(to)).unwrap();
                }
                let outcome = retry("record_move", || {
                    recorder.record_move(
                        Trigger::Rename,
                        caller(),
                        RepoPath::new(from).unwrap(),
                        RepoPath::new(to).unwrap(),
                    )
                });
                assert!(matches!(outcome, MoveOutcome::Journaled(_)), "{outcome:?}");
                promises.promise(&format!("R {step}"));
            }
            Op::Commit => {
                // As the commit loop does: a failed pass is retried on the
                // next tick, after whatever was recorded in between.
                match committer.commit_pending() {
                    Ok(_) => promises.promise(&format!("C {step}")),
                    Err(e) => eprintln!("crash child: commit failed, next tick retries: {e}"),
                }
            }
            Op::Push => {
                if let Some(head) = push_until_ok(&mut origin) {
                    promises.promise(&format!("U origin {head}"));
                }
                // Diverged after `Diverge`: never a success, saved once.
                let outcome = side.push_once();
                if let PushOutcome::Pushed { to, .. } | PushOutcome::UpToDate { head: to } = outcome
                {
                    promises.promise(&format!("U side {to}"));
                }
            }
            Op::Diverge => diverge(&env.side()),
        }
    }
    for point in POINTS {
        eprintln!("HITS {point} {}", agentfs_core::failpoints::hits(point));
    }
    eprintln!("TRACE {}", agentfs_core::failpoints::trace().join(","));
}

/// The child: runs the workload in `CRASH_CHILD` when set; a no-op in a
/// normal test run.
#[test]
fn crash_child() {
    let Some(dir) = std::env::var_os("CRASH_CHILD") else {
        return;
    };
    run_workload(&Env {
        dir: PathBuf::from(dir),
    });
}

struct ChildRun {
    status: std::process::ExitStatus,
    stderr: String,
}

fn run_child(dir: &Path, spec: &str) -> Result<ChildRun, String> {
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "crash_child", "--nocapture", "--test-threads=1"])
        .env("CRASH_CHILD", dir)
        .env(agentfs_core::failpoints::ENV_VAR, spec)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut pipe = child.stderr.take().unwrap();
    let reader = std::thread::spawn(move || {
        let mut s = String::new();
        let _ = pipe.read_to_string(&mut s);
        s
    });
    let deadline = Instant::now() + Duration::from_secs(120);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(format!("{spec}: the child hung"));
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    Ok(ChildRun {
        status,
        stderr: reader.join().unwrap(),
    })
}

/// Recovers as the next process would, then checks the oracle.
fn recover_and_check(env: &Env, mode: Mode) -> Result<(), String> {
    let model = model(&workload());
    let (_recorder, committer) = env.open();
    let mut last = String::new();
    let mut committed = false;
    for _ in 0..3 {
        match committer.commit_pending() {
            Ok(_) => {
                committed = true;
                break;
            }
            Err(e) => last = e.to_string(),
        }
    }
    if !committed {
        return Err(format!("the recovery commit failed: {last}"));
    }
    let mut origin = env.pusher(&committer, "origin");
    let mut pushed = false;
    for _ in 0..3 {
        if origin.push_once().is_success() {
            pushed = true;
            break;
        }
    }
    if !pushed {
        return Err(format!(
            "the recovery push failed: {:?}",
            origin.state().last_error
        ));
    }
    drop(origin);
    drop(committer);

    let promises = env.promises();
    let objects = env.state().join("lfs/objects");
    let indices =
        crash_oracle::check_history(&env.state().join("git"), BRANCH, &model, Some(&objects))?;
    let promised = match mode {
        Mode::Kill => promises.last_record(),
        Mode::Error => model.last(),
    };
    crash_oracle::check_promised(&indices, promised, mode)?;
    if let Some(c) = promises.last_commit() {
        // A commit pass that returned has its records in history.
        if indices.last().copied().unwrap_or(0) < c {
            return Err(format!(
                "commit through state {c} was promised but is not the head"
            ));
        }
    }
    crash_oracle::check_clean(&env.state(), BRANCH)?;
    let repo = Repository::open_bare(env.state().join("git")).unwrap();
    let Ok(head) = repo.refname_to_id(BRANCH) else {
        // Killed before the first record was durable: nothing to push.
        let origin = Repository::open_bare(env.origin()).unwrap();
        if origin.refname_to_id(BRANCH).is_ok() {
            return Err("origin has the branch, but the history has none".into());
        }
        return Ok(());
    };
    crash_oracle::check_remote(&env.origin(), BRANCH, VOLUME, head, true)?;
    crash_oracle::check_pushed(&env.state().join("git"), head, &promises.pushed())?;
    let push_state: serde_json::Value = serde_json::from_slice(
        &fs::read(env.state_dir().push_state("origin")).map_err(|e| e.to_string())?,
    )
    .map_err(|e| e.to_string())?;
    if push_state["last_pushed"].as_str() != Some(head.to_string().as_str()) {
        return Err(format!(
            "origin's push state is {push_state}, the head {head}"
        ));
    }
    check_side(env, &repo)?;
    Ok(())
}

/// `side` diverged once: at most one saved head, a commit of ours, and
/// the remote passes fsck.
fn check_side(env: &Env, local: &Repository) -> Result<(), String> {
    let side = Repository::open_bare(env.side()).unwrap();
    let saved: Vec<Oid> = side
        .references_glob(&format!("refs/agentfs/{VOLUME}/diverged/*"))
        .unwrap()
        .filter_map(|r| r.ok()?.target())
        .collect();
    if saved.len() > 1 {
        return Err(format!("side holds {} saved heads: {saved:?}", saved.len()));
    }
    let head = local.refname_to_id(BRANCH).unwrap();
    for oid in saved {
        if !(oid == head || local.graph_descendant_of(head, oid).unwrap_or(false)) {
            return Err(format!("side's saved head {oid} is not in local history"));
        }
    }
    crash_oracle::fsck(&env.side())
}

fn hits_of(stderr: &str) -> BTreeMap<String, u64> {
    stderr
        .lines()
        .filter_map(|l| {
            let mut parts = l.strip_prefix("HITS ")?.split(' ');
            Some((parts.next()?.to_owned(), parts.next()?.parse().ok()?))
        })
        .collect()
}

/// One crash case: the failpoint spec, the point whose trigger ends the
/// run, that point's action, and how the run ends.
struct Case {
    spec: String,
    point: &'static str,
    mode: Mode,
}

/// One case: run the child with the spec, check that the point fired
/// and how the child ended, recover, check the oracle.
fn case(c: &Case) -> Result<(), String> {
    let tmp = tempfile::tempdir().unwrap();
    let env = Env::new(tmp.path());
    let (spec, point, mode) = (&c.spec, c.point, c.mode);
    let action = match mode {
        Mode::Kill => "kill",
        Mode::Error => "error",
    };
    let run = run_child(&env.dir, spec)?;
    let trigger = format!("agentfs failpoint {point} triggered ({action})");
    let context = |what: String| {
        let tail: Vec<&str> = run.stderr.lines().rev().take(25).collect();
        let tail: Vec<&str> = tail.into_iter().rev().collect();
        format!(
            "{spec}: {what}\n--- child stderr (tail) ---\n{}",
            tail.join("\n")
        )
    };
    if !run.stderr.contains(&trigger) {
        return Err(context("the failpoint never fired".into()));
    }
    match mode {
        Mode::Kill if run.status.signal() != Some(libc_sigkill()) => {
            return Err(context(format!("expected SIGKILL, got {:?}", run.status)));
        }
        Mode::Error if !run.status.success() => {
            return Err(context(format!("the child failed: {:?}", run.status)));
        }
        _ => {}
    }
    recover_and_check(&env, mode).map_err(context)
}

fn libc_sigkill() -> i32 {
    rustix::process::Signal::KILL.as_raw()
}

/// The baseline: no failure, every engine point reached, and the
/// oracle holds. Returns each point's hit count and the order of hits.
fn baseline() -> (BTreeMap<String, u64>, Vec<String>) {
    let tmp = tempfile::tempdir().unwrap();
    let env = Env::new(tmp.path());
    let run = run_child(&env.dir, "").unwrap();
    assert!(
        run.status.success(),
        "baseline child failed:\n{}",
        run.stderr
    );
    let hits = hits_of(&run.stderr);
    let missing: Vec<&&str> = ENGINE_POINTS
        .iter()
        .filter(|p| hits.get(**p).copied().unwrap_or(0) == 0)
        .collect();
    assert!(
        missing.is_empty(),
        "the workload never reaches {missing:?}; hits: {hits:?}"
    );
    recover_and_check(&env, Mode::Error).unwrap();
    let trace = run
        .stderr
        .lines()
        .find_map(|l| l.strip_prefix("TRACE "))
        .map(|t| t.split(',').map(str::to_owned).collect())
        .unwrap_or_default();
    (hits, trace)
}

/// Steps that can fail after their commit is in history. Each is failed
/// once and the process is then killed in the next commit, so whatever
/// the failure left behind (in memory or on disk) must survive a restart:
/// an error alone can be healed by in-process state and hide the damage.
const FAIL_THEN_KILL: &[&str] = &[
    "commit.after_ref",
    "commit.after_sync",
    "journal.truncate.after_tmp",
    "journal.truncate.after_rename",
    "commit.before_pending_remove",
    "pending.remove.between",
    "push.after_remote_update",
    "state.json.before_rename",
];

#[test]
fn every_engine_point_is_reached() {
    let (_hits, trace) = baseline();
    assert!(!trace.is_empty(), "the child printed no trace");
}

/// Kill at the 1st and 3rd hit, and fail at the 1st and 3rd hit, of every
/// engine point; recover; check the oracle.
#[test]
fn kill_or_fail_at_every_engine_point() {
    let (hits, trace) = baseline();
    let mut cases = Vec::new();
    for point in ENGINE_POINTS {
        for nth in [1, 3] {
            if hits.get(*point).copied().unwrap_or(0) < nth {
                continue;
            }
            cases.push(Case {
                spec: format!("{point}=kill@{nth}"),
                point,
                mode: Mode::Kill,
            });
            cases.push(Case {
                spec: format!("{point}=error@{nth}"),
                point,
                mode: Mode::Error,
            });
        }
    }
    for point in FAIL_THEN_KILL {
        let first = trace
            .iter()
            .position(|p| p == point)
            .unwrap_or_else(|| panic!("the baseline never reaches {point}"));
        let commits_before = trace[..first]
            .iter()
            .filter(|p| *p == "commit.after_tree")
            .count();
        cases.push(Case {
            spec: format!(
                "{point}=error@1;commit.after_tree=kill@{}",
                commits_before + 1
            ),
            point: "commit.after_tree",
            mode: Mode::Kill,
        });
    }
    let total = cases.len();
    let cases = Arc::new(Mutex::new(cases));
    let failures = Arc::new(Mutex::new(Vec::new()));
    let workers = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(2)
        .clamp(2, 8);
    let threads: Vec<_> = (0..workers)
        .map(|_| {
            let cases = cases.clone();
            let failures = failures.clone();
            std::thread::spawn(move || {
                loop {
                    let next = cases.lock().unwrap().pop();
                    let Some(c) = next else {
                        return;
                    };
                    if let Err(e) = case(&c) {
                        failures.lock().unwrap().push(e);
                    }
                }
            })
        })
        .collect();
    for t in threads {
        t.join().unwrap();
    }
    let failures = failures.lock().unwrap();
    assert!(
        failures.is_empty(),
        "{} of {total} crash cases failed:\n\n{}",
        failures.len(),
        failures.join("\n\n")
    );
    eprintln!("{total} crash cases passed");
}
