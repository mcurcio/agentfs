//! Submitting a patch (design §12, §19.1): the producer side.
//!
//! A producer (iac, CI, an operator) runs `agentfs patch submit` in its own
//! git repository. It drives the producer's `git`, so the repository's
//! remote configuration, credential helpers and SSH setup apply, and
//! shallow clones work; the state area is never involved.
//!
//! 1. `git ls-remote` lists the volume's branch, inbox and applied refs.
//! 2. `git fetch` brings the branch head `H`.
//! 3. The patch is a commit whose parent is `H` or one of its ancestors:
//!    a given commit with exactly one parent, or one built here on `H`
//!    from `--put`/`--delete` (with a temporary index), or, for `--skip`,
//!    an empty commit on `H`.
//! 4. Its number is one past the highest inbox or applied number on the
//!    remote (or the number being replaced).
//! 5. `git push --force-with-lease=<ref>: <oid>:<ref>` creates the inbox
//!    ref only if it does not exist yet (`<ref>:<oid>` for a replacement,
//!    which must still hold what was listed). A submitter that loses a race
//!    for a number lists again and tries the next one, up to `attempts`
//!    times.
//!
//! Applied refs are pushed before inbox refs are deleted on the remote
//! (see [`crate::push`]), so the highest number listed never goes down
//! and a number is never reused.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::{Command, Stdio};

use super::{applied_prefix, inbox_prefix, inbox_ref, seq_of_ref};

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
    #[error("{commit} has {parents} parents; a patch is a commit with exactly one parent")]
    NotSingleParent { commit: String, parents: usize },
    #[error(
        "{commit}'s parent {parent} is not on the volume's branch {branch} (head {head}); \
         rebase the patch onto the branch{shallow}"
    )]
    ParentNotInHistory {
        commit: String,
        parent: String,
        branch: String,
        head: String,
        shallow: &'static str,
    },
    #[error("patch {0} is not queued on the remote, so it cannot be replaced")]
    NotQueued(u64),
    #[error("lost the race for an inbox number {attempts} times; try again")]
    Raced { attempts: u32 },
    #[error("the remote refused {ref_name}: {message}")]
    Refused { ref_name: String, message: String },
    #[error("{0}")]
    BadRequest(String),
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
    /// An existing commit (any revision), with one parent on the branch.
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
}

/// What was submitted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Submitted {
    pub inbox: u64,
    pub ref_name: String,
    pub oid: String,
    pub parent: String,
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
    let mut fetch = vec!["fetch", "--quiet", "--no-tags", "--no-write-fetch-head"];
    let depth;
    if let Some(d) = req.depth {
        depth = format!("--depth={d}");
        fetch.push(&depth);
    }
    fetch.push(&req.remote);
    fetch.push(&head);
    let fetched = git.run(&fetch, &[])?;
    if !fetched.success {
        // Older servers refuse fetching by object id; fetch the ref.
        fetch.pop();
        fetch.retain(|a| *a != "--no-write-fetch-head");
        fetch.push(&branch_ref);
        check(git, &fetch, &[])?;
    }
    let (oid, parent) = match &req.source {
        Source::Commit(rev) => existing_commit(git, req, rev, &head)?,
        Source::Build {
            puts,
            deletes,
            message,
            author,
        } => (
            build_commit(git, &head, puts, deletes, message, author.as_deref())?,
            head.clone(),
        ),
        Source::Skip(n) => (
            build_commit(
                git,
                &head,
                &[],
                &[],
                &format!("agentfs: skip patch {n}"),
                None,
            )?,
            head.clone(),
        ),
    };
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
        let ref_name = inbox_ref(&req.volume, n);
        let Some(current) = listing.get(&ref_name) else {
            return Err(SubmitError::NotQueued(n));
        };
        return match push(git, req, &oid, &ref_name, current)? {
            PushLine::Ok => Ok(Submitted {
                inbox: n,
                ref_name,
                oid,
                parent,
                raced: 0,
            }),
            PushLine::Rejected(message) => Err(SubmitError::Refused { ref_name, message }),
        };
    }
    let mut listing = listing;
    let attempts = req.attempts.max(1);
    for raced in 0..attempts {
        let n = next_seq(&req.volume, listing.keys());
        let ref_name = inbox_ref(&req.volume, n);
        match push(git, req, &oid, &ref_name, "")? {
            PushLine::Ok => {
                return Ok(Submitted {
                    inbox: n,
                    ref_name,
                    oid,
                    parent,
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
                            parent,
                            raced,
                        });
                    }
                    None => return Err(SubmitError::Refused { ref_name, message }),
                }
            }
        }
    }
    Err(SubmitError::Raced { attempts })
}

