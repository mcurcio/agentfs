//! Delivering a published change (design §11.5 "Delivery", §19.1 "Fork
//! delivery"): the library's four built-in methods, for a receiver to
//! program. The CSI driver never delivers (gate answer "2 a for CSI and b
//! for the library"); it only publishes.
//!
//! A Strategy: one contract, [`Delivery`], four single-responsibility
//! implementations, and one factory, [`delivery`], the only place a
//! [`DeliveryMethod`] becomes one and is validated. Every method runs in
//! the receiver's repository with its own git and credentials, on a
//! [`PublishedChange`] that [`super::listing::read`] checked and fetched.
//!
//! - [`InboxDelivery`], at most once: `patch submit` of the change commit
//!   into the inbox of the volume the runner names (never the change's
//!   own `Agentfs-Source`); the remote already queuing or having applied
//!   it is [`Delivered::AlreadyDelivered`], and the queue's duplicate rule
//!   (§12) is the guarantee.
//! - [`BranchDelivery`]: a pull request's head, `<prefix><fork>`, created
//!   only if absent. "Already delivered" means the ref is at exactly the
//!   commit this delivery creates (deterministic, so content-based, never
//!   a trailer match); a ref at another commit is refused, never moved.
//! - [`CommandDelivery`], at least once: a program, run without a shell,
//!   with the payload on stdin.
//! - [`WebhookDelivery`], at least once: an HTTP POST of the payload, with
//!   `Idempotency-Key: <change>`.
//!
//! A receiver that dedupes on [`DeliveryPayload::change`] turns the two
//! at-least-once methods into at most once.

use std::path::PathBuf;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::command::{Finished, RunError, run_with_stdin};
use crate::config::validate_name;
use crate::patch::submit::{
    self, DEFAULT_ATTEMPTS, GitRunner, PushLine, Source, SubmitError, SubmitRequest, check,
    parse_ls_remote,
};

use super::identity::{change_ref, finish_ref};
use super::listing::{ListError, PublishedChange, read_commit};

/// The payload's schema version: a receiver refuses one it does not know.
pub const PAYLOAD_SCHEMA: u32 = 1;
/// The branch method's default prefix: `agentfs-review/<fork>`.
pub const DEFAULT_PREFIX: &str = "agentfs-review/";
/// The command and webhook methods' default timeout.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(60);

/// What the command and webhook methods send, as JSON (schema 1).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeliveryPayload {
    /// [`PAYLOAD_SCHEMA`].
    pub schema: u32,
    pub fork: String,
    pub source: String,
    pub base: String,
    /// The change commit: receivers dedupe on this.
    pub change: String,
    pub finish: String,
    /// Where to fetch the change from.
    pub fetch: FetchRefs,
}

/// The fork remote, as the runner named it, and the fork's refs there.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FetchRefs {
    pub remote: String,
    pub change: String,
    pub finish: String,
    pub history: String,
}

impl DeliveryPayload {
    pub fn new(change: &PublishedChange, fork_remote: &str) -> Self {
        Self {
            schema: PAYLOAD_SCHEMA,
            fork: change.fork.clone(),
            source: change.source.clone(),
            base: change.base.clone(),
            change: change.change.clone(),
            finish: change.finish.clone(),
            fetch: FetchRefs {
                remote: fork_remote.to_owned(),
                change: change_ref(&change.fork),
                finish: finish_ref(&change.fork),
                history: change.history_ref.clone(),
            },
        }
    }
}

/// What the branch method pushes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Shape {
    /// One commit: the change's tree on base (or on `onto`).
    #[default]
    Squash,
    /// The fork's history as published, up to its finish commit.
    History,
}

