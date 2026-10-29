//! Submitting a patch (design §12, §19.1): the producer side.
//!
//! A producer (iac, CI, an operator) runs `agentfs patch submit` in its own
//! git repository. It drives the producer's `git`, so the repository's
//! remote configuration, credential helpers and SSH setup apply, and
//! shallow clones work; the state area is never involved.
//!
//! 1. `git ls-remote` lists the volume's branch, inbox and applied refs.
//! 2. `git fetch` brings the branch head `H`.
//! 3. The patch is a branch: a given tip (any revision) whose commits since
//!    its merge base with `H` (the patch's base) are a line of
//!    single-parent commits, or one commit built here on `H` from
//!    `--put`/`--delete` (with a temporary index), or, for `--skip`, an
//!    empty commit on `H`.
//!    A given commit taking a new number that is already queued
//!    (`inbox/*`) or applied (`applied/*`, or `last_applied` in the status
//!    ref) is refused early
//!    ([`SubmitError::AlreadySubmitted`]), pushing nothing: a change lands
//!    at most once. Two submits at once can both pass this check; the
//!    queue's duplicate rule (§12) is the guarantee.
//! 4. Its number is one past the highest inbox or applied number on the
//!    remote, or, for `--replace N`/`--skip N`, `N`: a queued patch, or a
//!    gap (a number below the highest one that the remote has neither as
//!    inbox nor applied), which is filled create-only. A number the volume
//!    has applied (the remote's `applied/N`, or `last_applied` in its
//!    status ref at or above N with N not in its queue; the status reaches
//!    the remote even while applied refs are held back) is refused: a
//!    replacement would only be ignored as stale. A patch the queue set
//!    aside below the last applied number is still queued, so replaceable.
//!    With `barrier`, a built or skip patch gets `Agentfs-Barrier: yes`
//!    and a given tip must carry it.
//! 5. `git push --force-with-lease=<ref>: <oid>:<ref>` creates the inbox
//!    ref only if it does not exist yet (`<ref>:<oid>` for a replacement,
//!    which must still hold what was listed). A submitter that loses a race
//!    for a number lists again and tries the next one, up to `attempts`
//!    times.
//!
//! [`approve`] (`agentfs patch approve`) writes
//! `refs/agentfs/<volume>/approved/<n>` = the queued tip, for
//! `agentfs.patchAccept=approval`.
//!
//! Applied refs are pushed before inbox refs are deleted on the remote
//! (see [`crate::push`]), so the highest number listed never goes down
//! and a number is never reused.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::{Command, Stdio};

use super::integrate::message_is_barrier;
use super::{
    BARRIER_TRAILER, applied_prefix, applied_ref, approved_ref, inbox_prefix, inbox_ref,
    seq_of_ref, status_ref,
};

/// The default number of numbers tried before giving up on a race.
pub const DEFAULT_ATTEMPTS: u32 = 10;

#[derive(Debug, thiserror::Error)]
pub enum SubmitError {
    #[error("`git {args}` failed ({status}): {stderr}")]
    Git {
        args: String,
        status: String,
        stderr: String,
    },
    #[error("running git: {0}")]
    Spawn(#[source] std::io::Error),
    #[error("the remote has no branch {0}: the volume has not pushed yet")]
    NoBranch(String),
    #[error(
        "{commit} in the patch has {parents} parents; a patch is a line of single-parent commits"
    )]
    MergeInBranch { commit: String, parents: usize },
    #[error(
        "{commit} shares no history with the volume's branch {branch} (head {head}); rebase it \
         onto the branch{shallow}"
    )]
    ParentNotInHistory {
        commit: String,
        branch: String,
        head: String,
        shallow: &'static str,
    },
    #[error(
        "patch {0} is not queued on the remote and is not a gap below the highest number, so \
         it cannot be replaced or skipped"
    )]
    NotQueued(u64),
    #[error(
        "patch {0} is already applied on the volume (the remote's applied ref or status says \
         so); a replacement would never be applied: submit a new patch instead"
    )]
    AlreadyApplied(u64),
    #[error(
        "commit {commit} is already queued as inbox {inbox:?} or applied as {applied:?}; a \
         change lands at most once"
    )]
    AlreadySubmitted {
        commit: String,
        inbox: Option<u64>,
        applied: Option<u64>,
    },
    #[error("lost the race for an inbox number {attempts} times; try again")]
    Raced { attempts: u32 },
    #[error("the remote refused {ref_name}: {message}")]
    Refused { ref_name: String, message: String },
    #[error("{0}")]
    BadRequest(String),
    #[error(
        "{commit} does not carry `Agentfs-Barrier: yes` in its last paragraph; add it with `git \
         commit --amend --trailer 'Agentfs-Barrier: yes'`"
    )]
    NotABarrier { commit: String },
}

/// The output of one git command.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GitOutput {
    pub success: bool,
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

/// Runs git commands in the producer's repository.
pub trait GitRunner {
    /// `git <args>` with extra environment variables. A command that ran
    /// and failed is `Ok` with `success == false`.
    fn run(&self, args: &[&str], env: &[(&str, &str)]) -> Result<GitOutput, SubmitError>;
}

/// The `git` CLI, in `repo_dir`.
#[derive(Debug, Clone)]
pub struct GitCli {
    pub repo_dir: PathBuf,
    pub git: PathBuf,
}

impl GitCli {
    pub fn new(repo_dir: impl Into<PathBuf>) -> Self {
        Self {
            repo_dir: repo_dir.into(),
            git: PathBuf::from("git"),
        }
    }
}

impl GitRunner for GitCli {
    fn run(&self, args: &[&str], env: &[(&str, &str)]) -> Result<GitOutput, SubmitError> {
        let output = Command::new(&self.git)
            .arg("-C")
            .arg(&self.repo_dir)
            .args(args)
            .envs(env.iter().copied())
            .stdin(Stdio::null())
            .output()
            .map_err(SubmitError::Spawn)?;
        Ok(GitOutput {
            success: output.status.success(),
            code: output.status.code(),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        })
    }
}

/// What the patch is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    /// The tip of a branch (any revision): its commits since the volume's
    /// branch are the patch.
    Commit(String),
    /// A commit built on the remote branch head.
    Build {
        /// `(path in the tree, file to read)`.
        puts: Vec<(String, PathBuf)>,
        deletes: Vec<String>,
        message: String,
        /// `Name <email>`; the user's git identity when `None`.
        author: Option<String>,
    },
    /// An empty commit on the head replacing patch `n`, so the queue moves
    /// past it.
    Skip(u64),
}

