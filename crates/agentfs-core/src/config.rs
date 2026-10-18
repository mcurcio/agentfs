//! Operator settings from the history repository's git config (design §4.3,
//! §11.1).
//!
//! Only the repository's own config file is read: system and global git
//! config never influence a volume. Remotes are the standard
//! `[remote "<name>"]` sections with `url` (and optionally `pushurl`), plus
//! agentfs's own per-remote keys.
//!
//! The one exception is `include.path` (design §4.3): an operator-owned
//! fragment, such as a Kubernetes ConfigMap, that libgit2 expands in place.
//! agentfs is stricter than git about it: every include must be an
//! absolute path to a readable regular file (git skips a missing one
//! silently, which would make the operator's remotes and settings vanish
//! without a sign), and conditional `includeIf.*.path` is refused, since it
//! would be evaluated differently with and without a repository. Keeping
//! includes out of the backing tree and the mount point is the mount
//! preflight's job.
//!
//! Git LFS settings (design §7) come from the same file only: the size
//! threshold `agentfs.lfsThreshold`, git-lfs's `lfs.url`, `lfs.pushurl`
//! and `lfs.concurrenttransfers`, and per remote `lfsurl`, `lfspushurl` and `agentfsLfsCredentials`. A
//! tracked `.lfsconfig` is content and is never read.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::fork::identity::{self as fork_identity, ForkIdentity, ForkLink, PushTarget};
use crate::identity::Templates;
use crate::proxy::ProxySetting;
use crate::remote_url::{self, Transport};

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error(transparent)]
    Git(#[from] git2::Error),
    #[error("agentfs.volume is not set")]
    MissingVolume,
    #[error("{key}: {value:?} is not a duration (use e.g. 200ms, 60s, 5m)")]
    BadDuration { key: String, value: String },
    #[error("{key}: {value:?} is not one of {allowed}")]
    BadChoice {
        key: String,
        value: String,
        allowed: &'static str,
    },
    #[error(
        "{key}: {value:?} is not a valid name (letters, digits, `.`, `_` and `-`, \
         starting with a letter or digit, without `..` or a `.lock` suffix)"
    )]
    BadName { key: String, value: String },
    #[error("{key}: {value:?} is not a positive size (use e.g. 512k, 100m, 2g)")]
    BadSize { key: String, value: String },
    #[error("{key}: {value:?} is shorter than the minimum, {min:?}")]
    TooShort {
        key: String,
        value: String,
        min: Duration,
    },
    #[error("{key}: {value:?} must be an absolute path")]
    RelativePath { key: String, value: String },
    #[error("{key} applies only to ssh remotes, and this remote pushes to a non-ssh URL")]
    SshOnly { key: String },
    #[error("{key} must be set for an ssh remote: an absolute path (design §11.1)")]
    SshNeeds { key: String },
    #[error(
        "{remote}.agentfsSshKey or {remote}.agentfsSshAgent must be set for an ssh remote: an \
         absolute path (design §11.1)"
    )]
    SshNoIdentity { remote: String },
    #[error(
        "{remote}.agentfsSshKey and {remote}.agentfsSshAgent are both set; an ssh remote uses \
         one of them"
    )]
    SshKeyAndAgent { remote: String },
    #[error("{key} needs {remote}.agentfsSshKey: it is the passphrase of that key")]
    SshPassphraseWithoutKey { key: String, remote: String },
    #[error("{key}: ssh remotes authenticate with agentfsSshKey, not agentfsCredentials")]
    SshCredentials { key: String },
    #[error(
        "{key}: an ssh URL must not carry a password; ssh remotes authenticate with agentfsSshKey"
    )]
    SshPassword { key: String },
    #[error("{key}: {value:?} is not a valid ssh URL (it needs a host, and a numeric port if any)")]
    SshUrl { key: String, value: String },
    #[error("{key}: ssh remotes need agentfs built with the `ssh` cargo feature")]
    SshUnsupported { key: String },
    #[error("include.path: {value:?} must be an absolute path")]
    IncludeNotAbsolute { value: String },
    #[error(
        "include.path: {} cannot be read ({source}); git would skip it silently, agentfs \
         refuses: create it, or remove the include",
        path.display()
    )]
    IncludeUnreadable {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("{key}: conditional includes are not supported; use include.path")]
    ConditionalInclude { key: String },
    #[error(
        "include.path: {} sets {key}, which only `agentfs init` sets, in the state area's \
         own config",
        path.display()
    )]
    IncludeSetsIdentity { path: PathBuf, key: &'static str },
    #[error("{key}: {value:?} is not a full commit id")]
    BadForkBase { key: String, value: String },
    #[error(
        "agentfs.fork.source and agentfs.fork.base must be set together, and \
         agentfs.fork.pushTarget, agentfs.fork.link and agentfs.fork.sourceState only with \
         them"
    )]
    PartialForkIdentity,
    #[error(
        "agentfs.fork.sourceState is set in an unlinked fork: an unlinked fork delivers nothing \
         to its source and records no path to it"
    )]
    UnlinkedSourceState,
    #[error("agentfs.branch is set in a fork's config; a fork's push target is fixed at create")]
    ForkBranch,
    #[error("agentfs.start.remote, .branch and .commit must be set together")]
    PartialStartPoint,
    #[error(transparent)]
    BadStartPoint(crate::fork::identity::BadStartPoint),
    #[error(
        "remote.{remote}: a fork remote must name its own LFS endpoint (lfsurl or lfspushurl): \
         lfs.url and lfs.pushurl would send a fork's LFS objects to the source's store"
    )]
    ForkRemoteGlobalLfs { remote: String },
    #[error(
        "{key}: {value:?} is not an LFS endpoint agentfs can use (an http:// or https:// \
         URL, a file:// URL or a path; design §7)"
    )]
    LfsUrl { key: String, value: String },
    #[error("{key}: https LFS endpoints need agentfs built with the `https` cargo feature")]
    LfsHttpsUnsupported { key: String },
    #[error(
        "{key}: {value:?} is not a number of concurrent transfers from 1 to \
         {MAX_CONCURRENT_TRANSFERS}"
    )]
    BadConcurrency { key: String, value: String },
    #[error("{key}: {value:?} is not a proxy agentfs can use: {why}")]
    BadProxy {
        key: String,
        value: String,
        why: String,
    },
    /// A global setting that makes every remote misconfigured.
    #[error("{0}")]
    Global(String),
}

/// Every git config key agentfs reads, in the order docs/configuration.md
/// lists them (design §4.3, §7, §9, §11.1). `<name>` stands for a
/// remote's name and `<condition>` for an `includeIf` condition.
///
/// Every read goes through this module's helpers, which note the key in
/// tests: a unit test checks this list against what [`Settings::load`]
/// reads, and `tests/docs_reference.rs` checks docs/configuration.md
/// against this list.
pub const KEYS: &[&str] = &[
    "agentfs.volume",
    "agentfs.branch",
    "agentfs.respectTreeRules",
    "agentfs.nested",
    "agentfs.commitWindow",
    "agentfs.pushInterval",
    "agentfs.sweepInterval",
    "agentfs.repackInterval",
    "agentfs.remotes",
    "agentfs.patchRemote",
    "agentfs.patchFetchTimeout",
    "agentfs.lfsThreshold",
    "agentfs.forkRefs",
    "agentfs.identity.committerName",
    "agentfs.identity.committerEmail",
    "agentfs.identity.authorName",
    "agentfs.identity.authorEmail",
    "agentfs.fork.source",
    "agentfs.fork.base",
    "agentfs.fork.pushTarget",
    "agentfs.fork.link",
    "agentfs.fork.sourceState",
    "agentfs.start.remote",
    "agentfs.start.branch",
    "agentfs.start.commit",
    "lfs.url",
    "lfs.pushurl",
    "lfs.concurrenttransfers",
    "http.proxy",
    "include.path",
    "includeIf.<condition>.path",
    "remote.<name>.url",
    "remote.<name>.pushurl",
    "remote.<name>.agentfsMaxBlobSize",
    "remote.<name>.agentfsMaxPushSize",
    "remote.<name>.agentfsMaxLfsObjectSize",
    "remote.<name>.agentfsCredentials",
    "remote.<name>.agentfsSshKey",
    "remote.<name>.agentfsKnownHosts",
    "remote.<name>.agentfsSshAgent",
    "remote.<name>.agentfsSshKeyPassphrase",
    "remote.<name>.lfsurl",
    "remote.<name>.lfspushurl",
    "remote.<name>.agentfsLfsCredentials",
    "remote.<name>.proxy",
    "remote.<name>.agentfsForkRemote",
];

/// Notes that `key` was read, for the test that checks [`KEYS`] against
/// the reads. A no-op outside tests.
#[cfg(not(test))]
fn note(_key: &str) {}

#[cfg(test)]
fn note(key: &str) {
    reads::note(key);
}

/// The keys read on this thread, normalised to [`KEYS`]'s placeholders.
#[cfg(test)]
pub(crate) mod reads {
    use std::cell::RefCell;
    use std::collections::BTreeSet;

    thread_local! {
        static READ: RefCell<BTreeSet<String>> = const { RefCell::new(BTreeSet::new()) };
    }

    /// `remote.X.k` becomes `remote.<name>.k`; anything else is kept.
    pub(crate) fn normalise(key: &str) -> String {
        if let Some(rest) = key.strip_prefix("remote.")
            && let Some((_, k)) = rest.rsplit_once('.')
        {
            return format!("remote.<name>.{k}");
        }
        key.to_owned()
    }

    pub(crate) fn note(key: &str) {
        READ.with(|r| r.borrow_mut().insert(normalise(key)));
    }

    /// The keys read on this thread since the last call, which clears
    /// them.
    pub(crate) fn take() -> BTreeSet<String> {
        READ.with(|r| std::mem::take(&mut *r.borrow_mut()))
    }
}

/// How nested repositories are handled (design §5).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Nested {
    Exclude,
    WorkingTree,
}

/// One push remote (design §11.1, §11.3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteSettings {
    pub name: String,
    /// `remote.<name>.url`.
    pub url: String,
    /// `remote.<name>.pushurl`, which wins over `url` for pushing, as in git.
    pub push_url: Option<String>,
    /// `remote.<name>.agentfsMaxBlobSize`: the largest blob this remote
    /// accepts, in bytes.
    pub max_blob_size: Option<u64>,
    /// `remote.<name>.agentfsMaxPushSize`: the largest git pack one push
    /// to this remote may send, in bytes (design §11.3,
    /// [`crate::push_size`]). LFS uploads are not counted.
    pub max_push_size: Option<u64>,
    /// `remote.<name>.agentfsMaxLfsObjectSize`: the largest LFS object
    /// this remote's LFS server accepts, in bytes (design §7, §11.3).
    pub max_lfs_object_size: Option<u64>,
    /// `remote.<name>.agentfsCredentials`: an absolute path to the
    /// credential file ([`crate::credentials`]). Not for ssh remotes.
    pub credentials: Option<PathBuf>,
    /// `remote.<name>.agentfsSshKey`: an absolute path to the private key
    /// of an ssh remote ([`crate::credentials::SshKeyFile`]). An ssh remote
    /// needs this or `agentfsSshAgent`; refused for any other remote.
    pub ssh_key: Option<PathBuf>,
    /// `remote.<name>.agentfsKnownHosts`: an absolute path to the
    /// known_hosts file an ssh remote's host key is checked against
    /// ([`crate::known_hosts`]). Required for ssh remotes, refused for any
    /// other.
    pub known_hosts: Option<PathBuf>,
    /// `remote.<name>.agentfsSshAgent`: an absolute path to the socket of
    /// the ssh-agent an ssh remote authenticates with, instead of
    /// `agentfsSshKey` (design §11.1).
    pub ssh_agent: Option<PathBuf>,
    /// `remote.<name>.agentfsSshKeyPassphrase`: an absolute path to the
    /// file holding the passphrase of `agentfsSshKey`
    /// ([`crate::credentials::PassphraseFile`]).
    pub ssh_key_passphrase: Option<PathBuf>,
    /// `remote.<name>.lfsurl`: the LFS endpoint for this remote, unless
    /// `lfs.url` is set (design §7).
    pub lfs_url: Option<String>,
    /// `remote.<name>.lfspushurl`: the LFS endpoint uploads go to, unless
    /// `lfs.pushurl` or `lfs.url` is set.
    pub lfs_push_url: Option<String>,
    /// `remote.<name>.agentfsLfsCredentials`: an absolute path to the
    /// credential file for an LFS endpoint on another host than the push
    /// URL's ([`crate::credentials`] format).
    pub lfs_credentials: Option<PathBuf>,
    /// The proxy this remote's `http(s)` traffic goes through
    /// ([`crate::proxy`]): `remote.<name>.proxy`, else `http.proxy`; an
    /// empty `remote.<name>.proxy` means none.
    pub proxy: Option<ProxySetting>,
    /// `remote.<name>.agentfsForkRemote` (bool, default false): this
    /// section is only for forks of the volume (design §11.5). It is never
    /// one of the volume's own push remotes.
    pub fork_remote: bool,
}

/// The agentfs keys of a remote section that name files, which the mount
/// preflight keeps out of the backing tree (design §17.1).
pub const REMOTE_FILE_KEYS: [&str; 6] = [
    "agentfsCredentials",
    "agentfsSshKey",
    "agentfsKnownHosts",
    "agentfsLfsCredentials",
    "agentfsSshAgent",
    "agentfsSshKeyPassphrase",
];

/// A `[remote "<name>"]` section that is left out because it is invalid.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvalidRemote {
    pub name: String,
    pub error: String,
    /// The section's absolute file paths (`(key, path)`, keys from
    /// [`REMOTE_FILE_KEYS`]), read separately so the mount preflight checks
    /// them even though the section is not used.
    pub files: Vec<(&'static str, PathBuf)>,
    /// The section sets `agentfsForkRemote` (or a value that cannot be
    /// read as false): it is only for forks, never the volume's own.
    pub fork_remote: bool,
}

impl RemoteSettings {
    /// The URL pushes go to.
    pub fn push_url(&self) -> &str {
        self.push_url.as_deref().unwrap_or(&self.url)
    }

    /// The transport of the push URL.
    pub fn transport(&self) -> Transport {
        remote_url::transport(self.push_url())
    }

    /// The files this remote names, as `(key, path)`.
    pub fn files(&self) -> Vec<(&'static str, &Path)> {
        REMOTE_FILE_KEYS
            .into_iter()
            .zip([
                &self.credentials,
                &self.ssh_key,
                &self.known_hosts,
                &self.lfs_credentials,
                &self.ssh_agent,
                &self.ssh_key_passphrase,
            ])
            .filter_map(|(key, path)| Some((key, path.as_deref()?)))
            .collect()
    }
}

/// The default `lfs.concurrenttransfers`, as in git-lfs.
pub const DEFAULT_CONCURRENT_TRANSFERS: usize = 8;
/// The largest `lfs.concurrenttransfers` accepted.
pub const MAX_CONCURRENT_TRANSFERS: usize = 64;