/// How a change is delivered: the runner's own settings, never the
/// change's.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeliveryMethod {
    /// At most once (the §12 duplicate rule). `volume` comes from the
    /// runner's flags, never from the change.
    Inbox {
        remote: String,
        volume: String,
        /// The volume's branch, without `refs/heads/` (default
        /// `agentfs/<volume>`).
        branch: Option<String>,
    },
    /// A pull request's head: ref `<prefix><fork>`, created only if
    /// absent. Squash: one commit, tree = the change's, parent = base (or
    /// `onto`'s head after a three-way merge against base; a conflict is
    /// refused). History: the fork's history ref as published (`onto` not
    /// allowed).
    Branch {
        remote: String,
        prefix: String,
        shape: Shape,
        onto: Option<String>,
    },
    /// At least once: `argv[0]` run without a shell, the payload as JSON
    /// on stdin, an empty environment but `PATH`, killed after `timeout`.
    Command {
        argv: Vec<String>,
        timeout: Duration,
    },
    /// At least once: POST of the payload as `application/json` with
    /// `Idempotency-Key: <change>`, `Authorization: Bearer <contents of
    /// credential_file>` when set, no redirects, `timeout`.
    Webhook {
        url: String,
        credential_file: Option<PathBuf>,
        timeout: Duration,
    },
}

/// What a delivery did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Delivered {
    /// Queued as inbox number `inbox`.
    Submitted { inbox: u64 },
    /// `ref_name` created at `commit`.
    Pushed { ref_name: String, commit: String },
    /// The command or webhook accepted it.
    Accepted,
    /// The receiver already has exactly this delivery.
    AlreadyDelivered,
}

#[derive(Debug, thiserror::Error)]
pub enum DeliveryError {
    #[error(transparent)]
    Submit(#[from] SubmitError),
    #[error("the receiver refused the change: {detail}")]
    ReceiverRefused { detail: String },
    #[error("the receiver is unavailable: {detail}")]
    ReceiverUnavailable { detail: String },
    #[error("{0}")]
    BadMethod(String),
}

impl DeliveryError {
    /// Unavailable, and Submit errors that are races or the network (a
    /// failed `ls-remote`, `fetch` or `push`); never ReceiverRefused or
    /// BadMethod.
    pub fn is_retryable(&self) -> bool {
        match self {
            Self::ReceiverUnavailable { .. } => true,
            Self::Submit(SubmitError::Raced { .. }) => true,
            Self::Submit(SubmitError::Git { args, .. }) => {
                ["ls-remote", "fetch", "push"].contains(&args.split(' ').next().unwrap_or(""))
            }
            _ => false,
        }
    }
}

/// The delivery contract.
pub trait Delivery {
    fn deliver(&self, change: &PublishedChange) -> Result<Delivered, DeliveryError>;
}

/// The one place a method becomes a [`Delivery`]. It validates the method
/// first: a valid volume name, a non-empty argv, an absolute
/// `credential_file`, an `http(s)` url (`https` only with the feature), a
/// prefix that makes valid branch names, a valid `onto` branch, and no
/// `onto` with [`Shape::History`].
pub fn delivery<'g, G: GitRunner + 'g>(
    method: &DeliveryMethod,
    git: &'g G,
    fork_remote: &str,
) -> Result<Box<dyn Delivery + 'g>, DeliveryError> {
    let bad = |m: String| Err(DeliveryError::BadMethod(m));
    match method {
        DeliveryMethod::Inbox {
            remote,
            volume,
            branch,
        } => {
            if validate_name("volume", volume).is_err() {
                return bad(format!("{volume:?} is not a volume name"));
            }
            if remote.is_empty() {
                return bad("the inbox method needs a remote".into());
            }
            if let Some(b) = branch
                && !valid_branch(b)
            {
                return bad(format!("{b:?} is not a valid branch name"));
            }
            Ok(Box::new(InboxDelivery::new(
                remote.clone(),
                volume.clone(),
                branch.clone(),
                git,
            )))
        }
        DeliveryMethod::Branch {
            remote,
            prefix,
            shape,
            onto,
        } => {
            if remote.is_empty() {
                return bad("the branch method needs a remote".into());
            }
            if !valid_branch(&format!("{prefix}fork")) || prefix.starts_with("refs/") {
                return bad(format!(
                    "{prefix:?} is not a valid branch prefix (refs/heads/<prefix><fork>)"
                ));
            }
            if let Some(o) = onto {
                if *shape == Shape::History {
                    return bad(
                        "--onto applies to the squash shape only: the history shape pushes \
                         the fork's history as published"
                            .into(),
                    );
                }
                if !valid_branch(o) {
                    return bad(format!("{o:?} is not a valid branch name"));
                }
            }
            Ok(Box::new(BranchDelivery::new(
                remote.clone(),
                prefix.clone(),
                *shape,
                onto.clone(),
                git,
            )))
        }
        DeliveryMethod::Command { argv, timeout } => {
            if argv.first().is_none_or(String::is_empty) {
                return bad("the command method needs a program (after `--`)".into());
            }
            Ok(Box::new(CommandDelivery::new(
                argv.clone(),
                *timeout,
                fork_remote,
            )))
        }
        DeliveryMethod::Webhook {
            url,
            credential_file,
            timeout,
        } => {
            let lower = url.to_ascii_lowercase();
            let https = lower.starts_with("https://");
            if !https && !lower.starts_with("http://") {
                return bad(format!("{url:?} is not an http:// or https:// URL"));
            }
            if https && !cfg!(feature = "https") {
                return bad(format!(
                    "{url:?} is https, and this build has no TLS (the `https` feature)"
                ));
            }
            if let Some(f) = credential_file
                && !f.is_absolute()
            {
                return bad(format!(
                    "the credential file {} is not an absolute path",
                    f.display()
                ));
            }
            Ok(Box::new(WebhookDelivery::new(
                url.clone(),
                credential_file.clone(),
                *timeout,
                fork_remote,
            )))
        }
    }
}