/// One submission.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubmitRequest {
    /// A remote name or URL, as `git push` takes it.
    pub remote: String,
    pub volume: String,
    /// The branch, without `refs/heads/` (default `agentfs/<volume>`).
    pub branch: String,
    pub source: Source,
    /// Replace queued patch `n` instead of taking a new number.
    pub replace: Option<u64>,
    /// Numbers tried before giving up on a race.
    pub attempts: u32,
    /// `git fetch --depth`, for shallow producers.
    pub depth: Option<u32>,
    /// Make the patch a barrier: a built or skip patch gets the
    /// `Agentfs-Barrier: yes` trailer; a given tip must already carry it.
    pub barrier: bool,
}

/// `agentfs patch approve`: approves queued patch `inbox` as listed (or
/// as `expect`, which must match).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApproveRequest {
    /// A remote name or URL, as `git push` takes it.
    pub remote: String,
    pub volume: String,
    pub inbox: u64,
    /// The tip the approver means: refused if the queue holds another.
    pub expect: Option<String>,
}

/// What was approved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Approved {
    pub inbox: u64,
    /// `refs/agentfs/<volume>/approved/<n>`.
    pub ref_name: String,
    pub tip: String,
}

/// What was submitted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Submitted {
    pub inbox: u64,
    pub ref_name: String,
    pub oid: String,
    /// The patch's base: the merge base of the tip and the remote head.
    pub base: String,
    /// The patch's commits, base exclusive, tip inclusive.
    pub commits: usize,
    /// Numbers lost to other submitters first.
    pub raced: u32,
}

/// The remote's refs from `git ls-remote` output: `<oid>\t<ref>` lines.
pub fn parse_ls_remote(output: &str) -> BTreeMap<String, String> {
    output
        .lines()
        .filter_map(|line| {
            let (oid, name) = line.split_once('\t')?;
            Some((name.trim().to_owned(), oid.trim().to_owned()))
        })
        .collect()
}

/// One past the highest inbox or applied number among `refs`; 1 when
/// there is none. Malformed names do not count.
pub fn next_seq<'r>(volume: &str, refs: impl IntoIterator<Item = &'r String>) -> u64 {
    let inbox = inbox_prefix(volume);
    let applied = applied_prefix(volume);
    refs.into_iter()
        .filter_map(|name| seq_of_ref(&inbox, name).or_else(|| seq_of_ref(&applied, name)))
        .max()
        .map_or(1, |n| n + 1)
}

/// The inbox and applied numbers under which `volume`'s queue lists one
/// commit: `names` are the remote refs that hold that commit.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ListedAs {
    pub inbox: Option<u64>,
    pub applied: Option<u64>,
}

/// [`ListedAs`] of the refs `names` (each holding the commit asked
/// about): the first `inbox/<n>` and the first `applied/<n>` of `volume`
/// by [`super::seq_of_ref`]; malformed names do not count. The one home of
/// "this commit is already queued or applied", for `patch submit` and a
/// fork's roll-up into its parent ([`crate::fork::finish`]).
pub fn listed_as<'r>(volume: &str, names: impl IntoIterator<Item = &'r String>) -> ListedAs {
    let inbox = inbox_prefix(volume);
    let applied = applied_prefix(volume);
    let mut listed = ListedAs::default();
    for name in names {
        if listed.inbox.is_none() {
            listed.inbox = seq_of_ref(&inbox, name);
        }
        if listed.applied.is_none() {
            listed.applied = seq_of_ref(&applied, name);
        }
    }
    listed
}

/// How `git push --porcelain` reported one ref.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PushLine {
    /// Created, updated or already there.
    Ok,
    /// Rejected, with the reason in parentheses.
    Rejected(String),
}

/// `ref_name`'s line in `git push --porcelain` output:
/// `<flag>\t<from>:<to>\t<summary> (<reason>)`.
pub fn parse_porcelain(output: &str, ref_name: &str) -> Option<PushLine> {
    output.lines().find_map(|line| {
        let mut fields = line.split('\t');
        let flag = fields.next()?;
        let refs = fields.next()?;
        let summary = fields.next().unwrap_or("");
        let (_, to) = refs.rsplit_once(':')?;
        if to != ref_name {
            return None;
        }
        Some(match flag {
            "!" => PushLine::Rejected(summary.to_owned()),
            _ => PushLine::Ok,
        })
    })
}