/// Git LFS settings (design §7), all optional.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LfsSettings {
    /// `agentfs.lfsThreshold`: content larger than this many bytes is
    /// stored as LFS. Unset (the default) means no size rule: only
    /// `agentfs.store=lfs` and operator `filter=lfs` select LFS.
    pub threshold: Option<u64>,
    /// `lfs.url`: the LFS endpoint for every remote, as in git-lfs.
    pub url: Option<String>,
    /// `lfs.pushurl`: the LFS endpoint uploads go to, for every remote.
    pub push_url: Option<String>,
    /// `lfs.concurrenttransfers`: how many objects one remote's transfer
    /// moves at once (design §7), [`DEFAULT_CONCURRENT_TRANSFERS`] unless
    /// set; 1 to [`MAX_CONCURRENT_TRANSFERS`].
    pub concurrent_transfers: usize,
    /// Why `lfs.concurrenttransfers` cannot be used, if it cannot: then
    /// every remote is misconfigured, as with a bad `lfs.url`.
    pub concurrent_transfers_error: Option<String>,
}

impl Default for LfsSettings {
    fn default() -> Self {
        Self {
            threshold: None,
            url: None,
            push_url: None,
            concurrent_transfers: DEFAULT_CONCURRENT_TRANSFERS,
            concurrent_transfers_error: None,
        }
    }
}

/// `lfs.concurrenttransfers`: the value, or why it is refused.
fn concurrent_transfers(config: &git2::Config) -> Result<(usize, Option<String>), ConfigError> {
    let key = "lfs.concurrenttransfers";
    let Some(value) = string(config, key)? else {
        return Ok((DEFAULT_CONCURRENT_TRANSFERS, None));
    };
    match value.trim().parse::<usize>() {
        Ok(n) if (1..=MAX_CONCURRENT_TRANSFERS).contains(&n) => Ok((n, None)),
        _ => Ok((
            DEFAULT_CONCURRENT_TRANSFERS,
            Some(
                ConfigError::BadConcurrency {
                    key: key.into(),
                    value,
                }
                .to_string(),
            ),
        )),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Settings {
    pub volume: String,
    /// Full ref name of the volume's history branch.
    pub branch_ref: String,
    pub respect_tree_rules: bool,
    pub nested: Nested,
    pub commit_window: Duration,
    pub push_interval: Duration,
    /// `agentfs.sweepInterval`: how often the FUSE layer's sweep records
    /// files that stay open and changed, and retries failed records (design
    /// §3.1, §3.5). At least [`MIN_SWEEP_INTERVAL`].
    pub sweep_interval: Duration,
    /// `agentfs.repackInterval`: how often the daemon repacks the history
    /// with the git CLI (design §8). `None` (`0` or `off`) never repacks.
    /// At least [`MIN_REPACK_INTERVAL`].
    pub repack_interval: Option<Duration>,
    pub identity: Templates,
    /// Git LFS (design §7).
    pub lfs: LfsSettings,
    /// `http.proxy`, normalised ([`crate::proxy`]): the proxy of every
    /// remote without its own `remote.<name>.proxy`. A bad value
    /// misconfigures every remote; `None` then.
    pub http_proxy: Option<String>,
    /// How the volume declares its push remotes, and the valid ones,
    /// sorted by name (design §11.1; F8 S3).
    pub remotes: Remotes,
    /// Remote sections that could not be used, sorted by name. They are
    /// left out of `remotes` and reported as alarms; they never fail the
    /// volume (design §11.1).
    pub invalid_remotes: Vec<InvalidRemote>,
    /// `agentfs.patchRemote`: where the patch queue is fetched from and
    /// its refs pushed to (design §12).
    pub patch_remote: PatchRemote,
    /// `agentfs.patchFetchTimeout`: how long the mount waits for the
    /// patch fetch (design §12). Default 30s.
    pub patch_fetch_timeout: Duration,
    /// Every `include.path`, in order: absolute, readable config fragments
    /// whose keys count as the repository's own (design §4.3).
    pub includes: Vec<PathBuf>,
    /// This volume's fork identity; `None` when it is not a fork (design
    /// §11.5).
    pub fork: Option<ForkIdentity>,
    /// Where forks of this volume publish, and where a fork publishes: the
    /// one valid section with `agentfsForkRemote = true`, else for a
    /// local-only volume (or a fork recording its local-only source's
    /// state area, [`Settings::source_state`]) the source's own history
    /// ([`ForkRemote::SourceHistory`]), else the remote the patch remote
    /// resolves to (design §19.1, gate answer Q1 b), else `None`. `None`
    /// too when a flagged section is invalid or more than one is flagged:
    /// a fork never falls back past the operator's choice. Always `None`
    /// in an unlinked fork, which delivers nothing ([`ForkLink`]); its
    /// `remotes` are still the remote a linked fork would publish to.
    pub fork_remote: Option<ForkRemote>,
    /// A fork of a local-only source: the source's state area
    /// (`agentfs.fork.sourceState`, written at create from the fork's own
    /// config only, like the fork identity), into whose history the fork
    /// publishes ([`crate::fork::receive`]). `None` for any other volume.
    pub source_state: Option<PathBuf>,
    /// `agentfs.forkRefs`: the push target new forks of this volume get
    /// (design §11.5), fixed in each fork's identity at create, so a
    /// change here never moves a fork that exists.
    pub fork_refs: PushTarget,
    /// This volume's start point (agentfs.start.*), read only from the volume's own config, like
    /// the fork identity; `None` when it was not started from another branch.
    pub start: Option<crate::fork::identity::StartPoint>,
}

/// How a volume declares its push remotes (F8 S3, R4: zero remotes is a
/// declared mode); [`Settings::load`] is the one decider. Push sections and
/// the flagged fork-remote section are disjoint inputs, so the two cannot
/// disagree. A fork's mode follows its fork remote: `Pushed` with that one
/// remote, `LocalOnly` when it publishes into its source's history,
/// `NoneValid` when a flagged section is invalid, else `Undeclared`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Remotes {
    /// One or more valid push sections, sorted by name; invalid sections
    /// beside them alarm (§11.1).
    Pushed(crate::non_empty::NonEmpty<RemoteSettings>),
    /// Push sections, all invalid: each alarms; commits wait. Never
    /// reported local-only.
    NoneValid,
    /// `agentfs.remotes = none` and no push section: status, telemetry
    /// and alarms say local-only. A remote added later (and `none`
    /// removed) gets all history: the branch, and the refs of the forks
    /// received meanwhile (`fork::receive::received_refs`).
    LocalOnly,
    /// Neither, or `none` beside a push section (`beside` names it):
    /// pushes nothing and raises `RemotesUndeclared`; `agentfs init` and
    /// CreateVolume refuse it; an existing volume keeps opening
    /// (principle 1).
    Undeclared { beside: Option<String> },
}

impl Remotes {
    /// The valid push remotes: empty unless [`Remotes::Pushed`]. For the
    /// readers that only iterate; anything that reports or decides on the
    /// mode matches on the enum.
    pub fn pushed(&self) -> &[RemoteSettings] {
        match self {
            Self::Pushed(remotes) => remotes,
            Self::NoneValid | Self::LocalOnly | Self::Undeclared { .. } => &[],
        }
    }

    /// [`Remotes::pushed`], to change in a test.
    #[cfg(test)]
    pub(crate) fn pushed_mut(&mut self) -> &mut [RemoteSettings] {
        match self {
            Self::Pushed(remotes) => remotes.as_mut_slice(),
            Self::NoneValid | Self::LocalOnly | Self::Undeclared { .. } => &mut [],
        }
    }

    /// The mode's name in status, telemetry (`agentfs.remotes.mode`) and
    /// logs.
    pub fn mode(&self) -> &'static str {
        match self {
            Self::Pushed(_) => "pushed",
            Self::NoneValid => "none_valid",
            Self::LocalOnly => "local_only",
            Self::Undeclared { .. } => "undeclared",
        }
    }
}

/// `agentfs.remotes`: its only value.
pub const REMOTES_NONE: &str = "none";

/// Declares the volume at `git_dir` local-only: `agentfs.remotes = none`
/// in its own config (`agentfs init --local-only`, F8 S3).
pub fn declare_local_only(git_dir: &Path) -> Result<(), ConfigError> {
    let mut config = git2::Config::open(&git_dir.join("config"))?;
    config.set_str("agentfs.remotes", REMOTES_NONE)?;
    Ok(())
}

/// The error of each push section beside `agentfs.remotes = none`.
const BESIDE_NONE: &str = "agentfs.remotes = none is set beside this push section: remove one";

/// How a volume that is not a fork declares its remotes: `declared_none`
/// is `agentfs.remotes = none`; `own` its valid push sections and
/// `invalid` every invalid section (flagged ones are not push sections).
/// Valid sections beside `none` are never pushed: they join `invalid`, so
/// they alarm and the mount preflight still checks their files.
fn declared(
    declared_none: bool,
    own: Vec<RemoteSettings>,
    invalid: &mut Vec<InvalidRemote>,
) -> Remotes {
    let any_invalid = invalid.iter().any(|r| !r.fork_remote);
    if !declared_none {
        return match crate::non_empty::NonEmpty::new(own) {
            Some(own) => Remotes::Pushed(own),
            None if any_invalid => Remotes::NoneValid,
            None => Remotes::Undeclared { beside: None },
        };
    }
    if own.is_empty() && !any_invalid {
        return Remotes::LocalOnly;
    }
    invalid.extend(own.into_iter().map(|r| {
        InvalidRemote {
            files: r
                .files()
                .into_iter()
                .map(|(k, p)| (k, p.to_path_buf()))
                .collect(),
            name: r.name,
            error: BESIDE_NONE.into(),
            fork_remote: false,
        }
    }));
    invalid.sort_by(|a, b| a.name.cmp(&b.name));
    Remotes::Undeclared {
        beside: invalid
            .iter()
            .find(|r| !r.fork_remote)
            .map(|r| r.name.clone()),
    }
}

/// Where a volume's forks publish (design §11.5; F8 S3). `None` in
/// `Settings`: forks are refused, as today.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ForkRemote {
    /// The one valid section with `agentfsForkRemote = true`, else the
    /// remote the patch remote resolves to; any source mode. Boxed: the
    /// other variant is empty (`clippy::large_enum_variant`).
    Remote(Box<RemoteSettings>),
    /// Only for a [`Remotes::LocalOnly`] source with no flagged section:
    /// the fork publishes `agentfs/<fork>` into the source's own history
    /// ([`crate::fork::receive::receive_fork_change`]), never applied.
    SourceHistory,
}

/// `agentfs.patchRemote` (design §12).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PatchRemote {
    /// Unset: the only remote, if exactly one is configured and valid.
    Auto,
    /// `none`: no patches are fetched and no queue refs pushed.
    Disabled,
    /// A remote by name. One that is not configured (or not valid)
    /// disables fetching and raises an alarm at mount; it never fails the
    /// volume.
    Named(String),
}

impl Settings {
    /// The remote the patch queue uses, if any (design §12). A section
    /// with `agentfsForkRemote` is never one: it is not in `remotes`, and
    /// an invalid one does not count against the automatic choice.
    pub fn patch_remote(&self) -> Option<&RemoteSettings> {
        resolve_patch_remote(
            &self.patch_remote,
            self.remotes.pushed(),
            &self.invalid_remotes,
        )
    }

    /// The fork remote when it is a remote section, not the source's
    /// history.
    pub fn fork_remote_section(&self) -> Option<&RemoteSettings> {
        match &self.fork_remote {
            Some(ForkRemote::Remote(remote)) => Some(remote),
            Some(ForkRemote::SourceHistory) | None => None,
        }
    }

    /// Whether this volume is a fork (design §11.5).
    pub fn is_fork(&self) -> bool {
        self.fork.is_some()
    }
}

/// `setting` against the valid `remotes` and the `invalid` sections;
/// sections with `agentfsForkRemote` are never candidates.
fn resolve_patch_remote<'r>(
    setting: &PatchRemote,
    remotes: &'r [RemoteSettings],
    invalid: &[InvalidRemote],
) -> Option<&'r RemoteSettings> {
    let mut own = remotes.iter().filter(|r| !r.fork_remote);
    match setting {
        PatchRemote::Disabled => None,
        PatchRemote::Auto => match (own.next(), own.next()) {
            (Some(only), None) if invalid.iter().all(|r| r.fork_remote) => Some(only),
            _ => None,
        },
        PatchRemote::Named(name) => own.find(|r| r.name == *name),
    }
}

/// The error of each flagged section when more than one is flagged.
const MANY_FORK_REMOTES: &str = "more than one remote sets agentfsForkRemote";

/// The fork identity in `config` (design §11.5): both keys or neither.
fn fork_identity_of(
    config: &git2::Config,
    volume: &str,
) -> Result<Option<ForkIdentity>, ConfigError> {
    let source = string(config, fork_identity::SOURCE_KEY)?;
    let base = string(config, fork_identity::BASE_KEY)?;
    let push_target = push_target(config, fork_identity::PUSH_TARGET_KEY)?;
    let link = fork_link(config)?;
    let (source, base, push_target, link) = match (source, base, push_target, link) {
        (None, None, None, None) => return Ok(None),
        (Some(source), Some(base), push_target, link) => (
            source,
            base,
            push_target.unwrap_or_default(),
            link.unwrap_or_default(),
        ),
        _ => return Err(ConfigError::PartialForkIdentity),
    };
    validate_name(fork_identity::SOURCE_KEY, &source)?;
    let bad_base = || ConfigError::BadForkBase {
        key: fork_identity::BASE_KEY.into(),
        value: base.clone(),
    };
    if !is_full_oid(&base) {
        return Err(bad_base());
    }
    let base_oid = git2::Oid::from_str(&base).map_err(|_| bad_base())?;
    Ok(Some(ForkIdentity {
        fork: volume.to_owned(),
        source,
        base: base_oid,
        push_target,
        link,
    }))
}

/// `agentfs.fork.link` (design §11.5): `linked` or `unlinked`; unset is
/// `None`.
fn fork_link(config: &git2::Config) -> Result<Option<ForkLink>, ConfigError> {
    let key = fork_identity::LINK_KEY;
    match string(config, key)? {
        None => Ok(None),
        Some(value) => ForkLink::parse(&value)
            .map(Some)
            .ok_or_else(|| ConfigError::BadChoice {
                key: key.into(),
                value,
                allowed: "linked, unlinked",
            }),
    }
}

/// `agentfs.fork.sourceState` (F8 S3): an absolute path, set only beside
/// a linked fork's identity.
fn source_state_of(
    config: &git2::Config,
    fork: Option<&ForkIdentity>,
) -> Result<Option<PathBuf>, ConfigError> {
    let key = crate::fork::receive::SOURCE_STATE_KEY;
    match string(config, key)? {
        None => Ok(None),
        Some(_) if fork.is_none() => Err(ConfigError::PartialForkIdentity),
        Some(_) if fork.is_some_and(|f| f.link == ForkLink::Unlinked) => {
            Err(ConfigError::UnlinkedSourceState)
        }
        Some(value) if !Path::new(&value).is_absolute() => Err(ConfigError::RelativePath {
            key: key.into(),
            value,
        }),
        Some(value) => Ok(Some(PathBuf::from(value))),
    }
}