/// `refs/heads/<name>` is a valid ref name.
fn valid_branch(name: &str) -> bool {
    !name.is_empty() && git2::Reference::is_valid_name(&format!("refs/heads/{name}"))
}

/// The inbox method: `patch submit` of the change commit.
pub struct InboxDelivery<'g, G: GitRunner> {
    remote: String,
    volume: String,
    branch: Option<String>,
    git: &'g G,
}

impl<'g, G: GitRunner> InboxDelivery<'g, G> {
    pub fn new(remote: String, volume: String, branch: Option<String>, git: &'g G) -> Self {
        Self {
            remote,
            volume,
            branch,
            git,
        }
    }
}

impl<G: GitRunner> Delivery for InboxDelivery<'_, G> {
    fn deliver(&self, change: &PublishedChange) -> Result<Delivered, DeliveryError> {
        let request = SubmitRequest {
            remote: self.remote.clone(),
            volume: self.volume.clone(),
            branch: self
                .branch
                .clone()
                .unwrap_or_else(|| format!("agentfs/{}", self.volume)),
            source: Source::Commit(change.change.clone()),
            replace: None,
            attempts: DEFAULT_ATTEMPTS,
            depth: None,
            barrier: false,
        };
        match submit::submit(self.git, &request) {
            Ok(done) => Ok(Delivered::Submitted { inbox: done.inbox }),
            Err(SubmitError::AlreadySubmitted { .. }) => Ok(Delivered::AlreadyDelivered),
            Err(e) => Err(e.into()),
        }
    }
}

/// The branch method: a pull request's head, created only if absent.
pub struct BranchDelivery<'g, G: GitRunner> {
    remote: String,
    prefix: String,
    shape: Shape,
    onto: Option<String>,
    git: &'g G,
}

impl<'g, G: GitRunner> BranchDelivery<'g, G> {
    pub fn new(
        remote: String,
        prefix: String,
        shape: Shape,
        onto: Option<String>,
        git: &'g G,
    ) -> Self {
        Self {
            remote,
            prefix,
            shape,
            onto,
            git,
        }
    }

    fn unavailable(&self, what: &str, out: &submit::GitOutput) -> DeliveryError {
        DeliveryError::ReceiverUnavailable {
            detail: format!("git {what} {} failed: {}", self.remote, out.stderr.trim()),
        }
    }

    fn ls_remote(
        &self,
        refs: &[&str],
    ) -> Result<std::collections::BTreeMap<String, String>, DeliveryError> {
        let mut args = vec!["ls-remote", "--refs", self.remote.as_str()];
        args.extend_from_slice(refs);
        let out = self.git.run(&args, &[])?;
        if !out.success {
            return Err(self.unavailable("ls-remote", &out));
        }
        Ok(parse_ls_remote(&out.stdout))
    }