/// Submits a patch (see the module docs).
pub fn submit(git: &impl GitRunner, req: &SubmitRequest) -> Result<Submitted, SubmitError> {
    if req.volume.is_empty() || req.volume.contains('/') {
        return Err(SubmitError::BadRequest(format!(
            "{:?} is not a volume name",
            req.volume
        )));
    }
    let branch_ref = format!("refs/heads/{}", req.branch);
    let listing = ls_remote(git, req)?;
    let head = listing
        .get(&branch_ref)
        .cloned()
        .ok_or_else(|| SubmitError::NoBranch(branch_ref.clone()))?;
    // The head and enough history to check the patch's parent.
    fetch_commit(git, &req.remote, &head, &branch_ref, req.depth)?;
    let barrier = |message: &str| match req.barrier {
        true => format!("{}\n\n{BARRIER_TRAILER}: yes\n", message.trim_end()),
        false => message.to_owned(),
    };
    let (oid, base, commits) = match &req.source {
        Source::Commit(rev) => {
            let tip = branch_tip(git, req, rev, &head)?;
            if req.barrier {
                let message = check(git, &["show", "-s", "--format=%B", &tip.0], &[])?.stdout;
                if !message_is_barrier(&message) {
                    return Err(SubmitError::NotABarrier { commit: tip.0 });
                }
            }
            tip
        }
        Source::Build {
            puts,
            deletes,
            message,
            author,
        } => {
            let message = if message.trim().is_empty() {
                "agentfs patch"
            } else {
                message
            };
            (
                build_commit(
                    git,
                    &head,
                    puts,
                    deletes,
                    &barrier(message),
                    author.as_deref(),
                )?,
                head.clone(),
                1,
            )
        }
        Source::Skip(n) => (
            build_commit(
                git,
                &head,
                &[],
                &[],
                &barrier(&format!("agentfs: skip patch {n}")),
                None,
            )?,
            head.clone(),
            1,
        ),
    };
    // A given commit taking a new number; `--replace N` names its number
    // deliberately and is checked against applied numbers below.
    if let (Source::Commit(_), None) = (&req.source, req.replace) {
        refuse_submitted(git, req, &listing, &oid)?;
    }
    let replace = match (&req.source, req.replace) {
        (Source::Skip(n), None) => Some(*n),
        (Source::Skip(n), Some(r)) if r != *n => {
            return Err(SubmitError::BadRequest(
                "--skip and --replace name different patches".into(),
            ));
        }
        (_, r) => r,
    };
    if let Some(n) = replace {
        if n == 0 {
            return Err(SubmitError::BadRequest("inbox numbers start at 1".into()));
        }
        let ref_name = inbox_ref(&req.volume, n);
        // Applied already, even if the remote still lists it: a
        // replacement would only ever be ignored as stale. A number at or
        // below the last applied one that the status still lists as queued
        // was set aside, and is replaceable (bead agentfs-mia.2).
        if listing.contains_key(&applied_ref(&req.volume, n))
            || remote_status(git, &req.remote, &req.volume, &listing)
                .is_some_and(|status| applied_per_status(&status, n))
        {
            return Err(SubmitError::AlreadyApplied(n));
        }
        let expected = match listing.get(&ref_name) {
            Some(current) => current.clone(),
            // A gap (a missing number below the highest one): filled
            // create-only.
            None if n < next_seq(&req.volume, listing.keys()) => String::new(),
            None => return Err(SubmitError::NotQueued(n)),
        };
        return match push(git, req, &oid, &ref_name, &expected)? {
            PushLine::Ok => Ok(Submitted {
                inbox: n,
                ref_name,
                oid,
                base,
                commits,
                raced: 0,
            }),
            PushLine::Rejected(message) => Err(SubmitError::Refused { ref_name, message }),
        };
    }
    let mut listing = listing;
    let attempts = req.attempts.max(1);
    let mut refused = None;
    for raced in 0..attempts {
        let n = next_seq(&req.volume, listing.keys());
        let ref_name = inbox_ref(&req.volume, n);
        match push(git, req, &oid, &ref_name, "")? {
            PushLine::Ok => {
                return Ok(Submitted {
                    inbox: n,
                    ref_name,
                    oid,
                    base,
                    commits,
                    raced,
                });
            }
            PushLine::Rejected(message) => {
                // Lost the number if someone else's patch holds it now.
                listing = ls_remote(git, req)?;
                match listing.get(&ref_name) {
                    Some(other) if *other != oid => continue,
                    Some(_) => {
                        return Ok(Submitted {
                            inbox: n,
                            ref_name,
                            oid,
                            base,
                            commits,
                            raced,
                        });
                    }
                    // Refused for another reason, or lost a lock race
                    // whose winner gave up too: try again.
                    None => refused = Some((ref_name, message)),
                }
            }
        }
    }
    match refused {
        Some((ref_name, message)) => Err(SubmitError::Refused { ref_name, message }),
        None => Err(SubmitError::Raced { attempts }),
    }
}

/// Fetches commit `oid`, listed at `ref_name` on `remote`, with its
/// history (`--depth` when given), writing no ref. Older servers refuse
/// fetching by object id, so it falls back to fetching the ref (which
/// writes only `FETCH_HEAD`).
pub fn fetch_commit(
    git: &impl GitRunner,
    remote: &str,
    oid: &str,
    ref_name: &str,
    depth: Option<u32>,
) -> Result<(), SubmitError> {
    let mut fetch = vec!["fetch", "--quiet", "--no-tags", "--no-write-fetch-head"];
    let depth = depth.map(|d| format!("--depth={d}"));
    if let Some(d) = &depth {
        fetch.push(d);
    }
    fetch.push(remote);
    fetch.push(oid);
    let fetched = git.run(&fetch, &[])?;
    if !fetched.success {
        fetch.pop();
        fetch.retain(|a| *a != "--no-write-fetch-head");
        fetch.push(ref_name);
        check(git, &fetch, &[])?;
    }
    Ok(())
}

fn ls_remote(
    git: &impl GitRunner,
    req: &SubmitRequest,
) -> Result<BTreeMap<String, String>, SubmitError> {
    list_queue(git, &req.remote, &req.volume, Some(&req.branch))
}

/// `git ls-remote` of the volume's queue refs (`refs/agentfs/<volume>/*`)
/// and, when given, its branch.
fn list_queue(
    git: &impl GitRunner,
    remote: &str,
    volume: &str,
    branch: Option<&str>,
) -> Result<BTreeMap<String, String>, SubmitError> {
    let branch_ref = branch.map(|b| format!("refs/heads/{b}"));
    let queue = format!("refs/agentfs/{volume}/*");
    let mut args = vec!["ls-remote", "--refs", remote];
    if let Some(b) = &branch_ref {
        args.push(b);
    }
    args.push(&queue);
    let out = check(git, &args, &[])?;
    let prefix = format!("refs/agentfs/{volume}/");
    Ok(parse_ls_remote(&out.stdout)
        .into_iter()
        .filter(|(name, _)| Some(name) == branch_ref.as_ref() || name.starts_with(&prefix))
        .collect())
}

/// Approves queued patch `req.inbox` (see [`ApproveRequest`]): its tip as
/// the remote lists it goes to `refs/agentfs/<volume>/approved/<n>`,
/// leased on what that ref held when listed. Idempotent when the approval
/// already names that tip. The volume fetches approvals with its inbox and
/// lands an awaiting patch under `agentfs.patchAccept=approval` once its
/// approval names its tip (bead agentfs-mia.2).
pub fn approve(git: &impl GitRunner, req: &ApproveRequest) -> Result<Approved, SubmitError> {
    if req.volume.is_empty() || req.volume.contains('/') {
        return Err(SubmitError::BadRequest(format!(
            "{:?} is not a volume name",
            req.volume
        )));
    }
    let listing = list_queue(git, &req.remote, &req.volume, None)?;
    let inbox = inbox_ref(&req.volume, req.inbox);
    let tip = listing
        .get(&inbox)
        .cloned()
        .ok_or(SubmitError::NotQueued(req.inbox))?;
    if let Some(expect) = &req.expect
        && *expect != tip
    {
        return Err(SubmitError::Refused {
            ref_name: inbox,
            message: format!("the queued patch is now {tip}"),
        });
    }
    let ref_name = approved_ref(&req.volume, req.inbox);
    let approved = |ref_name| Approved {
        inbox: req.inbox,
        ref_name,
        tip: tip.clone(),
    };
    let lease = listing.get(&ref_name).cloned().unwrap_or_default();
    if lease == tip {
        return Ok(approved(ref_name));
    }
    // git pushes only objects it has.
    fetch_commit(git, &req.remote, &tip, &inbox, None)?;
    match push_leased(git, &req.remote, &tip, &ref_name, &lease)? {
        PushLine::Ok => Ok(approved(ref_name)),
        PushLine::Rejected(message) => Err(SubmitError::Refused { ref_name, message }),
    }
}