/// The start point in `config` (F8 S1): the three `agentfs.start.*` keys
/// or none, read as [`StartPoint::parse`] reads a StorageClass's
/// `startFrom`, so both spellings have one validator.
///
/// [`StartPoint::parse`]: crate::fork::identity::StartPoint::parse
fn start_point_of(
    config: &git2::Config,
) -> Result<Option<crate::fork::identity::StartPoint>, ConfigError> {
    let remote = string(config, fork_identity::START_REMOTE_KEY)?;
    let branch = string(config, fork_identity::START_BRANCH_KEY)?;
    let commit = string(config, fork_identity::START_COMMIT_KEY)?;
    match (remote, branch, commit) {
        (None, None, None) => Ok(None),
        (Some(remote), Some(branch), Some(commit)) => {
            crate::fork::identity::StartPoint::parse(&format!("{remote}:{branch}@{commit}"))
                .map(Some)
                .map_err(ConfigError::BadStartPoint)
        }
        _ => Err(ConfigError::PartialStartPoint),
    }
}

/// A push target key (`agentfs.forkRefs`, `agentfs.fork.pushTarget`):
/// `branch` or `hidden`; unset is `None`.
fn push_target(config: &git2::Config, key: &str) -> Result<Option<PushTarget>, ConfigError> {
    match string(config, key)?.as_deref() {
        None => Ok(None),
        Some("branch") => Ok(Some(PushTarget::Branch)),
        Some("hidden") => Ok(Some(PushTarget::Hidden)),
        Some(other) => Err(ConfigError::BadChoice {
            key: key.into(),
            value: other.into(),
            allowed: "branch, hidden",
        }),
    }
}

/// 40 lowercase hex digits: a full commit id as agentfs writes one.
pub fn is_full_oid(value: &str) -> bool {
    value.len() == 40
        && value
            .bytes()
            .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// The volume's own remotes and the fork remote, from every valid and
/// invalid section (design §11.5): see [`Settings::fork_remote`].
struct SplitRemotes {
    remotes: Remotes,
    invalid: Vec<InvalidRemote>,
    fork_remote: Option<ForkRemote>,
}

/// What [`split_remotes`] weighs beside the sections.
struct Declaration<'a> {
    patch_remote: &'a PatchRemote,
    /// `agentfs.remotes = none`.
    none: bool,
    /// The volume is a fork.
    fork: bool,
    /// The fork is unlinked ([`ForkLink::Unlinked`]).
    unlinked: bool,
    /// The fork records its local-only source's state area.
    source_history: bool,
}

fn split_remotes(
    valid: Vec<RemoteSettings>,
    mut invalid: Vec<InvalidRemote>,
    declaration: &Declaration<'_>,
) -> SplitRemotes {
    let (mut flagged, own): (Vec<_>, Vec<_>) = valid.into_iter().partition(|r| r.fork_remote);
    if flagged.len() > 1 {
        for r in flagged.drain(..) {
            invalid.push(InvalidRemote {
                files: r
                    .files()
                    .into_iter()
                    .map(|(k, p)| (k, p.to_path_buf()))
                    .collect(),
                name: r.name,
                error: MANY_FORK_REMOTES.into(),
                fork_remote: true,
            });
        }
        invalid.sort_by(|a, b| a.name.cmp(&b.name));
    }
    let any_flagged_invalid = invalid.iter().any(|r| r.fork_remote);
    let flagged = match flagged.pop() {
        Some(one) if !any_flagged_invalid => Ok(Some(one)),
        Some(_) => Err(()),
        None if any_flagged_invalid => Err(()),
        None => Ok(None),
    };
    let remote = |r: RemoteSettings| ForkRemote::Remote(Box::new(r));
    if declaration.fork {
        let fork_remote = match flagged {
            Ok(Some(one)) => Some(remote(one)),
            Err(()) => None,
            Ok(None) if declaration.source_history => Some(ForkRemote::SourceHistory),
            Ok(None) => resolve_patch_remote(declaration.patch_remote, &own, &invalid)
                .cloned()
                .map(remote),
        };
        let remotes = match &fork_remote {
            Some(ForkRemote::Remote(one)) => {
                Remotes::Pushed(crate::non_empty::NonEmpty::one((**one).clone()))
            }
            Some(ForkRemote::SourceHistory) => Remotes::LocalOnly,
            None if any_flagged_invalid => Remotes::NoneValid,
            // An unlinked fork of a local-only source records no path to
            // it (design §11.5): with no section at all it is local-only.
            None if declaration.unlinked && own.is_empty() && invalid.is_empty() => {
                Remotes::LocalOnly
            }
            None => Remotes::Undeclared { beside: None },
        };
        return SplitRemotes {
            remotes,
            // An unflagged section is not a fork's remote: nothing to raise.
            invalid: invalid.into_iter().filter(|r| r.fork_remote).collect(),
            // An unlinked fork pushes its history where a linked one would
            // publish, and delivers nothing: it has no fork remote.
            fork_remote: fork_remote.filter(|_| !declaration.unlinked),
        };
    }
    let remotes = declared(declaration.none, own, &mut invalid);
    let fork_remote = match flagged {
        Ok(Some(one)) => Some(remote(one)),
        Err(()) => None,
        Ok(None) if remotes == Remotes::LocalOnly => Some(ForkRemote::SourceHistory),
        Ok(None) => resolve_patch_remote(declaration.patch_remote, remotes.pushed(), &invalid)
            .cloned()
            .map(remote),
    };
    SplitRemotes {
        remotes,
        invalid,
        fork_remote,
    }
}

/// Whether `<git_dir>/config` names a fork identity (any of
/// `agentfs.fork.source`, `.base`, `.pushTarget`), read without the rest
/// of the settings: whether a volume is a fork does not depend on
/// settings that may fail to load (an include removed or holding a bad
/// value). No include may set these keys
/// ([`ConfigError::IncludeSetsIdentity`]); libgit2 still reads includes
/// here, skipping a missing one, so one that sets them makes the volume
/// look like a fork whose settings then fail to load, and one that does
/// not parse is an error.
pub fn declares_fork(git_dir: &Path) -> Result<bool, git2::Error> {
    let config = git2::Config::open(&git_dir.join("config"))?;
    for key in [
        fork_identity::SOURCE_KEY,
        fork_identity::BASE_KEY,
        fork_identity::PUSH_TARGET_KEY,
        fork_identity::LINK_KEY,
        crate::fork::receive::SOURCE_STATE_KEY,
    ] {
        if is_set(&config, key)? {
            return Ok(true);
        }
    }
    Ok(false)
}

impl Settings {
    /// Reads settings from `<git_dir>/config` only.
    pub fn load(git_dir: &Path) -> Result<Self, ConfigError> {
        let config = git2::Config::open(&git_dir.join("config"))?;
        Self::from_config(&config)
    }

    pub fn from_config(config: &git2::Config) -> Result<Self, ConfigError> {
        let includes = includes(config)?;
        let volume = string(config, "agentfs.volume")?.ok_or(ConfigError::MissingVolume)?;
        validate_name("agentfs.volume", &volume)?;
        let own_branch = string(config, "agentfs.branch")?;
        let fork = fork_identity_of(config, &volume)?;
        let start = start_point_of(config)?;
        if fork.is_some() && own_branch.is_some() {
            return Err(ConfigError::ForkBranch);
        }
        let branch_ref = match &fork {
            Some(identity) => identity.history_ref(),
            None => format!(
                "refs/heads/{}",
                own_branch.unwrap_or_else(|| format!("agentfs/{volume}"))
            ),
        };
        let defaults = Templates::default();
        let (concurrent_transfers, concurrent_transfers_error) = concurrent_transfers(config)?;
        let lfs = LfsSettings {
            threshold: match int(config, "agentfs.lfsThreshold")? {
                None => None,
                Some(n) if n > 0 => Some(n as u64),
                Some(n) => {
                    return Err(ConfigError::BadSize {
                        key: "agentfs.lfsThreshold".into(),
                        value: n.to_string(),
                    });
                }
            },
            url: string(config, "lfs.url")?,
            push_url: string(config, "lfs.pushurl")?,
            concurrent_transfers,
            concurrent_transfers_error,
        };
        let http_proxy = match string(config, "http.proxy")? {
            None => Ok(None),
            Some(value) => crate::proxy::parse(&value).map_err(|why| {
                ConfigError::BadProxy {
                    key: "http.proxy".into(),
                    value: crate::proxy::redact(&value),
                    why,
                }
                .to_string()
            }),
        };
        let (valid, invalid) = remotes(config, &lfs, &http_proxy)?;
        let patch_remote = match string(config, "agentfs.patchRemote")?.as_deref() {
            None | Some("") => PatchRemote::Auto,
            Some("none") => PatchRemote::Disabled,
            Some(name) => {
                validate_name("agentfs.patchRemote", name)?;
                PatchRemote::Named(name.to_owned())
            }
        };
        let none = match string(config, "agentfs.remotes")?.as_deref() {
            None => false,
            Some(REMOTES_NONE) => true,
            Some(other) => {
                return Err(ConfigError::BadChoice {
                    key: "agentfs.remotes".into(),
                    value: other.into(),
                    allowed: REMOTES_NONE,
                });
            }
        };
        let source_state = source_state_of(config, fork.as_ref())?;
        let split = split_remotes(
            valid,
            invalid,
            &Declaration {
                patch_remote: &patch_remote,
                none,
                fork: fork.is_some(),
                unlinked: fork.as_ref().is_some_and(|f| f.link == ForkLink::Unlinked),
                source_history: source_state.is_some(),
            },
        );
        let fork_refs = push_target(config, "agentfs.forkRefs")?.unwrap_or_default();
        Ok(Self {
            branch_ref,
            respect_tree_rules: boolean(config, "agentfs.respectTreeRules")?.unwrap_or(false),
            nested: match string(config, "agentfs.nested")?.as_deref() {
                None | Some("exclude") => Nested::Exclude,
                Some("working-tree") => Nested::WorkingTree,
                Some(other) => {
                    return Err(ConfigError::BadChoice {
                        key: "agentfs.nested".into(),
                        value: other.into(),
                        allowed: "exclude, working-tree",
                    });
                }
            },
            commit_window: duration(config, "agentfs.commitWindow")?
                .unwrap_or(Duration::from_millis(200)),
            sweep_interval: sweep_interval(config)?,
            repack_interval: repack_interval(config)?,
            push_interval: duration(config, "agentfs.pushInterval")?
                .unwrap_or(Duration::from_secs(60)),
            identity: Templates {
                committer_name: string(config, "agentfs.identity.committerName")?
                    .unwrap_or(defaults.committer_name),
                committer_email: string(config, "agentfs.identity.committerEmail")?
                    .unwrap_or(defaults.committer_email),
                author_name: string(config, "agentfs.identity.authorName")?
                    .unwrap_or(defaults.author_name),
                author_email: string(config, "agentfs.identity.authorEmail")?
                    .unwrap_or(defaults.author_email),
            },
            // A fork has no patch queue (design §11.5).
            patch_remote: if fork.is_some() {
                PatchRemote::Disabled
            } else {
                patch_remote
            },
            patch_fetch_timeout: patch_fetch_timeout(config)?,
            lfs,
            http_proxy: http_proxy.ok().flatten(),
            remotes: split.remotes,
            invalid_remotes: split.invalid,
            fork_remote: split.fork_remote,
            source_state,
            fork_refs,
            fork,
            start,
            includes,
            volume,
        })
    }
}

/// Every `include.path`, in order, each absolute and a readable regular
/// file. Any `includeIf.<condition>.path` is refused.
fn includes(config: &git2::Config) -> Result<Vec<PathBuf>, ConfigError> {
    note("includeIf.<condition>.path");
    {
        let mut entries = config.entries(Some(r"^includeif\..*\.path$"))?;
        if let Some(entry) = entries.next() {
            let entry = entry?;
            return Err(ConfigError::ConditionalInclude {
                key: entry
                    .name()
                    .unwrap_or("includeIf.<condition>.path")
                    .to_owned(),
            });
        }
    }
    let mut paths = Vec::new();
    note("include.path");
    let mut entries = match config.multivar("include.path", None) {
        Ok(entries) => entries,
        Err(e) if e.code() == git2::ErrorCode::NotFound => return Ok(paths),
        Err(e) => return Err(e.into()),
    };
    while let Some(entry) = entries.next() {
        let entry = entry?;
        let value = String::from_utf8_lossy(entry.value_bytes()).into_owned();
        let path = PathBuf::from(&value);
        if !path.is_absolute() {
            return Err(ConfigError::IncludeNotAbsolute { value });
        }
        let readable = std::fs::File::open(&path).and_then(|file| {
            if file.metadata()?.is_file() {
                Ok(())
            } else {
                Err(std::io::Error::other("not a regular file"))
            }
        });
        if let Err(source) = readable {
            return Err(ConfigError::IncludeUnreadable { path, source });
        }
        // The volume's identity belongs to its state area (`agentfs
        // init`): a shared fragment naming one would silently turn every
        // volume using it into the same volume.
        // A fork's identity likewise (design §11.5), and a started
        // volume's start point (F8 S1).
        let fragment = git2::Config::open(&path)?;
        for key in [
            "agentfs.volume",
            "agentfs.branch",
            fork_identity::SOURCE_KEY,
            fork_identity::BASE_KEY,
            fork_identity::PUSH_TARGET_KEY,
            fork_identity::LINK_KEY,
            crate::fork::receive::SOURCE_STATE_KEY,
            fork_identity::START_REMOTE_KEY,
            fork_identity::START_BRANCH_KEY,
            fork_identity::START_COMMIT_KEY,
        ] {
            if is_set(&fragment, key)? {
                return Err(ConfigError::IncludeSetsIdentity { path, key });
            }
        }
        paths.push(path);
    }
    Ok(paths)
}

/// Every `[remote "<name>"]` with a `url`, sorted by name: the valid ones,
/// and the invalid ones with their errors. A remote only affects pushing,
/// so a mistake in one never fails the volume (design §11.1).
fn remotes(
    config: &git2::Config,
    lfs: &LfsSettings,
    http_proxy: &Result<Option<String>, String>,
) -> Result<(Vec<RemoteSettings>, Vec<InvalidRemote>), ConfigError> {
    let mut names = BTreeSet::new();
    note("remote.<name>.url");
    {
        let mut entries = config.entries(Some(r"^remote\..*\.url$"))?;
        while let Some(entry) = entries.next() {
            let entry = entry?;
            let Ok(full) = entry.name() else { continue };
            if let Some(name) = full
                .strip_prefix("remote.")
                .and_then(|rest| rest.strip_suffix(".url"))
            {
                names.insert(name.to_owned());
            }
        }
    }
    let mut valid = Vec::new();
    let mut invalid = Vec::new();
    for name in names {
        let checked = remote(config, &name, http_proxy)
            .and_then(|r| validate_lfs(&r, lfs).map(|()| r))
            .and_then(|r| validate_fork_lfs(&r, lfs).map(|()| r));
        match checked {
            Ok(remote) => valid.push(remote),
            Err(e) => invalid.push(InvalidRemote {
                files: remote_files(config, &name),
                // Fails closed: a flag that cannot be read is not "false".
                fork_remote: !matches!(
                    boolean(config, &format!("remote.{name}.agentfsForkRemote")),
                    Ok(None | Some(false))
                ),
                name,
                error: e.to_string(),
            }),
        }
    }
    Ok((valid, invalid))
}