    /// The squash commit: author and committer copied from the change
    /// commit and a fixed message, so the same inputs give the same id.
    /// On `onto`, the tree is a three-way merge of `onto`'s head and the
    /// change against base.
    fn squash(
        &self,
        change: &PublishedChange,
        onto: Option<&str>,
    ) -> Result<String, DeliveryError> {
        let read = read_commit(self.git, &change.change).map_err(|e| match e {
            ListError::Git(e) => DeliveryError::Submit(e),
            ListError::Unreachable(detail) => DeliveryError::ReceiverUnavailable { detail },
        })?;
        let Some((commit, _)) = read else {
            return Err(DeliveryError::BadMethod(format!(
                "the change {} cannot be read in this repository",
                change.change
            )));
        };
        let (tree, parent) = match onto {
            None => (commit.tree.clone(), change.base.clone()),
            Some(head) => (self.merge(change, head)?, head.to_owned()),
        };
        let idents = [
            (
                "author",
                &commit.author,
                ["GIT_AUTHOR_NAME", "GIT_AUTHOR_EMAIL", "GIT_AUTHOR_DATE"],
            ),
            (
                "committer",
                &commit.committer,
                [
                    "GIT_COMMITTER_NAME",
                    "GIT_COMMITTER_EMAIL",
                    "GIT_COMMITTER_DATE",
                ],
            ),
        ];
        let mut values: Vec<(&str, String)> = Vec::new();
        for (who, header, keys) in idents {
            let Some((name, email, date)) = header.as_deref().and_then(split_ident) else {
                return Err(DeliveryError::BadMethod(format!(
                    "the change {} has no readable {who} line",
                    change.change
                )));
            };
            values.extend(keys.into_iter().zip([name, email, date]));
        }
        let env: Vec<(&str, &str)> = values.iter().map(|(k, v)| (*k, v.as_str())).collect();
        let subject = format!("change of fork {}", change.fork);
        let trailer = format!("Agentfs-Change: {}", change.change);
        let out = check(
            self.git,
            &[
                "commit-tree",
                "--no-gpg-sign",
                &tree,
                "-p",
                &parent,
                "-m",
                &subject,
                "-m",
                &trailer,
            ],
            &env,
        )?;
        Ok(out.stdout.trim().to_owned())
    }

    /// `git merge-tree --write-tree` of `onto`'s head and the change
    /// against base: the merged tree, or the conflicting paths refused.
    fn merge(&self, change: &PublishedChange, head: &str) -> Result<String, DeliveryError> {
        let merge_base = format!("--merge-base={}", change.base);
        let args = [
            "merge-tree",
            "--write-tree",
            "--name-only",
            "--no-messages",
            merge_base.as_str(),
            head,
            change.change.as_str(),
        ];
        let out = self.git.run(&args, &[])?;
        let mut lines = out.stdout.lines();
        let tree = lines.next().unwrap_or("").trim().to_owned();
        match out.code {
            Some(0) if !tree.is_empty() => Ok(tree),
            Some(1) => {
                let paths: Vec<&str> = lines.take_while(|l| !l.is_empty()).collect();
                Err(DeliveryError::ReceiverRefused {
                    detail: format!(
                        "the change conflicts with {} at {head}: {}",
                        self.onto.as_deref().unwrap_or("onto"),
                        paths.join(", ")
                    ),
                })
            }
            _ => Err(DeliveryError::BadMethod(format!(
                "git merge-tree failed ({}); --onto needs git 2.40 or later",
                out.stderr.trim()
            ))),
        }
    }
}

/// `Name <email> <time> <offset>` as (name, email, `@<time> <offset>`),
/// the date in git's internal format.
fn split_ident(ident: &str) -> Option<(String, String, String)> {
    let (who, offset) = ident.rsplit_once(' ')?;
    let (who, time) = who.rsplit_once(' ')?;
    let (name, rest) = who.rsplit_once('<')?;
    let email = rest.strip_suffix('>')?;
    time.parse::<i64>().ok()?;
    Some((
        name.trim().to_owned(),
        email.to_owned(),
        format!("@{time} {offset}"),
    ))
}

