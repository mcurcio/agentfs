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

use crate::identity::Templates;
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
    "agentfs.patchRemote",
    "agentfs.patchFetchTimeout",
    "agentfs.lfsThreshold",
    "agentfs.identity.committerName",
    "agentfs.identity.committerEmail",
    "agentfs.identity.authorName",
    "agentfs.identity.authorEmail",
    "lfs.url",
    "lfs.pushurl",
    "lfs.concurrenttransfers",
    "include.path",
    "includeIf.<condition>.path",
    "remote.<name>.url",
    "remote.<name>.pushurl",
    "remote.<name>.agentfsMaxBlobSize",
    "remote.<name>.agentfsMaxPushSize",
    "remote.<name>.agentfsCredentials",
    "remote.<name>.agentfsSshKey",
    "remote.<name>.agentfsKnownHosts",
    "remote.<name>.agentfsSshAgent",
    "remote.<name>.agentfsSshKeyPassphrase",
    "remote.<name>.lfsurl",
    "remote.<name>.lfspushurl",
    "remote.<name>.agentfsLfsCredentials",
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
    /// Push remotes, sorted by name.
    pub remotes: Vec<RemoteSettings>,
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
    /// The remote the patch queue uses, if any (design §12).
    pub fn patch_remote(&self) -> Option<&RemoteSettings> {
        match &self.patch_remote {
            PatchRemote::Disabled => None,
            PatchRemote::Auto => match self.remotes.as_slice() {
                [only] if self.invalid_remotes.is_empty() => Some(only),
                _ => None,
            },
            PatchRemote::Named(name) => self.remotes.iter().find(|r| r.name == *name),
        }
    }
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
        let branch =
            string(config, "agentfs.branch")?.unwrap_or_else(|| format!("agentfs/{volume}"));
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
        let (remotes, invalid_remotes) = remotes(config, &lfs)?;
        Ok(Self {
            branch_ref: format!("refs/heads/{branch}"),
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
            patch_remote: match string(config, "agentfs.patchRemote")?.as_deref() {
                None | Some("") => PatchRemote::Auto,
                Some("none") => PatchRemote::Disabled,
                Some(name) => {
                    validate_name("agentfs.patchRemote", name)?;
                    PatchRemote::Named(name.to_owned())
                }
            },
            patch_fetch_timeout: patch_fetch_timeout(config)?,
            lfs,
            remotes,
            invalid_remotes,
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
        let fragment = git2::Config::open(&path)?;
        for key in ["agentfs.volume", "agentfs.branch"] {
            if string(&fragment, key)?.is_some() {
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
        match remote(config, &name).and_then(|r| validate_lfs(&r, lfs).map(|()| r)) {
            Ok(remote) => valid.push(remote),
            Err(e) => invalid.push(InvalidRemote {
                files: remote_files(config, &name),
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
fn remote(config: &git2::Config, name: &str) -> Result<RemoteSettings, ConfigError> {
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
        credentials: path("agentfsCredentials")?,
        ssh_key: path("agentfsSshKey")?,
        known_hosts: path("agentfsKnownHosts")?,
        ssh_agent: path("agentfsSshAgent")?,
        ssh_key_passphrase: path("agentfsSshKeyPassphrase")?,
        lfs_url: string(config, &key("lfsurl"))?,
        lfs_push_url: string(config, &key("lfspushurl"))?,
        lfs_credentials: path("agentfsLfsCredentials")?,
        name: name.to_owned(),
    };
    validate_transport(&remote)?;
    Ok(remote)
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
                 [remote \"web\"]\n\turl = http://git.example/r.git\n\
                 \tpushurl = http://git.example/p.git\n\tagentfsMaxBlobSize = 100m\n\
                 \tagentfsCredentials = /run/c\n\tlfsurl = http://lfs.example/web\n\
                 \tlfspushurl = http://lfs.example/web-push\n\tagentfsLfsCredentials = /run/l\n\
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
            settings.remotes.len() + settings.invalid_remotes.len(),
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
             \tagentfsMaxPushSize = 2g\n\
             [remote \"alpha\"]\n\turl = https://example.com/a.git\n\
             \tpushurl = https://push.example.com/a.git\n\
             \tagentfsCredentials = /run/secrets/alpha\n\
             [remote \"nourl\"]\n\tpushurl = /nowhere\n\
             [remote \"a.b\"]\n\turl = file:///srv/ab.git\n",
        );
        let s = Settings::from_config(&c).unwrap();
        let names: Vec<&str> = s.remotes.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(names, ["a.b", "alpha", "zeta"]);
        let alpha = &s.remotes[1];
        assert_eq!(alpha.url, "https://example.com/a.git");
        assert_eq!(alpha.push_url(), "https://push.example.com/a.git");
        assert_eq!(
            alpha.credentials.as_deref(),
            Some(Path::new("/run/secrets/alpha"))
        );
        assert_eq!(alpha.max_blob_size, None);
        let zeta = &s.remotes[2];
        assert_eq!(zeta.push_url(), "/srv/zeta.git");
        assert_eq!(zeta.max_blob_size, Some(104_857_600));
        assert_eq!(zeta.max_push_size, Some(2 << 30));
        assert_eq!(alpha.max_push_size, None);
        assert_eq!(s.remotes[0].url, "file:///srv/ab.git");

        let (_d, c) = config_with("[agentfs]\n\tvolume = v\n");
        assert!(Settings::from_config(&c).unwrap().remotes.is_empty());
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
            let names: Vec<&str> = s.remotes.iter().map(|r| r.name.as_str()).collect();
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
        assert!(s.remotes.iter().any(|r| r.name == "good"), "{section}");
        s
    }

    fn invalid_error(section: &str) -> InvalidRemote {
        let s = one_remote(section);
        assert_eq!(s.remotes.len(), 1, "{section}: {:?}", s.remotes);
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
            let r = s.remotes.iter().find(|r| r.name == "r").unwrap();
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
                assert_eq!(s.remotes.len(), 2, "{url}: {:?}", s.invalid_remotes);
            }
            // An agent instead of a key, and a key with a passphrase file.
            for identity in [
                "\tagentfsSshAgent = /run/agent.sock\n",
                "\tagentfsSshKey = /k\n\tagentfsSshKeyPassphrase = /run/p\n",
            ] {
                let s = one_remote(&format!(
                    "\turl = ssh://h/r\n{identity}\tagentfsKnownHosts = /kh\n"
                ));
                assert_eq!(s.remotes.len(), 2, "{identity}: {:?}", s.invalid_remotes);
                let r = s.remotes.iter().find(|r| r.name == "r").unwrap();
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
        let r = &s.remotes[0];
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
            assert_eq!(s.remotes.len(), 2, "{:?}", s.invalid_remotes);
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
            assert_eq!(s.remotes.len(), 1, "{value}");
        }
        for bad in ["0", "65", "x", "-1", "\"\""] {
            let (_d, c) = config_with(&format!(
                "[agentfs]\n\tvolume = v\n[lfs]\n\tconcurrenttransfers = {bad}\n\
                 [remote \"a\"]\n\turl = /a\n[remote \"b\"]\n\turl = /b\n"
            ));
            // Never the volume: every remote.
            let s = Settings::from_config(&c).unwrap();
            assert!(s.remotes.is_empty(), "{bad}");
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
    fn a_bad_global_lfs_url_misconfigures_every_remote_but_not_the_volume() {
        let (_d, c) = config_with(
            "[agentfs]\n\tvolume = v\n[lfs]\n\turl = ssh://lfs.example/r\n\
             [remote \"a\"]\n\turl = /a\n[remote \"b\"]\n\turl = /b\n",
        );
        let s = Settings::from_config(&c).unwrap();
        assert!(s.remotes.is_empty());
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
        assert_eq!(s.remotes.len(), 1);
        assert_eq!(s.remotes[0].url, "https://git.example/r.git");
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
}
