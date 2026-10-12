//! Operator settings from the history repository's git config (design §4.3,
//! §11.1).
//!
//! Only the repository's own config file is read: system and global git
//! config never influence a volume. Remotes are the standard
//! `[remote "<name>"]` sections with `url` (and optionally `pushurl`), plus
//! agentfs's own per-remote keys.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::identity::Templates;

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
    /// `remote.<name>.agentfsCredentials`: an absolute path to the
    /// credential file ([`crate::credentials`]).
    pub credentials: Option<PathBuf>,
}

/// A `[remote "<name>"]` section that is left out because it is invalid.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvalidRemote {
    pub name: String,
    pub error: String,
}

impl RemoteSettings {
    /// The URL pushes go to.
    pub fn push_url(&self) -> &str {
        self.push_url.as_deref().unwrap_or(&self.url)
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
    pub identity: Templates,
    /// Push remotes, sorted by name.
    pub remotes: Vec<RemoteSettings>,
    /// Remote sections that could not be used, sorted by name. They are
    /// left out of `remotes` and reported as alarms; they never fail the
    /// volume (design §11.1).
    pub invalid_remotes: Vec<InvalidRemote>,
}

impl Settings {
    /// Reads settings from `<git_dir>/config` only.
    pub fn load(git_dir: &Path) -> Result<Self, ConfigError> {
        let config = git2::Config::open(&git_dir.join("config"))?;
        Self::from_config(&config)
    }

    pub fn from_config(config: &git2::Config) -> Result<Self, ConfigError> {
        let volume = string(config, "agentfs.volume")?.ok_or(ConfigError::MissingVolume)?;
        validate_name("agentfs.volume", &volume)?;
        let branch =
            string(config, "agentfs.branch")?.unwrap_or_else(|| format!("agentfs/{volume}"));
        let defaults = Templates::default();
        let (remotes, invalid_remotes) = remotes(config)?;
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
            remotes,
            invalid_remotes,
            volume,
        })
    }
}

/// Every `[remote "<name>"]` with a `url`, sorted by name: the valid ones,
/// and the invalid ones with their errors. A remote only affects pushing,
/// so a mistake in one never fails the volume (design §11.1).
fn remotes(
    config: &git2::Config,
) -> Result<(Vec<RemoteSettings>, Vec<InvalidRemote>), ConfigError> {
    let mut names = BTreeSet::new();
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
        match remote(config, &name) {
            Ok(remote) => valid.push(remote),
            Err(e) => invalid.push(InvalidRemote {
                name,
                error: e.to_string(),
            }),
        }
    }
    Ok((valid, invalid))
}

/// One `[remote "<name>"]` section.
fn remote(config: &git2::Config, name: &str) -> Result<RemoteSettings, ConfigError> {
    validate_name(&format!("remote.{name}"), name)?;
    let key = |k: &str| format!("remote.{name}.{k}");
    let Some(url) = string(config, &key("url"))? else {
        return Err(git2::Error::from_str("remote url vanished while reading").into());
    };
    let max_blob_size = match int(config, &key("agentfsMaxBlobSize"))? {
        None => None,
        Some(n) if n > 0 => Some(n as u64),
        Some(n) => {
            return Err(ConfigError::BadSize {
                key: key("agentfsMaxBlobSize"),
                value: n.to_string(),
            });
        }
    };
    let credentials = match string(config, &key("agentfsCredentials"))? {
        None => None,
        Some(path) if Path::new(&path).is_absolute() => Some(PathBuf::from(path)),
        Some(path) => {
            return Err(ConfigError::RelativePath {
                key: key("agentfsCredentials"),
                value: path,
            });
        }
    };
    Ok(RemoteSettings {
        push_url: string(config, &key("pushurl"))?,
        url,
        max_blob_size,
        credentials,
        name: name.to_owned(),
    })
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

fn string(config: &git2::Config, key: &str) -> Result<Option<String>, git2::Error> {
    match config.get_string(key) {
        Ok(v) => Ok(Some(v)),
        Err(e) if e.code() == git2::ErrorCode::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

fn boolean(config: &git2::Config, key: &str) -> Result<Option<bool>, git2::Error> {
    match config.get_bool(key) {
        Ok(v) => Ok(Some(v)),
        Err(e) if e.code() == git2::ErrorCode::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

/// An integer with git's `k`, `m` and `g` suffixes, as for `lfsThreshold`.
fn int(config: &git2::Config, key: &str) -> Result<Option<i64>, git2::Error> {
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
    fn remotes_are_parsed() {
        let (_d, c) = config_with(
            "[agentfs]\n\tvolume = v\n\
             [remote \"zeta\"]\n\turl = /srv/zeta.git\n\tagentfsMaxBlobSize = 100m\n\
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

    #[test]
    fn durations() {
        assert_eq!(parse_duration("200ms"), Some(Duration::from_millis(200)));
        assert_eq!(parse_duration("2h"), Some(Duration::from_secs(7200)));
        assert_eq!(parse_duration("10"), None);
        assert_eq!(parse_duration("s"), None);
        assert_eq!(parse_duration("1d"), None);
    }
}