impl<G: GitRunner> Delivery for BranchDelivery<'_, G> {
    fn deliver(&self, change: &PublishedChange) -> Result<Delivered, DeliveryError> {
        let ref_name = format!("refs/heads/{}{}", self.prefix, change.fork);
        let onto_ref = self.onto.as_ref().map(|o| format!("refs/heads/{o}"));
        let mut wanted = vec![ref_name.as_str()];
        wanted.extend(onto_ref.as_deref());
        let listed = self.ls_remote(&wanted)?;
        let commit = match self.shape {
            Shape::History => change.finish.clone(),
            Shape::Squash => {
                let head = match &onto_ref {
                    None => None,
                    Some(r) => {
                        let Some(head) = listed.get(r) else {
                            return Err(DeliveryError::ReceiverRefused {
                                detail: format!(
                                    "{} has no branch {r} to deliver onto",
                                    self.remote
                                ),
                            });
                        };
                        submit::fetch_commit(self.git, &self.remote, head, r, None)?;
                        Some(head.as_str())
                    }
                };
                self.squash(change, head)?
            }
        };
        let refused = |at: &str| DeliveryError::ReceiverRefused {
            detail: format!(
                "{ref_name} on {} is at {at}, not {commit}, the commit this delivery \
                 creates; it is never moved",
                self.remote
            ),
        };
        match listed.get(&ref_name) {
            Some(at) if *at == commit => return Ok(Delivered::AlreadyDelivered),
            Some(at) => return Err(refused(at)),
            None => {}
        }
        match submit::push_leased(self.git, &self.remote, &commit, &ref_name, "")? {
            PushLine::Ok => Ok(Delivered::Pushed {
                ref_name: ref_name.clone(),
                commit: commit.clone(),
            }),
            PushLine::Rejected(message) => {
                // Another delivery of the same change may have won.
                match self.ls_remote(&[&ref_name])?.get(&ref_name) {
                    Some(at) if *at == commit => Ok(Delivered::AlreadyDelivered),
                    Some(at) => Err(refused(at)),
                    None => Err(DeliveryError::ReceiverRefused {
                        detail: format!("{} refused {ref_name}: {message}", self.remote),
                    }),
                }
            }
        }
    }
}

/// The command method: a program, no shell, the payload on stdin.
pub struct CommandDelivery {
    argv: Vec<String>,
    timeout: Duration,
    fork_remote: String,
}

impl CommandDelivery {
    pub fn new(argv: Vec<String>, timeout: Duration, fork_remote: &str) -> Self {
        Self {
            argv,
            timeout,
            fork_remote: fork_remote.to_owned(),
        }
    }
}

/// The exit status a receiving program uses for "try again"
/// (`EX_TEMPFAIL`).
const EXIT_TEMPFAIL: i32 = 75;

impl Delivery for CommandDelivery {
    /// Through [`crate::command::run_with_stdin`]: exit 0 accepts, 75 is
    /// unavailable (retried), another status or a signal is a refusal; a
    /// program that cannot start is a bad method, one killed at the
    /// timeout unavailable.
    fn deliver(&self, change: &PublishedChange) -> Result<Delivered, DeliveryError> {
        let payload = serde_json::to_vec(&DeliveryPayload::new(change, &self.fork_remote))
            .map_err(|e| DeliveryError::BadMethod(e.to_string()))?;
        let program = &self.argv[0];
        let finished = match run_with_stdin(&self.argv, &payload, self.timeout) {
            Ok(finished) => finished,
            Err(RunError::NotStarted(e)) => {
                return Err(DeliveryError::BadMethod(format!(
                    "cannot run {program:?}: {e}"
                )));
            }
            Err(RunError::TimedOut(after)) => {
                return Err(DeliveryError::ReceiverUnavailable {
                    detail: format!("{program:?} was killed after {after:?}"),
                });
            }
            Err(RunError::Wait(e)) => {
                return Err(DeliveryError::ReceiverUnavailable {
                    detail: format!("waiting for {program:?}: {e}"),
                });
            }
        };
        match finished {
            Finished::Code(0) => Ok(Delivered::Accepted),
            Finished::Code(EXIT_TEMPFAIL) => Err(DeliveryError::ReceiverUnavailable {
                detail: format!("{program:?} exited with {EXIT_TEMPFAIL} (try again)"),
            }),
            Finished::Code(code) => Err(DeliveryError::ReceiverRefused {
                detail: format!("{program:?} exited with {code}"),
            }),
            Finished::Signal(signal) => Err(DeliveryError::ReceiverRefused {
                detail: format!("{program:?} was killed by signal {signal}"),
            }),
        }
    }
}