/// The absolute paths an invalid section names, whatever else is wrong
/// with it.
fn remote_files(config: &git2::Config, name: &str) -> Vec<(&'static str, PathBuf)> {
    REMOTE_FILE_KEYS
        .into_iter()
        .filter_map(|key| {
            let value = string(config, &format!("remote.{name}.{key}")).ok()??;
            Path::new(&value)
                .is_absolute()
                .then(|| (key, PathBuf::from(value)))
        })
        .collect()
}

/// One `[remote "<name>"]` section.
fn remote(
    config: &git2::Config,
    name: &str,
    http_proxy: &Result<Option<String>, String>,
) -> Result<RemoteSettings, ConfigError> {
    validate_name(&format!("remote.{name}"), name)?;
    let key = |k: &str| format!("remote.{name}.{k}");
    let Some(url) = string(config, &key("url"))? else {
        return Err(git2::Error::from_str("remote url vanished while reading").into());
    };
    let max_blob_size = positive_size(config, &key("agentfsMaxBlobSize"))?;
    let path = |k: &str| match string(config, &key(k))? {
        None => Ok(None),
        Some(path) if Path::new(&path).is_absolute() => Ok(Some(PathBuf::from(path))),
        Some(path) => Err(ConfigError::RelativePath {
            key: key(k),
            value: path,
        }),
    };
    let remote = RemoteSettings {
        push_url: string(config, &key("pushurl"))?,
        url,
        max_blob_size,
        max_push_size: positive_size(config, &key("agentfsMaxPushSize"))?,
        max_lfs_object_size: positive_size(config, &key("agentfsMaxLfsObjectSize"))?,
        credentials: path("agentfsCredentials")?,
        ssh_key: path("agentfsSshKey")?,
        known_hosts: path("agentfsKnownHosts")?,
        ssh_agent: path("agentfsSshAgent")?,
        ssh_key_passphrase: path("agentfsSshKeyPassphrase")?,
        lfs_url: string(config, &key("lfsurl"))?,
        lfs_push_url: string(config, &key("lfspushurl"))?,
        lfs_credentials: path("agentfsLfsCredentials")?,
        proxy: remote_proxy(config, name, http_proxy)?,
        fork_remote: boolean(config, &key("agentfsForkRemote"))?.unwrap_or(false),
        name: name.to_owned(),
    };
    validate_transport(&remote)?;
    Ok(remote)
}

/// A remote's effective proxy (design §11.1): `remote.<name>.proxy` (empty:
/// none), else `http.proxy`. A bad `http.proxy` misconfigures every
/// remote.
fn remote_proxy(
    config: &git2::Config,
    name: &str,
    http_proxy: &Result<Option<String>, String>,
) -> Result<Option<ProxySetting>, ConfigError> {
    let global = match http_proxy {
        Ok(global) => global,
        Err(error) => return Err(ConfigError::Global(error.clone())),
    };
    let key = format!("remote.{name}.proxy");
    match string(config, &key)? {
        Some(value) => match crate::proxy::parse(&value) {
            Ok(url) => Ok(url.map(|url| ProxySetting { url, key })),
            Err(why) => Err(ConfigError::BadProxy {
                key,
                // Never echo a password the value may carry.
                value: crate::proxy::redact(&value),
                why,
            }),
        },
        None => Ok(global.clone().map(|url| ProxySetting {
            url,
            key: "http.proxy".into(),
        })),
    }
}

/// Checks the agentfs keys against the push URL's transport (design
/// §11.1): ssh remotes need a known_hosts file and either a key (with an
/// optional passphrase file) or an ssh-agent socket, and take no password
/// or token; other remotes take none of the ssh keys.
fn validate_transport(remote: &RemoteSettings) -> Result<(), ConfigError> {
    let key = |k: &str| format!("remote.{}.{k}", remote.name);
    let url_key = key(if remote.push_url.is_some() {
        "pushurl"
    } else {
        "url"
    });
    if remote.transport() != Transport::Ssh {
        for (k, set) in [
            ("agentfsSshKey", remote.ssh_key.is_some()),
            ("agentfsKnownHosts", remote.known_hosts.is_some()),
            ("agentfsSshAgent", remote.ssh_agent.is_some()),
            (
                "agentfsSshKeyPassphrase",
                remote.ssh_key_passphrase.is_some(),
            ),
        ] {
            if set {
                return Err(ConfigError::SshOnly { key: key(k) });
            }
        }
        return Ok(());
    }
    if remote_url::has_password(remote.push_url()) {
        return Err(ConfigError::SshPassword { key: url_key });
    }
    if remote_url::ssh_endpoint(remote.push_url()).is_none() {
        return Err(ConfigError::SshUrl {
            key: url_key,
            // No password: has_password was checked first.
            value: remote.push_url().to_owned(),
        });
    }
    if remote.credentials.is_some() {
        return Err(ConfigError::SshCredentials {
            key: key("agentfsCredentials"),
        });
    }
    let section = format!("remote.{}", remote.name);
    match (remote.ssh_key.is_some(), remote.ssh_agent.is_some()) {
        (false, false) => return Err(ConfigError::SshNoIdentity { remote: section }),
        (true, true) => return Err(ConfigError::SshKeyAndAgent { remote: section }),
        _ => {}
    }
    if remote.ssh_key_passphrase.is_some() && remote.ssh_key.is_none() {
        return Err(ConfigError::SshPassphraseWithoutKey {
            key: key("agentfsSshKeyPassphrase"),
            remote: section,
        });
    }
    if remote.known_hosts.is_none() {
        return Err(ConfigError::SshNeeds {
            key: key("agentfsKnownHosts"),
        });
    }
    if !cfg!(feature = "ssh") {
        return Err(ConfigError::SshUnsupported { key: url_key });
    }
    Ok(())
}

/// Checks every LFS endpoint that could apply to `remote` (design §7): the
/// global `lfs.url` and `lfs.pushurl` and the remote's own `lfsurl` and
/// `lfspushurl`. Each must be an `http(s)://` URL, a `file://` URL or a
/// path; `https` needs the `https` cargo feature. A bad global key
/// (including `lfs.concurrenttransfers`) makes every remote misconfigured,
/// reported per remote, so it never fails the volume.
fn validate_lfs(remote: &RemoteSettings, lfs: &LfsSettings) -> Result<(), ConfigError> {
    if let Some(error) = &lfs.concurrent_transfers_error {
        return Err(ConfigError::Global(error.clone()));
    }
    let remote_key = |k: &str| format!("remote.{}.{k}", remote.name);
    let keys = [
        ("lfs.url".to_owned(), &lfs.url),
        ("lfs.pushurl".to_owned(), &lfs.push_url),
        (remote_key("lfsurl"), &remote.lfs_url),
        (remote_key("lfspushurl"), &remote.lfs_push_url),
    ];
    for (key, value) in keys {
        let Some(value) = value else { continue };
        match remote_url::transport(value) {
            Transport::Http | Transport::Local if !value.trim().is_empty() => {}
            Transport::Https if cfg!(feature = "https") => {}
            Transport::Https => return Err(ConfigError::LfsHttpsUnsupported { key }),
            _ => {
                return Err(ConfigError::LfsUrl {
                    key,
                    // Never echo a password a URL may carry.
                    value: crate::push::redact_url(value),
                });
            }
        }
    }
    Ok(())
}

/// A flagged fork remote (design §11.5) needs its own LFS endpoint while
/// `lfs.url` or `lfs.pushurl` is set: those name the source's store, and a
/// fork's LFS objects must not reach it. The section is invalid otherwise
/// (fails closed: forks are refused, the source pushes as before). The
/// default fork remote, the source's own, is unaffected: its store is the
/// source's anyway.
fn validate_fork_lfs(remote: &RemoteSettings, lfs: &LfsSettings) -> Result<(), ConfigError> {
    let global = lfs.url.is_some() || lfs.push_url.is_some();
    let own = remote.lfs_url.is_some() || remote.lfs_push_url.is_some();
    if remote.fork_remote && global && !own {
        return Err(ConfigError::ForkRemoteGlobalLfs {
            remote: remote.name.clone(),
        });
    }
    Ok(())
}

/// Checks a volume or remote name (design §11.4). Names become ref path
/// components (`refs/agentfs/<volume>/…`) and state file names, so they are
/// limited to `[A-Za-z0-9][A-Za-z0-9._-]*`, without `..` or a `.lock`
/// suffix. A `/` would let one volume's namespace contain another's.
pub fn validate_name(key: &str, value: &str) -> Result<(), ConfigError> {
    let bytes = value.as_bytes();
    let ok = bytes.first().is_some_and(u8::is_ascii_alphanumeric)
        && bytes
            .iter()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
        && !value.contains("..")
        && !value.ends_with(".lock")
        && !value.ends_with('.');
    if ok {
        Ok(())
    } else {
        Err(ConfigError::BadName {
            key: key.into(),
            value: value.into(),
        })
    }
}