/// [`SubmitError::AlreadySubmitted`] when `oid` is a queued or applied
/// patch commit on the remote: an `inbox/*` or `applied/*` ref at it, or
/// the status ref's `last_applied` patch. An early refusal only: two
/// submits at once both pass it, and the queue's duplicate rule (§12) is
/// what makes a change land at most once.
fn refuse_submitted(
    git: &impl GitRunner,
    req: &SubmitRequest,
    listing: &BTreeMap<String, String>,
    oid: &str,
) -> Result<(), SubmitError> {
    let listed = listed_as(
        &req.volume,
        listing
            .iter()
            .filter(|(_, o)| o.as_str() == oid)
            .map(|(name, _)| name),
    );
    let inbox = listed.inbox;
    let applied = listed.applied.or_else(|| {
        let last = remote_status(git, &req.remote, &req.volume, listing)?;
        let last = last.get("last_applied")?;
        (last.get("patch")?.as_str()? == oid)
            .then(|| last.get("inbox")?.as_u64())
            .flatten()
    });
    if inbox.is_some() || applied.is_some() {
        return Err(SubmitError::AlreadySubmitted {
            commit: oid.to_owned(),
            inbox,
            applied,
        });
    }
    Ok(())
}

/// Whether the remote's `status.json` says patch `n` is applied:
/// `last_applied.inbox` is at least `n` and its queue does not list `n` (a
/// set-aside patch is still queued). The status reaches the remote even
/// while the branch and the applied refs cannot, so it catches a patch
/// applied whose applied ref is held back.
fn applied_per_status(status: &serde_json::Value, n: u64) -> bool {
    let last = status
        .get("last_applied")
        .and_then(|l| l.get("inbox"))
        .and_then(serde_json::Value::as_u64);
    let queued = status
        .get("queue")
        .and_then(serde_json::Value::as_array)
        .is_some_and(|q| {
            q.iter()
                .any(|e| e.get("inbox").and_then(serde_json::Value::as_u64) == Some(n))
        });
    last.is_some_and(|last| last >= n) && !queued
}

/// The remote's `status.json`, from its status ref. `None` when there is
/// no status or it cannot be read: best effort.
fn remote_status(
    git: &impl GitRunner,
    remote: &str,
    volume: &str,
    listing: &BTreeMap<String, String>,
) -> Option<serde_json::Value> {
    let name = status_ref(volume);
    let oid = listing.get(&name)?;
    let base = ["fetch", "--quiet", "--no-tags", "--no-write-fetch-head"];
    let by_oid = [&base[..], &[remote, oid.as_str()]].concat();
    let fetched = git.run(&by_oid, &[]).is_ok_and(|o| o.success)
        || git
            .run(&["fetch", "--quiet", "--no-tags", remote, &name], &[])
            .is_ok_and(|o| o.success);
    if !fetched {
        return None;
    }
    let spec = format!("{oid}:{}", super::status::STATUS_FILE);
    let out = git.run(&["cat-file", "blob", &spec], &[]).ok()?;
    if !out.success {
        return None;
    }
    serde_json::from_str(&out.stdout).ok()
}

/// `git push` of `oid` to `ref_name`, leased on the remote holding
/// `expected` there (empty: nothing).
fn push(
    git: &impl GitRunner,
    req: &SubmitRequest,
    oid: &str,
    ref_name: &str,
    expected: &str,
) -> Result<PushLine, SubmitError> {
    push_leased(git, &req.remote, oid, ref_name, expected)
}

/// `git push --force-with-lease=<ref_name>:<expected> <oid>:<ref_name>` to
/// `remote`: the ref moves only if the remote holds `expected` there, and
/// with `expected` empty it is created only if it does not exist. Never a
/// plain force. Also used by the fork delivery's branch method
/// ([`crate::fork::deliver`]).
pub fn push_leased(
    git: &impl GitRunner,
    remote: &str,
    oid: &str,
    ref_name: &str,
    expected: &str,
) -> Result<PushLine, SubmitError> {
    let lease = format!("--force-with-lease={ref_name}:{expected}");
    let spec = format!("{oid}:{ref_name}");
    let args = ["push", "--porcelain", "--no-verify", &lease, remote, &spec];
    let out = git.run(&args, &[])?;
    match parse_porcelain(&out.stdout, ref_name) {
        Some(line) => Ok(line),
        None if out.success => Ok(PushLine::Ok),
        None => Err(git_error(&args, &out)),
    }
}

/// A given branch tip: `(tip, base, commit count)`. Its base is the merge
/// base of the tip and `head`; its commits since the base must each have
/// one parent, and there must be at least one.
fn branch_tip(
    git: &impl GitRunner,
    req: &SubmitRequest,
    rev: &str,
    head: &str,
) -> Result<(String, String, usize), SubmitError> {
    let spec = format!("{rev}^{{commit}}");
    let tip = check(git, &["rev-parse", "--verify", "--quiet", &spec], &[])?
        .stdout
        .trim()
        .to_owned();
    let args = ["merge-base", tip.as_str(), head];
    let merge_base = git.run(&args, &[])?;
    let base = match (merge_base.success, merge_base.code) {
        (true, _) => merge_base.stdout.trim().to_owned(),
        (false, Some(1)) => {
            let shallow = git
                .run(&["rev-parse", "--is-shallow-repository"], &[])
                .map(|o| o.stdout.trim() == "true")
                .unwrap_or(false);
            return Err(SubmitError::ParentNotInHistory {
                commit: tip,
                branch: req.branch.clone(),
                head: head.to_owned(),
                shallow: if shallow {
                    " (this clone is shallow: a larger --depth may find it)"
                } else {
                    ""
                },
            });
        }
        (false, _) => return Err(git_error(&args, &merge_base)),
    };
    let range = format!("{base}..{tip}");
    let lines = check(git, &["rev-list", "--parents", &range], &[])?;
    let mut commits = 0;
    for line in lines.stdout.lines().filter(|l| !l.trim().is_empty()) {
        let mut fields = line.split_whitespace();
        let commit = fields.next().unwrap_or_default();
        let parents = fields.count();
        if parents != 1 {
            return Err(SubmitError::MergeInBranch {
                commit: commit.to_owned(),
                parents,
            });
        }
        commits += 1;
    }
    if commits == 0 {
        return Err(SubmitError::BadRequest(
            "the commit is already on the volume's branch".into(),
        ));
    }
    Ok((tip, base, commits))
}