/// The webhook method: one POST of the payload.
pub struct WebhookDelivery {
    url: String,
    credential_file: Option<PathBuf>,
    timeout: Duration,
    fork_remote: String,
}

impl WebhookDelivery {
    pub fn new(
        url: String,
        credential_file: Option<PathBuf>,
        timeout: Duration,
        fork_remote: &str,
    ) -> Self {
        Self {
            url,
            credential_file,
            timeout,
            fork_remote: fork_remote.to_owned(),
        }
    }
}

impl Delivery for WebhookDelivery {
    fn deliver(&self, change: &PublishedChange) -> Result<Delivered, DeliveryError> {
        let payload = serde_json::to_vec(&DeliveryPayload::new(change, &self.fork_remote))
            .map_err(|e| DeliveryError::BadMethod(e.to_string()))?;
        // No redirects and never a proxy from the environment, as the LFS
        // client (design §7).
        let agent = crate::lfs::client::agent(1, None);
        let mut request = agent
            .post(self.url.as_str())
            .header("Content-Type", "application/json")
            .header("Idempotency-Key", &change.change);
        if let Some(file) = &self.credential_file {
            let token = std::fs::read_to_string(file).map_err(|e| {
                DeliveryError::BadMethod(format!(
                    "reading the credential file {}: {e}",
                    file.display()
                ))
            })?;
            request = request.header("Authorization", format!("Bearer {}", token.trim()));
        }
        let response = request
            .config()
            .timeout_global(Some(self.timeout))
            .build()
            .send(&payload[..]);
        let response = match response {
            Ok(r) => r,
            Err(e @ (ureq::Error::BadUri(_) | ureq::Error::Http(_))) => {
                return Err(DeliveryError::BadMethod(format!(
                    "the webhook request to {} is malformed: {e}",
                    self.url
                )));
            }
            Err(e) => {
                return Err(DeliveryError::ReceiverUnavailable {
                    detail: format!("POST {}: {e}", self.url),
                });
            }
        };
        let status = response.status().as_u16();
        let location = response
            .headers()
            .get("location")
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        let body = crate::lfs::client::read_small(response);
        let body = String::from_utf8_lossy(&body);
        let detail = format!("POST {} answered {status}: {}", self.url, body.trim());
        match status {
            200..=299 => Ok(Delivered::Accepted),
            409 => Ok(Delivered::AlreadyDelivered),
            429 | 500..=599 => Err(DeliveryError::ReceiverUnavailable { detail }),
            300..=399 => Err(DeliveryError::ReceiverRefused {
                detail: format!(
                    "POST {} answered {status}, a redirect to {}, which is never followed",
                    self.url,
                    location.as_deref().unwrap_or("nowhere")
                ),
            }),
            _ => Err(DeliveryError::ReceiverRefused { detail }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn change() -> PublishedChange {
        PublishedChange {
            fork: "src-0011aabb".into(),
            source: "src".into(),
            base: "1".repeat(40),
            change: "2".repeat(40),
            finish: "3".repeat(40),
            history_ref: "refs/heads/agentfs/src-0011aabb".into(),
        }
    }

    /// C6: schema 1, every field, and a serde round trip.
    #[test]
    fn the_payload_is_schema_1_and_round_trips() {
        let payload = DeliveryPayload::new(&change(), "https://example.com/forks.git");
        let json = serde_json::to_value(&payload).unwrap();
        assert_eq!(
            json,
            serde_json::json!({
                "schema": 1,
                "fork": "src-0011aabb",
                "source": "src",
                "base": "1".repeat(40),
                "change": "2".repeat(40),
                "finish": "3".repeat(40),
                "fetch": {
                    "remote": "https://example.com/forks.git",
                    "change": "refs/agentfs/src-0011aabb/change",
                    "finish": "refs/agentfs/src-0011aabb/finish",
                    "history": "refs/heads/agentfs/src-0011aabb",
                },
            })
        );
        let back: DeliveryPayload = serde_json::from_value(json).unwrap();
        assert_eq!(back, payload);
    }

    struct NoGit;

    impl GitRunner for NoGit {
        fn run(&self, args: &[&str], _: &[(&str, &str)]) -> Result<submit::GitOutput, SubmitError> {
            panic!("validation ran git {args:?}");
        }
    }

    fn refused(method: DeliveryMethod) -> String {
        match delivery(&method, &NoGit, "origin") {
            Err(DeliveryError::BadMethod(m)) => m,
            Err(e) => panic!("{method:?}: {e}"),
            Ok(_) => panic!("{method:?} was accepted"),
        }
    }

    #[test]
    fn the_factory_validates_every_method() {
        let branch = |prefix: &str, shape, onto: Option<&str>| DeliveryMethod::Branch {
            remote: "origin".into(),
            prefix: prefix.into(),
            shape,
            onto: onto.map(str::to_owned),
        };
        assert!(
            delivery(
                &branch(DEFAULT_PREFIX, Shape::Squash, Some("main")),
                &NoGit,
                "o"
            )
            .is_ok()
        );
        assert!(delivery(&branch("", Shape::History, None), &NoGit, "o").is_ok());
        assert!(refused(branch(DEFAULT_PREFIX, Shape::History, Some("main"))).contains("onto"));
        for bad in ["a..b/", "x y/", "refs/heads/", "/", "a.lock/", "~"] {
            refused(branch(bad, Shape::Squash, None));
        }
        refused(branch(DEFAULT_PREFIX, Shape::Squash, Some("a..b")));
        let inbox = |volume: &str, branch: Option<&str>| DeliveryMethod::Inbox {
            remote: "origin".into(),
            volume: volume.into(),
            branch: branch.map(str::to_owned),
        };
        assert!(delivery(&inbox("v", None), &NoGit, "o").is_ok());
        refused(inbox("a/b", None));
        refused(inbox("v", Some("a b")));
        let command = |argv: &[&str]| DeliveryMethod::Command {
            argv: argv.iter().map(|s| s.to_string()).collect(),
            timeout: DEFAULT_TIMEOUT,
        };
        assert!(delivery(&command(&["true"]), &NoGit, "o").is_ok());
        refused(command(&[]));
        refused(command(&[""]));
        let webhook = |url: &str, file: Option<&str>| DeliveryMethod::Webhook {
            url: url.into(),
            credential_file: file.map(PathBuf::from),
            timeout: DEFAULT_TIMEOUT,
        };
        assert!(
            delivery(
                &webhook("http://127.0.0.1:1/x", Some("/run/token")),
                &NoGit,
                "o"
            )
            .is_ok()
        );
        refused(webhook("ftp://example.com/", None));
        refused(webhook("example.com", None));
        refused(webhook("http://example.com/", Some("token")));
        let https = delivery(&webhook("https://example.com/", None), &NoGit, "o");
        assert_eq!(https.is_ok(), cfg!(feature = "https"));
    }

    #[test]
    fn only_the_unavailable_and_the_network_are_retried() {
        let git = |args: &str| {
            DeliveryError::Submit(SubmitError::Git {
                args: args.into(),
                status: "exit 128".into(),
                stderr: String::new(),
            })
        };
        assert!(DeliveryError::ReceiverUnavailable { detail: "x".into() }.is_retryable());
        assert!(DeliveryError::Submit(SubmitError::Raced { attempts: 10 }).is_retryable());
        assert!(git("ls-remote --refs origin").is_retryable());
        assert!(git("fetch --quiet origin").is_retryable());
        assert!(git("push --porcelain").is_retryable());
        assert!(!git("commit-tree t").is_retryable());
        assert!(!DeliveryError::ReceiverRefused { detail: "x".into() }.is_retryable());
        assert!(!DeliveryError::BadMethod("x".into()).is_retryable());
        assert!(!DeliveryError::Submit(SubmitError::NoBranch("b".into())).is_retryable());
    }

    #[test]
    fn splits_identities() {
        assert_eq!(
            split_ident("Ada Lovelace <ada@example.com> 1700000000 +0100"),
            Some((
                "Ada Lovelace".into(),
                "ada@example.com".into(),
                "@1700000000 +0100".into()
            ))
        );
        assert_eq!(split_ident("Ada <ada@example.com>"), None);
        assert_eq!(split_ident("Ada ada@example.com 1 +0000"), None);
    }
}
