//! Published changes on a fork remote (design §11.5), read in the
//! receiver's repository with its own git and credentials, like `patch
//! submit` ([`crate::patch::submit`]): no state area is ever read.
//!
//! One `git ls-remote` of the forks' change, finish and history refs, one
//! `git fetch` of the finish and change refs (it writes no ref but
//! `FETCH_HEAD`), then, per fork, the checks of [`list`]. A change that
//! fails one is [`Skipped`] with why, never passed on. A listing is only
//! as trustworthy as the remote's writers: whoever may write
//! `refs/agentfs/*` there can publish a change that passes.

use std::collections::BTreeMap;

use git2::Oid;
use serde::Serialize;

use crate::config::validate_name;
use crate::history::finish_from_message;
use crate::patch::submit::{GitRunner, SubmitError, parse_ls_remote};

use super::identity::{PushTarget, change_ref, finish_ref};

/// A published change that passed every check of [`list`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PublishedChange {
    pub fork: String,
    pub source: String,
    pub base: String,
    pub change: String,
    pub finish: String,
    /// Where the fork's history is on the remote.
    pub history_ref: String,
}

/// A ref left out of a listing, and why.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Skipped {
    pub ref_name: String,
    pub reason: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Listing {
    pub changes: Vec<PublishedChange>,
    pub skipped: Vec<Skipped>,
}

#[derive(Debug, thiserror::Error)]
pub enum ListError {
    #[error(transparent)]
    Git(#[from] SubmitError),
    #[error("{0}")]
    Unreachable(String),
}

/// Every published change on `remote` (a name or URL in the receiver's
/// repository), optionally of one source. An entry is kept only if: the
/// fork name in the ref is a valid volume name; its finish commit's last
/// paragraph parses ([`finish_from_message`]) with `Agentfs-Fork` equal to
/// it; the change ref's oid equals `Agentfs-Change`; the change has
/// exactly one parent, equal to `Agentfs-Base`; and the change's tree
/// equals the finish commit's tree. Anything else is skipped with why.
pub fn list(
    git: &impl GitRunner,
    remote: &str,
    source: Option<&str>,
) -> Result<Listing, ListError> {
    collect(git, remote, None, source)
}

/// The published change of `fork` on `remote`, checked as [`list`] checks
/// it: `None` when there is none, or it fails a check.
pub fn read(
    git: &impl GitRunner,
    remote: &str,
    fork: &str,
) -> Result<Option<PublishedChange>, ListError> {
    if validate_name("fork", fork).is_err() {
        return Ok(None);
    }
    let listing = collect(git, remote, Some(fork), None)?;
    Ok(listing.changes.into_iter().find(|c| c.fork == fork))
}

/// One fork's refs on the remote.
#[derive(Default)]
struct Refs {
    change: Option<String>,
    finish: Option<String>,
}

fn collect(
    git: &impl GitRunner,
    remote: &str,
    only: Option<&str>,
    source: Option<&str>,
) -> Result<Listing, ListError> {
    let patterns: Vec<String> = match only {
        Some(fork) => vec![
            change_ref(fork),
            finish_ref(fork),
            PushTarget::Branch.history_ref(fork),
            PushTarget::Hidden.history_ref(fork),
        ],
        None => [
            "refs/agentfs/*/change",
            "refs/agentfs/*/finish",
            "refs/agentfs/*/history",
            "refs/heads/agentfs/*",
        ]
        .map(str::to_owned)
        .to_vec(),
    };
    let mut args = vec!["ls-remote", "--refs", remote];
    args.extend(patterns.iter().map(String::as_str));
    let out = git.run(&args, &[])?;
    if !out.success {
        return Err(ListError::Unreachable(format!(
            "git ls-remote {remote} failed: {}",
            out.stderr.trim()
        )));
    }
    let listed = parse_ls_remote(&out.stdout);
    let mut listing = Listing::default();
    let mut forks: BTreeMap<String, Refs> = BTreeMap::new();
    for (name, oid) in &listed {
        let Some((fork, kind)) = name
            .strip_prefix("refs/agentfs/")
            .and_then(|rest| rest.rsplit_once('/'))
        else {
            continue;
        };
        if kind != "change" && kind != "finish" {
            continue;
        }
        if validate_name("fork", fork).is_err() {
            listing.skipped.push(Skipped {
                ref_name: name.clone(),
                reason: format!("{fork:?} is not a valid volume name"),
            });
            continue;
        }
        let refs = forks.entry(fork.to_owned()).or_default();
        if kind == "change" {
            refs.change = Some(oid.clone());
        } else {
            refs.finish = Some(oid.clone());
        }
    }
    let wanted: Vec<String> = forks
        .iter()
        .flat_map(|(fork, refs)| {
            let change = refs.change.as_ref().map(|_| change_ref(fork));
            let finish = refs.finish.as_ref().map(|_| finish_ref(fork));
            change.into_iter().chain(finish)
        })
        .collect();
    if !wanted.is_empty() {
        let mut args = vec!["fetch", "--quiet", "--no-tags", remote];
        args.extend(wanted.iter().map(String::as_str));
        let out = git.run(&args, &[])?;
        if !out.success {
            return Err(ListError::Unreachable(format!(
                "git fetch {remote} failed: {}",
                out.stderr.trim()
            )));
        }
    }
    for (fork, refs) in forks {
        match check(git, &fork, &refs, &listed)? {
            Checked::Kept(change) => {
                if source.is_none_or(|s| s == change.source) {
                    listing.changes.push(change);
                }
            }
            Checked::Skipped(ref_name, reason) => {
                listing.skipped.push(Skipped { ref_name, reason });
            }
        }
    }
    Ok(listing)
}

enum Checked {
    Kept(PublishedChange),
    Skipped(String, String),
}

/// The checks of [`list`] for one fork.
fn check(
    git: &impl GitRunner,
    fork: &str,
    refs: &Refs,
    listed: &BTreeMap<String, String>,
) -> Result<Checked, ListError> {
    let change_name = change_ref(fork);
    let finish_name = finish_ref(fork);
    let skip = |name: &str, reason: String| Ok(Checked::Skipped(name.to_owned(), reason));
    let Some(finish) = &refs.finish else {
        return skip(&change_name, "a change ref with no finish ref".into());
    };
    let Some((finish_commit, finish_oid)) = read_commit(git, finish)? else {
        return skip(
            &finish_name,
            format!("the finish commit {finish} cannot be read"),
        );
    };
    let Some(record) = finish_from_message(&finish_commit.message, finish_oid) else {
        return skip(
            &finish_name,
            "the finish commit has no agentfs finish trailers in its last paragraph".into(),
        );
    };
    if record.fork != fork {
        return skip(
            &finish_name,
            format!(
                "the finish commit names fork {:?}, not {fork:?}",
                record.fork
            ),
        );
    }
    let Some(expected) = record.change else {
        return skip(
            &finish_name,
            "the fork finished without a change: nothing to receive".into(),
        );
    };
    let Some(change) = &refs.change else {
        return skip(
            &finish_name,
            format!("the finish commit names change {expected}, and there is no change ref"),
        );
    };
    if *change != expected.to_string() {
        return skip(
            &change_name,
            format!(
                "the change ref holds {change}, not {expected}, the finish commit's Agentfs-Change"
            ),
        );
    }
    let Some((change_commit, _)) = read_commit(git, change)? else {
        return skip(&change_name, format!("the change {change} cannot be read"));
    };
    let base = record.base.to_string();
    if change_commit.parents != [base.clone()] {
        return skip(
            &change_name,
            format!(
                "the change's parents are {:?}, not base {base} alone",
                change_commit.parents
            ),
        );
    }
    if change_commit.tree != finish_commit.tree {
        return skip(
            &change_name,
            format!(
                "the change's tree {} is not the finish commit's {}",
                change_commit.tree, finish_commit.tree
            ),
        );
    }
    // Where the history is: the history ref at the finish commit, else
    // whichever is listed, else the default branch.
    let candidates = [
        PushTarget::Branch.history_ref(fork),
        PushTarget::Hidden.history_ref(fork),
    ];
    let history_ref = candidates
        .iter()
        .find(|r| listed.get(*r) == Some(finish))
        .or_else(|| candidates.iter().find(|r| listed.contains_key(*r)))
        .unwrap_or(&candidates[0])
        .clone();
    Ok(Checked::Kept(PublishedChange {
        fork: fork.to_owned(),
        source: record.source,
        base,
        change: change.clone(),
        finish: finish.clone(),
        history_ref,
    }))
}

/// A commit as `git cat-file commit` shows it.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct RawCommit {
    pub tree: String,
    pub parents: Vec<String>,
    /// The `author` and `committer` headers' values: `Name <email> <time>
    /// <offset>`.
    pub author: Option<String>,
    pub committer: Option<String>,
    pub message: String,
}

