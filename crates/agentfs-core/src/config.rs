//! Operator settings from the history repository's git config (design §4.3).
//!
//! Only the repository's own config file is read: system and global git
//! config never influence a volume.

use std::path::Path;
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
}

/// How nested repositories are handled (design §5).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Nested {
    Exclude,
    WorkingTree,
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
    pub identity: Templates,
}

impl Settings {
    /// Reads settings from `<git_dir>/config` only.
    pub fn load(git_dir: &Path) -> Result<Self, ConfigError> {
        let config = git2::Config::open(&git_dir.join("config"))?;
        Self::from_config(&config)
    }

    pub fn from_config(config: &git2::Config) -> Result<Self, ConfigError> {
        let volume = string(config, "agentfs.volume")?.ok_or(ConfigError::MissingVolume)?;
        let branch =
            string(config, "agentfs.branch")?.unwrap_or_else(|| format!("agentfs/{volume}"));
        let defaults = Templates::default();
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
            volume,
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
        assert_eq!(s.identity, Templates::default());
    }

    #[test]
    fn overrides() {
        let (_d, c) = config_with(
            "[agentfs]\n\tvolume = v\n\tbranch = custom\n\trespectTreeRules = true\n\
             \tnested = working-tree\n\tcommitWindow = 50ms\n\tpushInterval = 5m\n\
             [agentfs \"identity\"]\n\tcommitterName = bot {replica}\n",
        );
        let s = Settings::from_config(&c).unwrap();
        assert_eq!(s.branch_ref, "refs/heads/custom");
        assert!(s.respect_tree_rules);
        assert_eq!(s.nested, Nested::WorkingTree);
        assert_eq!(s.commit_window, Duration::from_millis(50));
        assert_eq!(s.push_interval, Duration::from_secs(300));
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