/// A commit on `head` with `puts` written and `deletes` removed, built in
/// a temporary index so the producer's own index and work tree are left
/// alone.
fn build_commit(
    git: &impl GitRunner,
    head: &str,
    puts: &[(String, PathBuf)],
    deletes: &[String],
    message: &str,
    author: Option<&str>,
) -> Result<String, SubmitError> {
    let index = TempIndex::new();
    let index = index.0.to_string_lossy().into_owned();
    let env = [("GIT_INDEX_FILE", index.as_str())];
    check(git, &["read-tree", head], &env)?;
    for (path, file) in puts {
        let file_arg = file.to_string_lossy().into_owned();
        let blob = check(git, &["hash-object", "-w", "--", &file_arg], &env)?
            .stdout
            .trim()
            .to_owned();
        let executable = {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::metadata(file)
                .map_err(SubmitError::Spawn)?
                .permissions()
                .mode()
                & 0o111
                != 0
        };
        let mode = if executable { "100755" } else { "100644" };
        let info = format!("{mode},{blob},{path}");
        check(git, &["update-index", "--add", "--cacheinfo", &info], &env)?;
    }
    for path in deletes {
        check(git, &["update-index", "--force-remove", "--", path], &env)?;
    }
    let tree = check(git, &["write-tree"], &env)?.stdout.trim().to_owned();
    let mut commit_env: Vec<(&str, &str)> = Vec::new();
    let parsed;
    if let Some(author) = author {
        parsed = parse_author(author)?;
        commit_env.push(("GIT_AUTHOR_NAME", &parsed.0));
        commit_env.push(("GIT_AUTHOR_EMAIL", &parsed.1));
    }
    let message = if message.trim().is_empty() {
        "agentfs patch"
    } else {
        message
    };
    let out = check(
        git,
        &["commit-tree", &tree, "-p", head, "-m", message],
        &commit_env,
    )?;
    Ok(out.stdout.trim().to_owned())
}

/// A temporary index file, removed when dropped (git creates it).
struct TempIndex(PathBuf);

impl TempIndex {
    fn new() -> Self {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        Self(std::env::temp_dir().join(format!(
            "agentfs-patch-{}-{nanos}.index",
            std::process::id()
        )))
    }
}

impl Drop for TempIndex {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
        let _ = std::fs::remove_file(self.0.with_extension("index.lock"));
    }
}

/// `Name <email>`.
pub fn parse_author(author: &str) -> Result<(String, String), SubmitError> {
    let bad = || SubmitError::BadRequest(format!("--author {author:?} is not `Name <email>`"));
    let (name, rest) = author.split_once('<').ok_or_else(bad)?;
    let email = rest.strip_suffix('>').ok_or_else(bad)?;
    let name = name.trim();
    if name.is_empty() || email.is_empty() || email.contains(['<', '>']) {
        return Err(bad());
    }
    Ok((name.to_owned(), email.to_owned()))
}

/// `git <args>`, an error unless it succeeded.
pub(crate) fn check(
    git: &impl GitRunner,
    args: &[&str],
    env: &[(&str, &str)],
) -> Result<GitOutput, SubmitError> {
    let out = git.run(args, env)?;
    if out.success {
        Ok(out)
    } else {
        Err(git_error(args, &out))
    }
}

