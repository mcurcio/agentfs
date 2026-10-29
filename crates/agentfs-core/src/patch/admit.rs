//! Who may push what to `agentfs serve-git` (design §12, §19.2 item 11;
//! bead agentfs-mia.9).
//!
//! `serve-git` makes the state area's history repository a git server for
//! patch pushes. Its pushers are the operator's, in the volume's git
//! config:
//!
//! ```text
//! [agentfsPusher "iac"]
//!     tokenFile = /run/secrets/iac-token
//!     right = offer
//!     right = approve
//! ```
//!
//! - [`pushers`] reads them; [`authenticate`] finds the pusher a request's
//!   `Authorization` header names (HTTP Basic with the pusher's name and
//!   token, or a bearer token), comparing tokens in constant time against
//!   each token file's trimmed contents, read at every request.
//! - [`admit`] judges a whole push before git keeps any of it (the
//!   `pre-receive` hook, objects still quarantined): every update must be
//!   allowed, else nothing is. Pushers write only the queue's inbox and
//!   approvals: `inbox/<n>` with [`Right::Offer`] (and [`Right::Barrier`]
//!   for a tip carrying `Agentfs-Barrier: yes`), `approved/<n>` with
//!   [`Right::Approve`]. The branch, `applied/*`, the status, deletes and
//!   every other ref are refused. An offered tip must also read as a patch
//!   against the branch head ([`PatchBranch::read`]), so nothing the queue
//!   could never land is kept.
//! - [`Right::Integrator`] is checked by `serve-git` itself: it may ask the
//!   volume to apply its queue.

use std::collections::BTreeSet;
use std::path::PathBuf;

use base64::Engine as _;
use git2::{Oid, Repository};
use serde::{Deserialize, Serialize};

use super::integrate::PatchBranch;
use super::{approved_prefix, inbox_prefix, seq_of_ref};
use crate::config::{self, ConfigError};

/// The config section of a pusher.
pub const PUSHER_SECTION: &str = "agentfsPusher";

/// What a pusher may do (`agentfsPusher.<name>.right`, multi-valued).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Right {
    /// Create or replace `inbox/<n>`.
    Offer,
    /// Create or move `approved/<n>`.
    Approve,
    /// Offer a barrier patch (with [`Right::Offer`]).
    Barrier,
    /// Ask the volume to apply its queue (`POST /agentfs/apply`).
    Integrator,
}

impl Right {
    /// The config spelling.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Offer => "offer",
            Self::Approve => "approve",
            Self::Barrier => "barrier",
            Self::Integrator => "integrator",
        }
    }

    fn parse(value: &str) -> Option<Self> {
        [Self::Offer, Self::Approve, Self::Barrier, Self::Integrator]
            .into_iter()
            .find(|r| r.as_str() == value.trim())
    }
}

/// One `[agentfsPusher "<name>"]` section.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pusher {
    pub name: String,
    pub rights: BTreeSet<Right>,
    /// The file holding its token (absolute).
    pub token_file: PathBuf,
}

impl Pusher {
    /// Whether `given` is this pusher's token: the token file's trimmed
    /// contents, read now. An unreadable or empty token file matches
    /// nothing.
    pub fn token_matches(&self, given: &str) -> bool {
        let Ok(expected) = std::fs::read_to_string(&self.token_file) else {
            return false;
        };
        let expected = expected.trim();
        !expected.is_empty() && tokens_match(given.as_bytes(), expected.as_bytes())
    }
}

/// `[agentfsPusher "<name>"]` sections of the volume's config, sorted by
/// name. A section needs `tokenFile` (an absolute path); each `right` is
/// one of `offer`, `approve`, `barrier`, `integrator`.
pub fn pushers(config: &git2::Config) -> Result<Vec<Pusher>, ConfigError> {
    let mut names = config::subsections(config, PUSHER_SECTION, "tokenFile")?;
    names.extend(config::subsections(config, PUSHER_SECTION, "right")?);
    let mut out = Vec::new();
    for name in names {
        config::validate_name(&format!("{PUSHER_SECTION}.<name>"), &name)?;
        let token_file =
            config::absolute_path(config, &format!("{PUSHER_SECTION}.{name}.tokenFile"))?
                .ok_or_else(|| ConfigError::PusherNeedsToken { name: name.clone() })?;
        let key = format!("{PUSHER_SECTION}.{name}.right");
        let mut rights = BTreeSet::new();
        for value in config::strings(config, &key)? {
            let right = Right::parse(&value).ok_or_else(|| ConfigError::BadChoice {
                key: key.clone(),
                value: value.clone(),
                allowed: "offer, approve, barrier, integrator",
            })?;
            rights.insert(right);
        }
        out.push(Pusher {
            name,
            rights,
            token_file,
        });
    }
    Ok(out)
}