fn ls_remote(
    git: &impl GitRunner,
    req: &SubmitRequest,
) -> Result<BTreeMap<String, String>, SubmitError> {
    let branch_ref = format!("refs/heads/{}", req.branch);
    let queue = format!("refs/agentfs/{}/*", req.volume);
    let out = check(
        git,
        &["ls-remote", "--refs", &req.remote, &branch_ref, &queue],
        &[],
    )?;
    let prefix = format!("refs/agentfs/{}/", req.volume);
    Ok(parse_ls_remote(&out.stdout)
        .into_iter()
        .filter(|(name, _)| *name == branch_ref || name.starts_with(&prefix))
        .collect())
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
    let lease = format!("--force-with-lease={ref_name}:{expected}");
    let spec = format!("{oid}:{ref_name}");
    let args = [
        "push",
        "--porcelain",
        "--no-verify",
        &lease,
        &req.remote,
        &spec,
    ];
    let out = git.run(&args, &[])?;
    match parse_porcelain(&out.stdout, ref_name) {
        Some(line) => Ok(line),
        None if out.success => Ok(PushLine::Ok),
        None => Err(git_error(&args, &out)),
    }
}

/// A given commit: one parent, on the branch.
fn existing_commit(
    git: &impl GitRunner,
    req: &SubmitRequest,
    rev: &str,
    head: &str,
) -> Result<(String, String), SubmitError> {
    let spec = format!("{rev}^{{commit}}");
    let oid = check(git, &["rev-parse", "--verify", "--quiet", &spec], &[])?
        .stdout
        .trim()
        .to_owned();
    let parents = check(git, &["rev-list", "--parents", "-n", "1", &oid], &[])?;
    let parents: Vec<String> = parents
        .stdout
        .split_whitespace()
        .skip(1)
        .map(str::to_owned)
        .collect();
    let [parent] = parents.as_slice() else {
        return Err(SubmitError::NotSingleParent {
            commit: oid,
            parents: parents.len(),
        });
    };
    let ancestor = git.run(&["merge-base", "--is-ancestor", parent, head], &[])?;
    if ancestor.success {
        return Ok((oid, parent.clone()));
    }
    let shallow = git
        .run(&["rev-parse", "--is-shallow-repository"], &[])
        .map(|o| o.stdout.trim() == "true")
        .unwrap_or(false);
    Err(SubmitError::ParentNotInHistory {
        commit: oid,
        parent: parent.clone(),
        branch: req.branch.clone(),
        head: head.to_owned(),
        shallow: if shallow {
            " (this clone is shallow: a larger --depth may find it)"
        } else {
            ""
        },
    })
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

fn check(
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
                "fetch" | "merge-base" => Self::ok(""),
                "rev-parse" => Self::ok(&format!("{PATCH}\n")),
                "rev-list" => Self::ok(&format!("{PATCH} {HEAD}\n")),
                "push" => {
                    let spec = args.last().unwrap();
                    let (oid, name) = spec.split_once(':').unwrap();
                    let lease = args[3].strip_prefix("--force-with-lease=").unwrap();
                    let (_, expected) = lease.rsplit_once(':').unwrap();
                    let n = super::super::seq_of_ref("refs/agentfs/v/inbox/", name).unwrap();
                    if self.steal.borrow().contains(&n) {
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
        assert_eq!(done.parent, HEAD);
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

    #[test]
    fn no_branch_no_patch() {
        let git = FakeGit::new(&[]);
        assert!(matches!(
            submit(&git, &request(Source::Commit("HEAD".into()))).unwrap_err(),
            SubmitError::NoBranch(_)
        ));
    }
}