fn git_error(args: &[&str], out: &GitOutput) -> SubmitError {
    SubmitError::Git {
        args: args.join(" "),
        status: out
            .code
            .map_or_else(|| "killed".to_owned(), |c| format!("exit {c}")),
        stderr: out.stderr.trim().to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use super::*;

    #[test]
    fn listed_as_reads_the_first_inbox_and_applied_number() {
        let refs =
            |names: &[&str]| -> Vec<String> { names.iter().map(|s| s.to_string()).collect() };
        assert_eq!(listed_as("v", &refs(&[])), ListedAs::default());
        assert_eq!(
            listed_as(
                "v",
                &refs(&[
                    "refs/agentfs/v/applied/0000000002",
                    "refs/agentfs/v/applied/0000000004",
                    "refs/agentfs/v/inbox/0000000003",
                    "refs/agentfs/v/inbox/0000000009",
                ])
            ),
            ListedAs {
                inbox: Some(3),
                applied: Some(2),
            }
        );
        // Malformed names, other volumes and other kinds do not count.
        assert_eq!(
            listed_as(
                "v",
                &refs(&[
                    "refs/agentfs/v/inbox/7",
                    "refs/agentfs/v/applied/00000000099",
                    "refs/agentfs/other/inbox/0000000001",
                    "refs/agentfs/v/held/0000000001",
                    "refs/agentfs/v/status",
                ])
            ),
            ListedAs::default()
        );
    }

    #[test]
    fn next_seq_over_inbox_and_applied() {
        let refs =
            |names: &[&str]| -> Vec<String> { names.iter().map(|s| s.to_string()).collect() };
        assert_eq!(next_seq("v", &refs(&[])), 1);
        assert_eq!(
            next_seq(
                "v",
                &refs(&[
                    "refs/heads/agentfs/v",
                    "refs/agentfs/v/applied/0000000001",
                    "refs/agentfs/v/applied/0000000002",
                    "refs/agentfs/v/inbox/0000000005",
                    "refs/agentfs/v/inbox/7",
                    "refs/agentfs/v/inbox/00000000099",
                    "refs/agentfs/other/inbox/0000000100",
                    "refs/agentfs/v/status",
                ])
            ),
            6
        );
        assert_eq!(
            next_seq("v", &refs(&["refs/agentfs/v/applied/0000000009"])),
            10
        );
    }

    #[test]
    fn parses_ls_remote() {
        let out = "1111111111111111111111111111111111111111\trefs/heads/agentfs/v\n\
                   2222222222222222222222222222222222222222\trefs/agentfs/v/inbox/0000000001\n";
        let refs = parse_ls_remote(out);
        assert_eq!(refs.len(), 2);
        assert_eq!(
            refs["refs/agentfs/v/inbox/0000000001"],
            "2222222222222222222222222222222222222222"
        );
    }

    #[test]
    fn parses_porcelain() {
        let r = "refs/agentfs/v/inbox/0000000002";
        let ok = format!("To /tmp/r.git\n*\tabc:{r}\t[new reference]\nDone\n");
        assert_eq!(parse_porcelain(&ok, r), Some(PushLine::Ok));
        let stale = format!("To /tmp/r.git\n!\tabc:{r}\t[rejected] (stale info)\nDone\n");
        assert_eq!(
            parse_porcelain(&stale, r),
            Some(PushLine::Rejected("[rejected] (stale info)".into()))
        );
        let nonff = format!("!\tabc:{r}\t[rejected] (non-fast-forward)\n");
        assert!(matches!(
            parse_porcelain(&nonff, r),
            Some(PushLine::Rejected(m)) if m.contains("non-fast-forward")
        ));
        assert_eq!(
            parse_porcelain(&ok, "refs/agentfs/v/inbox/0000000003"),
            None
        );
    }

    #[test]
    fn parses_authors() {
        assert_eq!(
            parse_author("Ada Lovelace <ada@example.com>").unwrap(),
            ("Ada Lovelace".into(), "ada@example.com".into())
        );
        for bad in ["Ada", "<a@b>", "Ada <>", "Ada a@b>", "Ada <a<b>"] {
            assert!(parse_author(bad).is_err(), "{bad}");
        }
    }

    /// A scripted git: answers by the first argument, records every call.
    struct FakeGit {
        calls: RefCell<Vec<Vec<String>>>,
        /// Remote refs, as ls-remote lists them.
        remote: RefCell<BTreeMap<String, String>>,
        /// Numbers someone else takes just before this submitter's push.
        steal: RefCell<Vec<u64>>,
        /// `status.json` of the remote's status ref, for `git cat-file`.
        status: RefCell<Option<String>>,
        /// The given tip's message, for `git show -s --format=%B`.
        message: RefCell<String>,
    }

    const HEAD: &str = "1111111111111111111111111111111111111111";
    const PATCH: &str = "2222222222222222222222222222222222222222";
    const OTHER: &str = "3333333333333333333333333333333333333333";

    impl FakeGit {
        fn new(refs: &[(&str, &str)]) -> Self {
            Self {
                calls: RefCell::new(Vec::new()),
                remote: RefCell::new(
                    refs.iter()
                        .map(|(n, o)| (n.to_string(), o.to_string()))
                        .collect(),
                ),
                steal: RefCell::new(Vec::new()),
                status: RefCell::new(None),
                message: RefCell::new("patch\n".into()),
            }
        }

        fn ok(stdout: &str) -> Result<GitOutput, SubmitError> {
            Ok(GitOutput {
                success: true,
                code: Some(0),
                stdout: stdout.into(),
                stderr: String::new(),
            })
        }
    }

    impl GitRunner for FakeGit {
        fn run(&self, args: &[&str], _env: &[(&str, &str)]) -> Result<GitOutput, SubmitError> {
            self.calls
                .borrow_mut()
                .push(args.iter().map(|s| s.to_string()).collect());
            match args[0] {
                "ls-remote" => {
                    let out: String = self
                        .remote
                        .borrow()
                        .iter()
                        .map(|(n, o)| format!("{o}\t{n}\n"))
                        .collect();
                    Self::ok(&out)
                }
                "fetch" | "read-tree" | "update-index" => Self::ok(""),
                "merge-base" => Self::ok(&format!("{HEAD}\n")),
                "write-tree" => Self::ok(&format!("{OTHER}\n")),
                "commit-tree" => Self::ok(&format!("{PATCH}\n")),
                "cat-file" => match self.status.borrow().as_deref() {
                    Some(json) => Self::ok(json),
                    None => Ok(GitOutput {
                        success: false,
                        code: Some(128),
                        ..GitOutput::default()
                    }),
                },
                "rev-parse" => Self::ok(&format!("{PATCH}\n")),
                "show" => Self::ok(&self.message.borrow()),
                "rev-list" => Self::ok(&format!("{PATCH} {HEAD}\n")),
                "push" => {
                    let spec = args.last().unwrap();
                    let (oid, name) = spec.split_once(':').unwrap();
                    let lease = args[3].strip_prefix("--force-with-lease=").unwrap();
                    let (_, expected) = lease.rsplit_once(':').unwrap();
                    let n = super::super::seq_of_ref("refs/agentfs/v/inbox/", name);
                    if n.is_some_and(|n| self.steal.borrow().contains(&n)) {
                        self.remote.borrow_mut().insert(name.into(), OTHER.into());
                    }
                    let mut remote = self.remote.borrow_mut();
                    let current = remote.get(name).cloned().unwrap_or_default();
                    if current != expected {
                        return Ok(GitOutput {
                            success: false,
                            code: Some(1),
                            stdout: format!("!\t{spec}\t[rejected] (stale info)\nDone\n"),
                            stderr: String::new(),
                        });
                    }
                    remote.insert(name.into(), oid.into());
                    Self::ok(&format!("*\t{spec}\t[new reference]\nDone\n"))
                }
                other => panic!("unexpected git {other}"),
            }
        }
    }

    fn request(source: Source) -> SubmitRequest {
        SubmitRequest {
            remote: "origin".into(),
            volume: "v".into(),
            branch: "agentfs/v".into(),
            source,
            replace: None,
            attempts: 3,
            depth: None,
            barrier: false,
        }
    }

    #[test]
    fn takes_the_next_number_and_retries_when_raced() {
        let git = FakeGit::new(&[
            ("refs/heads/agentfs/v", HEAD),
            ("refs/agentfs/v/applied/0000000001", OTHER),
        ]);
        git.steal.borrow_mut().extend([2, 3]);
        let done = submit(&git, &request(Source::Commit("HEAD".into()))).unwrap();
        assert_eq!(done.inbox, 4);
        assert_eq!(done.raced, 2);
        assert_eq!(done.oid, PATCH);
        assert_eq!(done.base, HEAD);
        assert_eq!(done.commits, 1);
        assert_eq!(
            git.remote.borrow()["refs/agentfs/v/inbox/0000000004"],
            PATCH
        );
        let pushes: Vec<String> = git
            .calls
            .borrow()
            .iter()
            .filter(|c| c[0] == "push")
            .map(|c| c[3].clone())
            .collect();
        assert_eq!(
            pushes,
            [
                "--force-with-lease=refs/agentfs/v/inbox/0000000002:",
                "--force-with-lease=refs/agentfs/v/inbox/0000000003:",
                "--force-with-lease=refs/agentfs/v/inbox/0000000004:",
            ]
        );
    }

    #[test]
    fn gives_up_after_attempts() {
        let git = FakeGit::new(&[("refs/heads/agentfs/v", HEAD)]);
        git.steal.borrow_mut().extend(1..=10);
        let err = submit(&git, &request(Source::Commit("HEAD".into()))).unwrap_err();
        assert!(matches!(err, SubmitError::Raced { attempts: 3 }), "{err}");
    }

    #[test]
    fn replace_leases_on_the_listed_oid() {
        let git = FakeGit::new(&[
            ("refs/heads/agentfs/v", HEAD),
            ("refs/agentfs/v/inbox/0000000002", OTHER),
        ]);
        let mut req = request(Source::Commit("HEAD".into()));
        req.replace = Some(2);
        let done = submit(&git, &req).unwrap();
        assert_eq!(done.inbox, 2);
        let push = git
            .calls
            .borrow()
            .iter()
            .find(|c| c[0] == "push")
            .cloned()
            .unwrap();
        assert_eq!(
            push[3],
            format!("--force-with-lease=refs/agentfs/v/inbox/0000000002:{OTHER}")
        );
        req.replace = Some(3);
        assert!(matches!(
            submit(&git, &req).unwrap_err(),
            SubmitError::NotQueued(3)
        ));
    }

    /// A missing number below the highest one is a gap: `--skip N` and
    /// `--replace N` fill it create-only.
    #[test]
    fn skip_and_replace_fill_a_gap() {
        let git = FakeGit::new(&[
            ("refs/heads/agentfs/v", HEAD),
            ("refs/agentfs/v/applied/0000000001", OTHER),
            ("refs/agentfs/v/inbox/0000000003", OTHER),
        ]);
        let done = submit(&git, &request(Source::Skip(2))).unwrap();
        assert_eq!(done.inbox, 2);
        let push = git
            .calls
            .borrow()
            .iter()
            .find(|c| c[0] == "push")
            .cloned()
            .unwrap();
        assert_eq!(
            push[3],
            "--force-with-lease=refs/agentfs/v/inbox/0000000002:"
        );
        assert_eq!(
            git.remote.borrow()["refs/agentfs/v/inbox/0000000002"],
            PATCH
        );
        // Not a gap: above the highest number.
        let mut req = request(Source::Commit("HEAD".into()));
        req.replace = Some(4);
        assert!(matches!(
            submit(&git, &req).unwrap_err(),
            SubmitError::NotQueued(4)
        ));
        // A gap filled by --replace.
        git.remote
            .borrow_mut()
            .insert("refs/agentfs/v/inbox/0000000006".into(), OTHER.into());
        req.replace = Some(5);
        assert_eq!(submit(&git, &req).unwrap().inbox, 5);
    }

    /// A number the volume applied is refused, even while the remote still
    /// lists its inbox ref (applied ref held back, or not deleted yet).
    #[test]
    fn replacing_an_applied_patch_is_refused() {
        let git = FakeGit::new(&[
            ("refs/heads/agentfs/v", HEAD),
            ("refs/agentfs/v/inbox/0000000004", OTHER),
            ("refs/agentfs/v/inbox/0000000005", OTHER),
            ("refs/agentfs/v/status", OTHER),
        ]);
        *git.status.borrow_mut() = Some(r#"{"last_applied": {"inbox": 4}}"#.into());
        let mut req = request(Source::Commit("HEAD".into()));
        req.replace = Some(4);
        assert!(matches!(
            submit(&git, &req).unwrap_err(),
            SubmitError::AlreadyApplied(4)
        ));
        assert!(matches!(
            submit(&git, &request(Source::Skip(4))).unwrap_err(),
            SubmitError::AlreadyApplied(4)
        ));
        req.replace = Some(5);
        assert_eq!(submit(&git, &req).unwrap().inbox, 5);
        // The applied ref says so too.
        git.remote
            .borrow_mut()
            .insert("refs/agentfs/v/applied/0000000005".into(), OTHER.into());
        assert!(matches!(
            submit(&git, &req).unwrap_err(),
            SubmitError::AlreadyApplied(5)
        ));
        assert!(git.calls.borrow().iter().all(|c| c[0] != "push"
            || c[3] != format!("--force-with-lease=refs/agentfs/v/inbox/0000000004:{OTHER}")));
    }

    /// B7: a commit already queued or applied is refused early, and
    /// nothing is pushed.
    #[test]
    fn a_queued_or_applied_commit_is_refused_early() {
        let pushes = |git: &FakeGit| git.calls.borrow().iter().filter(|c| c[0] == "push").count();
        let git = FakeGit::new(&[
            ("refs/heads/agentfs/v", HEAD),
            ("refs/agentfs/v/inbox/0000000003", PATCH),
        ]);
        match submit(&git, &request(Source::Commit("HEAD".into()))).unwrap_err() {
            SubmitError::AlreadySubmitted {
                commit,
                inbox,
                applied,
            } => {
                assert_eq!(commit, PATCH);
                assert_eq!(inbox, Some(3));
                assert_eq!(applied, None);
            }
            other => panic!("{other}"),
        }
        assert_eq!(pushes(&git), 0);
        let git = FakeGit::new(&[
            ("refs/heads/agentfs/v", HEAD),
            ("refs/agentfs/v/applied/0000000002", PATCH),
        ]);
        assert!(matches!(
            submit(&git, &request(Source::Commit("HEAD".into()))).unwrap_err(),
            SubmitError::AlreadySubmitted {
                inbox: None,
                applied: Some(2),
                ..
            }
        ));
        assert_eq!(pushes(&git), 0);
        // Applied, its applied ref held back: the status says so.
        let git = FakeGit::new(&[
            ("refs/heads/agentfs/v", HEAD),
            ("refs/agentfs/v/status", OTHER),
        ]);
        *git.status.borrow_mut() = Some(format!(
            r#"{{"last_applied": {{"inbox": 5, "patch": "{PATCH}"}}}}"#
        ));
        assert!(matches!(
            submit(&git, &request(Source::Commit("HEAD".into()))).unwrap_err(),
            SubmitError::AlreadySubmitted {
                applied: Some(5),
                ..
            }
        ));
        assert_eq!(pushes(&git), 0);
        // Another patch applied last: submitted.
        *git.status.borrow_mut() = Some(format!(
            r#"{{"last_applied": {{"inbox": 5, "patch": "{OTHER}"}}}}"#
        ));
        assert_eq!(
            submit(&git, &request(Source::Commit("HEAD".into())))
                .unwrap()
                .inbox,
            1
        );
    }

    /// A patch the queue set aside below the last applied number is still
    /// in the status's queue, so it can be replaced (bead agentfs-mia.2).
    #[test]
    fn a_set_aside_patch_is_replaceable() {
        let git = FakeGit::new(&[
            ("refs/heads/agentfs/v", HEAD),
            ("refs/agentfs/v/inbox/0000000002", OTHER),
            ("refs/agentfs/v/status", OTHER),
        ]);
        *git.status.borrow_mut() =
            Some(r#"{"last_applied": {"inbox": 3}, "queue": [{"inbox": 2, "patch": "x"}]}"#.into());
        let mut req = request(Source::Commit("HEAD".into()));
        req.replace = Some(2);
        assert_eq!(submit(&git, &req).unwrap().inbox, 2);
        assert_eq!(submit(&git, &request(Source::Skip(2))).unwrap().inbox, 2);
        // Not queued any more: applied.
        *git.status.borrow_mut() = Some(r#"{"last_applied": {"inbox": 3}, "queue": []}"#.into());
        assert!(matches!(
            submit(&git, &req).unwrap_err(),
            SubmitError::AlreadyApplied(2)
        ));
    }

    /// `--barrier`: a built or skip patch gets the trailer; a given tip
    /// must carry it.
    #[test]
    fn barriers_carry_the_trailer() {
        let git = FakeGit::new(&[("refs/heads/agentfs/v", HEAD)]);
        let mut req = request(Source::Build {
            puts: Vec::new(),
            deletes: vec!["gone".into()],
            message: "drop gone".into(),
            author: None,
        });
        req.barrier = true;
        submit(&git, &req).unwrap();
        let message = |git: &FakeGit| {
            let calls = git.calls.borrow();
            let call = calls.iter().rev().find(|c| c[0] == "commit-tree").unwrap();
            call[call.iter().position(|a| a == "-m").unwrap() + 1].clone()
        };
        assert_eq!(message(&git), "drop gone\n\nAgentfs-Barrier: yes\n");
        let git = FakeGit::new(&[
            ("refs/heads/agentfs/v", HEAD),
            ("refs/agentfs/v/inbox/0000000004", OTHER),
        ]);
        let mut skip = request(Source::Skip(4));
        skip.barrier = true;
        submit(&git, &skip).unwrap();
        assert_eq!(
            message(&git),
            "agentfs: skip patch 4\n\nAgentfs-Barrier: yes\n"
        );
        let git = FakeGit::new(&[("refs/heads/agentfs/v", HEAD)]);
        let mut req = request(Source::Commit("HEAD".into()));
        req.barrier = true;
        assert!(matches!(
            submit(&git, &req).unwrap_err(),
            SubmitError::NotABarrier { .. }
        ));
        *git.message.borrow_mut() = "patch\n\nAgentfs-Barrier: yes\n".into();
        assert_eq!(submit(&git, &req).unwrap().inbox, 1);
    }

    fn approve_request(expect: Option<&str>) -> ApproveRequest {
        ApproveRequest {
            remote: "origin".into(),
            volume: "v".into(),
            inbox: 2,
            expect: expect.map(str::to_owned),
        }
    }

    #[test]
    fn approve_names_the_listed_tip() {
        let pushes = |git: &FakeGit| git.calls.borrow().iter().filter(|c| c[0] == "push").count();
        let git = FakeGit::new(&[("refs/heads/agentfs/v", HEAD)]);
        assert!(matches!(
            approve(&git, &approve_request(None)).unwrap_err(),
            SubmitError::NotQueued(2)
        ));
        git.remote
            .borrow_mut()
            .insert("refs/agentfs/v/inbox/0000000002".into(), PATCH.into());
        match approve(&git, &approve_request(Some(OTHER))).unwrap_err() {
            SubmitError::Refused { ref_name, message } => {
                assert_eq!(ref_name, "refs/agentfs/v/inbox/0000000002");
                assert_eq!(message, format!("the queued patch is now {PATCH}"));
            }
            other => panic!("{other}"),
        }
        assert_eq!(pushes(&git), 0);
        let done = approve(&git, &approve_request(Some(PATCH))).unwrap();
        assert_eq!(done.ref_name, "refs/agentfs/v/approved/0000000002");
        assert_eq!(done.tip, PATCH);
        assert_eq!(
            git.remote.borrow()["refs/agentfs/v/approved/0000000002"],
            PATCH
        );
        let push = git
            .calls
            .borrow()
            .iter()
            .find(|c| c[0] == "push")
            .cloned()
            .unwrap();
        assert_eq!(
            push[3],
            "--force-with-lease=refs/agentfs/v/approved/0000000002:"
        );
        // Again: nothing to push.
        approve(&git, &approve_request(None)).unwrap();
        assert_eq!(pushes(&git), 1);
        // A replaced patch: approved again, leased on the old approval.
        git.remote
            .borrow_mut()
            .insert("refs/agentfs/v/inbox/0000000002".into(), OTHER.into());
        assert_eq!(approve(&git, &approve_request(None)).unwrap().tip, OTHER);
        let last = git
            .calls
            .borrow()
            .iter()
            .rev()
            .find(|c| c[0] == "push")
            .cloned()
            .unwrap();
        assert_eq!(
            last[3],
            format!("--force-with-lease=refs/agentfs/v/approved/0000000002:{PATCH}")
        );
    }

    #[test]
    fn no_branch_no_patch() {
        let git = FakeGit::new(&[]);
        assert!(matches!(
            submit(&git, &request(Source::Commit("HEAD".into()))).unwrap_err(),
            SubmitError::NoBranch(_)
        ));
    }
}