/// Whether two tokens are equal, in time that depends only on their
/// lengths.
pub fn tokens_match(given: &[u8], expected: &[u8]) -> bool {
    if given.len() != expected.len() {
        return false;
    }
    given
        .iter()
        .zip(expected)
        .fold(0u8, |diff, (a, b)| diff | (a ^ b))
        == 0
}

/// The pusher an `Authorization` header authenticates: `Basic` with the
/// pusher's name and token, or `Bearer <token>` (any pusher's). `None`
/// for no header, another scheme, or a wrong name or token.
pub fn authenticate<'p>(pushers: &'p [Pusher], authorization: Option<&str>) -> Option<&'p Pusher> {
    let (scheme, value) = authorization?.trim().split_once(' ')?;
    let value = value.trim();
    if scheme.eq_ignore_ascii_case("basic") {
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(value)
            .ok()?;
        let decoded = String::from_utf8(decoded).ok()?;
        let (user, token) = decoded.split_once(':')?;
        pushers
            .iter()
            .find(|p| p.name == user)
            .filter(|p| p.token_matches(token))
    } else if scheme.eq_ignore_ascii_case("bearer") {
        // Every pusher is compared, so the time does not say which matched.
        pushers
            .iter()
            .fold(None, |found, p| match p.token_matches(value) {
                true => found.or(Some(p)),
                false => found,
            })
    } else {
        None
    }
}

/// One line of a `pre-receive` hook's input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefUpdate {
    pub old: Oid,
    pub new: Oid,
    pub name: String,
}

impl RefUpdate {
    /// `<old> <new> <ref>`, as git gives it to `pre-receive`.
    pub fn parse(line: &str) -> Option<Self> {
        let mut words = line.split_whitespace();
        let old = Oid::from_str(words.next()?).ok()?;
        let new = Oid::from_str(words.next()?).ok()?;
        let name = words.next()?.to_owned();
        words.next().is_none().then_some(Self { old, new, name })
    }
}

/// Why a push was refused (all of it: nothing is kept).
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum PushRefused {
    #[error("{pusher} may not {what} ({right:?} right needed)")]
    NotAllowed {
        pusher: String,
        what: String,
        right: Right,
    },
    #[error("{name} is not a ref pushers write")]
    NotAPatchRef { name: String },
    #[error("patch {inbox}: {message}")]
    NotAPatch { inbox: u64, message: String },
    #[error("deleting {name} is not allowed")]
    Delete { name: String },
}

