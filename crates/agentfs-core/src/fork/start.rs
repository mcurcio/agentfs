//! Starting a volume from a remote branch (F8 S1; R2: continuing a
//! volume elsewhere is a new volume started from the old branch).
//!
//! [`start_from`] makes a new worktree whose history begins at a pinned
//! commit of another worktree's branch: it fetches that branch once from
//! the start remote (a push section of the new volume's own profile, with
//! its credentials), checks that the pinned commit is the branch's tip or
//! an ancestor of it, writes the identity (`agentfs.volume`, its own
//! branch, and the three `agentfs.start.*` keys) into the new volume's own
//! config, points the new branch at the commit, and writes the commit's
//! tree into the backing tree through the same confined writer as
//! [`super::create::create`]. LFS objects the tree references are
//! downloaded from the start remote's LFS endpoint. It writes nothing to
//! any remote, and never the start branch, which belongs to another
//! worktree (rule 1). Idempotent per name: a retry (CreateVolume retried)
//! resumes, and a volume that exists keeps the start point it was made
//! with.
//!
//! [`accept`] is the operator's answer to the start-branch alarm
//! ([`crate::push::StartBranchMoved`]): it saves an old-branch tip under
//! this volume's own `refs/agentfs/<volume>/accepted/<tip>`, after which
//! the alarm stops watching every commit that tip reaches. It takes no
//! state lock (it creates one ref and adds objects, both atomic in git),
//! so it runs beside a running history service.

use std::io;
use std::path::{Path, PathBuf};

use git2::{AutotagOption, ErrorCode, FetchOptions, FetchPrune, Oid, Repository};

use crate::config::{ConfigError, RemoteSettings, Settings};
use crate::events::PushFailureKind;
use crate::history::HistoryError;
use crate::lfs::fetch::Fetcher;
use crate::push::{classify, scrub_userinfo};
use crate::remote_auth::{AuthState, RemoteAuth};
use crate::restore::RestoreError;
use crate::volume::{self, StateDir, VolumeError};

use super::identity::{StartPoint, accepted_ref};

/// What to start.
#[derive(Debug, Clone, Copy)]
pub struct StartRequest<'a> {
    /// The new worktree's state area: new, or this volume's from an
    /// earlier try.
    pub state: &'a StateDir,
    /// The new worktree's backing tree: empty, or this volume's from an
    /// earlier try.
    pub tree: &'a Path,
    /// The new volume's name, unique per PV (`{pvName}`, rule 1).
    pub volume: &'a str,
    /// The new volume's `include.path` entries: its profile's config,
    /// which names `start.remote`'s section and credentials.
    pub includes: &'a [PathBuf],
    pub start: &'a StartPoint,
}

