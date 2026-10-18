//! End-to-end tests of `agentfs fork` (design §11.5, §17.1) against
//! `file://` bare remotes, no FUSE: init, record, fork create, record in
//! the fork, fork finish, then the change submitted twice with
//! `agentfs patch submit` (which drives the real `git`) and applied once;
//! refused and abandoned forks, settings that fail closed, finished forks
//! read-only, `fork status` and `fork list`.
//!
//! The git-driven parts are skipped, with a message, when `git` is not
//! installed. The CI `test` job runs them against the runner's git.

use std::fs;
use std::path::Path;
use std::process::{Command, Output};
use std::time::Duration;

use agentfs_core::lock::StateLock;
use agentfs_core::volume::StateDir;
use git2::{Oid, Repository};

mod fork_setup;

use fork_setup::{Setup, have_git, text};

impl Setup {
    /// On the source's first-parent history, the commits applying patch
    /// commit `oid`: (applied, duplicate markers).
    fn patch_commits(&self, oid: &str) -> (usize, usize) {
        let repo = Repository::open_bare(self.path("state/git")).unwrap();
        let mut walk = repo.revwalk().unwrap();
        walk.push(self.source_head()).unwrap();
        walk.simplify_first_parent().unwrap();
        let line = format!("Agentfs-Patch: {oid}\n");
        let messages: Vec<String> = walk
            .map(|o| {
                repo.find_commit(o.unwrap())
                    .unwrap()
                    .message()
                    .unwrap()
                    .to_owned()
            })
            .filter(|m| m.contains(&line))
            .collect();
        let duplicates = messages
            .iter()
            .filter(|m| m.starts_with("duplicate of patch "))
            .count();
        (messages.len() - duplicates, duplicates)
    }

    /// `fork create` of the source into `state` and `tree`; returns the
    /// fork's name.
    fn create_fork_in(&self, state: &str, tree: &str) -> String {
        let source = self.path("state").display().to_string();
        let out = self.ok(state, tree, &["fork", "create", "--source-state", &source]);
        out.strip_prefix("fork ")
            .and_then(|l| l.split(' ').next())
            .unwrap_or_else(|| panic!("{out}"))
            .to_owned()
    }

    /// Key `key` of the state area `state`'s own config.
    fn own_config(&self, state: &str, key: &str) -> Option<String> {
        let path = self.path(state).join("git/config");
        git2::Config::open(&path).unwrap().get_string(key).ok()
    }

    fn submit_command(&self, producer: &Path, commit: &str) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_agentfs"));
        cmd.args(["patch", "submit", commit, "--volume", "src", "--repo"])
            .arg(producer)
            .args(["--attempts", "20"]);
        self.hermetic(&mut cmd);
        cmd
    }
}

/// A1, A11: a flagged fork remote is never the source's, and a fork
/// publishes there.
#[test]
fn a_source_never_pushes_to_its_fork_remote() {
    let s = Setup::new(true);
    s.record("state", "tree", "a.txt", "a\n");
    let out = s.ok("state", "tree", &["push"]);
    assert!(!out.contains("forks"), "{out}");
    assert!(s.refs("forks.git").is_empty(), "{:?}", s.refs("forks.git"));
    assert_eq!(
        s.remote_ref("origin.git", "refs/heads/agentfs/src"),
        Some(s.source_head())
    );

    let fork = s.create_fork();
    assert!(fork.starts_with("src-"), "{fork}");
    // The fork's tree is base's.
    assert_eq!(
        fs::read_to_string(s.path("fork-tree/a.txt")).unwrap(),
        "a\n"
    );
    s.record("fork", "fork-tree", "b.txt", "b\n");
    let out = s.ok("fork", "fork-tree", &["fork", "finish"]);
    assert!(out.contains("published to forks"), "{out}");
    let branch = format!("refs/heads/agentfs/{fork}");
    let finish = s.remote_ref("forks.git", &format!("refs/agentfs/{fork}/finish"));
    assert!(finish.is_some());
    assert_eq!(s.remote_ref("forks.git", &branch), finish);
    assert!(
        s.remote_ref("forks.git", &format!("refs/agentfs/{fork}/change"))
            .is_some()
    );
    // Nothing of the fork reached the source's remote.
    assert!(s.remote_ref("origin.git", &branch).is_none());
    // A second create in the same state area is the same fork.
    let again = s.create_fork();
    assert_eq!(again, fork);
}