/// Judges a whole push (all or nothing; see the module docs). `repo` sees
/// the quarantined objects (`GIT_OBJECT_DIRECTORY` and
/// `GIT_ALTERNATE_OBJECT_DIRECTORIES` from receive-pack); `branch_ref` is
/// the volume's branch, whose head offered patches are read against.
pub fn admit(
    repo: &Repository,
    volume: &str,
    branch_ref: &str,
    pusher: &Pusher,
    updates: &[RefUpdate],
) -> Result<(), PushRefused> {
    let inbox = inbox_prefix(volume);
    let approved = approved_prefix(volume);
    let need = |right: Right, what: String| {
        if pusher.rights.contains(&right) {
            Ok(())
        } else {
            Err(PushRefused::NotAllowed {
                pusher: pusher.name.clone(),
                what,
                right,
            })
        }
    };
    for update in updates {
        if update.new.is_zero() {
            return Err(PushRefused::Delete {
                name: update.name.clone(),
            });
        }
        if let Some(n) = seq_of_ref(&inbox, &update.name) {
            need(Right::Offer, format!("offer patch {n}"))?;
            let head = repo
                .refname_to_id(branch_ref)
                .map_err(|_| PushRefused::NotAPatch {
                    inbox: n,
                    message: format!("{branch_ref} has no commits yet; nothing to patch"),
                })?;
            let patch =
                PatchBranch::read(repo, head, update.new).map_err(|r| PushRefused::NotAPatch {
                    inbox: n,
                    message: format!("{}: {}", r.reason, r.message),
                })?;
            if patch.barrier {
                need(Right::Barrier, format!("offer barrier patch {n}"))?;
            }
        } else if let Some(n) = seq_of_ref(&approved, &update.name) {
            need(Right::Approve, format!("approve patch {n}"))?;
        } else {
            return Err(PushRefused::NotAPatchRef {
                name: update.name.clone(),
            });
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::patch::{applied_ref, approved_ref, inbox_ref, status_ref};

    const BRANCH: &str = "refs/heads/agentfs/v";

    fn pusher(rights: &[Right]) -> Pusher {
        Pusher {
            name: "iac".into(),
            rights: rights.iter().copied().collect(),
            token_file: "/nonexistent".into(),
        }
    }

    struct Repo {
        _dir: tempfile::TempDir,
        repo: Repository,
    }

    impl Repo {
        fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let repo = Repository::init_bare(dir.path()).unwrap();
            let r = Self { _dir: dir, repo };
            let head = r.commit(&[], "a.txt", "a0\n", "base");
            r.repo.reference(BRANCH, head, true, "test").unwrap();
            r
        }

        fn head(&self) -> Oid {
            self.repo.refname_to_id(BRANCH).unwrap()
        }

        fn commit(&self, parents: &[Oid], path: &str, content: &str, message: &str) -> Oid {
            let blob = self.repo.blob(content.as_bytes()).unwrap();
            let mut tree = self.repo.treebuilder(None).unwrap();
            tree.insert(path, blob, 0o100644).unwrap();
            let tree = self.repo.find_tree(tree.write().unwrap()).unwrap();
            let sig = git2::Signature::now("t", "t@e").unwrap();
            let parents: Vec<_> = parents
                .iter()
                .map(|p| self.repo.find_commit(*p).unwrap())
                .collect();
            let parents: Vec<_> = parents.iter().collect();
            self.repo
                .commit(None, &sig, &sig, message, &tree, &parents)
                .unwrap()
        }

        fn admit(&self, rights: &[Right], updates: &[(&str, Oid)]) -> Result<(), PushRefused> {
            let updates: Vec<RefUpdate> = updates
                .iter()
                .map(|(name, new)| RefUpdate {
                    old: Oid::ZERO_SHA1,
                    new: *new,
                    name: (*name).to_owned(),
                })
                .collect();
            admit(&self.repo, "v", BRANCH, &pusher(rights), &updates)
        }
    }

    #[test]
    fn offers_and_approvals_need_their_rights() {
        let r = Repo::new();
        let patch = r.commit(&[r.head()], "a.txt", "a1\n", "one");
        let inbox = inbox_ref("v", 1);
        let approved = approved_ref("v", 1);
        assert_eq!(r.admit(&[Right::Offer], &[(&inbox, patch)]), Ok(()));
        assert_eq!(
            r.admit(&[Right::Approve], &[(&inbox, patch)]),
            Err(PushRefused::NotAllowed {
                pusher: "iac".into(),
                what: "offer patch 1".into(),
                right: Right::Offer,
            })
        );
        assert_eq!(r.admit(&[Right::Approve], &[(&approved, patch)]), Ok(()));
        assert!(matches!(
            r.admit(&[Right::Offer], &[(&approved, patch)]),
            Err(PushRefused::NotAllowed {
                right: Right::Approve,
                ..
            })
        ));
    }

    /// Mutation probe P1: a push is judged whole; one refused update
    /// refuses every other.
    #[test]
    fn one_refused_update_refuses_the_push() {
        let r = Repo::new();
        let patch = r.commit(&[r.head()], "a.txt", "a1\n", "one");
        let refused = r.admit(
            &[Right::Offer],
            &[(&inbox_ref("v", 1), patch), (&approved_ref("v", 1), patch)],
        );
        assert!(
            matches!(
                refused,
                Err(PushRefused::NotAllowed {
                    right: Right::Approve,
                    ..
                })
            ),
            "{refused:?}"
        );
    }

    #[test]
    fn barriers_need_the_barrier_right() {
        let r = Repo::new();
        let barrier = r.commit(
            &[r.head()],
            "a.txt",
            "a1\n",
            "one\n\nAgentfs-Barrier: yes\n",
        );
        let inbox = inbox_ref("v", 1);
        assert!(matches!(
            r.admit(&[Right::Offer], &[(&inbox, barrier)]),
            Err(PushRefused::NotAllowed {
                right: Right::Barrier,
                ..
            })
        ));
        assert_eq!(
            r.admit(&[Right::Offer, Right::Barrier], &[(&inbox, barrier)]),
            Ok(())
        );
    }

    #[test]
    fn only_inbox_and_approvals_are_written() {
        let r = Repo::new();
        let all = [
            Right::Offer,
            Right::Approve,
            Right::Barrier,
            Right::Integrator,
        ];
        let patch = r.commit(&[r.head()], "a.txt", "a1\n", "one");
        for name in [
            BRANCH.to_owned(),
            applied_ref("v", 1),
            status_ref("v"),
            "refs/agentfs/v/held/0000000001".to_owned(),
            inbox_ref("other", 1),
            "refs/agentfs/v/inbox/1".to_owned(),
            "refs/heads/main".to_owned(),
        ] {
            assert_eq!(
                r.admit(&all, &[(&name, patch)]),
                Err(PushRefused::NotAPatchRef { name: name.clone() }),
                "{name}"
            );
        }
        let inbox = inbox_ref("v", 1);
        assert_eq!(
            r.admit(&all, &[(&inbox, Oid::ZERO_SHA1)]),
            Err(PushRefused::Delete { name: inbox })
        );
    }

    #[test]
    fn an_unlandable_tip_is_refused() {
        let r = Repo::new();
        let root = r.commit(&[], "b.txt", "b\n", "unrelated");
        let orphan = r.commit(&[root], "b.txt", "b1\n", "on it");
        match r.admit(&[Right::Offer], &[(&inbox_ref("v", 1), orphan)]) {
            Err(PushRefused::NotAPatch { inbox: 1, message }) => {
                assert!(message.starts_with("parent_not_in_history: "), "{message}")
            }
            other => panic!("{other:?}"),
        }
    }

    /// Mutation probe P3: a prefix of the token is not the token.
    #[test]
    fn tokens_compare_whole() {
        assert!(tokens_match(b"secret", b"secret"));
        assert!(!tokens_match(b"secre", b"secret"));
        assert!(!tokens_match(b"secret", b"secre"));
        assert!(!tokens_match(b"", b"secret"));
        assert!(!tokens_match(b"secreT", b"secret"));
    }

    #[test]
    fn requests_authenticate_by_name_and_token() {
        let dir = tempfile::tempdir().unwrap();
        let token = |name: &str, value: &str| {
            let path = dir.path().join(name);
            std::fs::write(&path, value).unwrap();
            path
        };
        let pushers = vec![
            Pusher {
                name: "iac".into(),
                rights: BTreeSet::new(),
                token_file: token("iac", "s3cret\n"),
            },
            Pusher {
                name: "ci".into(),
                rights: BTreeSet::new(),
                token_file: token("ci", "other"),
            },
            Pusher {
                name: "empty".into(),
                rights: BTreeSet::new(),
                token_file: token("empty", "\n"),
            },
        ];
        let basic = |s: &str| {
            format!(
                "Basic {}",
                base64::engine::general_purpose::STANDARD.encode(s)
            )
        };
        let who = |h: Option<&str>| authenticate(&pushers, h).map(|p| p.name.as_str());
        assert_eq!(who(Some(&basic("iac:s3cret"))), Some("iac"));
        assert_eq!(who(Some(&basic("ci:s3cret"))), None);
        assert_eq!(who(Some(&basic("iac:s3cre"))), None);
        assert_eq!(who(Some(&basic("nobody:s3cret"))), None);
        assert_eq!(who(Some(&basic("empty:"))), None);
        assert_eq!(who(Some("Bearer other")), Some("ci"));
        assert_eq!(who(Some("bearer s3cret")), Some("iac"));
        assert_eq!(who(Some("Bearer ")), None);
        assert_eq!(who(Some("Digest x")), None);
        assert_eq!(who(None), None);
    }

    #[test]
    fn pushers_are_read_from_config() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config");
        let load = |text: &str| {
            std::fs::write(&path, text).unwrap();
            pushers(&git2::Config::open(&path).unwrap())
        };
        let found = load(
            "[agentfsPusher \"iac\"]\n\ttokenFile = /run/iac\n\tright = offer\n\tright = \
             approve\n[agentfsPusher \"reader\"]\n\ttokenFile = /run/r\n",
        )
        .unwrap();
        assert_eq!(
            found,
            [
                Pusher {
                    name: "iac".into(),
                    rights: [Right::Offer, Right::Approve].into_iter().collect(),
                    token_file: "/run/iac".into(),
                },
                Pusher {
                    name: "reader".into(),
                    rights: BTreeSet::new(),
                    token_file: "/run/r".into(),
                },
            ]
        );
        assert!(matches!(
            load("[agentfsPusher \"x\"]\n\tright = offer\n"),
            Err(ConfigError::PusherNeedsToken { .. })
        ));
        assert!(matches!(
            load("[agentfsPusher \"x\"]\n\ttokenFile = rel\n"),
            Err(ConfigError::RelativePath { .. })
        ));
        assert!(matches!(
            load("[agentfsPusher \"x\"]\n\ttokenFile = /t\n\tright = land\n"),
            Err(ConfigError::BadChoice { .. })
        ));
        assert_eq!(load("[agentfs]\n\tvolume = v\n").unwrap(), []);
    }

    #[test]
    fn hook_lines_parse() {
        let a = "1".repeat(40);
        let z = "0".repeat(40);
        assert_eq!(
            RefUpdate::parse(&format!("{z} {a} refs/x")),
            Some(RefUpdate {
                old: Oid::ZERO_SHA1,
                new: Oid::from_str(&a).unwrap(),
                name: "refs/x".into(),
            })
        );
        assert_eq!(RefUpdate::parse("nonsense"), None);
        assert_eq!(RefUpdate::parse(&format!("{z} {a} refs/x extra")), None);
    }
}