#[derive(Debug, thiserror::Error)]
pub enum StartError {
    /// `start.remote` is not a valid push section of the new volume's
    /// profile (CreateVolume: `InvalidArgument`).
    #[error(
        "startFrom names remote {remote:?}, which is not a push remote of the volume's profile"
    )]
    NoSuchRemote { remote: String },
    /// The profile declares no remote (F8 S3): there is nothing to start
    /// from (`InvalidArgument`).
    #[error(
        "the volume's profile is local-only (agentfs.remotes = none): it has no remote to start from"
    )]
    LocalOnly,
    /// The start branch is the new volume's own branch (rule 1;
    /// `InvalidArgument`).
    #[error("startFrom names branch {branch:?}, which is this volume's own branch")]
    OwnBranch { branch: String },
    /// The remote's branch does not reach the pinned commit
    /// (`FailedPrecondition`).
    #[error("commit {commit} is not on {branch} at the start remote")]
    NotReachable { commit: Oid, branch: String },
    /// The start remote could not be fetched (`Unavailable`: the CO
    /// retries).
    #[error("fetching from remote {remote}: {message}")]
    Unreachable { remote: String, message: String },
    /// The start branch does not exist on the remote
    /// (`FailedPrecondition`).
    #[error("remote {remote} has no branch {branch}")]
    NoSuchBranch { remote: String, branch: String },
    /// An LFS object of the pinned tree could not be downloaded
    /// (`Unavailable`).
    #[error("LFS object of {path} could not be downloaded from the start remote: {message}")]
    LfsObjectMissing { path: String, message: String },
    /// The state area belongs to another volume or start point
    /// (`AlreadyExists`).
    #[error("the state area already belongs to {found}; nothing was changed")]
    Exists { found: String },
    #[error("reading the volume's settings: {0}")]
    Settings(ConfigError),
    #[error(transparent)]
    Volume(#[from] VolumeError),
    #[error(transparent)]
    History(#[from] HistoryError),
    #[error(transparent)]
    Io(#[from] io::Error),
}

/// Starts the volume `request` names, or returns the start point an
/// earlier call created it with (see the module docs).
pub fn start_from(request: &StartRequest<'_>) -> Result<StartPoint, StartError> {
    let start = request.start;
    if start.branch == format!("agentfs/{}", request.volume) {
        return Err(StartError::OwnBranch {
            branch: start.branch.clone(),
        });
    }
    volume::init_started(request.state, request.volume, start, request.includes).map_err(|e| {
        match e {
            VolumeError::OtherVolume { found, .. }
            | VolumeError::OtherStart { found, .. }
            | VolumeError::OtherFork { found, .. } => StartError::Exists { found },
            e => StartError::Volume(e),
        }
    })?;
    let settings = Settings::load(&request.state.git()).map_err(StartError::Settings)?;
    let remote = settings
        .remotes
        .iter()
        .find(|r| r.name == start.remote)
        .ok_or_else(|| StartError::NoSuchRemote {
            remote: start.remote.clone(),
        })?;
    let repo = Repository::open_bare(request.state.git()).map_err(HistoryError::from)?;
    let branch_ref = settings.branch_ref.as_str();
    match repo.refname_to_id(branch_ref) {
        // The volume has recorded since it was created: its tree is its
        // own, and so is everything else.
        Ok(head) if head != start.commit => return Ok(start.clone()),
        // An earlier try pointed the branch at the commit.
        Ok(_) => {}
        Err(e) if e.code() == ErrorCode::NotFound => {
            let refname = start.refname();
            let tip = fetch_branch(&repo, remote, &refname)
                .map_err(|(_, message)| StartError::Unreachable {
                    remote: remote.name.clone(),
                    message,
                })?
                .ok_or_else(|| StartError::NoSuchBranch {
                    remote: remote.name.clone(),
                    branch: start.branch.clone(),
                })?;
            if !reaches(&repo, tip, start.commit) {
                return Err(StartError::NotReachable {
                    commit: start.commit,
                    branch: start.branch.clone(),
                });
            }
            repo.reference(
                branch_ref,
                start.commit,
                false,
                &format!("agentfs: started from {start}"),
            )
            .map_err(HistoryError::from)?;
            super::create::sync_parents(&request.state.git(), branch_ref)?;
        }
        Err(e) => return Err(HistoryError::from(e).into()),
    }
    // LFS content of the pinned tree from the start remote's endpoint.
    let mut from_start = settings.clone();
    from_start.remotes.retain(|r| r.name == start.remote);
    let objects = Fetcher::new(request.state, &from_start)?;
    super::create::materialize(&repo, branch_ref, start.commit, request.tree, &objects).map_err(
        |e| match e {
            RestoreError::LfsObject { path, error, .. } => StartError::LfsObjectMissing {
                path,
                message: error,
            },
            RestoreError::Git(e) => StartError::History(e.into()),
            RestoreError::Io(e) => StartError::Io(e),
            e => StartError::Io(io::Error::other(format!(
                "writing {}'s tree: {e}",
                start.commit
            ))),
        },
    )?;
    Ok(start.clone())
}

/// Whether `commit` is in `repo` and is `tip` or an ancestor of it.
fn reaches(repo: &Repository, tip: Oid, commit: Oid) -> bool {
    repo.odb().is_ok_and(|odb| odb.exists(commit))
        && (commit == tip || repo.graph_descendant_of(tip, commit).unwrap_or(false))
}

/// Fetches `refname` from `remote` into `repo`, with the remote's
/// authentication and proxy (its push URL, as every agentfs connection to
/// it), writing no ref and no `FETCH_HEAD`: the objects only. The tip the
/// remote advertised, `None` when it has no such ref. A failure is the
/// remote's ([`PushFailureKind`] and a message scrubbed of userinfo).
fn fetch_branch(
    repo: &Repository,
    remote: &RemoteSettings,
    refname: &str,
) -> Result<Option<Oid>, (PushFailureKind, String)> {
    crate::history::init_libgit2();
    let url = remote.push_url();
    let auth = RemoteAuth::new(remote);
    let state = AuthState::new();
    let _scope = auth.ssh_scope(&state);
    let failed = |e: git2::Error| {
        let message = scrub_userinfo(e.message(), url);
        if let Some(why) = state.host_key_failure() {
            return (PushFailureKind::UntrustedHost, why);
        }
        let kind = if state.auth_failed() {
            PushFailureKind::Auth
        } else {
            match classify(&e, false) {
                PushFailureKind::Refused | PushFailureKind::NonFastForward => {
                    PushFailureKind::Unreachable
                }
                kind => kind,
            }
        };
        (kind, message)
    };
    let mut git_remote = repo.remote_anonymous(url).map_err(failed)?;
    let fetched = {
        let mut options = FetchOptions::new();
        options
            .remote_callbacks(auth.callbacks(&state))
            .proxy_options(crate::proxy::git_options(remote.proxy.as_ref(), url))
            .prune(FetchPrune::Off)
            .update_fetchhead(false)
            .download_tags(AutotagOption::None);
        git_remote.fetch(&[refname], Some(&mut options), None)
    };
    fetched.map_err(failed)?;
    // The advertisement stays readable after the fetch disconnects.
    let tip = git_remote
        .list()
        .map_err(failed)?
        .iter()
        .find(|head| head.name() == refname)
        .map(|head| head.oid());
    Ok(tip)
}

#[derive(Debug, thiserror::Error)]
pub enum AcceptError {
    /// The volume has no start point.
    #[error("volume {volume} was not started from another branch: there is nothing to accept")]
    NotStarted { volume: String },
    #[error("the start remote {remote} is not a push remote of this volume")]
    NoSuchRemote { remote: String },
    #[error("fetching {branch} from remote {remote}: {message}")]
    Unreachable {
        remote: String,
        branch: String,
        message: String,
    },
    /// `tip` is not the start branch's tip or an ancestor of it on the
    /// remote: only commits of the start branch are accepted.
    #[error("commit {tip} is not on {branch} at remote {remote}")]
    NotOnStartBranch {
        tip: Oid,
        remote: String,
        branch: String,
    },
    #[error("reading the volume's settings: {0}")]
    Settings(ConfigError),
    #[error(transparent)]
    History(#[from] HistoryError),
    #[error(transparent)]
    Io(#[from] io::Error),
}

/// Saves `tip`, a commit of this volume's start branch, as
/// `refs/agentfs/<volume>/accepted/<tip>` (created, never moved; a second
/// accept of the same tip is a no-op), fetching its objects from the
/// start remote first. Returns the ref's name. The next push to the start
/// remote pushes it create-only with the branch and no longer raises the
/// alarm for commits `tip` reaches.
pub fn accept(state: &StateDir, tip: Oid) -> Result<String, AcceptError> {
    crate::history::init_libgit2();
    let settings = Settings::load(&state.git()).map_err(AcceptError::Settings)?;
    let Some(start) = &settings.start else {
        return Err(AcceptError::NotStarted {
            volume: settings.volume.clone(),
        });
    };
    let remote = settings
        .remotes
        .iter()
        .find(|r| r.name == start.remote)
        .ok_or_else(|| AcceptError::NoSuchRemote {
            remote: start.remote.clone(),
        })?;
    let repo = Repository::open_bare(state.git()).map_err(HistoryError::from)?;
    let name = accepted_ref(&settings.volume, tip);
    // Accepted refs never move: one that exists is this tip's.
    if repo.find_reference(&name).is_ok() {
        return Ok(name);
    }
    let not_on_branch = || AcceptError::NotOnStartBranch {
        tip,
        remote: remote.name.clone(),
        branch: start.branch.clone(),
    };
    let branch_tip = fetch_branch(&repo, remote, &start.refname())
        .map_err(|(_, message)| AcceptError::Unreachable {
            remote: remote.name.clone(),
            branch: start.branch.clone(),
            message,
        })?
        .ok_or_else(not_on_branch)?;
    if !reaches(&repo, branch_tip, tip) {
        return Err(not_on_branch());
    }
    match repo.reference(&name, tip, false, "agentfs start accept") {
        Ok(_) => {}
        // Another accept of the same tip got there first.
        Err(e) if e.code() == ErrorCode::Exists => {}
        Err(e) => return Err(HistoryError::from(e).into()),
    }
    super::create::sync_parents(&state.git(), &name)?;
    Ok(name)
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    /// A bare `file://` remote that another worktree's branch is written
    /// to, and the profile of the volumes started from it.
    struct World {
        dir: tempfile::TempDir,
        remote: Repository,
    }

    impl World {
        fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let remote = Repository::init_bare(dir.path().join("remote.git")).unwrap();
            let url = format!("file://{}", dir.path().join("remote.git").display());
            let profile = format!("[remote \"origin\"]\n\turl = {url}\n");
            fs::write(dir.path().join("profile.config"), profile).unwrap();
            Self { dir, remote }
        }

        /// A commit on `refname` on the remote holding `files`, on top of
        /// its tip.
        fn commit(&self, refname: &str, files: &[(&str, &str)]) -> Oid {
            let parent = self
                .remote
                .refname_to_id(refname)
                .ok()
                .map(|id| self.remote.find_commit(id).unwrap());
            let mut builder = self
                .remote
                .treebuilder(parent.as_ref().map(|p| p.tree().unwrap()).as_ref())
                .unwrap();
            for (name, content) in files {
                let blob = self.remote.blob(content.as_bytes()).unwrap();
                builder.insert(name, blob, 0o100644).unwrap();
            }
            let tree = self.remote.find_tree(builder.write().unwrap()).unwrap();
            let sig = git2::Signature::now("v1", "v1@example.invalid").unwrap();
            let parents: Vec<&git2::Commit<'_>> = parent.iter().collect();
            self.remote
                .commit(Some(refname), &sig, &sig, "old node", &tree, &parents)
                .unwrap()
        }

        fn state(&self, volume: &str) -> StateDir {
            StateDir::new(self.dir.path().join(volume).join("state"))
        }

        fn tree(&self, volume: &str) -> PathBuf {
            self.dir.path().join(volume).join("tree")
        }

        fn start(&self, volume: &str, value: &str) -> Result<StartPoint, StartError> {
            let start = StartPoint::parse(value).unwrap();
            let includes = [self.dir.path().join("profile.config")];
            start_from(&StartRequest {
                state: &self.state(volume),
                tree: &self.tree(volume),
                volume,
                includes: &includes,
                start: &start,
            })
        }
    }

    const BRANCH: &str = "refs/heads/agentfs/v1";

    #[test]
    fn a_volume_starts_from_a_pinned_commit_of_another_branch() {
        let w = World::new();
        let f = w.commit(BRANCH, &[("f", "f")]);
        let g = w.commit(BRANCH, &[("g", "g")]);
        let value = format!("origin:agentfs/v1@{f}");
        let start = w.start("v2", &value).unwrap();
        assert_eq!(start.to_string(), value);
        let repo = Repository::open_bare(w.state("v2").git()).unwrap();
        assert_eq!(repo.refname_to_id("refs/heads/agentfs/v2").unwrap(), f);
        assert!(
            repo.find_reference(BRANCH).is_err(),
            "the start branch is not copied"
        );
        assert_eq!(fs::read_to_string(w.tree("v2").join("f")).unwrap(), "f");
        assert!(!w.tree("v2").join("g").exists(), "g is after the pin");
        let settings = Settings::load(&w.state("v2").git()).unwrap();
        assert_eq!(settings.start, Some(start.clone()));
        // A retry (CreateVolume retried) is the same volume.
        assert_eq!(w.start("v2", &value).unwrap(), start);
        // Nothing was written to the remote.
        assert_eq!(w.remote.refname_to_id(BRANCH).unwrap(), g);
        assert!(w.remote.find_reference("refs/heads/agentfs/v2").is_err());
        // The same name with another start point is another volume.
        let err = w
            .start("v2", &format!("origin:agentfs/v1@{g}"))
            .unwrap_err();
        assert!(
            matches!(&err, StartError::Exists { found } if found.contains(&f.to_string())),
            "{err:?}"
        );
    }

    #[test]
    fn what_start_from_refuses() {
        let w = World::new();
        let f = w.commit(BRANCH, &[("f", "f")]);
        let elsewhere = w.commit("refs/heads/other", &[("x", "x")]);
        assert!(matches!(
            w.start("v1", &format!("origin:agentfs/v1@{f}")),
            Err(StartError::OwnBranch { .. })
        ));
        assert!(!w.state("v1").root().exists(), "OwnBranch creates nothing");
        for (volume, commit) in [("v3", elsewhere.to_string()), ("v4", "1".repeat(40))] {
            let err = w
                .start(volume, &format!("origin:agentfs/v1@{commit}"))
                .unwrap_err();
            assert!(matches!(err, StartError::NotReachable { .. }), "{err:?}");
        }
        assert!(matches!(
            w.start("v5", &format!("origin:agentfs/none@{f}")),
            Err(StartError::NoSuchBranch { .. })
        ));
        assert!(matches!(
            w.start("v6", &format!("backup:agentfs/v1@{f}")),
            Err(StartError::NoSuchRemote { remote }) if remote == "backup"
        ));
        fs::write(
            w.dir.path().join("profile.config"),
            format!(
                "[remote \"origin\"]\n\turl = {}/nope.git\n",
                w.dir.path().display()
            ),
        )
        .unwrap();
        assert!(matches!(
            w.start("v7", &format!("origin:agentfs/v1@{f}")),
            Err(StartError::Unreachable { .. })
        ));
    }

    #[test]
    fn accepting_a_tip_of_the_start_branch() {
        let w = World::new();
        let f = w.commit(BRANCH, &[("f", "f")]);
        w.start("v2", &format!("origin:agentfs/v1@{f}")).unwrap();
        let g = w.commit(BRANCH, &[("g", "g")]);
        let state = w.state("v2");
        let name = accept(&state, g).unwrap();
        assert_eq!(name, accepted_ref("v2", g));
        let repo = Repository::open_bare(state.git()).unwrap();
        assert_eq!(repo.refname_to_id(&name).unwrap(), g);
        // Created once: a second accept is a no-op.
        assert_eq!(accept(&state, g).unwrap(), name);
        // Only commits of the start branch.
        let elsewhere = w.commit("refs/heads/other", &[("x", "x")]);
        assert!(matches!(
            accept(&state, elsewhere),
            Err(AcceptError::NotOnStartBranch { tip, .. }) if tip == elsewhere
        ));
        // An ancestor of the tip is on it.
        assert!(accept(&state, f).is_ok());
        // A volume that was not started has nothing to accept.
        let plain = w.state("plain");
        volume::init(&plain, "plain", None).unwrap();
        assert!(matches!(
            accept(&plain, g),
            Err(AcceptError::NotStarted { volume }) if volume == "plain"
        ));
    }
}