/// A string key, `None` when unset. Every string read of agentfs's config
/// goes through here, so it is checked against [`KEYS`].
pub(crate) fn string(config: &git2::Config, key: &str) -> Result<Option<String>, git2::Error> {
    note(key);
    match config.get_string(key) {
        Ok(v) => Ok(Some(v)),
        Err(e) if e.code() == git2::ErrorCode::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

/// Whether `key` is set in `config`, read without noting it: a check that
/// a fragment leaves a key alone, not a read of the setting.
fn is_set(config: &git2::Config, key: &str) -> Result<bool, git2::Error> {
    match config.get_entry(key) {
        Ok(_) => Ok(true),
        Err(e) if e.code() == git2::ErrorCode::NotFound => Ok(false),
        Err(e) => Err(e),
    }
}

fn boolean(config: &git2::Config, key: &str) -> Result<Option<bool>, git2::Error> {
    note(key);
    match config.get_bool(key) {
        Ok(v) => Ok(Some(v)),
        Err(e) if e.code() == git2::ErrorCode::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

/// A size with `k`, `m` or `g` suffixes that must be positive when set.
fn positive_size(config: &git2::Config, key: &str) -> Result<Option<u64>, ConfigError> {
    match int(config, key)? {
        None => Ok(None),
        Some(n) if n > 0 => Ok(Some(n as u64)),
        Some(n) => Err(ConfigError::BadSize {
            key: key.to_owned(),
            value: n.to_string(),
        }),
    }
}

/// An integer with git's `k`, `m` and `g` suffixes, as for `lfsThreshold`.
fn int(config: &git2::Config, key: &str) -> Result<Option<i64>, git2::Error> {
    note(key);
    match config.get_i64(key) {
        Ok(v) => Ok(Some(v)),
        Err(e) if e.code() == git2::ErrorCode::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

fn duration(config: &git2::Config, key: &str) -> Result<Option<Duration>, ConfigError> {
    let Some(value) = string(config, key)? else {
        return Ok(None);
    };
    parse_duration(&value)
        .map(Some)
        .ok_or(ConfigError::BadDuration {
            key: key.into(),
            value,
        })
}

/// The shortest `agentfs.sweepInterval` accepted.
pub const MIN_SWEEP_INTERVAL: Duration = Duration::from_secs(1);

/// `agentfs.sweepInterval`, default 30s. Shorter than
/// [`MIN_SWEEP_INTERVAL`] is an error: a sweep fstats every open, written
/// file, so a tiny interval would cost the daemon for no real gain.
fn sweep_interval(config: &git2::Config) -> Result<Duration, ConfigError> {
    let key = "agentfs.sweepInterval";
    match duration(config, key)? {
        None => Ok(Duration::from_secs(30)),
        Some(d) if d >= MIN_SWEEP_INTERVAL => Ok(d),
        Some(_) => Err(ConfigError::TooShort {
            key: key.into(),
            value: string(config, key)?.unwrap_or_default(),
            min: MIN_SWEEP_INTERVAL,
        }),
    }
}

/// The shortest `agentfs.repackInterval` accepted.
pub const MIN_REPACK_INTERVAL: Duration = Duration::from_secs(60);

/// The default `agentfs.repackInterval`.
pub const DEFAULT_REPACK_INTERVAL: Duration = Duration::from_secs(3600);

/// The default `agentfs.patchFetchTimeout` (design §12).
pub const DEFAULT_PATCH_FETCH_TIMEOUT: Duration = Duration::from_secs(30);

/// `agentfs.patchFetchTimeout`: default [`DEFAULT_PATCH_FETCH_TIMEOUT`],
/// `0` or `off` (zero: the fetch is skipped), else a duration.
fn patch_fetch_timeout(config: &git2::Config) -> Result<Duration, ConfigError> {
    let key = "agentfs.patchFetchTimeout";
    match string(config, key)?.as_deref() {
        Some("0" | "off") => Ok(Duration::ZERO),
        _ => Ok(duration(config, key)?.unwrap_or(DEFAULT_PATCH_FETCH_TIMEOUT)),
    }
}

/// `agentfs.repackInterval`: default [`DEFAULT_REPACK_INTERVAL`], `0` or
/// `off` for never, else a duration of at least [`MIN_REPACK_INTERVAL`].
fn repack_interval(config: &git2::Config) -> Result<Option<Duration>, ConfigError> {
    let key = "agentfs.repackInterval";
    match string(config, key)?.as_deref() {
        None => return Ok(Some(DEFAULT_REPACK_INTERVAL)),
        Some("0" | "off") => return Ok(None),
        Some(_) => {}
    }
    match duration(config, key)? {
        Some(d) if d >= MIN_REPACK_INTERVAL => Ok(Some(d)),
        _ => Err(ConfigError::TooShort {
            key: key.into(),
            value: string(config, key)?.unwrap_or_default(),
            min: MIN_REPACK_INTERVAL,
        }),
    }
}

/// Parses `<integer><unit>` with unit `ms`, `s`, `m` or `h`.
pub fn parse_duration(value: &str) -> Option<Duration> {
    let value = value.trim();
    let split = value.find(|c: char| !c.is_ascii_digit())?;
    let (number, unit) = value.split_at(split);
    let n: u64 = number.parse().ok()?;
    match unit {
        "ms" => Some(Duration::from_millis(n)),
        "s" => Some(Duration::from_secs(n)),
        "m" => Some(Duration::from_secs(n.checked_mul(60)?)),
        "h" => Some(Duration::from_secs(n.checked_mul(3600)?)),
        _ => None,
    }
}

/// Parses a positive byte count with an optional `k`, `m` or `g` suffix
/// (powers of 1024, either case), as git reads sizes: `512k`, `100m`, `2g`.
/// Zero is not a size.
pub fn parse_size(value: &str) -> Option<u64> {
    let value = value.trim();
    let (number, shift) = match value.as_bytes().last()? {
        b'k' | b'K' => (&value[..value.len() - 1], 10),
        b'm' | b'M' => (&value[..value.len() - 1], 20),
        b'g' | b'G' => (&value[..value.len() - 1], 30),
        _ => (value, 0),
    };
    if number.is_empty() || !number.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let n: u64 = number.parse().ok()?;
    let n = n.checked_mul(1u64 << shift)?;
    (n > 0).then_some(n)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config_with(lines: &str) -> (tempfile::TempDir, git2::Config) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config");
        std::fs::write(&path, lines).unwrap();
        let config = git2::Config::open(&path).unwrap();
        (dir, config)
    }

    /// [`KEYS`] is exactly what loading a config that sets every key
    /// reads: a key read but not registered (or registered but never
    /// read) fails here, and docs/configuration.md is checked against
    /// [`KEYS`] by `tests/docs_reference.rs`.
    #[test]
    fn every_config_read_is_registered() {
        let dir = tempfile::tempdir().unwrap();
        let fragment = dir.path().join("fragment");
        std::fs::write(&fragment, "[agentfs]\n\tpushInterval = 2m\n").unwrap();
        let path = dir.path().join("config");
        std::fs::write(
            &path,
            format!(
                "[include]\n\tpath = {}\n\
                 [agentfs]\n\tvolume = v\n\tbranch = b\n\trespectTreeRules = true\n\
                 \tnested = working-tree\n\tcommitWindow = 50ms\n\tpushInterval = 5m\n\
                 \tsweepInterval = 5s\n\trepackInterval = 2h\n\tpatchRemote = web\n\
                 \tpatchFetchTimeout = 10s\n\tlfsThreshold = 8m\n\
                 [agentfs \"identity\"]\n\tcommitterName = c\n\tcommitterEmail = c@e\n\
                 \tauthorName = a\n\tauthorEmail = a@e\n\
                 [lfs]\n\turl = http://lfs.example/r\n\tpushurl = http://lfs.example/p\n\
                 \tconcurrenttransfers = 4\n\
                 [http]\n\tproxy = http://proxy.example:3128\n\
                 [remote \"web\"]\n\turl = http://git.example/r.git\n\
                 \tpushurl = http://git.example/p.git\n\tagentfsMaxBlobSize = 100m\n\
                 \tagentfsMaxLfsObjectSize = 2g\n\
                 \tagentfsCredentials = /run/c\n\tlfsurl = http://lfs.example/web\n\
                 \tlfspushurl = http://lfs.example/web-push\n\tagentfsLfsCredentials = /run/l\n\
                 \tproxy = http://other.example:8080\n\
                 [remote \"ssh\"]\n\turl = ssh://git@git.example/r.git\n\
                 \tagentfsSshKey = /run/k\n\tagentfsKnownHosts = /run/h\n",
                fragment.display()
            ),
        )
        .unwrap();
        reads::take();
        let settings = Settings::load(dir.path()).unwrap();
        let read = reads::take();
        // Both remotes were read whole, whether or not this build supports
        // ssh: their keys are read before the transport is validated.
        assert_eq!(
            settings.remotes.pushed().len() + settings.invalid_remotes.len(),
            2,
            "{settings:?}"
        );
        let registered: BTreeSet<String> = KEYS.iter().map(|k| (*k).to_owned()).collect();
        let unregistered: Vec<_> = read.difference(&registered).collect();
        assert!(
            unregistered.is_empty(),
            "read but not in config::KEYS (add them, and document them in \
             docs/configuration.md): {unregistered:?}"
        );
        let unread: Vec<_> = registered.difference(&read).collect();
        assert!(
            unread.is_empty(),
            "in config::KEYS but never read by Settings::load: {unread:?}"
        );
        assert_eq!(KEYS.len(), registered.len(), "config::KEYS has duplicates");
    }

    #[test]
    fn read_keys_are_normalised() {
        assert_eq!(
            reads::normalise("remote.origin.agentfsSshKey"),
            "remote.<name>.agentfsSshKey"
        );
        assert_eq!(reads::normalise("remote.a.b.url"), "remote.<name>.url");
        assert_eq!(reads::normalise("agentfs.volume"), "agentfs.volume");
    }

    #[test]
    fn defaults() {
        let (_d, c) = config_with("[agentfs]\n\tvolume = hermes\n");
        let s = Settings::from_config(&c).unwrap();
        assert_eq!(s.branch_ref, "refs/heads/agentfs/hermes");
        assert!(!s.respect_tree_rules);
        assert_eq!(s.nested, Nested::Exclude);
        assert_eq!(s.commit_window, Duration::from_millis(200));
        assert_eq!(s.push_interval, Duration::from_secs(60));
        assert_eq!(s.sweep_interval, Duration::from_secs(30));
        assert_eq!(s.repack_interval, Some(Duration::from_secs(3600)));
        assert_eq!(s.identity, Templates::default());
    }

    #[test]
    fn overrides() {
        let (_d, c) = config_with(
            "[agentfs]\n\tvolume = v\n\tbranch = custom\n\trespectTreeRules = true\n\
             \tnested = working-tree\n\tcommitWindow = 50ms\n\tpushInterval = 5m\n\tsweepInterval = 5s\n\
             [agentfs \"identity\"]\n\tcommitterName = bot {replica}\n",
        );
        let s = Settings::from_config(&c).unwrap();
        assert_eq!(s.branch_ref, "refs/heads/custom");
        assert!(s.respect_tree_rules);
        assert_eq!(s.nested, Nested::WorkingTree);
        assert_eq!(s.commit_window, Duration::from_millis(50));
        assert_eq!(s.push_interval, Duration::from_secs(300));
        assert_eq!(s.sweep_interval, Duration::from_secs(5));
        assert_eq!(s.identity.committer_name, "bot {replica}");
    }

    #[test]
    fn errors() {
        let (_d, c) = config_with("[agentfs]\n\tbranch = x\n");
        assert!(matches!(
            Settings::from_config(&c),
            Err(ConfigError::MissingVolume)
        ));
        let (_d, c) = config_with("[agentfs]\n\tvolume = v\n\tpushInterval = soon\n");
        assert!(matches!(
            Settings::from_config(&c),
            Err(ConfigError::BadDuration { .. })
        ));
        let (_d, c) = config_with("[agentfs]\n\tvolume = v\n\tnested = maybe\n");
        assert!(matches!(
            Settings::from_config(&c),
            Err(ConfigError::BadChoice { .. })
        ));
        let (_d, c) = config_with("[agentfs]\n\tvolume = v\n\tsweepInterval = abc\n");
        assert!(matches!(
            Settings::from_config(&c),
            Err(ConfigError::BadDuration { .. })
        ));
        for bad in ["0s", "500ms"] {
            let (_d, c) = config_with(&format!(
                "[agentfs]\n\tvolume = v\n\tsweepInterval = {bad}\n"
            ));
            assert!(
                matches!(Settings::from_config(&c), Err(ConfigError::TooShort { .. })),
                "{bad}"
            );
        }
    }

    #[test]
    fn patch_remote_and_fetch_timeout() {
        let load = |extra: &str| {
            let (_d, c) = config_with(&format!(
                "[agentfs]\n\tvolume = v\n{extra}[remote \"origin\"]\n\turl = /tmp/r.git\n"
            ));
            Settings::from_config(&c)
        };
        let s = load("").unwrap();
        assert_eq!(s.patch_remote, PatchRemote::Auto);
        assert_eq!(s.patch_remote().unwrap().name, "origin");
        assert_eq!(s.patch_fetch_timeout, DEFAULT_PATCH_FETCH_TIMEOUT);
        let s = load("\tpatchRemote = none\n\tpatchFetchTimeout = 5s\n").unwrap();
        assert_eq!(s.patch_remote, PatchRemote::Disabled);
        assert!(s.patch_remote().is_none());
        assert_eq!(s.patch_fetch_timeout, Duration::from_secs(5));
        let s = load("\tpatchRemote = origin\n").unwrap();
        assert_eq!(s.patch_remote().unwrap().name, "origin");
        // A remote that is not configured disables the queue's fetch.
        let s = load("\tpatchRemote = other\n").unwrap();
        assert_eq!(s.patch_remote, PatchRemote::Named("other".into()));
        assert!(s.patch_remote().is_none());
        assert!(load("\tpatchRemote = a/b\n").is_err());
        assert!(matches!(
            load("\tpatchFetchTimeout = soon\n"),
            Err(ConfigError::BadDuration { .. })
        ));
        for off in ["0", "off"] {
            let s = load(&format!("\tpatchFetchTimeout = {off}\n")).unwrap();
            assert_eq!(s.patch_fetch_timeout, Duration::ZERO, "{off}");
        }
        // Two remotes and no choice: none.
        let (_d, c) = config_with(
            "[agentfs]\n\tvolume = v\n[remote \"a\"]\n\turl = /tmp/a.git\n[remote \"b\"]\n\turl = /tmp/b.git\n",
        );
        assert!(Settings::from_config(&c).unwrap().patch_remote().is_none());
    }

    #[test]
    fn repack_interval() {
        let get = |value: &str| {
            let (_d, c) = config_with(&format!(
                "[agentfs]\n\tvolume = v\n\trepackInterval = {value}\n"
            ));
            Settings::from_config(&c).map(|s| s.repack_interval)
        };
        assert_eq!(get("6h").unwrap(), Some(Duration::from_secs(6 * 3600)));
        assert_eq!(get("1m").unwrap(), Some(Duration::from_secs(60)));
        assert_eq!(get("0").unwrap(), None);
        assert_eq!(get("off").unwrap(), None);
        assert!(matches!(get("30s"), Err(ConfigError::TooShort { .. })));
        assert!(matches!(get("never"), Err(ConfigError::BadDuration { .. })));
    }

    #[test]
    fn remotes_are_parsed() {
        let (_d, c) = config_with(
            "[agentfs]\n\tvolume = v\n\
             [remote \"zeta\"]\n\turl = /srv/zeta.git\n\tagentfsMaxBlobSize = 100m\n\
             \tagentfsMaxPushSize = 2g\n\tagentfsMaxLfsObjectSize = 5g\n\
             [remote \"alpha\"]\n\turl = https://example.com/a.git\n\
             \tpushurl = https://push.example.com/a.git\n\
             \tagentfsCredentials = /run/secrets/alpha\n\
             [remote \"nourl\"]\n\tpushurl = /nowhere\n\
             [remote \"a.b\"]\n\turl = file:///srv/ab.git\n",
        );
        let s = Settings::from_config(&c).unwrap();
        let names: Vec<&str> = s.remotes.pushed().iter().map(|r| r.name.as_str()).collect();
        assert_eq!(names, ["a.b", "alpha", "zeta"]);
        let alpha = &s.remotes.pushed()[1];
        assert_eq!(alpha.url, "https://example.com/a.git");
        assert_eq!(alpha.push_url(), "https://push.example.com/a.git");
        assert_eq!(
            alpha.credentials.as_deref(),
            Some(Path::new("/run/secrets/alpha"))
        );
        assert_eq!(alpha.max_blob_size, None);
        let zeta = &s.remotes.pushed()[2];
        assert_eq!(zeta.push_url(), "/srv/zeta.git");
        assert_eq!(zeta.max_blob_size, Some(104_857_600));
        assert_eq!(zeta.max_push_size, Some(2 << 30));
        assert_eq!(alpha.max_push_size, None);
        assert_eq!(zeta.max_lfs_object_size, Some(5 << 30));
        assert_eq!(alpha.max_lfs_object_size, None);
        assert_eq!(s.remotes.pushed()[0].url, "file:///srv/ab.git");

        let (_d, c) = config_with("[agentfs]\n\tvolume = v\n");
        assert!(
            Settings::from_config(&c)
                .unwrap()
                .remotes
                .pushed()
                .is_empty()
        );
    }

    #[test]
    fn remote_and_volume_errors() {
        // An invalid remote is left out and reported; the volume still
        // opens, with its other remotes.
        let invalid = |section: &str| {
            let (_d, c) = config_with(&format!(
                "[agentfs]\n\tvolume = v\n[remote \"good\"]\n\turl = /g\n{section}"
            ));
            let s = Settings::from_config(&c).unwrap();
            let names: Vec<&str> = s.remotes.pushed().iter().map(|r| r.name.as_str()).collect();
            assert_eq!(names, ["good"], "{section}");
            assert_eq!(s.invalid_remotes.len(), 1, "{section}");
            s.invalid_remotes[0].clone()
        };
        for size in ["0", "-1"] {
            let bad = invalid(&format!(
                "[remote \"r\"]\n\turl = /x\n\tagentfsMaxBlobSize = {size}\n"
            ));
            assert_eq!(bad.name, "r");
            assert!(bad.error.contains("agentfsMaxBlobSize"), "{bad:?}");
        }
        for size in ["0", "-5m", "big"] {
            let bad = invalid(&format!(
                "[remote \"r\"]\n\turl = /x\n\tagentfsMaxPushSize = {size}\n"
            ));
            assert_eq!(bad.name, "r");
            if size != "big" {
                assert!(bad.error.contains("agentfsMaxPushSize"), "{bad:?}");
            }
        }
        for size in ["0", "-1k"] {
            let bad = invalid(&format!(
                "[remote \"r\"]\n\turl = /x\n\tagentfsMaxLfsObjectSize = {size}\n"
            ));
            assert!(bad.error.contains("agentfsMaxLfsObjectSize"), "{bad:?}");
        }
        let bad = invalid("[remote \"r\"]\n\turl = /x\n\tagentfsMaxLfsObjectSize = huge\n");
        assert_eq!(bad.name, "r");
        let bad = invalid("[remote \"r\"]\n\turl = /x\n\tagentfsMaxBlobSize = lots\n");
        assert_eq!(bad.name, "r");
        let bad = invalid("[remote \"r\"]\n\turl = /x\n\tagentfsCredentials = token\n");
        assert!(bad.error.contains("absolute"), "{bad:?}");
        let bad = invalid("[remote \"team/backup\"]\n\turl = /x\n");
        assert_eq!(bad.name, "team/backup");
        assert!(bad.error.contains("not a valid name"), "{bad:?}");
        for volume in ["a/b", "-v", "\"\"", ".v", "a..b", "v.lock", "v.", "v*"] {
            let (_d, c) = config_with(&format!("[agentfs]\n\tvolume = {volume}\n"));
            assert!(
                matches!(Settings::from_config(&c), Err(ConfigError::BadName { .. })),
                "{volume}"
            );
        }
        for ok in ["v", "hermes-0", "a.b_c", "9"] {
            validate_name("k", ok).unwrap();
        }
    }

    /// The settings of `[remote "r"]` with `section` as its body, next to a
    /// valid `[remote "good"]`.
    fn one_remote(section: &str) -> Settings {
        let (_d, c) = config_with(&format!(
            "[agentfs]\n\tvolume = v\n[remote \"good\"]\n\turl = /g\n[remote \"r\"]\n{section}"
        ));
        let s = Settings::from_config(&c).unwrap();
        assert!(
            s.remotes.pushed().iter().any(|r| r.name == "good"),
            "{section}"
        );
        s
    }

    fn invalid_error(section: &str) -> InvalidRemote {
        let s = one_remote(section);
        assert_eq!(s.remotes.pushed().len(), 1, "{section}: {:?}", s.remotes);
        assert_eq!(s.invalid_remotes.len(), 1, "{section}");
        s.invalid_remotes[0].clone()
    }

    #[test]
    fn ssh_remotes() {
        let full = "\turl = ssh://git@git.example:2222/o/r.git\n\
                    \tagentfsSshKey = /run/secrets/ssh/ssh-privatekey\n\
                    \tagentfsKnownHosts = /run/secrets/ssh/known_hosts\n";
        let s = one_remote(full);
        if cfg!(feature = "ssh") {
            let r = s.remotes.pushed().iter().find(|r| r.name == "r").unwrap();
            assert_eq!(r.transport(), Transport::Ssh);
            assert_eq!(
                r.ssh_key.as_deref(),
                Some(Path::new("/run/secrets/ssh/ssh-privatekey"))
            );
            assert_eq!(
                r.files(),
                [
                    (
                        "agentfsSshKey",
                        Path::new("/run/secrets/ssh/ssh-privatekey")
                    ),
                    (
                        "agentfsKnownHosts",
                        Path::new("/run/secrets/ssh/known_hosts")
                    )
                ]
            );
            // scp-like and a pushurl.
            for url in [
                "\turl = git@github.com:o/r.git\n",
                "\turl = https://example.com/r\n\tpushurl = git+ssh://h/r\n",
            ] {
                let s = one_remote(&format!(
                    "{url}\tagentfsSshKey = /k\n\tagentfsKnownHosts = /kh\n"
                ));
                assert_eq!(
                    s.remotes.pushed().len(),
                    2,
                    "{url}: {:?}",
                    s.invalid_remotes
                );
            }
            // An agent instead of a key, and a key with a passphrase file.
            for identity in [
                "\tagentfsSshAgent = /run/agent.sock\n",
                "\tagentfsSshKey = /k\n\tagentfsSshKeyPassphrase = /run/p\n",
            ] {
                let s = one_remote(&format!(
                    "\turl = ssh://h/r\n{identity}\tagentfsKnownHosts = /kh\n"
                ));
                assert_eq!(
                    s.remotes.pushed().len(),
                    2,
                    "{identity}: {:?}",
                    s.invalid_remotes
                );
                let r = s.remotes.pushed().iter().find(|r| r.name == "r").unwrap();
                assert_eq!(r.files().len(), 2 + usize::from(r.ssh_key.is_some()));
            }
        } else {
            let bad = invalid_error(full);
            assert!(bad.error.contains("`ssh` cargo feature"), "{bad:?}");
            assert_eq!(bad.files.len(), 2, "{bad:?}");
        }
    }

    #[test]
    fn ssh_misconfigurations() {
        for (section, expected) in [
            // A key or known_hosts on a non-ssh remote.
            (
                "\turl = https://h/r\n\tagentfsSshKey = /k\n",
                "remote.r.agentfsSshKey applies only to ssh remotes",
            ),
            (
                "\turl = /srv/r.git\n\tagentfsKnownHosts = /kh\n",
                "remote.r.agentfsKnownHosts applies only to ssh remotes",
            ),
            // pushurl wins: a non-ssh push URL takes no ssh keys.
            (
                "\turl = ssh://h/r\n\tpushurl = https://h/r\n\tagentfsSshKey = /k\n\
                 \tagentfsKnownHosts = /kh\n",
                "applies only to ssh remotes",
            ),
            // An ssh remote missing either file.
            (
                "\turl = ssh://h/r\n\tagentfsKnownHosts = /kh\n",
                "remote.r.agentfsSshKey or remote.r.agentfsSshAgent must be set",
            ),
            // A key and an agent, or a passphrase without a key.
            (
                "\turl = ssh://h/r\n\tagentfsSshKey = /k\n\tagentfsSshAgent = /a.sock\n\
                 \tagentfsKnownHosts = /kh\n",
                "are both set",
            ),
            (
                "\turl = ssh://h/r\n\tagentfsSshAgent = /a.sock\n\
                 \tagentfsSshKeyPassphrase = /p\n\tagentfsKnownHosts = /kh\n",
                "remote.r.agentfsSshKeyPassphrase needs remote.r.agentfsSshKey",
            ),
            (
                "\turl = https://h/r\n\tagentfsSshAgent = /a.sock\n",
                "remote.r.agentfsSshAgent applies only to ssh remotes",
            ),
            (
                "\turl = /srv/r.git\n\tagentfsSshKeyPassphrase = /p\n",
                "remote.r.agentfsSshKeyPassphrase applies only to ssh remotes",
            ),
            (
                "\turl = ssh://h/r\n\tagentfsSshAgent = agent.sock\n\tagentfsKnownHosts = /kh\n",
                "must be an absolute path",
            ),
            (
                "\turl = git@h:r\n\tagentfsSshKey = /k\n",
                "remote.r.agentfsKnownHosts must be set",
            ),
            // Tokens and passwords are not for ssh.
            (
                "\turl = ssh://h/r\n\tagentfsSshKey = /k\n\tagentfsKnownHosts = /kh\n\
                 \tagentfsCredentials = /c\n",
                "authenticate with agentfsSshKey, not agentfsCredentials",
            ),
            (
                "\turl = ssh://u:secret@h/r\n\tagentfsSshKey = /k\n\tagentfsKnownHosts = /kh\n",
                "remote.r.url: an ssh URL must not carry a password",
            ),
            (
                "\turl = /x\n\tpushurl = ssh://h:port/r\n\tagentfsSshKey = /k\n\
                 \tagentfsKnownHosts = /kh\n",
                "remote.r.pushurl: \"ssh://h:port/r\" is not a valid ssh URL",
            ),
            // Relative paths.
            (
                "\turl = ssh://h/r\n\tagentfsSshKey = id_ed25519\n\tagentfsKnownHosts = /kh\n",
                "must be an absolute path",
            ),
            (
                "\turl = ssh://h/r\n\tagentfsSshKey = /k\n\tagentfsKnownHosts = known_hosts\n",
                "must be an absolute path",
            ),
        ] {
            let bad = invalid_error(section);
            assert!(bad.error.contains(expected), "{section}: {}", bad.error);
            assert!(!bad.error.contains("secret"), "{}", bad.error);
        }
    }

    #[test]
    fn invalid_sections_keep_their_absolute_files() {
        let bad = invalid_error(
            "\turl = ssh://h/r\n\tagentfsCredentials = /c\n\tagentfsSshKey = /k\n\
             \tagentfsKnownHosts = relative\n",
        );
        assert_eq!(
            bad.files,
            [
                ("agentfsCredentials", PathBuf::from("/c")),
                ("agentfsSshKey", PathBuf::from("/k"))
            ]
        );
        let bad =
            invalid_error("\turl = /x\n\tagentfsMaxBlobSize = 0\n\tagentfsCredentials = /c\n");
        assert_eq!(bad.files, [("agentfsCredentials", PathBuf::from("/c"))]);
    }

    #[test]
    fn lfs_settings() {
        let (_d, c) = config_with("[agentfs]\n\tvolume = v\n");
        assert_eq!(
            Settings::from_config(&c).unwrap().lfs,
            LfsSettings::default()
        );
        let (_d, c) = config_with(
            "[agentfs]\n\tvolume = v\n\tlfsThreshold = 8m\n\
             [lfs]\n\turl = http://lfs.example/r\n\tpushurl = /srv/lfs.git\n\
             [remote \"r\"]\n\turl = http://git.example/r.git\n\
             \tlfsurl = http://lfs.example/r/info/lfs\n\
             \tlfspushurl = file:///srv/up.git\n\
             \tagentfsLfsCredentials = /run/secrets/lfs\n",
        );
        let s = Settings::from_config(&c).unwrap();
        assert_eq!(
            s.lfs,
            LfsSettings {
                threshold: Some(8 << 20),
                url: Some("http://lfs.example/r".into()),
                push_url: Some("/srv/lfs.git".into()),
                ..LfsSettings::default()
            }
        );
        let r = &s.remotes.pushed()[0];
        assert_eq!(r.lfs_url.as_deref(), Some("http://lfs.example/r/info/lfs"));
        assert_eq!(r.lfs_push_url.as_deref(), Some("file:///srv/up.git"));
        assert_eq!(
            r.lfs_credentials.as_deref(),
            Some(Path::new("/run/secrets/lfs"))
        );
        assert_eq!(
            r.files(),
            [("agentfsLfsCredentials", Path::new("/run/secrets/lfs"))]
        );
        for (value, ok) in [
            ("1", Some(1)),
            ("512k", Some(512 << 10)),
            ("2g", Some(2 << 30)),
        ] {
            let (_d, c) = config_with(&format!(
                "[agentfs]\n\tvolume = v\n\tlfsThreshold = {value}\n"
            ));
            assert_eq!(Settings::from_config(&c).unwrap().lfs.threshold, ok);
        }
        for bad in ["0", "-5"] {
            let (_d, c) = config_with(&format!(
                "[agentfs]\n\tvolume = v\n\tlfsThreshold = {bad}\n"
            ));
            assert!(
                matches!(Settings::from_config(&c), Err(ConfigError::BadSize { ref key, .. })
                    if key == "agentfs.lfsThreshold"),
                "{bad}"
            );
        }
        let (_d, c) = config_with("[agentfs]\n\tvolume = v\n\tlfsThreshold = lots\n");
        assert!(Settings::from_config(&c).is_err());
    }

    #[test]
    fn lfs_misconfigurations() {
        for (section, expected) in [
            (
                "\turl = /x\n\tlfsurl = ssh://git@h/r\n",
                "remote.r.lfsurl: \"ssh://***@h/r\" is not an LFS endpoint",
            ),
            (
                "\turl = /x\n\tlfspushurl = git@h:r\n",
                "remote.r.lfspushurl: \"git@h:r\" is not an LFS endpoint",
            ),
            (
                "\turl = /x\n\tlfsurl = git://h/r\n",
                "remote.r.lfsurl: \"git://h/r\" is not an LFS endpoint",
            ),
            (
                "\turl = /x\n\tagentfsLfsCredentials = lfs-token\n",
                "must be an absolute path",
            ),
        ] {
            let bad = invalid_error(section);
            assert!(bad.error.contains(expected), "{section}: {}", bad.error);
        }
        // A password in the URL is never echoed.
        let bad = invalid_error("\turl = /x\n\tlfsurl = ftp://u:hunter2@h/r\n");
        assert!(!bad.error.contains("hunter2"), "{}", bad.error);
        // The invalid section still names its LFS credential file for the
        // mount preflight.
        let bad = invalid_error(
            "\turl = /x\n\tlfsurl = ssh://h/r\n\tagentfsLfsCredentials = /run/secrets/lfs\n",
        );
        assert_eq!(
            bad.files,
            [("agentfsLfsCredentials", PathBuf::from("/run/secrets/lfs"))]
        );
        // https needs the feature.
        let s = one_remote("\turl = /x\n\tlfsurl = https://lfs.example/r\n");
        if cfg!(feature = "https") {
            assert_eq!(s.remotes.pushed().len(), 2, "{:?}", s.invalid_remotes);
        } else {
            assert_eq!(s.invalid_remotes.len(), 1);
            assert!(
                s.invalid_remotes[0].error.contains("`https` cargo feature"),
                "{:?}",
                s.invalid_remotes
            );
        }
    }

    #[test]
    fn lfs_concurrent_transfers() {
        let (_d, c) = config_with("[agentfs]\n\tvolume = v\n");
        let s = Settings::from_config(&c).unwrap();
        assert_eq!(s.lfs.concurrent_transfers, DEFAULT_CONCURRENT_TRANSFERS);
        assert_eq!(DEFAULT_CONCURRENT_TRANSFERS, 8);
        for (value, n) in [("1", 1), ("64", 64), (" 4 ", 4)] {
            let (_d, c) = config_with(&format!(
                "[agentfs]\n\tvolume = v\n[lfs]\n\tconcurrenttransfers = {value}\n\
                 [remote \"a\"]\n\turl = /a\n"
            ));
            let s = Settings::from_config(&c).unwrap();
            assert_eq!(s.lfs.concurrent_transfers, n, "{value}");
            assert_eq!(s.remotes.pushed().len(), 1, "{value}");
        }
        for bad in ["0", "65", "x", "-1", "\"\""] {
            let (_d, c) = config_with(&format!(
                "[agentfs]\n\tvolume = v\n[lfs]\n\tconcurrenttransfers = {bad}\n\
                 [remote \"a\"]\n\turl = /a\n[remote \"b\"]\n\turl = /b\n"
            ));
            // Never the volume: every remote.
            let s = Settings::from_config(&c).unwrap();
            assert!(s.remotes.pushed().is_empty(), "{bad}");
            assert_eq!(s.invalid_remotes.len(), 2, "{bad}");
            for r in &s.invalid_remotes {
                assert!(
                    r.error.starts_with("lfs.concurrenttransfers:"),
                    "{bad}: {}",
                    r.error
                );
            }
        }
    }

    #[test]
    fn proxy_settings() {
        let (_d, c) = config_with(
            "[agentfs]\n\tvolume = v\n[http]\n\tproxy = proxy.example:3128\n\
             [remote \"a\"]\n\turl = http://git.example/a.git\n\
             [remote \"b\"]\n\turl = http://git.example/b.git\n\tproxy = http://other:8080/\n\
             [remote \"c\"]\n\turl = http://git.example/c.git\n\tproxy = \"\"\n\
             [remote \"d\"]\n\turl = http://git.example/d.git\n\tproxy = https://p:1\n\
             [remote \"e\"]\n\turl = http://git.example/e.git\n\tproxy = http://u:pw@p:1\n\
             [remote \"f\"]\n\turl = http://git.example/f.git\n\tproxy = socks5://p:1\n\
             [remote \"g\"]\n\turl = http://git.example/g.git\n\tproxy = http://p:1/x\n",
        );
        let s = Settings::from_config(&c).unwrap();
        assert_eq!(s.http_proxy.as_deref(), Some("http://proxy.example:3128"));
        let proxy = |name: &str| {
            s.remotes
                .pushed()
                .iter()
                .find(|r| r.name == name)
                .unwrap()
                .proxy
                .clone()
        };
        assert_eq!(
            proxy("a"),
            Some(ProxySetting {
                url: "http://proxy.example:3128".into(),
                key: "http.proxy".into()
            })
        );
        assert_eq!(
            proxy("b"),
            Some(ProxySetting {
                url: "http://other:8080".into(),
                key: "remote.b.proxy".into()
            })
        );
        assert_eq!(proxy("c"), None, "an empty value disables the proxy");
        let bad: Vec<&str> = s.invalid_remotes.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(bad, ["d", "e", "f", "g"]);
        for r in &s.invalid_remotes {
            assert!(
                r.error.starts_with(&format!("remote.{}.proxy:", r.name)),
                "{}",
                r.error
            );
            assert!(!r.error.contains("pw"), "{}", r.error);
        }
        // A bad http.proxy misconfigures every remote, not the volume.
        let (_d, c) = config_with(
            "[agentfs]\n\tvolume = v\n[http]\n\tproxy = socks5://p:1\n\
             [remote \"a\"]\n\turl = /a\n[remote \"b\"]\n\turl = /b\n\tproxy = \"\"\n",
        );
        let s = Settings::from_config(&c).unwrap();
        assert!(s.remotes.pushed().is_empty());
        assert_eq!(s.http_proxy, None);
        for r in &s.invalid_remotes {
            assert!(r.error.starts_with("http.proxy:"), "{}", r.error);
        }
        // No proxy anywhere.
        let (_d, c) = config_with("[agentfs]\n\tvolume = v\n[remote \"a\"]\n\turl = /a\n");
        let s = Settings::from_config(&c).unwrap();
        assert_eq!(
            (s.http_proxy.clone(), s.remotes.pushed()[0].proxy.clone()),
            (None, None)
        );
    }

    #[test]
    fn a_bad_global_lfs_url_misconfigures_every_remote_but_not_the_volume() {
        let (_d, c) = config_with(
            "[agentfs]\n\tvolume = v\n[lfs]\n\turl = ssh://lfs.example/r\n\
             [remote \"a\"]\n\turl = /a\n[remote \"b\"]\n\turl = /b\n",
        );
        let s = Settings::from_config(&c).unwrap();
        assert!(s.remotes.pushed().is_empty());
        let names: Vec<&str> = s.invalid_remotes.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(names, ["a", "b"]);
        for bad in &s.invalid_remotes {
            assert!(bad.error.starts_with("lfs.url:"), "{}", bad.error);
        }
    }

    #[test]
    fn sizes() {
        assert_eq!(parse_size("100"), Some(100));
        assert_eq!(parse_size("512k"), Some(512 << 10));
        assert_eq!(parse_size("64M"), Some(64 << 20));
        assert_eq!(parse_size("2g"), Some(2 << 30));
        for bad in ["", "0", "0k", "k", "-1", "1.5m", "1t", "big", " m"] {
            assert_eq!(parse_size(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn durations() {
        assert_eq!(parse_duration("200ms"), Some(Duration::from_millis(200)));
        assert_eq!(parse_duration("2h"), Some(Duration::from_secs(7200)));
        assert_eq!(parse_duration("10"), None);
        assert_eq!(parse_duration("s"), None);
        assert_eq!(parse_duration("1d"), None);
    }

    /// A config file at `<dir>/config` including `fragment` (written to
    /// `<dir>/fragment`) with `include_line`, loaded the way
    /// [`Settings::load`] loads it.
    fn with_include(
        own: &str,
        fragment: &str,
    ) -> (tempfile::TempDir, Result<Settings, ConfigError>) {
        let dir = tempfile::tempdir().unwrap();
        let fragment_path = dir.path().join("fragment");
        std::fs::write(&fragment_path, fragment).unwrap();
        let own = own.replace("FRAGMENT", fragment_path.to_str().unwrap());
        std::fs::write(dir.path().join("config"), own).unwrap();
        let settings = Settings::load(dir.path());
        (dir, settings)
    }

    #[test]
    fn included_fragments_supply_settings_and_remotes() {
        let (dir, s) = with_include(
            "[agentfs]\n\tvolume = v\n[include]\n\tpath = FRAGMENT\n",
            "[agentfs]\n\tpushInterval = 7s\n\tnested = working-tree\n\
             [remote \"origin\"]\n\turl = https://git.example/r.git\n\
             \tagentfsCredentials = /run/secrets/agentfs/origin\n",
        );
        let s = s.unwrap();
        assert_eq!(s.push_interval, Duration::from_secs(7));
        assert_eq!(s.nested, Nested::WorkingTree);
        assert_eq!(s.remotes.pushed().len(), 1);
        assert_eq!(s.remotes.pushed()[0].url, "https://git.example/r.git");
        assert_eq!(s.includes, [dir.path().join("fragment")]);
        // No includes: an empty list.
        let (_d, c) = config_with("[agentfs]\n\tvolume = v\n");
        assert!(Settings::from_config(&c).unwrap().includes.is_empty());
    }

    #[test]
    fn the_own_file_wins_over_an_earlier_include() {
        // As in git: the last value read wins, so keys after the include
        // override it.
        let (_d, s) = with_include(
            "[include]\n\tpath = FRAGMENT\n[agentfs]\n\tvolume = v\n\tpushInterval = 9s\n",
            "[agentfs]\n\tpushInterval = 7s\n",
        );
        let s = s.unwrap();
        assert_eq!(s.push_interval, Duration::from_secs(9));
    }

    #[test]
    fn bad_includes_are_refused() {
        // Missing: git and libgit2 skip it; agentfs refuses.
        let (_d, s) = with_include(
            "[agentfs]\n\tvolume = v\n[include]\n\tpath = /nonexistent/agentfs/fragment\n",
            "",
        );
        let err = s.unwrap_err();
        assert!(
            matches!(&err, ConfigError::IncludeUnreadable { path, .. }
                if path == Path::new("/nonexistent/agentfs/fragment")),
            "{err}"
        );
        assert!(err.to_string().contains("skip it silently"), "{err}");
        // A directory is not a config file.
        let (_d, s) = with_include("[agentfs]\n\tvolume = v\n[include]\n\tpath = /\n", "");
        assert!(
            matches!(s, Err(ConfigError::IncludeUnreadable { .. })),
            "{s:?}"
        );
        // Relative: git would resolve it against the config file's
        // directory; agentfs refuses it.
        let (_d, s) = with_include(
            "[agentfs]\n\tvolume = v\n[include]\n\tpath = fragment\n",
            "[agentfs]\n\tpushInterval = 7s\n",
        );
        assert!(
            matches!(&s, Err(ConfigError::IncludeNotAbsolute { value }) if value == "fragment"),
            "{s:?}"
        );
        // Conditional includes.
        let (_d, s) = with_include(
            "[agentfs]\n\tvolume = v\n[includeIf \"gitdir:/x/\"]\n\tpath = FRAGMENT\n",
            "",
        );
        assert!(
            matches!(&s, Err(ConfigError::ConditionalInclude { key }) if key.contains("gitdir")),
            "{s:?}"
        );
        // An invalid key in a fragment fails like one in the own file.
        let (_d, s) = with_include(
            "[agentfs]\n\tvolume = v\n[include]\n\tpath = FRAGMENT\n",
            "[agentfs]\n\tpushInterval = soon\n",
        );
        assert!(matches!(s, Err(ConfigError::BadDuration { .. })), "{s:?}");
        // The volume's identity cannot come from a fragment.
        for key in ["volume", "branch"] {
            let (_d, s) = with_include(
                "[agentfs]\n\tvolume = v\n[include]\n\tpath = FRAGMENT\n",
                &format!("[agentfs]\n\t{key} = other\n"),
            );
            assert!(
                matches!(&s, Err(ConfigError::IncludeSetsIdentity { key: k, .. })
                    if *k == format!("agentfs.{key}")),
                "{s:?}"
            );
        }
    }

    /// F8 S1: the start point is identity, all three keys or none, from
    /// the volume's own config only.
    #[test]
    fn a_started_volumes_start_point() {
        let own = |start: &str| {
            format!(
                "[agentfs]\n\tvolume = v2\n[agentfs \"start\"]\n{start}[include]\n\tpath = FRAGMENT\n"
            )
        };
        let all = format!("\tremote = origin\n\tbranch = agentfs/v1\n\tcommit = {BASE}\n");
        let (_d, s) = with_include(&own(&all), "[remote \"origin\"]\n\turl = /o\n");
        let s = s.unwrap();
        let start = s.start.expect("a start point");
        assert_eq!(start.to_string(), format!("origin:agentfs/v1@{BASE}"));
        assert_eq!(
            s.branch_ref, "refs/heads/agentfs/v2",
            "its own branch is unchanged"
        );
        let (_d, s) = with_include(
            "[agentfs]\n\tvolume = v\n[include]\n\tpath = FRAGMENT\n",
            "",
        );
        assert_eq!(s.unwrap().start, None);
        for partial in [
            "\tremote = origin\n".to_owned(),
            format!("\tbranch = agentfs/v1\n\tcommit = {BASE}\n"),
        ] {
            let (_d, s) = with_include(&own(&partial), "");
            assert!(matches!(s, Err(ConfigError::PartialStartPoint)), "{s:?}");
        }
        let bad = "\tremote = origin\n\tbranch = refs/agentfs/v1/diverged/x\n\tcommit = abc\n";
        let (_d, s) = with_include(&own(bad), "");
        assert!(
            matches!(
                s,
                Err(ConfigError::BadStartPoint(
                    crate::fork::identity::BadStartPoint::NotABranch { .. }
                ))
            ),
            "{s:?}"
        );
        // No include may name one.
        for key in ["remote", "branch", "commit"] {
            let (_d, s) = with_include(
                "[agentfs]\n\tvolume = v\n[include]\n\tpath = FRAGMENT\n",
                &format!("[agentfs \"start\"]\n\t{key} = x\n"),
            );
            assert!(
                matches!(&s, Err(ConfigError::IncludeSetsIdentity { key: k, .. })
                    if *k == format!("agentfs.start.{key}")),
                "{s:?}"
            );
        }
    }

    const BASE: &str = "0123456789abcdef0123456789abcdef01234567";

    fn fork_config(extra: &str) -> String {
        format!(
            "[agentfs]\n\tvolume = f-1\n[agentfs \"fork\"]\n\tsource = src\n\tbase = {BASE}\n\
             {extra}"
        )
    }

    /// A1: a section only for forks changes nothing about the source.
    #[test]
    fn a_fork_remote_section_is_never_the_sources() {
        let own = "[agentfs]\n\tvolume = v\n[remote \"origin\"]\n\turl = /o\n\
                   \tagentfsMaxBlobSize = 1m\n";
        let forks = "[remote \"forks\"]\n\turl = /f\n\tagentfsForkRemote = true\n\
                     \tagentfsMaxBlobSize = 1k\n";
        let (_d, c) = config_with(own);
        let without = Settings::from_config(&c).unwrap();
        let (_d, c) = config_with(&format!("{own}{forks}"));
        let with = Settings::from_config(&c).unwrap();
        assert_eq!(with.remotes, without.remotes);
        assert_eq!(
            crate::checks::Limits::from_remotes(with.remotes.pushed()),
            crate::checks::Limits::from_remotes(without.remotes.pushed())
        );
        assert_eq!(with.patch_remote(), without.patch_remote());
        assert_eq!(with.patch_remote().map(|r| r.name.as_str()), Some("origin"));
        assert!(with.invalid_remotes.is_empty());
        assert_eq!(
            with.fork_remote_section().map(|r| r.name.as_str()),
            Some("forks")
        );
        assert!(!with.is_fork());
        // Naming the fork remote as the patch remote resolves to none.
        let (_d, c) = config_with(&format!(
            "[agentfs]\n\tvolume = v\n\tpatchRemote = forks\n[remote \"origin\"]\n\turl = /o\n\
             {forks}"
        ));
        assert_eq!(Settings::from_config(&c).unwrap().patch_remote(), None);
        // An invalid flagged section does not count against the automatic
        // choice, and leaves no fork remote.
        let (_d, c) = config_with(&format!(
            "{own}[remote \"forks\"]\n\turl = /f\n\tagentfsForkRemote = true\n\
             \tagentfsMaxBlobSize = 0\n"
        ));
        let s = Settings::from_config(&c).unwrap();
        assert_eq!(s.patch_remote().map(|r| r.name.as_str()), Some("origin"));
        assert_eq!(s.invalid_remotes.len(), 1);
        assert!(s.invalid_remotes[0].fork_remote);
        assert_eq!(s.fork_remote, None);
    }

    /// B5: a flagged section with only the global LFS endpoints is
    /// invalid (no fork remote); its own lfsurl or lfspushurl makes it
    /// valid; the default fork remote, the source's own, is unaffected.
    #[test]
    fn a_fork_remote_needs_its_own_lfs_endpoint() {
        let own = "[agentfs]\n\tvolume = v\n[lfs]\n\turl = http://lfs.example/source\n\
                   [remote \"origin\"]\n\turl = /o\n";
        let forks = "[remote \"forks\"]\n\turl = /f\n\tagentfsForkRemote = true\n";
        let (_d, c) = config_with(&format!("{own}{forks}"));
        let s = Settings::from_config(&c).unwrap();
        assert_eq!(s.fork_remote, None);
        let names: Vec<&str> = s.remotes.pushed().iter().map(|r| r.name.as_str()).collect();
        assert_eq!(names, ["origin"]);
        assert_eq!(s.invalid_remotes.len(), 1);
        assert!(s.invalid_remotes[0].fork_remote);
        assert!(
            s.invalid_remotes[0]
                .error
                .contains("must name its own LFS endpoint"),
            "{}",
            s.invalid_remotes[0].error
        );
        for key in ["lfsurl", "lfspushurl"] {
            let (_d, c) = config_with(&format!("{own}{forks}\t{key} = http://lfs.example/forks\n"));
            let s = Settings::from_config(&c).unwrap();
            assert_eq!(
                s.fork_remote_section().map(|r| r.name.as_str()),
                Some("forks"),
                "{key}"
            );
        }
        let (_d, c) = config_with(own);
        let s = Settings::from_config(&c).unwrap();
        assert_eq!(
            s.fork_remote_section().map(|r| r.name.as_str()),
            Some("origin")
        );
        assert!(s.invalid_remotes.is_empty());
    }

    /// Two flagged sections: both invalid, no fork remote.
    #[test]
    fn two_fork_remotes_are_both_invalid() {
        let (_d, c) = config_with(
            "[agentfs]\n\tvolume = v\n[remote \"origin\"]\n\turl = /o\n\
             [remote \"a\"]\n\turl = /a\n\tagentfsForkRemote = true\n\
             [remote \"b\"]\n\turl = /b\n\tagentfsForkRemote = true\n",
        );
        let s = Settings::from_config(&c).unwrap();
        let names: Vec<&str> = s.remotes.pushed().iter().map(|r| r.name.as_str()).collect();
        assert_eq!(names, ["origin"]);
        let bad: Vec<(&str, &str)> = s
            .invalid_remotes
            .iter()
            .map(|r| (r.name.as_str(), r.error.as_str()))
            .collect();
        assert_eq!(bad, [("a", MANY_FORK_REMOTES), ("b", MANY_FORK_REMOTES)]);
        assert_eq!(s.fork_remote, None);
        assert_eq!(s.patch_remote().map(|r| r.name.as_str()), Some("origin"));
    }

    /// A2: a fork pushes only to its fork remote, at its own branch, and
    /// has no patch queue.
    #[test]
    fn a_fork_has_one_remote_and_no_patch_queue() {
        let (_d, c) = config_with(&fork_config(
            "[agentfs]\n\tpatchRemote = origin\n[remote \"origin\"]\n\turl = /o\n\
             [remote \"other\"]\n\turl = /x\n\
             [remote \"forks\"]\n\turl = /f\n\tagentfsForkRemote = true\n",
        ));
        let s = Settings::from_config(&c).unwrap();
        assert!(s.is_fork());
        let fork = s.fork.as_ref().unwrap();
        assert_eq!(fork.source, "src");
        assert_eq!(fork.base.to_string(), BASE);
        assert_eq!(s.branch_ref, "refs/heads/agentfs/f-1");
        let names: Vec<&str> = s.remotes.pushed().iter().map(|r| r.name.as_str()).collect();
        assert_eq!(names, ["forks"]);
        assert_eq!(s.patch_remote, PatchRemote::Disabled);
        assert_eq!(s.patch_remote(), None);
        // No flagged section: the source's patch remote (gate answer Q1 b).
        let (_d, c) = config_with(&fork_config("[remote \"origin\"]\n\turl = /o\n"));
        let s = Settings::from_config(&c).unwrap();
        let names: Vec<&str> = s.remotes.pushed().iter().map(|r| r.name.as_str()).collect();
        assert_eq!(names, ["origin"]);
        assert_eq!(
            s.fork_remote_section().map(|r| r.name.as_str()),
            Some("origin")
        );
        assert_eq!(s.patch_remote(), None);
        // Two remotes and no patch remote: no fork remote, nothing pushed.
        let (_d, c) = config_with(&fork_config(
            "[remote \"a\"]\n\turl = /a\n[remote \"b\"]\n\turl = /b\n",
        ));
        let s = Settings::from_config(&c).unwrap();
        assert!(s.remotes.pushed().is_empty());
        assert_eq!(s.fork_remote, None);
        // An unflagged invalid section is not the fork's: nothing raised.
        let (_d, c) = config_with(&fork_config(
            "[remote \"forks\"]\n\turl = /f\n\tagentfsForkRemote = true\n\
             [remote \"bad\"]\n\turl = /b\n\tagentfsMaxBlobSize = 0\n",
        ));
        let s = Settings::from_config(&c).unwrap();
        assert!(s.invalid_remotes.is_empty(), "{:?}", s.invalid_remotes);
        assert_eq!(s.remotes.pushed().len(), 1);
        // Two flagged sections: both invalid, nothing pushed.
        let (_d, c) = config_with(&fork_config(
            "[remote \"a\"]\n\turl = /a\n\tagentfsForkRemote = true\n\
             [remote \"b\"]\n\turl = /b\n\tagentfsForkRemote = true\n",
        ));
        let s = Settings::from_config(&c).unwrap();
        assert!(s.remotes.pushed().is_empty());
        assert_eq!(s.invalid_remotes.len(), 2);
        assert_eq!(s.fork_remote, None);
    }

    /// A3: a fork's identity is its own config's, whole and well formed.
    #[test]
    fn fork_identity_errors() {
        let (_d, c) = config_with(&fork_config("[agentfs]\n\tbranch = x\n"));
        assert!(matches!(
            Settings::from_config(&c),
            Err(ConfigError::ForkBranch)
        ));
        for base in ["abc", "0123456789ABCDEF0123456789ABCDEF01234567", "HEAD"] {
            let (_d, c) = config_with(&format!(
                "[agentfs]\n\tvolume = f\n[agentfs \"fork\"]\n\tsource = s\n\tbase = {base}\n"
            ));
            assert!(
                matches!(
                    Settings::from_config(&c),
                    Err(ConfigError::BadForkBase { .. })
                ),
                "{base}"
            );
        }
        let (_d, c) = config_with("[agentfs]\n\tvolume = f\n[agentfs \"fork\"]\n\tsource = s\n");
        assert!(matches!(
            Settings::from_config(&c),
            Err(ConfigError::PartialForkIdentity)
        ));
        let (_d, c) = config_with(&format!(
            "[agentfs]\n\tvolume = f\n[agentfs \"fork\"]\n\tsource = a/b\n\tbase = {BASE}\n"
        ));
        assert!(matches!(
            Settings::from_config(&c),
            Err(ConfigError::BadName { .. })
        ));
        for key in ["source", "base", "pushTarget", "link"] {
            let (_d, s) = with_include(
                "[agentfs]\n\tvolume = v\n[include]\n\tpath = FRAGMENT\n",
                &format!("[agentfs \"fork\"]\n\t{key} = x\n"),
            );
            assert!(
                matches!(&s, Err(ConfigError::IncludeSetsIdentity { key: k, .. })
                    if *k == format!("agentfs.fork.{key}")),
                "{s:?}"
            );
        }
    }

    /// D1, D2: a fork's push target is its own config's, `branch` when
    /// unset; `hidden` keeps its history off `refs/heads/`. A source's
    /// `agentfs.forkRefs` is the target new forks get. Any other value of
    /// either key is `BadChoice`, and `pushTarget` alone is a partial
    /// identity.
    #[test]
    fn push_targets() {
        let (_d, c) = config_with(&fork_config(
            "[agentfs \"fork\"]\n\tpushTarget = hidden\n[remote \"origin\"]\n\turl = /o\n",
        ));
        let s = Settings::from_config(&c).unwrap();
        assert_eq!(
            s.fork.as_ref().map(|f| f.push_target),
            Some(PushTarget::Hidden)
        );
        assert_eq!(s.branch_ref, "refs/agentfs/f-1/history");
        let (_d, c) = config_with(&fork_config("[agentfs \"fork\"]\n\tpushTarget = branch\n"));
        let s = Settings::from_config(&c).unwrap();
        assert_eq!(s.branch_ref, "refs/heads/agentfs/f-1");

        let (_d, c) = config_with("[agentfs]\n\tvolume = v\n");
        assert_eq!(
            Settings::from_config(&c).unwrap().fork_refs,
            PushTarget::Branch
        );
        for (value, want) in [
            ("branch", PushTarget::Branch),
            ("hidden", PushTarget::Hidden),
        ] {
            let (_d, c) = config_with(&format!("[agentfs]\n\tvolume = v\n\tforkRefs = {value}\n"));
            assert_eq!(Settings::from_config(&c).unwrap().fork_refs, want);
        }

        for value in ["Hidden", "heads", "", "refs/agentfs"] {
            let (_d, c) = config_with(&format!(
                "[agentfs]\n\tvolume = v\n\tforkRefs = \"{value}\"\n"
            ));
            assert!(
                matches!(
                    Settings::from_config(&c),
                    Err(ConfigError::BadChoice { ref key, allowed, .. })
                        if key == "agentfs.forkRefs" && allowed == "branch, hidden"
                ),
                "forkRefs = {value:?}"
            );
            let (_d, c) = config_with(&fork_config(&format!(
                "[agentfs \"fork\"]\n\tpushTarget = \"{value}\"\n"
            )));
            assert!(
                matches!(
                    Settings::from_config(&c),
                    Err(ConfigError::BadChoice { ref key, .. }) if key == "agentfs.fork.pushTarget"
                ),
                "pushTarget = {value:?}"
            );
        }
        let (_d, c) =
            config_with("[agentfs]\n\tvolume = f\n[agentfs \"fork\"]\n\tpushTarget = hidden\n");
        assert!(matches!(
            Settings::from_config(&c),
            Err(ConfigError::PartialForkIdentity)
        ));
    }

    /// A fork's link (design §11.5, §19.1 "Fork link"): its own config's,
    /// `linked` when unset. An unlinked fork has no fork remote, pushes
    /// its history where a linked one would publish, is local-only with no
    /// section at all, and may not record a source path. Any other value
    /// is `BadChoice`, and `link` alone is a partial identity.
    #[test]
    fn fork_links() {
        let link = |extra: &str| {
            let (_d, c) = config_with(&fork_config(extra));
            Settings::from_config(&c)
        };
        let origin = "[remote \"origin\"]\n\turl = /o\n";
        let linked = link(origin).unwrap();
        assert_eq!(linked.fork.as_ref().unwrap().link, ForkLink::Linked);
        let explicit = link(&format!("[agentfs \"fork\"]\n\tlink = linked\n{origin}")).unwrap();
        assert_eq!(explicit.fork, linked.fork);
        assert!(linked.fork_remote.is_some());

        let unlinked = "[agentfs \"fork\"]\n\tlink = unlinked\n";
        let s = link(&format!("{unlinked}{origin}")).unwrap();
        assert_eq!(s.fork.as_ref().unwrap().link, ForkLink::Unlinked);
        assert_eq!(s.fork_remote, None);
        assert_eq!(s.branch_ref, "refs/heads/agentfs/f-1");
        let names: Vec<&str> = s.remotes.pushed().iter().map(|r| r.name.as_str()).collect();
        assert_eq!(names, ["origin"]);
        assert_eq!(s.patch_remote(), None);
        // A flagged section: the fork remote, never the source's own.
        let flagged = "[remote \"forks\"]\n\turl = /f\n\tagentfsForkRemote = true\n";
        let s = link(&format!("{unlinked}{origin}{flagged}")).unwrap();
        assert_eq!(s.remotes.pushed()[0].name, "forks");
        assert_eq!(s.fork_remote, None);
        // No section: local-only; two unflagged and no patch remote:
        // nothing to push to, undeclared, as a linked fork.
        assert_eq!(link(unlinked).unwrap().remotes, Remotes::LocalOnly);
        let two = "[remote \"a\"]\n\turl = /a\n[remote \"b\"]\n\turl = /b\n";
        assert_eq!(
            link(&format!("{unlinked}{two}")).unwrap().remotes,
            Remotes::Undeclared { beside: None }
        );
        assert_eq!(
            link("").unwrap().remotes,
            Remotes::Undeclared { beside: None }
        );
        assert!(matches!(
            link(&format!("{unlinked}\tsourceState = /srv/src/state\n")),
            Err(ConfigError::UnlinkedSourceState)
        ));
        for value in ["Unlinked", "none", "", "true"] {
            assert!(
                matches!(
                    link(&format!("[agentfs \"fork\"]\n\tlink = \"{value}\"\n")),
                    Err(ConfigError::BadChoice { ref key, allowed, .. })
                        if key == "agentfs.fork.link" && allowed == "linked, unlinked"
                ),
                "link = {value:?}"
            );
        }
        let (_d, c) =
            config_with("[agentfs]\n\tvolume = f\n[agentfs \"fork\"]\n\tlink = unlinked\n");
        assert!(matches!(
            Settings::from_config(&c),
            Err(ConfigError::PartialForkIdentity)
        ));
    }

    /// F8 S3 acceptance 1: each `Remotes` answer from config fragments.
    #[test]
    fn remotes_are_declared_one_way() {
        let mode = |lines: &str| {
            let (_d, c) = config_with(&format!("[agentfs]\n\tvolume = v\n{lines}"));
            Settings::from_config(&c).unwrap().remotes
        };
        let pushed = mode("[remote \"b\"]\n\turl = /b\n[remote \"a\"]\n\turl = /a\n");
        let names: Vec<&str> = pushed.pushed().iter().map(|r| r.name.as_str()).collect();
        assert_eq!(names, ["a", "b"]);
        assert_eq!(pushed.mode(), "pushed");
        // An invalid section beside a valid one alarms; still pushed.
        let beside_bad = "[remote \"a\"]\n\turl = /a\n[remote \"bad\"]\n\turl = /b\n\
                          \tagentfsMaxBlobSize = 0\n";
        assert_eq!(mode(beside_bad).mode(), "pushed");
        assert_eq!(
            mode("[remote \"bad\"]\n\turl = /b\n\tagentfsMaxBlobSize = 0\n"),
            Remotes::NoneValid
        );
        assert_eq!(mode("[agentfs]\n\tremotes = none\n"), Remotes::LocalOnly);
        assert_eq!(mode(""), Remotes::Undeclared { beside: None });
        // A flagged section is not a push section: alone, nothing is
        // declared, and beside `none` it is local-only.
        let flagged = "[remote \"forks\"]\n\turl = /f\n\tagentfsForkRemote = true\n";
        assert_eq!(mode(flagged), Remotes::Undeclared { beside: None });
        assert_eq!(
            mode(&format!("[agentfs]\n\tremotes = none\n{flagged}")),
            Remotes::LocalOnly
        );
    }

    /// `none` beside a push section is undeclared, naming it; the section
    /// is never pushed, alarms, and its files stay checked.
    #[test]
    fn none_beside_a_section_is_undeclared() {
        let (_d, c) = config_with(
            "[agentfs]\n\tvolume = v\n\tremotes = none\n[remote \"origin\"]\n\turl = /o\n\
             \tagentfsCredentials = /run/c\n",
        );
        let s = Settings::from_config(&c).unwrap();
        assert_eq!(
            s.remotes,
            Remotes::Undeclared {
                beside: Some("origin".into())
            }
        );
        assert!(s.remotes.pushed().is_empty());
        assert_eq!(s.invalid_remotes.len(), 1);
        assert_eq!(s.invalid_remotes[0].name, "origin");
        assert_eq!(
            s.invalid_remotes[0].files,
            [("agentfsCredentials", PathBuf::from("/run/c"))]
        );
        assert_eq!(s.patch_remote(), None);
        assert_eq!(s.fork_remote, None);
        // An invalid section beside it counts too.
        let (_d, c) = config_with(
            "[agentfs]\n\tvolume = v\n\tremotes = none\n[remote \"bad\"]\n\turl = /b\n\
             \tagentfsMaxBlobSize = 0\n",
        );
        assert_eq!(
            Settings::from_config(&c).unwrap().remotes,
            Remotes::Undeclared {
                beside: Some("bad".into())
            }
        );
    }

    #[test]
    fn a_bad_remotes_value_is_refused() {
        for value in ["None", "local", "", "origin"] {
            let (_d, c) = config_with(&format!(
                "[agentfs]\n\tvolume = v\n\tremotes = \"{value}\"\n"
            ));
            assert!(
                matches!(
                    Settings::from_config(&c),
                    Err(ConfigError::BadChoice { ref key, allowed, .. })
                        if key == "agentfs.remotes" && allowed == "none"
                ),
                "remotes = {value:?}"
            );
        }
    }

    /// `ForkRemote::SourceHistory` only for a local-only source with no
    /// flagged section, and for a fork recording its source's state area.
    #[test]
    fn only_a_local_only_source_forks_into_its_history() {
        let fork_remote = |lines: &str| {
            let (_d, c) = config_with(&format!("[agentfs]\n\tvolume = v\n{lines}"));
            Settings::from_config(&c).unwrap().fork_remote
        };
        let none = "[agentfs]\n\tremotes = none\n";
        let flagged = "[remote \"forks\"]\n\turl = /f\n\tagentfsForkRemote = true\n";
        assert_eq!(fork_remote(none), Some(ForkRemote::SourceHistory));
        assert!(matches!(
            fork_remote(&format!("{none}{flagged}")),
            Some(ForkRemote::Remote(r)) if r.name == "forks"
        ));
        assert!(matches!(
            fork_remote("[remote \"origin\"]\n\turl = /o\n"),
            Some(ForkRemote::Remote(r)) if r.name == "origin"
        ));
        assert_eq!(fork_remote(""), None);
        assert_eq!(
            fork_remote("[remote \"bad\"]\n\turl = /b\n\tagentfsMaxBlobSize = 0\n"),
            None
        );
        // A fork: its source's state area recorded, it publishes there and
        // is local-only itself; a flagged section still wins.
        let state = "[agentfs \"fork\"]\n\tsourceState = /srv/src/state\n";
        let (_d, c) = config_with(&fork_config(state));
        let s = Settings::from_config(&c).unwrap();
        assert_eq!(s.fork_remote, Some(ForkRemote::SourceHistory));
        assert_eq!(s.remotes, Remotes::LocalOnly);
        assert_eq!(s.source_state, Some(PathBuf::from("/srv/src/state")));
        let (_d, c) = config_with(&fork_config(&format!("{state}{flagged}")));
        let s = Settings::from_config(&c).unwrap();
        assert!(matches!(&s.fork_remote, Some(ForkRemote::Remote(r)) if r.name == "forks"));
        assert_eq!(s.remotes.pushed()[0].name, "forks");
        // A fork with no fork remote is undeclared.
        let (_d, c) = config_with(&fork_config(""));
        assert_eq!(
            Settings::from_config(&c).unwrap().remotes,
            Remotes::Undeclared { beside: None }
        );
        // Only beside a fork identity, and only absolute.
        let (_d, c) = config_with(&format!("[agentfs]\n\tvolume = v\n{state}"));
        assert!(matches!(
            Settings::from_config(&c),
            Err(ConfigError::PartialForkIdentity)
        ));
        let (_d, c) = config_with(&fork_config("[agentfs \"fork\"]\n\tsourceState = rel\n"));
        assert!(matches!(
            Settings::from_config(&c),
            Err(ConfigError::RelativePath { ref key, .. }) if key == "agentfs.fork.sourceState"
        ));
    }
}