/// `oid`'s commit, `None` when git cannot show it as one.
pub(super) fn read_commit(
    git: &impl GitRunner,
    oid: &str,
) -> Result<Option<(RawCommit, Oid)>, ListError> {
    let Ok(parsed) = Oid::from_str(oid) else {
        return Ok(None);
    };
    let out = git.run(&["cat-file", "commit", oid], &[])?;
    if !out.success {
        return Ok(None);
    }
    Ok(parse_commit(&out.stdout).map(|c| (c, parsed)))
}

/// The tree, parents and message of a raw commit. Continuation lines of
/// a header (a signature) start with a space and are not headers.
fn parse_commit(raw: &str) -> Option<RawCommit> {
    let (headers, message) = raw.split_once("\n\n").unwrap_or((raw, ""));
    let mut tree = None;
    let mut parents = Vec::new();
    let mut author = None;
    let mut committer = None;
    for line in headers.lines() {
        if let Some(t) = line.strip_prefix("tree ") {
            tree.get_or_insert_with(|| t.to_owned());
        } else if let Some(p) = line.strip_prefix("parent ") {
            parents.push(p.to_owned());
        } else if let Some(a) = line.strip_prefix("author ") {
            author.get_or_insert_with(|| a.to_owned());
        } else if let Some(c) = line.strip_prefix("committer ") {
            committer.get_or_insert_with(|| c.to_owned());
        }
    }
    Some(RawCommit {
        tree: tree?,
        parents,
        author,
        committer,
        message: message.to_owned(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_raw_commits() {
        let raw = "tree aaaa\nparent bbbb\nparent cccc\nauthor A <a@b> 1 +0000\n\
                   committer A <a@b> 1 +0000\ngpgsig -----BEGIN-----\n \n parent dddd\n \
                   -----END-----\n\nsubject\n\nbody\n";
        let c = parse_commit(raw).unwrap();
        assert_eq!(c.tree, "aaaa");
        assert_eq!(c.parents, ["bbbb", "cccc"]);
        assert_eq!(c.author.as_deref(), Some("A <a@b> 1 +0000"));
        assert_eq!(c.committer.as_deref(), Some("A <a@b> 1 +0000"));
        assert_eq!(c.message, "subject\n\nbody\n");
        assert_eq!(parse_commit("parent bbbb\n\nno tree\n"), None);
    }
}