/// A11: exit statuses of `fork finish`: 75 while the state area is held,
/// 1 at --timeout with the remote unreachable, 0 once published, with the
/// same record each time. A10: the change, submitted twice at once,
/// applies once.
#[test]
fn finish_publishes_to_the_patch_remote_and_the_change_applies_once() {
    let s = Setup::new(false);
    s.record("state", "tree", "a.txt", "a\n");
    s.ok("state", "tree", &["push"]);
    let fork = s.create_fork();
    s.record("fork", "fork-tree", "b.txt", "from the fork\n");

    {
        let held =
            StateLock::acquire(&StateDir::new(s.path("fork")), "test", Duration::ZERO).unwrap();
        let out = s.agentfs("fork", "fork-tree", &["fork", "finish"]);
        assert_eq!(out.status.code(), Some(75), "{}", text(&out.stderr));
        drop(held);
    }

    let parked = s.path("parked.git");
    fs::rename(s.path("origin.git"), &parked).unwrap();
    let out = s.agentfs("fork", "fork-tree", &["fork", "finish", "--timeout", "1s"]);
    assert_eq!(out.status.code(), Some(1), "{}", text(&out.stderr));
    let first = text(&out.stdout).lines().next().unwrap().to_owned();
    assert!(
        first.starts_with(&format!("finished fork {fork} of src")),
        "{first}"
    );
    fs::rename(parked, s.path("origin.git")).unwrap();
    let out = s.ok("fork", "fork-tree", &["fork", "finish"]);
    assert_eq!(out.lines().next().unwrap(), first);
    assert!(out.contains("published to origin"), "{out}");
    let change = s
        .remote_ref("origin.git", &format!("refs/agentfs/{fork}/change"))
        .unwrap();
    // The source's branch is not moved by the fork.
    assert_eq!(
        s.remote_ref("origin.git", "refs/heads/agentfs/src"),
        Some(s.source_head())
    );

    if !have_git() {
        return;
    }
    // A receiver fetches the change and submits it, twice at once.
    let producer = s.path("producer");
    let mut clone = Command::new("git");
    clone
        .args(["clone", "--quiet", &s.url("origin.git")])
        .arg(&producer);
    s.hermetic(&mut clone);
    let out = clone.output().unwrap();
    assert!(out.status.success(), "{}", text(&out.stderr));
    let spec = format!("refs/agentfs/{fork}/change:refs/heads/change");
    let out = s.git(&producer, &["fetch", "--quiet", "origin", &spec]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    let oid = change.to_string();
    let handles: Vec<_> = (0..2)
        .map(|_| {
            let mut cmd = s.submit_command(&producer, &oid);
            std::thread::spawn(move || cmd.output().unwrap())
        })
        .collect();
    // Each queues it, or is refused early if the other's push landed
    // before its listing (B7); never anything else.
    let mut queued = 0;
    for h in handles {
        let out = h.join().unwrap();
        if out.status.success() {
            queued += 1;
        } else {
            assert!(
                text(&out.stderr).contains("a change lands at most once"),
                "{}",
                text(&out.stderr)
            );
        }
    }
    assert!(queued >= 1, "neither submit queued the change");
    // Two submitters of one commit at once may share an inbox number: the
    // second push finds the ref already at that commit.
    s.ok("state", "tree", &["patch", "apply"]);
    assert_eq!(
        fs::read_to_string(s.path("tree/b.txt")).unwrap(),
        "from the fork\n"
    );
    let (applied, duplicates) = s.patch_commits(&oid);
    assert_eq!(applied, 1, "one commit applies the change");
    // B7: the same change submitted again is refused early, queued or
    // applied, and pushes nothing.
    let before = s.refs("origin.git");
    let out = s.submit_command(&producer, &oid).output().unwrap();
    assert_eq!(out.status.code(), Some(1), "{}", text(&out.stderr));
    assert!(
        text(&out.stderr).contains("a change lands at most once"),
        "{}",
        text(&out.stderr)
    );
    assert_eq!(s.refs("origin.git"), before, "the refused submit pushed");
    s.ok("state", "tree", &["push"]);
    let before = s.refs("origin.git");
    let out = s.submit_command(&producer, &oid).output().unwrap();
    assert_eq!(out.status.code(), Some(1), "{}", text(&out.stderr));
    assert!(
        text(&out.stderr).contains("applied as Some("),
        "{}",
        text(&out.stderr)
    );
    assert_eq!(s.refs("origin.git"), before, "the refused submit pushed");
    // The workload reverts the file; the same change queued again by a
    // producer that passed the early check (plain git here) is a
    // duplicate, and the file stays reverted.
    s.record("state", "tree", "b.txt", "reverted\n");
    let next = s
        .refs("origin.git")
        .iter()
        .filter_map(|(name, _)| {
            name.strip_prefix("refs/agentfs/src/applied/")
                .or_else(|| name.strip_prefix("refs/agentfs/src/inbox/"))
                .and_then(|n| n.parse::<u64>().ok())
        })
        .max()
        .unwrap()
        + 1;
    let spec = format!("{oid}:refs/agentfs/src/inbox/{next:010}");
    let out = s.git(&producer, &["push", "--quiet", "origin", &spec]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    s.ok("state", "tree", &["patch", "apply"]);
    let (applied, more) = s.patch_commits(&oid);
    assert_eq!(applied, 1, "still one commit applies it");
    assert_eq!(
        more,
        duplicates + 1,
        "the resubmission is a duplicate marker"
    );
    assert_eq!(
        fs::read_to_string(s.path("tree/b.txt")).unwrap(),
        "reverted\n"
    );
}

/// A5: a CLI source whose remotes live in its own config is refused.
#[test]
fn a_source_with_settings_in_its_own_config_is_refused() {
    let s = Setup::new(false);
    s.record("state", "tree", "a.txt", "a\n");
    let mut config = git2::Config::open(&s.path("state/git/config")).unwrap();
    config.set_str("remote.own.url", "/nowhere").unwrap();
    let source = s.path("state").display().to_string();
    let out = s.agentfs(
        "fork",
        "fork-tree",
        &["fork", "create", "--source-state", &source],
    );
    assert_eq!(out.status.code(), Some(1), "{}", text(&out.stdout));
    assert!(
        text(&out.stderr).contains("remote.own.url"),
        "{}",
        text(&out.stderr)
    );
}

/// `fork status --json` of the fork in `fork`.
fn status_json(s: &Setup) -> serde_json::Value {
    let out = s.ok("fork", "fork-tree", &["fork", "status", "--json"]);
    serde_json::from_str(&out).unwrap_or_else(|e| panic!("{e}: {out}"))
}

fn stderr_of(out: &Output) -> String {
    text(&out.stderr)
}

/// B1, B8: a finish ref another writer holds refuses the publish for good:
/// exit 1 naming the runbook, status refused; `--retry` once the ref is
/// gone publishes.
#[test]
fn a_refused_publish_waits_for_retry() {
    let s = Setup::new(false);
    s.record("state", "tree", "a.txt", "a\n");
    s.ok("state", "tree", &["push"]);
    let fork = s.create_fork();
    s.record("fork", "fork-tree", "b.txt", "b\n");
    let finish = format!("refs/agentfs/{fork}/finish");
    let origin = Repository::open_bare(s.path("origin.git")).unwrap();
    origin
        .reference(&finish, s.source_head(), true, "another writer")
        .unwrap();
    let out = s.agentfs("fork", "fork-tree", &["fork", "finish", "--timeout", "30s"]);
    assert_eq!(out.status.code(), Some(1), "{}", stderr_of(&out));
    let err = stderr_of(&out);
    assert!(err.contains("refused (another_writer)"), "{err}");
    assert!(
        err.contains("docs/runbooks/fork-refused.md#another_writer"),
        "{err}"
    );
    let doc = status_json(&s);
    assert_eq!(doc["version"], 1);
    assert_eq!(doc["state"], "refused");
    assert_eq!(doc["refused"]["reason"], "another_writer");
    assert_eq!(doc["remote"], "origin");
    let text_status = s.ok("fork", "fork-tree", &["fork", "status"]);
    assert!(text_status.contains("state:     refused"), "{text_status}");
    // Not retried without --retry.
    let out = s.agentfs("fork", "fork-tree", &["fork", "finish"]);
    assert_eq!(out.status.code(), Some(1), "{}", stderr_of(&out));
    assert_eq!(s.remote_ref("origin.git", &finish), Some(s.source_head()));
    origin.find_reference(&finish).unwrap().delete().unwrap();
    let out = s.ok("fork", "fork-tree", &["fork", "finish", "--retry"]);
    assert!(out.contains("published to origin"), "{out}");
    let doc = status_json(&s);
    assert_eq!(doc["state"], "published");
    assert!(doc["published_at_ms"].as_u64().is_some(), "{doc}");
    assert!(doc["refused"].is_null(), "{doc}");
    assert_eq!(
        s.remote_ref("origin.git", &finish).map(|o| o.to_string()),
        doc["finish"].as_str().map(str::to_owned)
    );
}

/// B4: abandon on record; finish then pushes nothing; a published fork
/// cannot be abandoned; an empty reason fails; a held lock exits 75.
#[test]
fn an_abandoned_fork_is_never_published() {
    let s = Setup::new(false);
    s.record("state", "tree", "a.txt", "a\n");
    s.ok("state", "tree", &["push"]);
    let fork = s.create_fork();
    s.record("fork", "fork-tree", "b.txt", "b\n");
    let out = s.agentfs("fork", "fork-tree", &["fork", "abandon", "--reason", " "]);
    assert_eq!(out.status.code(), Some(1), "{}", stderr_of(&out));
    assert!(stderr_of(&out).contains("a reason is required"));
    {
        let held =
            StateLock::acquire(&StateDir::new(s.path("fork")), "test", Duration::ZERO).unwrap();
        let out = s.agentfs("fork", "fork-tree", &["fork", "abandon", "--reason", "x"]);
        assert_eq!(out.status.code(), Some(75), "{}", stderr_of(&out));
        drop(held);
    }
    let out = s.ok(
        "fork",
        "fork-tree",
        &["fork", "abandon", "--reason", "not needed"],
    );
    assert!(out.contains(&format!("abandoned fork {fork}")), "{out}");
    let doc = status_json(&s);
    assert_eq!(doc["state"], "abandoned");
    assert_eq!(doc["abandoned"]["reason"], "not needed");
    assert_eq!(doc["abandoned"]["state"], "active");
    let before = s.refs("origin.git");
    let out = s.agentfs("fork", "fork-tree", &["fork", "finish", "--timeout", "5s"]);
    assert_eq!(out.status.code(), Some(1), "{}", stderr_of(&out));
    assert!(stderr_of(&out).contains("abandoned"), "{}", stderr_of(&out));
    assert_eq!(s.refs("origin.git"), before, "an abandoned fork was pushed");

    // Published: nothing to discard.
    let s = Setup::new(false);
    s.record("state", "tree", "a.txt", "a\n");
    s.ok("state", "tree", &["push"]);
    s.create_fork();
    s.ok("fork", "fork-tree", &["fork", "finish"]);
    let out = s.agentfs(
        "fork",
        "fork-tree",
        &["fork", "abandon", "--reason", "late"],
    );
    assert_eq!(out.status.code(), Some(1), "{}", stderr_of(&out));
    assert!(
        stderr_of(&out).contains("is published"),
        "{}",
        stderr_of(&out)
    );
}

/// B6, B8: a finished fork is read-only for every command that changes
/// it; `fork status` reads each state and takes no lock.
#[test]
fn a_finished_fork_is_read_only() {
    let s = Setup::new(true);
    s.record("state", "tree", "a.txt", "a\n");
    s.ok("state", "tree", &["push"]);
    let fork = s.create_fork();
    {
        // A running serve-history holds the lock; status still reads.
        let held =
            StateLock::acquire(&StateDir::new(s.path("fork")), "test", Duration::ZERO).unwrap();
        let doc = status_json(&s);
        assert_eq!(doc["state"], "active");
        assert_eq!(doc["fork"].as_str(), Some(fork.as_str()));
        assert_eq!(doc["source"], "src");
        assert_eq!(
            doc["base"].as_str().map(str::to_owned),
            Some(s.source_head().to_string())
        );
        assert!(doc["finish"].is_null() && doc["change"].is_null(), "{doc}");
        drop(held);
    }
    s.record("fork", "fork-tree", "b.txt", "b\n");
    s.ok("fork", "fork-tree", &["fork", "finish"]);
    let doc = status_json(&s);
    assert_eq!(doc["state"], "published");
    assert_eq!(doc["remote"], "forks");
    assert!(
        doc["change"].is_string() && doc["finish"].is_string(),
        "{doc}"
    );
    let text_status = s.ok("fork", "fork-tree", &["fork", "status"]);
    for line in [
        "state:     published",
        "remote:    forks",
        "publish:   published at",
    ] {
        assert!(text_status.contains(line), "{line}: {text_status}");
    }
    fs::write(s.path("fork-tree/c.txt"), "c\n").unwrap();
    fs::create_dir_all(s.path("mnt")).unwrap();
    let mnt = s.path("mnt").display().to_string();
    let tree = s.path("fork-tree").display().to_string();
    for args in [
        vec!["record", "c.txt"],
        vec!["run"],
        vec!["mount", tree.as_str(), mnt.as_str()],
        vec!["restore", "b.txt", "HEAD"],
        vec!["revert", "HEAD"],
        vec!["patch", "apply"],
    ] {
        let out = s.agentfs("fork", "fork-tree", &args);
        assert_eq!(out.status.code(), Some(1), "{args:?}: {}", stderr_of(&out));
        assert!(
            stderr_of(&out).contains(&format!("fork {fork} is finished: it is read-only")),
            "{args:?}: {}",
            stderr_of(&out)
        );
    }
}

/// B5: a fork whose profile file is missing fails to open; a flagged fork
/// remote with a global lfs.url and no endpoint of its own leaves the
/// fork without a fork remote; the source still pushes.
#[test]
fn a_fork_with_settings_it_cannot_trust_fails_closed() {
    let s = Setup::new(true);
    s.record("state", "tree", "a.txt", "a\n");
    s.ok("state", "tree", &["push"]);
    s.create_fork();
    let profile = fs::read_to_string(s.path("profile")).unwrap();
    fs::write(
        s.path("profile"),
        format!("{profile}[lfs]\n\turl = http://lfs.example/source\n"),
    )
    .unwrap();
    let out = s.agentfs("fork", "fork-tree", &["fork", "finish", "--timeout", "1s"]);
    assert_eq!(out.status.code(), Some(1), "{}", stderr_of(&out));
    let err = stderr_of(&out);
    assert!(err.contains("must name its own LFS endpoint"), "{err}");
    assert!(err.contains("no valid fork remote"), "{err}");
    assert_eq!(status_json(&s)["state"], "active");
    // The source is unaffected: its own remote still takes its pushes.
    s.record("state", "tree", "a.txt", "again\n");
    let _ = s.agentfs("state", "tree", &["push"]);
    assert_eq!(
        s.remote_ref("origin.git", "refs/heads/agentfs/src"),
        Some(s.source_head())
    );

    fs::remove_file(s.path("profile")).unwrap();
    let missing = s.path("profile").display().to_string();
    for args in [
        vec!["fork", "status"],
        vec!["fork", "finish", "--timeout", "1s"],
        vec!["record", "a.txt"],
    ] {
        let out = s.agentfs("fork", "fork-tree", &args);
        assert_eq!(out.status.code(), Some(1), "{args:?}: {}", stderr_of(&out));
        assert!(
            stderr_of(&out).contains(&missing),
            "{args:?}: {}",
            stderr_of(&out)
        );
    }
}

/// A commit in `repo` with `tree` and `parents`, written to no ref.
fn commit(repo: &Repository, tree: Oid, parents: &[Oid], message: &str) -> Oid {
    let sig = git2::Signature::now("forger", "forger@example.invalid").unwrap();
    let tree = repo.find_tree(tree).unwrap();
    let parents: Vec<git2::Commit> = parents
        .iter()
        .map(|p| repo.find_commit(*p).unwrap())
        .collect();
    let refs: Vec<&git2::Commit> = parents.iter().collect();
    repo.commit(None, &sig, &sig, message, &tree, &refs)
        .unwrap()
}

/// A finish commit's message for `fork`.
fn finish_message(fork: &str, base: Oid, change: Option<Oid>) -> String {
    let change = change.map_or_else(String::new, |c| format!("Agentfs-Change: {c}\n"));
    format!(
        "finish fork {fork}\n\nAgentfs-Seq: 1\nAgentfs-Trigger: finish\nAgentfs-Fork: {fork}\n\
         Agentfs-Source: src\nAgentfs-Base: {base}\n{change}Agentfs-Fork-Remote: forks\n\
         Agentfs-Unrecorded: 0\n"
    )
}

/// B9: `fork list` in a receiver's own repository lists a published
/// change with every field, and skips forged ones with why.
#[test]
fn fork_list_checks_every_published_change() {
    if !have_git() {
        return;
    }
    let s = Setup::new(true);
    s.record("state", "tree", "a.txt", "a\n");
    s.ok("state", "tree", &["push"]);
    let fork = s.create_fork();
    s.record("fork", "fork-tree", "b.txt", "b\n");
    s.ok("fork", "fork-tree", &["fork", "finish"]);
    let base = s.source_head();
    let forks = Repository::open_bare(s.path("forks.git")).unwrap();
    let change = forks
        .refname_to_id(&format!("refs/agentfs/{fork}/change"))
        .unwrap();
    let finish = forks
        .refname_to_id(&format!("refs/agentfs/{fork}/finish"))
        .unwrap();
    let tree = forks.find_commit(finish).unwrap().tree_id();
    let empty = forks.treebuilder(None).unwrap().write().unwrap();
    let set = |name: &str, kind: &str, oid: Oid| {
        forks
            .reference(&format!("refs/agentfs/{name}/{kind}"), oid, true, "forged")
            .unwrap();
    };
    // Another parent than base.
    let c = commit(&forks, tree, &[finish], "change");
    set("forged-parent", "change", c);
    let msg = finish_message("forged-parent", base, Some(c));
    set("forged-parent", "finish", commit(&forks, tree, &[], &msg));
    // Another tree than the finish commit's.
    let c = commit(&forks, empty, &[base], "change");
    set("forged-tree", "change", c);
    let msg = finish_message("forged-tree", base, Some(c));
    set("forged-tree", "finish", commit(&forks, tree, &[], &msg));
    // A change ref that is not Agentfs-Change.
    let c = commit(&forks, tree, &[base], "another change");
    set("forged-oid", "change", c);
    let msg = finish_message("forged-oid", base, Some(change));
    set("forged-oid", "finish", commit(&forks, tree, &[], &msg));
    // No trailers.
    set("no-trailers", "change", change);
    set(
        "no-trailers",
        "finish",
        commit(&forks, tree, &[], "finish\n"),
    );
    // Not a volume name.
    set("_bad", "finish", finish);

    let producer = s.path("producer");
    let out = s.git(
        s.dir.path(),
        &["init", "--quiet", producer.to_str().unwrap()],
    );
    assert!(out.status.success(), "{}", stderr_of(&out));
    let list = |extra: &[&str]| {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_agentfs"));
        cmd.arg("--state")
            .arg(s.path("no-state-here"))
            .args(["fork", "list", "--json", "--remote"])
            .arg(s.url("forks.git"))
            .arg("--repo")
            .arg(&producer)
            .args(extra);
        s.hermetic(&mut cmd);
        let out = cmd.output().unwrap();
        assert!(out.status.success(), "{}", stderr_of(&out));
        serde_json::from_slice::<serde_json::Value>(&out.stdout).unwrap()
    };
    let listed = list(&[]);
    let changes = listed["changes"].as_array().unwrap();
    assert_eq!(changes.len(), 1, "{listed}");
    let c = &changes[0];
    assert_eq!(c["fork"].as_str(), Some(fork.as_str()));
    assert_eq!(c["source"], "src");
    assert_eq!(c["base"].as_str(), Some(base.to_string().as_str()));
    assert_eq!(c["change"].as_str(), Some(change.to_string().as_str()));
    assert_eq!(c["finish"].as_str(), Some(finish.to_string().as_str()));
    assert_eq!(
        c["history_ref"].as_str(),
        Some(format!("refs/heads/agentfs/{fork}").as_str())
    );
    let skipped: Vec<(String, String)> = listed["skipped"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| {
            (
                e["ref_name"].as_str().unwrap().to_owned(),
                e["reason"].as_str().unwrap().to_owned(),
            )
        })
        .collect();
    for (name, why) in [
        ("refs/agentfs/forged-parent/change", "not base"),
        (
            "refs/agentfs/forged-tree/change",
            "is not the finish commit's",
        ),
        ("refs/agentfs/forged-oid/change", "Agentfs-Change"),
        (
            "refs/agentfs/no-trailers/finish",
            "no agentfs finish trailers",
        ),
        ("refs/agentfs/_bad/finish", "not a valid volume name"),
    ] {
        assert!(
            skipped.iter().any(|(n, r)| n == name && r.contains(why)),
            "{name}: {skipped:?}"
        );
    }
    assert_eq!(skipped.len(), 5, "{skipped:?}");
    // --source filters.
    let other = list(&["--source", "other"]);
    assert!(other["changes"].as_array().unwrap().is_empty(), "{other}");
    let same = list(&["--source", "src"]);
    assert_eq!(same["changes"].as_array().unwrap().len(), 1, "{same}");
    assert!(
        !s.path("no-state-here").exists(),
        "fork list made a state area"
    );
}

/// D1: with `agentfs.forkRefs = hidden` in the source's profile, a new
/// fork's history is kept and published at `refs/agentfs/<fork>/history`,
/// never under `refs/heads/`, and finish, status, list and deliver work
/// unchanged; a fork created before the profile changed keeps its branch.
#[test]
fn a_hidden_fork_publishes_off_every_branch() {
    let s = Setup::new(true);
    s.record("state", "tree", "a.txt", "a\n");
    s.ok("state", "tree", &["push"]);
    let early = s.create_fork_in("fork", "fork-tree");
    let profile = fs::read_to_string(s.path("profile")).unwrap();
    fs::write(
        s.path("profile"),
        format!("{profile}[agentfs]\n\tforkRefs = hidden\n"),
    )
    .unwrap();
    let hidden = s.create_fork_in("fork2", "fork2-tree");
    assert_ne!(early, hidden);
    assert_eq!(
        s.own_config("fork2", "agentfs.fork.pushTarget").as_deref(),
        Some("hidden")
    );
    assert_eq!(s.own_config("fork", "agentfs.fork.pushTarget"), None);
    let fork2 = Repository::open_bare(s.path("fork2/git")).unwrap();
    let history = format!("refs/agentfs/{hidden}/history");
    assert_eq!(fork2.refname_to_id(&history).ok(), Some(s.source_head()));
    assert!(
        fork2
            .refname_to_id(&format!("refs/heads/agentfs/{hidden}"))
            .is_err()
    );

    s.record("fork", "fork-tree", "b.txt", "early\n");
    s.record("fork2", "fork2-tree", "b.txt", "hidden\n");
    for (state, tree) in [("fork", "fork-tree"), ("fork2", "fork2-tree")] {
        let out = s.ok(state, tree, &["fork", "finish"]);
        assert!(out.contains("published to forks"), "{out}");
    }
    let finish = |fork: &str| {
        s.remote_ref("forks.git", &format!("refs/agentfs/{fork}/finish"))
            .unwrap()
    };
    // Fixed at create: the early fork still publishes its branch.
    assert_eq!(
        s.remote_ref("forks.git", &format!("refs/heads/agentfs/{early}")),
        Some(finish(&early))
    );
    assert_eq!(s.remote_ref("forks.git", &history), Some(finish(&hidden)));
    let branches: Vec<String> = s
        .refs("forks.git")
        .into_iter()
        .map(|(name, _)| name)
        .filter(|name| name.starts_with("refs/heads/") && name.contains(&hidden))
        .collect();
    assert!(branches.is_empty(), "{branches:?}");
    assert!(
        s.refs("origin.git")
            .iter()
            .all(|(n, _)| !n.contains(&hidden))
    );
    let out = s.ok("fork2", "fork2-tree", &["fork", "status", "--json"]);
    let doc: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(doc["state"], "published");
    assert_eq!(
        doc["finish"].as_str().map(str::to_owned),
        Some(finish(&hidden).to_string())
    );

    if !have_git() {
        return;
    }
    let receiver = s.path("receiver");
    let out = s.git(
        s.dir.path(),
        &["init", "--quiet", receiver.to_str().unwrap()],
    );
    assert!(out.status.success(), "{}", stderr_of(&out));
    let forks = s.url("forks.git");
    let run = |args: &[&str]| {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_agentfs"));
        cmd.current_dir(s.dir.path())
            .args(args)
            .arg("--repo")
            .arg(&receiver);
        s.hermetic(&mut cmd);
        let out = cmd.output().unwrap();
        assert!(
            out.status.success(),
            "{args:?}: {}{}",
            text(&out.stdout),
            stderr_of(&out)
        );
        text(&out.stdout)
    };
    let listed: serde_json::Value =
        serde_json::from_str(&run(&["fork", "list", "--json", "--remote", &forks])).unwrap();
    let refs: Vec<(String, String)> = listed["changes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| {
            (
                c["fork"].as_str().unwrap().to_owned(),
                c["history_ref"].as_str().unwrap().to_owned(),
            )
        })
        .collect();
    let mut want = vec![
        (early.clone(), format!("refs/heads/agentfs/{early}")),
        (hidden.clone(), history.clone()),
    ];
    want.sort();
    let mut refs = refs;
    refs.sort();
    assert_eq!(refs, want, "{listed}");
    let out = run(&[
        "fork", "deliver", &hidden, "--from", &forks, "--to", "branch", "--shape", "history",
        "--prefix", "hist/",
    ]);
    assert!(out.contains("delivered change"), "{out}");
    assert_eq!(
        s.remote_ref("forks.git", &format!("refs/heads/hist/{hidden}")),
        Some(finish(&hidden))
    );
}

/// F8 S3 acceptance 4 (R4): a fork of a local-only source finishes into
/// the source's own history, with no remote: `agentfs/<fork>` and its
/// change and finish refs, never the source's branch or tree; a second
/// finish changes nothing. With the source's daemon running, the fork
/// publishes through it (its control socket); a source in use without
/// one exits 75.
#[test]
fn a_local_only_sources_fork_finishes_into_its_history() {
    let s = Setup::local_only();
    s.record("state", "tree", "a.txt", "a\n");
    let out = s.ok("state", "tree", &["push"]);
    assert_eq!(out.trim(), "local-only: nothing to push");
    let base = s.source_head();
    let fork = s.create_fork();
    s.record("fork", "fork-tree", "b.txt", "from the fork\n");

    {
        let held =
            StateLock::acquire(&StateDir::new(s.path("state")), "test", Duration::ZERO).unwrap();
        let out = s.agentfs("fork", "fork-tree", &["fork", "finish"]);
        assert_eq!(out.status.code(), Some(75), "{}", text(&out.stderr));
        drop(held);
    }
    let out = s.ok("fork", "fork-tree", &["fork", "finish"]);
    assert!(
        out.contains("published into the history of volume src"),
        "{out}"
    );
    let refs = s.refs("state/git");
    let get = |name: &str| refs.iter().find(|(n, _)| n == name).map(|(_, o)| *o);
    let finish = get(&format!("refs/agentfs/{fork}/finish")).unwrap();
    let change = get(&format!("refs/agentfs/{fork}/change")).unwrap();
    assert_eq!(get(&format!("refs/heads/agentfs/{fork}")), Some(finish));
    let repo = Repository::open_bare(s.path("state/git")).unwrap();
    let tree = repo.find_commit(change).unwrap().tree().unwrap();
    assert!(tree.get_name("b.txt").is_some());
    // Never applied.
    assert_eq!(s.source_head(), base);
    assert!(!s.path("tree/b.txt").exists());
    let status = status_json(&s);
    assert_eq!(status["state"], "published", "{status}");
    // Idempotent.
    let out = s.ok("fork", "fork-tree", &["fork", "finish"]);
    assert!(out.contains("published into"), "{out}");
    assert_eq!(s.refs("state/git"), refs);

    // A second fork, while the source's daemon holds its lock.
    let second = s.create_fork_in("fork2", "fork2-tree");
    s.record("fork2", "fork2-tree", "c.txt", "c\n");
    let mut daemon = Command::new(env!("CARGO_BIN_EXE_agentfs"));
    daemon
        .arg("--state")
        .arg(s.path("state"))
        .arg("--root")
        .arg(s.path("tree"))
        .arg("run");
    s.hermetic(&mut daemon);
    let mut daemon = daemon.spawn().unwrap();
    let socket = s.path("state/meta/control/agentfs.sock");
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    while !socket.exists() {
        assert!(std::time::Instant::now() < deadline, "no control socket");
        std::thread::sleep(Duration::from_millis(50));
    }
    let out = s.ok(
        "fork2",
        "fork2-tree",
        &["fork", "finish", "--timeout", "30s"],
    );
    daemon.kill().unwrap();
    daemon.wait().unwrap();
    assert!(out.contains("published into"), "{out}");
    assert!(
        s.remote_ref("state/git", &format!("refs/heads/agentfs/{second}"))
            .is_some()
    );
    assert_eq!(s.source_head(), base);
}

/// F8 S3: forks a local-only source received are its history too. Once a
/// remote is declared (`none` removed), `agentfs push` carries each
/// fork's branch, change and finish refs there, create-only; a remote
/// holding one at another commit fails the push and keeps its value.
#[test]
fn a_remote_added_to_a_local_only_source_gets_its_forks() {
    let s = Setup::local_only();
    s.record("state", "tree", "a.txt", "a\n");
    let fork = s.create_fork();
    s.record("fork", "fork-tree", "b.txt", "from the fork\n");
    s.ok("fork", "fork-tree", &["fork", "finish"]);
    let fork_branch = format!("refs/heads/agentfs/{fork}");
    let received: Vec<(String, Oid)> = s
        .refs("state/git")
        .into_iter()
        .filter(|(name, _)| {
            *name == fork_branch || name.starts_with(&format!("refs/agentfs/{fork}/"))
        })
        .collect();
    assert_eq!(received.len(), 3, "{received:?}");

    // A second remote, where another writer holds the fork's branch.
    let two = Repository::init_bare(s.path("two.git")).unwrap();
    let sig = git2::Signature::now("x", "x@example.invalid").unwrap();
    let tree = two
        .find_tree(two.treebuilder(None).unwrap().write().unwrap())
        .unwrap();
    let stray = two
        .commit(None, &sig, &sig, "another writer", &tree, &[])
        .unwrap();
    two.reference(&fork_branch, stray, false, "test").unwrap();
    fs::write(
        s.path("profile"),
        format!(
            "[remote \"origin\"]\n\turl = {}\n[remote \"two\"]\n\turl = {}\n",
            s.url("origin.git"),
            s.url("two.git")
        ),
    )
    .unwrap();
    let out = s.agentfs("state", "tree", &["push"]);
    let all = format!("{}{}", text(&out.stdout), text(&out.stderr));
    assert!(!out.status.success(), "{all}");
    assert!(all.contains("another writer"), "{all}");
    assert_eq!(
        s.remote_ref("two.git", &fork_branch),
        Some(stray),
        "never overwritten"
    );

    let origin = s.refs("origin.git");
    for (name, oid) in &received {
        assert!(
            origin.contains(&(name.clone(), *oid)),
            "{name} not on origin: {origin:?}"
        );
    }
    assert_eq!(
        s.remote_ref("origin.git", "refs/heads/agentfs/src"),
        Some(s.source_head())
    );
}

/// Design §11.5 "Fork link": `fork create --link unlinked` makes an
/// independent fork. It pushes its own branch where a linked fork would
/// publish (the flagged `forks` remote here), `fork finish` refuses it and
/// delivers nothing, and `fork status` says why it has no state. Of a
/// local-only source, it records no path and is local-only.
#[test]
fn an_unlinked_fork_pushes_its_branch_and_never_finishes() {
    let s = Setup::new(true);
    s.record("state", "tree", "a.txt", "a\n");
    let source = s.path("state").display().to_string();
    let unlinked = [
        "fork",
        "create",
        "--source-state",
        &source,
        "--link",
        "unlinked",
    ];
    let out = s.ok("fork", "fork-tree", &unlinked);
    assert!(out.contains("(unlinked"), "{out}");
    let fork = out
        .strip_prefix("fork ")
        .and_then(|l| l.split(' ').next())
        .unwrap()
        .to_owned();
    assert_eq!(
        s.own_config("fork", "agentfs.fork.link").as_deref(),
        Some("unlinked")
    );
    s.record("fork", "fork-tree", "b.txt", "b\n");
    s.ok("fork", "fork-tree", &["push"]);
    let branch = format!("refs/heads/agentfs/{fork}");
    let head = Repository::open_bare(s.path("fork/git"))
        .unwrap()
        .refname_to_id(&branch)
        .unwrap();
    assert_eq!(s.remote_ref("forks.git", &branch), Some(head));
    assert!(s.remote_ref("origin.git", &branch).is_none());

    let out = s.agentfs("fork", "fork-tree", &["fork", "finish"]);
    assert_eq!(out.status.code(), Some(1), "{}", text(&out.stdout));
    assert!(
        stderr_of(&out).contains("is unlinked"),
        "{}",
        stderr_of(&out)
    );
    let out = s.agentfs("fork", "fork-tree", &["fork", "status"]);
    assert!(!out.status.success());
    assert!(
        stderr_of(&out).contains("is unlinked"),
        "{}",
        stderr_of(&out)
    );
    // Nothing delivered, and the fork still records.
    for remote in ["forks.git", "origin.git"] {
        assert!(
            s.refs(remote)
                .iter()
                .all(|(r, _)| !r.starts_with("refs/agentfs/")),
            "{remote}: {:?}",
            s.refs(remote)
        );
    }
    s.record("fork", "fork-tree", "c.txt", "c\n");

    let s = Setup::local_only();
    s.record("state", "tree", "a.txt", "a\n");
    let source = s.path("state").display().to_string();
    let unlinked = [
        "fork",
        "create",
        "--source-state",
        &source,
        "--link",
        "unlinked",
    ];
    s.ok("fork", "fork-tree", &unlinked);
    assert_eq!(s.own_config("fork", "agentfs.fork.sourceState"), None);
    s.record("fork", "fork-tree", "b.txt", "b\n");
    let out = s.ok("fork", "fork-tree", &["push"]);
    assert_eq!(out.trim(), "local-only: nothing to push");
    // Nothing of the fork reached the source's history.
    let refs = s.refs("state/git");
    assert!(
        refs.iter()
            .all(|(r, _)| !r.starts_with("refs/heads/agentfs/src-")
                && !r.starts_with("refs/agentfs/src-")),
        "{refs:?}"
    );
}
