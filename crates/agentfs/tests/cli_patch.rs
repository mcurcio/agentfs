//! End-to-end tests of `agentfs patch` and the queue lines of `agentfs
//! status` and `agentfs push` (design §12, §18 test 15), against a
//! `file://` bare remote and the real `git`, which `patch submit` drives.
//!
//! Skipped, with a message, when `git` is not installed. The CI `test` job
//! runs them against the runner's git.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use git2::{Oid, Repository};

const BRANCH: &str = "refs/heads/agentfs/demo";

fn have_git() -> bool {
    let found = Command::new("git").arg("--version").output().is_ok();
    if !found {
        eprintln!("skipping: git is not installed");
    }
    found
}

/// A volume `demo` with a bare remote `origin`, and producer clones.
struct Setup {
    dir: tempfile::TempDir,
}

impl Setup {
    fn new() -> Self {
        let s = Self {
            dir: tempfile::tempdir().unwrap(),
        };
        fs::create_dir_all(s.tree()).unwrap();
        fs::create_dir_all(s.home()).unwrap();
        Repository::init_bare(s.remote()).unwrap();
        let out = s.agentfs(&["init", "--volume", "demo"]);
        assert!(out.status.success(), "{}", text(&out.stderr));
        let mut config = git2::Config::open(&s.state().join("git/config")).unwrap();
        config
            .set_str("remote.origin.url", &s.remote_url())
            .unwrap();
        s
    }

    fn state(&self) -> PathBuf {
        self.dir.path().join("state")
    }

    fn tree(&self) -> PathBuf {
        self.dir.path().join("tree")
    }

    fn home(&self) -> PathBuf {
        self.dir.path().join("home")
    }

    fn remote(&self) -> PathBuf {
        self.dir.path().join("remote.git")
    }

    fn remote_url(&self) -> String {
        format!("file://{}", self.remote().display())
    }

    fn hermetic(&self, cmd: &mut Command) {
        let home = self.home();
        cmd.env("HOME", &home)
            .env("XDG_CONFIG_HOME", home.join(".config"))
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_AUTHOR_NAME", "Producer")
            .env("GIT_AUTHOR_EMAIL", "producer@example.com")
            .env("GIT_COMMITTER_NAME", "Producer")
            .env("GIT_COMMITTER_EMAIL", "producer@example.com")
            .env_remove("AGENTFS_STATE")
            .env_remove("AGENTFS_ROOT")
            .env_remove("AGENTFS_VOLUME")
            .env("AGENTFS_REPLICA", "test-0");
        for (name, _) in std::env::vars_os() {
            if name.to_string_lossy().starts_with("OTEL_") {
                cmd.env_remove(name);
            }
        }
    }

    fn agentfs_command(&self, args: &[&str]) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_agentfs"));
        cmd.arg("--state")
            .arg(self.state())
            .arg("--root")
            .arg(self.tree())
            .args(args);
        self.hermetic(&mut cmd);
        cmd
    }

    fn agentfs(&self, args: &[&str]) -> Output {
        self.agentfs_command(args).output().unwrap()
    }

    fn ok(&self, args: &[&str]) -> String {
        let out = self.agentfs(args);
        assert!(
            out.status.success(),
            "agentfs {args:?}: {}{}",
            text(&out.stdout),
            text(&out.stderr)
        );
        text(&out.stdout)
    }

    fn record(&self, name: &str, content: &str) {
        fs::write(self.tree().join(name), content).unwrap();
        self.ok(&["record", name]);
    }

    fn git(&self, dir: &Path, args: &[&str]) -> Output {
        let mut cmd = Command::new("git");
        cmd.arg("-C").arg(dir).args(args);
        self.hermetic(&mut cmd);
        cmd.output().unwrap()
    }

    /// A clone of the remote: a producer's repository.
    fn producer(&self, name: &str) -> PathBuf {
        let dir = self.dir.path().join(name);
        let mut cmd = Command::new("git");
        cmd.args(["clone", "--quiet", &self.remote_url()]).arg(&dir);
        self.hermetic(&mut cmd);
        let out = cmd.output().unwrap();
        assert!(out.status.success(), "{}", text(&out.stderr));
        dir
    }

    /// `agentfs patch submit` in `producer`, without --state or --root.
    fn submit_command(&self, producer: &Path, args: &[&str]) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_agentfs"));
        cmd.args(["patch", "submit", "--volume", "demo", "--repo"])
            .arg(producer)
            .args(args);
        self.hermetic(&mut cmd);
        cmd
    }

    fn submit(&self, producer: &Path, args: &[&str]) -> Output {
        self.submit_command(producer, args).output().unwrap()
    }

    fn submit_ok(&self, producer: &Path, args: &[&str]) -> String {
        let out = self.submit(producer, args);
        assert!(
            out.status.success(),
            "submit {args:?}: {}{}",
            text(&out.stdout),
            text(&out.stderr)
        );
        text(&out.stdout)
    }

    fn file(&self, name: &str, content: &str) -> String {
        let path = self.dir.path().join(name);
        fs::write(&path, content).unwrap();
        path.display().to_string()
    }

    fn remote_ref(&self, name: &str) -> Option<Oid> {
        Repository::open_bare(self.remote())
            .unwrap()
            .refname_to_id(name)
            .ok()
    }

    fn local(&self) -> Repository {
        Repository::open_bare(self.state().join("git")).unwrap()
    }

    /// Commits on the local branch's first-parent history that apply
    /// patch `n`.
    fn inbox_commits(&self, n: u64) -> usize {
        let repo = self.local();
        let mut walk = repo.revwalk().unwrap();
        walk.push(repo.refname_to_id(BRANCH).unwrap()).unwrap();
        walk.simplify_first_parent().unwrap();
        let line = format!("Agentfs-Inbox: {n}\n");
        walk.filter(|oid| {
            repo.find_commit(*oid.as_ref().unwrap())
                .unwrap()
                .message()
                .unwrap()
                .contains(&line)
        })
        .count()
    }
}

impl Setup {
    /// Runs git in `dir`, which must succeed; its stdout.
    fn git_ok(&self, dir: &Path, args: &[&str]) -> String {
        let out = self.git(dir, args);
        assert!(
            out.status.success(),
            "git {args:?}: {}{}",
            text(&out.stdout),
            text(&out.stderr)
        );
        text(&out.stdout)
    }

    /// A producer clone on the volume's branch as the remote has it.
    fn producer_on_branch(&self, name: &str) -> PathBuf {
        let p = self.producer(name);
        self.git_ok(&p, &["checkout", "--quiet", "agentfs/demo"]);
        p
    }

    /// Writes `files` in producer `p` and commits them: the new commit.
    fn commit(&self, p: &Path, files: &[(&str, &str)], message: &str) -> Oid {
        for (name, content) in files {
            fs::write(p.join(name), content).unwrap();
            self.git_ok(p, &["add", name]);
        }
        self.git_ok(p, &["commit", "--quiet", "-m", message]);
        Oid::from_str(self.git_ok(p, &["rev-parse", "HEAD"]).trim()).unwrap()
    }

    fn branch_head(&self) -> Oid {
        self.local().refname_to_id(BRANCH).unwrap()
    }

    fn read(&self, name: &str) -> Option<String> {
        fs::read_to_string(self.tree().join(name)).ok()
    }
}

fn inbox(n: u64) -> String {
    format!("refs/agentfs/demo/inbox/{n:010}")
}

fn applied(n: u64) -> String {
    format!("refs/agentfs/demo/applied/{n:010}")
}

fn held(n: u64) -> String {
    format!("refs/agentfs/demo/held/{n:010}")
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

#[test]
fn concurrent_submitters_get_distinct_contiguous_numbers() {
    if !have_git() {
        return;
    }
    let s = Setup::new();
    s.record("a.txt", "a\n");
    s.ok(&["push"]);
    let producers: Vec<PathBuf> = (1..=8).map(|i| s.producer(&format!("p{i}"))).collect();
    let handles: Vec<_> = producers
        .iter()
        .enumerate()
        .map(|(i, p)| {
            let file = s.file(&format!("g{i}"), &format!("again {i}\n"));
            let put = format!("g{i}.txt={file}");
            let mut cmd = s.submit_command(p, &["--put", &put, "--attempts", "20"]);
            std::thread::spawn(move || cmd.output().unwrap())
        })
        .collect();
    for h in handles {
        let out = h.join().unwrap();
        assert!(out.status.success(), "{}", text(&out.stderr));
    }
    // 1..8, all distinct.
    let mut oids = std::collections::BTreeSet::new();
    for n in 1..=8 {
        let oid = s
            .remote_ref(&inbox(n))
            .unwrap_or_else(|| panic!("no inbox {n}"));
        assert!(oids.insert(oid));
    }
    assert_eq!(s.remote_ref(&inbox(9)), None);
}

#[test]
fn submit_builds_replaces_skips_and_refuses() {
    if !have_git() {
        return;
    }
    let s = Setup::new();
    s.record("a.txt", "a\n");
    s.record("gone.txt", "bye\n");
    s.ok(&["push"]);
    let head = s.remote_ref(BRANCH).unwrap();
    let p = s.producer("p");
    let file = s.file("new", "new content\n");
    let out = s.submit_ok(
        &p,
        &[
            "--put",
            &format!("dir/new.txt={file}"),
            "--delete",
            "gone.txt",
            "-m",
            "add new, drop gone",
            "--author",
            "Ada Lovelace <ada@example.com>",
        ],
    );
    assert!(
        out.contains("queued patch 1 as refs/agentfs/demo/inbox/0000000001"),
        "{out}"
    );
    let remote = Repository::open_bare(s.remote()).unwrap();
    let c = remote
        .find_commit(s.remote_ref(&inbox(1)).unwrap())
        .unwrap();
    assert_eq!(c.parent_id(0).unwrap(), head);
    assert_eq!(c.message(), Ok("add new, drop gone\n"));
    assert_eq!(c.author().name(), Ok("Ada Lovelace"));
    assert_eq!(c.author().email(), Ok("ada@example.com"));
    let tree = c.tree().unwrap();
    assert!(tree.get_path(Path::new("gone.txt")).is_err());
    assert!(tree.get_path(Path::new("a.txt")).is_ok());
    let blob = remote
        .find_blob(tree.get_path(Path::new("dir/new.txt")).unwrap().id())
        .unwrap();
    assert_eq!(blob.content(), b"new content\n");

    // A commit made in the producer, replacing 1.
    let out = s.git(&p, &["checkout", "--quiet", "agentfs/demo"]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    let file_path = p.join("a.txt");
    fs::write(&file_path, "edited\n").unwrap();
    let out = s.git(&p, &["commit", "--quiet", "-am", "edit a"]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    let out = s.submit_ok(&p, &["HEAD", "--replace", "1"]);
    assert!(out.contains("queued patch 1"), "{out}");
    let replaced = s.remote_ref(&inbox(1)).unwrap();
    assert_ne!(replaced, c.id());
    assert_eq!(
        remote.find_commit(replaced).unwrap().summary(),
        Ok(Some("edit a"))
    );
    // Not queued: nothing to replace.
    let out = s.submit(&p, &["HEAD", "--replace", "7"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(
        text(&out.stderr).contains("not queued"),
        "{}",
        text(&out.stderr)
    );

    // Skip: an empty commit on the head.
    s.submit_ok(&p, &["--skip", "1"]);
    let skip = remote
        .find_commit(s.remote_ref(&inbox(1)).unwrap())
        .unwrap();
    assert_eq!(skip.parent_id(0).unwrap(), head);
    assert_eq!(skip.tree_id(), remote.find_commit(head).unwrap().tree_id());

    // A parent the branch does not have.
    let out = s.git(&p, &["checkout", "--quiet", "--orphan", "elsewhere"]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    let out = s.git(&p, &["commit", "--quiet", "-m", "root"]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    fs::write(p.join("x.txt"), "x\n").unwrap();
    s.git(&p, &["add", "x.txt"]);
    let out = s.git(&p, &["commit", "--quiet", "-m", "on a foreign root"]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    let out = s.submit(&p, &["HEAD"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(
        text(&out.stderr).contains("shares no history with the volume's branch"),
        "{}",
        text(&out.stderr)
    );
    assert_eq!(s.remote_ref(&inbox(2)), None);
}

/// Design §18 test 15 at the CLI.
/// Acceptance 1 (bead agentfs-mia.2): 2 conflicts and is held whole
/// while 1 and 3 land; a fetch that brings inbox/2 again does not call it
/// stale; replaced with a rebased patch, it lands after 3.
#[test]
fn three_patches_the_second_conflicts() {
    if !have_git() {
        return;
    }
    let s = Setup::new();
    s.record("a.txt", "a0\n");
    s.record("b.txt", "b0\n");
    s.ok(&["push"]);
    let p = s.producer("p");
    let a1 = s.file("a1", "a1\n");
    let b1 = s.file("b1", "b-from-iac\n");
    let c1 = s.file("c1", "c\n");
    s.submit_ok(&p, &["--put", &format!("a.txt={a1}"), "-m", "one"]);
    s.submit_ok(&p, &["--put", &format!("b.txt={b1}"), "-m", "two"]);
    s.submit_ok(&p, &["--put", &format!("c.txt={c1}"), "-m", "three"]);
    // The workload changes b.txt meanwhile.
    s.record("b.txt", "b-from-workload\n");

    let out = s.agentfs(&["patch", "apply"]);
    let err = text(&out.stderr);
    assert_eq!(out.status.code(), Some(1), "{err}");
    assert!(err.contains("patch 1 applied as "), "{err}");
    assert!(err.contains("patch 3 applied as "), "{err}");
    assert!(err.contains("patch 2 held (conflict): "), "{err}");
    assert!(
        err.contains("patches: through 3 applied, 1 queued (1 set aside)"),
        "{err}"
    );
    assert!(err.contains("alarm: patch 2 ("), "{err}");
    assert!(!err.contains("blocked"), "{err}");
    assert_eq!(s.read("a.txt").as_deref(), Some("a1\n"));
    assert_eq!(s.read("b.txt").as_deref(), Some("b-from-workload\n"));
    assert_eq!(s.read("c.txt").as_deref(), Some("c\n"));
    assert_eq!(s.inbox_commits(1), 1);
    assert_eq!(s.inbox_commits(3), 1);
    let p2 = s.remote_ref(&inbox(2)).unwrap();
    assert_eq!(s.local().refname_to_id(&held(2)).ok(), Some(p2));

    let status = s.ok(&["status"]);
    assert!(
        status.contains("patches:   last applied 3, 1 queued (1 set aside), ok (from origin)"),
        "{status}"
    );
    let json = s.ok(&["patch", "status", "--json"]);
    let doc: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!(doc["status"]["blocked"], serde_json::Value::Null);
    let queue = doc["status"]["queue"].as_array().unwrap();
    assert_eq!(queue.len(), 1);
    assert_eq!(queue[0]["inbox"], 2);
    assert_eq!(queue[0]["verdict"]["kind"], "held");
    assert_eq!(queue[0]["verdict"]["reason"], "conflict");
    assert_eq!(queue[0]["verdict"]["paths"], serde_json::json!(["b.txt"]));
    assert_eq!(doc["cache"]["queue_depth"], 1);
    assert_eq!(doc["cache"]["held"], 1);
    let human = s.ok(&["patch", "status"]);
    assert!(human.contains("held:      2 (conflict): "), "{human}");

    // The push carries the branch, then the queue; inbox/2 stays.
    let out = s.ok(&["push"]);
    assert!(
        out.contains(
            "origin: queue: status pushed, 2 applied ref(s) created, 2 inbox ref(s) deleted"
        ),
        "{out}"
    );
    let local_head = s.branch_head();
    assert_eq!(s.remote_ref(BRANCH), Some(local_head));
    for n in [1, 3] {
        assert!(s.remote_ref(&applied(n)).is_some(), "applied {n}");
        assert_eq!(s.remote_ref(&inbox(n)), None, "inbox {n}");
    }
    assert_eq!(s.remote_ref(&inbox(2)), Some(p2));
    assert_eq!(s.remote_ref(&held(2)), None, "held marks stay local");
    let remote = Repository::open_bare(s.remote()).unwrap();
    let status = remote
        .find_commit(s.remote_ref("refs/agentfs/demo/status").unwrap())
        .unwrap();
    let blob = remote
        .find_blob(
            status
                .tree()
                .unwrap()
                .get_path(Path::new("status.json"))
                .unwrap()
                .id(),
        )
        .unwrap();
    let doc: serde_json::Value = serde_json::from_slice(blob.content()).unwrap();
    assert_eq!(doc["blocked"], serde_json::Value::Null);
    assert_eq!(doc["queue"][0]["verdict"]["kind"], "held");

    // A fetch brings inbox/2 again: judged again, never stale.
    let out = s.agentfs(&["patch", "apply"]);
    let err = text(&out.stderr);
    assert_eq!(out.status.code(), Some(1), "{err}");
    assert!(!err.contains("stale"), "{err}");
    assert!(err.contains("patch 2 held (conflict)"), "{err}");
    assert_eq!(s.branch_head(), local_head);

    // A crash between the commit and the ref moves.
    let local = s.local();
    let p3 = local.refname_to_id(&applied(3)).unwrap();
    local.find_reference(&applied(3)).unwrap().delete().unwrap();
    local.reference(&inbox(3), p3, true, "test").unwrap();
    fs::remove_file(s.state().join("meta/patch.json")).unwrap();
    let out = s.agentfs(&["patch", "apply", "--no-fetch"]);
    let err = text(&out.stderr);
    assert_eq!(out.status.code(), Some(1), "{err}");
    assert!(err.contains("were already applied"), "{err}");
    assert_eq!(s.inbox_commits(3), 1);
    assert_eq!(s.branch_head(), local_head);

    // Resolved: 2 rebased onto the pushed head. It is below the last
    // applied number, and replaceable because the status still lists it.
    let b2 = s.file("b2", "b-merged\n");
    s.submit_ok(
        &p,
        &[
            "--put",
            &format!("b.txt={b2}"),
            "--replace",
            "2",
            "-m",
            "two, rebased",
        ],
    );
    let out = s.agentfs(&["patch", "apply"]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    assert_eq!(s.read("b.txt").as_deref(), Some("b-merged\n"));
    assert_eq!(s.read("c.txt").as_deref(), Some("c\n"));
    for n in 1..=3 {
        assert_eq!(s.inbox_commits(n), 1, "patch {n}");
    }
    // Inbox 2 lands after inbox 3, on top of it.
    let local = s.local();
    let head = local.find_commit(s.branch_head()).unwrap();
    assert!(
        head.message().unwrap().contains("Agentfs-Inbox: 2\n"),
        "{:?}",
        head.message()
    );
    assert_eq!(head.parent_id(0).unwrap(), local_head);
    assert!(s.local().refname_to_id(&held(2)).is_err());
    let out = s.ok(&["push"]);
    assert!(out.contains("1 inbox ref(s) deleted"), "{out}");
    for n in 1..=3 {
        assert_eq!(s.remote_ref(&inbox(n)), None, "inbox {n}");
        assert!(s.remote_ref(&applied(n)).is_some(), "applied {n}");
    }
    let status = s.ok(&["status"]);
    assert!(
        status.contains("patches:   last applied 3, 0 queued, ok"),
        "{status}"
    );
    // A new submission numbers after the applied ones.
    let d = s.file("d", "d\n");
    let out = s.submit_ok(&p, &["--put", &format!("d.txt={d}")]);
    assert!(out.contains("queued patch 4"), "{out}");
}

/// Five lines, so edits at the first and the last merge cleanly.
const FIVE: &str = "1\n2\n3\n4\n5\n";

impl Setup {
    /// Queues, in producer `p`: 1 on a.txt, 2 on f.txt's first line (which
    /// the workload then changes too, so 2 conflicts), 3 on f.txt's last
    /// line (clean on its own), 4 on g.txt; `barrier` makes 2 a barrier.
    fn conflict_and_followers(&self, p: &Path, barrier: bool) {
        let put = |name: &str, path: &str, content: &str, extra: &[&str]| {
            let file = self.file(name, content);
            let mut args = vec!["--put".to_owned(), format!("{path}={file}")];
            args.extend(extra.iter().map(|a| a.to_string()));
            let args: Vec<&str> = args.iter().map(String::as_str).collect();
            self.submit_ok(p, &args);
        };
        put("p1", "a.txt", "a1\n", &[]);
        let two: &[&str] = if barrier { &["--barrier"] } else { &[] };
        put("p2", "f.txt", "uno\n2\n3\n4\n5\n", two);
        put("p3", "f.txt", "1\n2\n3\n4\nfive\n", &[]);
        put("p4", "g.txt", "g1\n", &[]);
        self.record("f.txt", "one\n2\n3\n4\n5\n");
    }
}

/// Acceptance 2: a later patch on the held patch's file waits behind it;
/// one on another file lands; once 2 is skipped, 3 lands.
#[test]
fn a_later_patch_on_the_same_file_waits() {
    if !have_git() {
        return;
    }
    let s = Setup::new();
    s.record("f.txt", FIVE);
    s.record("g.txt", "g0\n");
    s.ok(&["push"]);
    let p = s.producer("p");
    s.conflict_and_followers(&p, false);

    let out = s.agentfs(&["patch", "apply"]);
    let err = text(&out.stderr);
    assert_eq!(out.status.code(), Some(1), "{err}");
    assert!(err.contains("patch 2 held (conflict)"), "{err}");
    assert!(
        err.contains("patch 3 waiting behind 2 (same_files)"),
        "{err}"
    );
    assert!(err.contains("patch 4 applied as "), "{err}");
    assert_eq!(s.read("g.txt").as_deref(), Some("g1\n"));
    assert_eq!(s.read("f.txt").as_deref(), Some("one\n2\n3\n4\n5\n"));
    let human = s.ok(&["patch", "status"]);
    assert!(
        human.contains("waiting:   3 behind 2 (same_files)"),
        "{human}"
    );
    let json = s.ok(&["patch", "status", "--json"]);
    let doc: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!(
        doc["status"]["queue"][1]["verdict"],
        serde_json::json!({"kind": "waiting", "why": "same_files", "behind": [2]})
    );

    s.submit_ok(&p, &["--skip", "2"]);
    let out = s.agentfs(&["patch", "apply"]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    assert_eq!(s.read("f.txt").as_deref(), Some("one\n2\n3\n4\nfive\n"));
    for n in 1..=4 {
        assert_eq!(s.inbox_commits(n), 1, "patch {n}");
    }
}

/// Acceptance 3: a held barrier holds every later patch, on any file.
#[test]
fn a_barrier_holds_everything_behind_it() {
    if !have_git() {
        return;
    }
    let s = Setup::new();
    s.record("f.txt", FIVE);
    s.record("g.txt", "g0\n");
    s.ok(&["push"]);
    let p = s.producer("p");
    s.conflict_and_followers(&p, true);
    let remote = Repository::open_bare(s.remote()).unwrap();
    let barrier = remote
        .find_commit(s.remote_ref(&inbox(2)).unwrap())
        .unwrap();
    assert!(
        barrier
            .message()
            .unwrap()
            .ends_with("\n\nAgentfs-Barrier: yes\n"),
        "{:?}",
        barrier.message()
    );

    let out = s.agentfs(&["patch", "apply"]);
    let err = text(&out.stderr);
    assert_eq!(out.status.code(), Some(1), "{err}");
    assert!(err.contains("patch 1 applied as "), "{err}");
    assert!(err.contains("patch 2 held (conflict)"), "{err}");
    for n in [3, 4] {
        assert!(
            err.contains(&format!("patch {n} waiting behind 2 (barrier)")),
            "{err}"
        );
    }
    assert_eq!(s.read("g.txt").as_deref(), Some("g0\n"));
    assert_eq!(s.inbox_commits(4), 0);

    // A given commit must carry the trailer itself.
    let q = s.producer_on_branch("q");
    s.commit(&q, &[("h.txt", "h\n")], "add h");
    let out = s.submit(&q, &["--barrier", "HEAD"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(
        text(&out.stderr).contains("does not carry `Agentfs-Barrier: yes`"),
        "{}",
        text(&out.stderr)
    );
}

/// Acceptance 4: with `agentfs.patchAccept=approval` a patch awaits its
/// approval, lands once approved, and a replacement needs approving again.
#[test]
fn approval_rule() {
    if !have_git() {
        return;
    }
    let s = Setup::new();
    let mut config = git2::Config::open(&s.state().join("git/config")).unwrap();
    config.set_str("agentfs.patchAccept", "approval").unwrap();
    s.record("a.txt", "a0\n");
    s.ok(&["push"]);
    let p = s.producer("p");
    let approve = |n: &str, extra: &[&str]| {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_agentfs"));
        cmd.args(["patch", "approve", n, "--volume", "demo", "--repo"])
            .arg(&p)
            .args(extra);
        s.hermetic(&mut cmd);
        cmd.output().unwrap()
    };
    let a1 = s.file("a1", "a1\n");
    s.submit_ok(&p, &["--put", &format!("a.txt={a1}"), "-m", "one"]);

    let out = s.agentfs(&["patch", "apply"]);
    let err = text(&out.stderr);
    assert_eq!(out.status.code(), Some(1), "{err}");
    assert!(err.contains("patch 1 awaiting (approval)"), "{err}");
    assert!(!err.contains("alarm: "), "awaiting is no alarm: {err}");
    assert_eq!(s.read("a.txt").as_deref(), Some("a0\n"));
    let human = s.ok(&["patch", "status"]);
    assert!(human.contains("awaiting:  1 (approval)"), "{human}");

    // Approving another tip than the queued one is refused.
    let out = approve("1", &["--patch", &"0".repeat(40)]);
    assert_eq!(out.status.code(), Some(1));
    assert!(
        text(&out.stderr).contains("the queued patch is now "),
        "{}",
        text(&out.stderr)
    );
    let out = approve("1", &[]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    let tip = s.remote_ref(&inbox(1)).unwrap();
    assert!(
        text(&out.stdout).contains(&format!(
            "approved patch 1 ({tip}) as refs/agentfs/demo/approved/0000000001"
        )),
        "{}",
        text(&out.stdout)
    );
    let out = s.agentfs(&["patch", "apply"]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    assert_eq!(s.read("a.txt").as_deref(), Some("a1\n"));
    assert_eq!(s.inbox_commits(1), 1);

    // 2, approved, then replaced: awaiting again (P4).
    let b1 = s.file("b1", "b1\n");
    s.submit_ok(&p, &["--put", &format!("b.txt={b1}"), "-m", "two"]);
    let out = approve("2", &[]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    let b2 = s.file("b2", "b2\n");
    s.submit_ok(
        &p,
        &[
            "--put",
            &format!("b.txt={b2}"),
            "--replace",
            "2",
            "-m",
            "two'",
        ],
    );
    let out = s.agentfs(&["patch", "apply"]);
    let err = text(&out.stderr);
    assert_eq!(out.status.code(), Some(1), "{err}");
    assert!(err.contains("patch 2 awaiting (approval)"), "{err}");
    assert_eq!(s.read("b.txt"), None);
    let out = approve("2", &[]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    let out = s.agentfs(&["patch", "apply"]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    assert_eq!(s.read("b.txt").as_deref(), Some("b2\n"));
}

/// Acceptance 5: `agentfs.patchAccept=command` runs
/// `$GIT_DIR/hooks/patch-accept`: 0 lands, 1 holds as refused, 75 awaits;
/// another status, the timeout and a missing hook leave the patch not
/// judged and raise `PatchRuleFailed` (P5).
#[test]
fn command_rule() {
    if !have_git() {
        return;
    }
    let s = Setup::new();
    let git_dir = s.state().join("git");
    let mut config = git2::Config::open(&git_dir.join("config")).unwrap();
    config.set_str("agentfs.patchAccept", "command").unwrap();
    config.set_str("agentfs.patchAcceptTimeout", "1s").unwrap();
    s.record("a.txt", "a0\n");
    s.ok(&["push"]);
    let p = s.producer("p");
    let a1 = s.file("a1", "a1\n");
    s.submit_ok(&p, &["--put", &format!("a.txt={a1}"), "-m", "one"]);
    let apply = || {
        let out = s.agentfs(&["patch", "apply", "--no-fetch"]);
        (out.status.code(), text(&out.stderr))
    };
    let not_judged = "patch 1 not judged (command): ";
    let alarm = "the acceptance rule command could not judge patch 1";

    // No hook (this run fetches the inbox).
    let out = s.agentfs(&["patch", "apply"]);
    let (code, err) = (out.status.code(), text(&out.stderr));
    assert_eq!(code, Some(1), "{err}");
    assert!(err.contains(not_judged), "{err}");
    assert!(err.contains("could not start"), "{err}");
    assert!(err.contains(alarm), "{err}");

    let work = s.dir.path().join("hook");
    fs::create_dir_all(&work).unwrap();
    let hook = git_dir.join("hooks/patch-accept");
    fs::create_dir_all(hook.parent().unwrap()).unwrap();
    fs::write(
        &hook,
        format!(
            "#!/bin/sh\ncat > '{w}/input.json'\nmode=$(cat '{w}/mode')\ncase \"$mode\" in\n  \
             sleep) sleep 30 ;;\n  refuse) echo 'not on a Friday' >&2; echo more >&2; exit 1 ;;\n  \
             *) exit \"$mode\" ;;\nesac\n",
            w = work.display()
        ),
    )
    .unwrap();
    {
        use std::os::unix::fs::PermissionsExt as _;
        fs::set_permissions(&hook, fs::Permissions::from_mode(0o755)).unwrap();
    }
    let mode = |m: &str| fs::write(work.join("mode"), m).unwrap();

    mode("2");
    let (code, err) = apply();
    assert_eq!(code, Some(1), "{err}");
    assert!(err.contains(not_judged), "{err}");
    assert!(err.contains("exited with 2"), "{err}");
    assert!(err.contains(alarm), "{err}");

    mode("75");
    let (code, err) = apply();
    assert_eq!(code, Some(1), "{err}");
    assert!(err.contains("patch 1 awaiting (command)"), "{err}");
    assert!(!err.contains("alarm: "), "{err}");

    mode("sleep");
    let started = std::time::Instant::now();
    let (code, err) = apply();
    assert!(
        started.elapsed() < std::time::Duration::from_secs(20),
        "{:?}",
        started.elapsed()
    );
    assert_eq!(code, Some(1), "{err}");
    assert!(err.contains(not_judged), "{err}");
    assert!(err.contains("killed after"), "{err}");

    mode("refuse");
    let (code, err) = apply();
    assert_eq!(code, Some(1), "{err}");
    assert!(
        err.contains("patch 1 held (refused): not on a Friday\n"),
        "{err}"
    );
    let input: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(work.join("input.json")).unwrap()).unwrap();
    let tip = s.local().refname_to_id(&inbox(1)).unwrap();
    assert_eq!(input["schema"], 1);
    assert_eq!(input["volume"], "demo");
    assert_eq!(input["inbox"], 1);
    assert_eq!(input["tip"], tip.to_string());
    assert_eq!(input["head"], s.branch_head().to_string());
    assert_eq!(input["commits"], serde_json::json!([tip.to_string()]));
    assert_eq!(input["paths"], serde_json::json!(["a.txt"]));
    assert_eq!(input["barrier"], false);
    assert_eq!(s.read("a.txt").as_deref(), Some("a0\n"));

    mode("0");
    let (code, err) = apply();
    assert_eq!(code, Some(0), "{err}");
    assert_eq!(s.read("a.txt").as_deref(), Some("a1\n"));
    assert_eq!(s.inbox_commits(1), 1);
}

/// Acceptance 7: a held patch withdrawn on the remote loses its mark
/// (`PatchIgnored`, withdrawn), and the patch waiting on it lands.
#[test]
fn a_withdrawn_patch_frees_its_waiters() {
    if !have_git() {
        return;
    }
    let s = Setup::new();
    s.record("f.txt", FIVE);
    s.record("g.txt", "g0\n");
    s.ok(&["push"]);
    let p = s.producer("p");
    s.conflict_and_followers(&p, false);
    let out = s.agentfs(&["patch", "apply"]);
    assert_eq!(out.status.code(), Some(1), "{}", text(&out.stderr));
    assert!(s.local().refname_to_id(&held(2)).is_ok());

    Repository::open_bare(s.remote())
        .unwrap()
        .find_reference(&inbox(2))
        .unwrap()
        .delete()
        .unwrap();
    let out = s.agentfs(&["patch", "apply"]);
    let err = text(&out.stderr);
    assert!(out.status.success(), "{err}");
    assert!(
        err.contains("patch queue: ignoring refs/agentfs/demo/inbox/0000000002 (withdrawn)"),
        "{err}"
    );
    assert!(err.contains("alarm: "), "{err}");
    assert!(err.contains("patch 3 applied as "), "{err}");
    assert!(s.local().refname_to_id(&held(2)).is_err());
    assert_eq!(s.read("f.txt").as_deref(), Some("one\n2\n3\n4\nfive\n"));
    assert_eq!(s.inbox_commits(2), 0);
    assert_eq!(s.inbox_commits(3), 1);
}

#[test]
fn apply_needs_the_lock_and_a_root() {
    if !have_git() {
        return;
    }
    let s = Setup::new();
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_agentfs"));
    cmd.arg("--state").arg(s.state()).args(["patch", "apply"]);
    s.hermetic(&mut cmd);
    let out = cmd.output().unwrap();
    assert_eq!(out.status.code(), Some(1));
    assert!(
        text(&out.stderr).contains("--root"),
        "{}",
        text(&out.stderr)
    );
    // Nothing queued, no remote branch yet: nothing to do.
    let out = s.agentfs(&["patch", "apply"]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    let status = s.ok(&["status"]);
    assert!(
        status.contains("patches:   none applied, 0 queued, ok"),
        "{status}"
    );
}

/// A gap (a number missing below the highest one) is cleared with
/// `--skip N`, and the queue moves on.
#[test]
fn a_gap_is_cleared_with_skip() {
    if !have_git() {
        return;
    }
    let s = Setup::new();
    s.record("a.txt", "a0\n");
    s.ok(&["push"]);
    let p = s.producer("p");
    let a1 = s.file("a1", "a1\n");
    let b1 = s.file("b1", "b1\n");
    s.submit_ok(&p, &["--put", &format!("a.txt={a1}"), "-m", "one"]);
    s.submit_ok(&p, &["--put", &format!("b.txt={b1}"), "-m", "two"]);
    // Patch 1 vanishes from the remote.
    Repository::open_bare(s.remote())
        .unwrap()
        .find_reference(&inbox(1))
        .unwrap()
        .delete()
        .unwrap();
    let out = s.agentfs(&["patch", "apply"]);
    let err = text(&out.stderr);
    assert_eq!(out.status.code(), Some(1), "{err}");
    assert!(err.contains("patches: blocked at 1 (gap)"), "{err}");
    assert!(err.contains("`agentfs patch submit --skip 1`"), "{err}");

    // As the hint says.
    let out = s.submit_ok(&p, &["--skip", "1"]);
    assert!(out.contains("queued patch 1"), "{out}");
    let out = s.agentfs(&["patch", "apply"]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    assert_eq!(fs::read_to_string(s.tree().join("a.txt")).unwrap(), "a0\n");
    assert_eq!(fs::read_to_string(s.tree().join("b.txt")).unwrap(), "b1\n");
    assert_eq!(s.inbox_commits(1), 1);
    assert_eq!(s.inbox_commits(2), 1);

    // Applied: skipping it now is refused, by the applied ref or, without
    // it (held back while the branch cannot push), by the remote status.
    s.ok(&["push"]);
    let out = s.submit(&p, &["--skip", "2"]);
    assert!(
        text(&out.stderr).contains("already applied"),
        "{}",
        text(&out.stderr)
    );
    Repository::open_bare(s.remote())
        .unwrap()
        .find_reference(&applied(2))
        .unwrap()
        .delete()
        .unwrap();
    let out = s.submit(&p, &["--skip", "2"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(
        text(&out.stderr).contains("already applied"),
        "{}",
        text(&out.stderr)
    );
}

/// A queue that cannot run at all is an alarm and reads as blocked
/// (`failed`) in `agentfs status`, not as the previous run's state.
#[test]
fn a_queue_that_cannot_run_reads_as_blocked() {
    if !have_git() {
        return;
    }
    let s = Setup::new();
    s.record("a.txt", "a0\n");
    s.ok(&["patch", "apply", "--no-fetch"]);
    // The status ref cannot be written: a ref sits beneath its name. (A
    // lock file would not do: the open removes it as a crash's.)
    let repo = Repository::open_bare(s.state().join("git")).unwrap();
    let head = repo.refname_to_id("refs/heads/agentfs/demo").unwrap();
    if let Ok(mut r) = repo.find_reference("refs/agentfs/demo/status") {
        r.delete().unwrap();
    }
    repo.reference("refs/agentfs/demo/status/x", head, true, "block")
        .unwrap();
    let out = s.agentfs(&["patch", "apply", "--no-fetch"]);
    let err = text(&out.stderr);
    assert_eq!(out.status.code(), Some(1), "{err}");
    assert!(err.contains("alarm: "), "{err}");
    let status = s.ok(&["status"]);
    assert!(status.contains("blocked at 0 (failed)"), "{status}");
    let json = s.ok(&["patch", "status", "--json"]);
    let doc: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!(doc["cache"]["blocked"]["reason"], "failed");
}

/// Acceptance 1 (bead agentfs-mia.4): a branch of two commits lands as one
/// patch commit on a head that moved since the branch was cut.
#[test]
fn a_branch_lands_as_one_patch() {
    if !have_git() {
        return;
    }
    let s = Setup::new();
    s.record("seed.txt", "seed\n");
    s.ok(&["push"]);
    let base = s.remote_ref(BRANCH).unwrap();
    let p = s.producer_on_branch("p");
    let a = s.commit(&p, &[("a.txt", "a\n")], "add a");
    let b = s.commit(&p, &[("b.txt", "b\n")], "add b");
    let out = s.submit_ok(&p, &["HEAD"]);
    assert!(
        out.contains(&format!(
            "queued patch 1 as refs/agentfs/demo/inbox/0000000001 ({b}, 2 commit(s) on {base})"
        )),
        "{out}"
    );
    // The workload records a change meanwhile.
    s.record("w.txt", "w\n");
    let before = s.branch_head();

    let out = s.agentfs(&["patch", "apply"]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    assert_eq!(s.read("a.txt").as_deref(), Some("a\n"));
    assert_eq!(s.read("b.txt").as_deref(), Some("b\n"));
    assert_eq!(s.read("w.txt").as_deref(), Some("w\n"));
    let local = s.local();
    let landed = local.find_commit(s.branch_head()).unwrap();
    assert_eq!(landed.parent_count(), 1);
    assert_eq!(landed.parent_id(0).unwrap(), before);
    let message = landed.message().unwrap();
    let trailers = message.trim_end().rsplit("\n\n").next().unwrap();
    assert!(
        trailers.contains(&format!("Agentfs-Patch: {b}\n")),
        "{message}"
    );
    assert!(trailers.contains("Agentfs-Inbox: 1"), "{message}");
    assert!(message.starts_with("add b\n\n"), "{message}");
    let short = |o: Oid| o.to_string()[..12].to_owned();
    assert!(
        message.contains(&format!("\n{} add a\n{} add b\n", short(a), short(b))),
        "{message}"
    );
    assert_eq!(s.inbox_commits(1), 1);
    assert_eq!(local.refname_to_id(&applied(1)).unwrap(), b);
    let tree = landed.tree().unwrap();
    for name in ["a.txt", "b.txt", "w.txt", "seed.txt"] {
        assert!(tree.get_path(Path::new(name)).is_ok(), "{name}");
    }
}

/// Acceptance 2: a branch cut two commits behind the head, touching other
/// files, merges cleanly against its base.
#[test]
fn a_branch_on_an_older_head_merges() {
    if !have_git() {
        return;
    }
    let s = Setup::new();
    s.record("seed.txt", "one\ntwo\nthree\n");
    s.ok(&["push"]);
    let p = s.producer_on_branch("p");
    s.record("x.txt", "x\n");
    s.record("y.txt", "y\n");
    s.ok(&["push"]);
    s.commit(
        &p,
        &[("seed.txt", "one\ntwo\nthree, by iac\n")],
        "edit seed",
    );
    s.commit(&p, &[("c.txt", "c\n")], "add c");
    s.submit_ok(&p, &["HEAD"]);
    let out = s.agentfs(&["patch", "apply"]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    assert_eq!(
        s.read("seed.txt").as_deref(),
        Some("one\ntwo\nthree, by iac\n")
    );
    assert_eq!(s.read("c.txt").as_deref(), Some("c\n"));
    assert_eq!(s.read("x.txt").as_deref(), Some("x\n"));
    assert_eq!(s.read("y.txt").as_deref(), Some("y\n"));
    assert_eq!(s.inbox_commits(1), 1);
}

/// Acceptance 3 (bead agentfs-mia.4, held whole since mia.2): a branch
/// changing a path the workload changed too is held with a conflict
/// naming it; nothing is written and it stays queued.
#[test]
fn a_branch_that_conflicts_is_held() {
    if !have_git() {
        return;
    }
    let s = Setup::new();
    s.record("seed.txt", "seed\n");
    s.ok(&["push"]);
    let p = s.producer_on_branch("p");
    s.commit(&p, &[("a.txt", "a\n")], "add a");
    s.commit(&p, &[("seed.txt", "seed, by iac\n")], "edit seed");
    s.submit_ok(&p, &["HEAD"]);
    s.record("seed.txt", "seed, by the workload\n");
    let head = s.branch_head();

    let out = s.agentfs(&["patch", "apply"]);
    let err = text(&out.stderr);
    assert_eq!(out.status.code(), Some(1), "{err}");
    assert!(err.contains("patch 1 held (conflict)"), "{err}");
    assert!(err.contains("conflicts in seed.txt"), "{err}");
    assert_eq!(s.branch_head(), head);
    assert_eq!(
        s.read("seed.txt").as_deref(),
        Some("seed, by the workload\n")
    );
    assert_eq!(s.read("a.txt"), None);
    assert!(s.local().refname_to_id(&inbox(1)).is_ok());
    let json = s.ok(&["patch", "status", "--json"]);
    let doc: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!(doc["status"]["queue"][0]["verdict"]["reason"], "conflict");
    assert_eq!(
        doc["status"]["queue"][0]["verdict"]["paths"],
        serde_json::json!(["seed.txt"])
    );
    assert_eq!(doc["cache"]["queue_depth"], 1);
}

/// Acceptance 4: the volume's own `.gitattributes` never steers a merge,
/// even when the state area's HEAD names the volume's branch (some git
/// versions, ubuntu-24.04's 2.43 among them, read a bare repository's
/// attributes from `HEAD:.gitattributes`; 2.47 does not, so only those
/// catch a merge without the `attr.tree` override).
/// Here it asks for `merge=union`, which would merge both sides' edits.
#[test]
fn workload_attributes_do_not_steer_the_merge() {
    if !have_git() {
        return;
    }
    let s = Setup::new();
    s.record(".gitattributes", "f.txt merge=union\n");
    s.record("f.txt", "f\n");
    s.ok(&["push"]);
    let p = s.producer_on_branch("p");
    s.commit(&p, &[("f.txt", "f, by iac\n")], "edit f");
    s.submit_ok(&p, &["HEAD"]);
    s.record("f.txt", "f, by the workload\n");
    s.local().set_head(BRANCH).unwrap();

    let out = s.agentfs(&["patch", "apply"]);
    let err = text(&out.stderr);
    assert_eq!(out.status.code(), Some(1), "{err}");
    assert!(err.contains("patch 1 held (conflict)"), "{err}");
    assert_eq!(s.read("f.txt").as_deref(), Some("f, by the workload\n"));
}

/// Acceptance 5: a merge commit inside the branch is refused by submit,
/// and held as `merge_commit` when pushed with plain git.
#[test]
fn a_merge_inside_the_branch_is_refused() {
    if !have_git() {
        return;
    }
    let s = Setup::new();
    s.record("seed.txt", "seed\n");
    s.ok(&["push"]);
    let p = s.producer_on_branch("p");
    // The remote head itself is no patch.
    let out = s.submit(&p, &["HEAD"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(
        text(&out.stderr).contains("already on the volume's branch"),
        "{}",
        text(&out.stderr)
    );
    s.git_ok(&p, &["checkout", "--quiet", "-b", "side"]);
    s.commit(&p, &[("side.txt", "side\n")], "side");
    s.git_ok(&p, &["checkout", "--quiet", "agentfs/demo"]);
    s.commit(&p, &[("a.txt", "a\n")], "add a");
    s.git_ok(
        &p,
        &["merge", "--quiet", "--no-ff", "-m", "merge side", "side"],
    );
    let merge = Oid::from_str(s.git_ok(&p, &["rev-parse", "HEAD"]).trim()).unwrap();
    s.commit(&p, &[("b.txt", "b\n")], "add b");

    let out = s.submit(&p, &["HEAD"]);
    let err = text(&out.stderr);
    assert_eq!(out.status.code(), Some(1), "{err}");
    assert!(
        err.contains(&format!(
            "{merge} in the patch has 2 parents; a patch is a line of single-parent commits"
        )),
        "{err}"
    );
    assert_eq!(s.remote_ref(&inbox(1)), None);

    // Pushed with plain git, the queue refuses it.
    s.git_ok(
        &p,
        &["push", "--quiet", "origin", &format!("HEAD:{}", inbox(1))],
    );
    let head = s.branch_head();
    let out = s.agentfs(&["patch", "apply"]);
    let err = text(&out.stderr);
    assert_eq!(out.status.code(), Some(1), "{err}");
    assert!(err.contains("patch 1 held (merge_commit)"), "{err}");
    assert!(
        err.contains(&format!("{merge} in the patch has 2 parents")),
        "{err}"
    );
    assert_eq!(s.branch_head(), head);
    assert_eq!(s.read("a.txt"), None);
}

impl Setup {
    /// The state area's git config, to set keys in.
    fn config(&self) -> git2::Config {
        git2::Config::open(&self.state().join("git/config")).unwrap()
    }

    /// Writes the operator's `$GIT_DIR/info/attributes`.
    fn info_attributes(&self, rules: &str) {
        let info = self.state().join("git/info");
        fs::create_dir_all(&info).unwrap();
        fs::write(info.join("attributes"), rules).unwrap();
    }

    /// `f.txt` recorded and pushed, a patch from producer `p` editing it
    /// (and adding `g.txt`), then the workload's own edit of `f.txt`.
    fn both_edit_f(&self) {
        self.record("f.txt", "one\n");
        self.ok(&["push"]);
        let p = self.producer_on_branch("p");
        self.commit(&p, &[("f.txt", "one\niac\n"), ("g.txt", "g\n")], "edit f");
        self.submit_ok(&p, &["HEAD"]);
        self.record("f.txt", "one\nworkload\n");
    }
}

/// Merge rules (bead agentfs-mia.3), acceptance 1: a branch whose first
/// commit conflicts with the head and whose second commit reverts that
/// hunk. `agentfs.patchIntegrate=rebase` replays it commit by commit and
/// holds it at the first; `merge` (the default) merges the whole diff,
/// which no longer touches the conflicting hunk, and lands it (P1).
#[test]
fn rebase_replays_commit_by_commit() {
    if !have_git() {
        return;
    }
    let s = Setup::new();
    s.record("f.txt", "f\n");
    s.ok(&["push"]);
    let p = s.producer_on_branch("p");
    s.commit(&p, &[("f.txt", "f, by iac\n")], "edit f");
    s.commit(&p, &[("f.txt", "f\n"), ("g.txt", "g\n")], "revert f, add g");
    s.submit_ok(&p, &["HEAD"]);
    s.record("f.txt", "f, by the workload\n");
    s.config()
        .set_str("agentfs.patchIntegrate", "rebase")
        .unwrap();

    let out = s.agentfs(&["patch", "apply"]);
    let err = text(&out.stderr);
    assert_eq!(out.status.code(), Some(1), "{err}");
    assert!(err.contains("patch 1 held (conflict)"), "{err}");
    assert!(err.contains("conflicts in f.txt"), "{err}");
    assert_eq!(s.read("g.txt"), None);

    s.config()
        .set_str("agentfs.patchIntegrate", "merge")
        .unwrap();
    let out = s.agentfs(&["patch", "apply"]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    assert_eq!(s.read("f.txt").as_deref(), Some("f, by the workload\n"));
    assert_eq!(s.read("g.txt").as_deref(), Some("g\n"));
    assert_eq!(s.inbox_commits(1), 1);
}

/// Acceptance 2: `merge=union` in the workload's own `.gitattributes`
/// does not steer the merge (a conflict, P2); the same rule in the
/// operator's `$GIT_DIR/info/attributes` does: both edits land.
#[test]
fn an_operator_merge_driver_resolves() {
    if !have_git() {
        return;
    }
    let s = Setup::new();
    s.record(".gitattributes", "*.txt merge=union\n");
    s.both_edit_f();
    // Some git versions read a bare repository's attributes from HEAD.
    s.local().set_head(BRANCH).unwrap();

    let out = s.agentfs(&["patch", "apply"]);
    let err = text(&out.stderr);
    assert_eq!(out.status.code(), Some(1), "{err}");
    assert!(err.contains("patch 1 held (conflict)"), "{err}");
    assert_eq!(s.read("f.txt").as_deref(), Some("one\nworkload\n"));

    s.info_attributes("*.txt merge=union\n");
    let out = s.agentfs(&["patch", "apply"]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    assert_eq!(s.read("f.txt").as_deref(), Some("one\nworkload\niac\n"));
    assert_eq!(s.read("g.txt").as_deref(), Some("g\n"));
    assert_eq!(s.inbox_commits(1), 1);
}

/// Acceptance 3: a custom driver of the repository's config
/// (`merge.ours2.driver`, named in `info/attributes`) runs and resolves
/// to ours: the head's `f.txt` stays, the rest of the patch lands.
#[test]
fn a_custom_driver_runs() {
    if !have_git() {
        return;
    }
    let s = Setup::new();
    let ran = s.dir.path().join("ran");
    // `sh <script>`: the script is read, never executed as a file just
    // written (ETXTBSY under parallel tests).
    let driver = s.file(
        "ours2.sh",
        &format!("echo \"$@\" >> '{}'\nexit 0\n", ran.display()),
    );
    s.config()
        .set_str("merge.ours2.driver", &format!("sh {driver} %O %A %B"))
        .unwrap();
    s.info_attributes("f.txt merge=ours2\n");
    s.both_edit_f();

    let out = s.agentfs(&["patch", "apply"]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    assert_eq!(s.read("f.txt").as_deref(), Some("one\nworkload\n"));
    assert_eq!(s.read("g.txt").as_deref(), Some("g\n"));
    assert!(fs::read_to_string(&ran).unwrap().contains(".merge_file_"));
    assert_eq!(s.inbox_commits(1), 1);
}

/// Acceptance 4: a configured driver whose program is missing leaves
/// every patch not judged, `merge driver <name>`, with `PatchRuleFailed`
/// (rule `merge`); the run goes on to the next patch rather than stopping
/// (P3). With the driver fixed, both land.
#[test]
fn a_missing_driver_is_not_judged() {
    if !have_git() {
        return;
    }
    let s = Setup::new();
    s.config()
        .set_str("merge.gone.driver", "agentfs-no-such-driver %O %A %B")
        .unwrap();
    s.record("a.txt", "a0\n");
    s.record("b.txt", "b0\n");
    s.ok(&["push"]);
    let p = s.producer("p");
    let a1 = s.file("a1", "a1\n");
    s.submit_ok(&p, &["--put", &format!("a.txt={a1}"), "-m", "one"]);
    let b1 = s.file("b1", "b1\n");
    s.submit_ok(&p, &["--put", &format!("b.txt={b1}"), "-m", "two"]);

    let out = s.agentfs(&["patch", "apply"]);
    let err = text(&out.stderr);
    assert_eq!(out.status.code(), Some(1), "{err}");
    for n in [1, 2] {
        assert!(
            err.contains(&format!(
                "patch {n} not judged (merge driver gone): the merge driver gone could not \
                 run: agentfs-no-such-driver is not a program in "
            )),
            "{err}"
        );
        assert!(
            err.contains(&format!(
                "the acceptance rule merge could not judge patch {n}"
            )),
            "{err}"
        );
    }
    assert!(!err.contains("held (conflict)"), "{err}");
    let json = s.ok(&["patch", "status", "--json"]);
    let doc: serde_json::Value = serde_json::from_str(&json).unwrap();
    let verdict = &doc["status"]["queue"][0]["verdict"];
    assert_eq!(verdict["kind"], "not_judged", "{json}");
    assert_eq!(verdict["rule"], "merge driver gone", "{json}");
    assert_eq!(s.read("a.txt").as_deref(), Some("a0\n"));
    assert!(s.local().refname_to_id(&held(1)).is_ok());

    s.config().set_str("merge.gone.driver", "true").unwrap();
    let out = s.agentfs(&["patch", "apply"]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    assert_eq!(s.read("a.txt").as_deref(), Some("a1\n"));
    assert_eq!(s.read("b.txt").as_deref(), Some("b1\n"));
}

/// Bead agentfs-mia.5: the notify hook and the apply answers.
impl Setup {
    /// Installs `$GIT_DIR/hooks/patch-notify`: it appends each notice (one
    /// JSON line) to a log and exits with the code in `notify-mode` (0 when
    /// there is none). The log's path.
    fn notify_hook(&self) -> PathBuf {
        let log = self.dir.path().join("notices.jsonl");
        let mode = self.dir.path().join("notify-mode");
        let hook = self.state().join("git/hooks/patch-notify");
        fs::create_dir_all(hook.parent().unwrap()).unwrap();
        fs::write(
            &hook,
            format!(
                "#!/bin/sh\ncat >> '{log}'\necho >> '{log}'\nexit \"$(cat '{mode}' 2>/dev/null || \
                 echo 0)\"\n",
                log = log.display(),
                mode = mode.display()
            ),
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt as _;
        fs::set_permissions(&hook, fs::Permissions::from_mode(0o755)).unwrap();
        log
    }

    fn notify_mode(&self, code: u8) {
        fs::write(self.dir.path().join("notify-mode"), code.to_string()).unwrap();
    }

    /// The notices the hook was given, in order.
    fn notices(&self) -> Vec<serde_json::Value> {
        fs::read_to_string(self.dir.path().join("notices.jsonl"))
            .unwrap_or_default()
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }

    /// Two patches on a.txt and b.txt; the workload changes b.txt, so the
    /// second conflicts.
    fn one_lands_two_conflicts(&self) -> PathBuf {
        self.record("a.txt", "a0\n");
        self.record("b.txt", "b0\n");
        self.ok(&["push"]);
        let p = self.producer("p");
        let a1 = self.file("a1", "a1\n");
        let b1 = self.file("b1", "b-from-iac\n");
        self.submit_ok(&p, &["--put", &format!("a.txt={a1}"), "-m", "one"]);
        self.submit_ok(&p, &["--put", &format!("b.txt={b1}"), "-m", "two"]);
        self.record("b.txt", "b-from-workload\n");
        p
    }
}

/// `(inbox, from kind, to kind, landed)` of a notice's changes.
fn changes(notice: &serde_json::Value) -> Vec<(u64, String, String, bool)> {
    let kind = |v: &serde_json::Value| v["kind"].as_str().unwrap_or("none").to_owned();
    notice["changes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| {
            (
                c["inbox"].as_u64().unwrap(),
                kind(&c["from"]),
                kind(&c["to"]),
                c["landed"].is_string(),
            )
        })
        .collect()
}

fn change(n: u64, from: &str, to: &str, landed: bool) -> (u64, String, String, bool) {
    (n, from.to_owned(), to.to_owned(), landed)
}

/// Acceptance 1: one notice per run that changed a verdict, none for a run
/// that changed nothing.
#[test]
fn the_hook_is_told_each_change() {
    if !have_git() {
        return;
    }
    let s = Setup::new();
    s.notify_hook();
    let p = s.one_lands_two_conflicts();

    let out = s.agentfs(&["patch", "apply"]);
    assert_eq!(out.status.code(), Some(1), "{}", text(&out.stderr));
    let notices = s.notices();
    assert_eq!(notices.len(), 1, "{notices:?}");
    let first = &notices[0];
    assert_eq!(first["schema"], 1);
    assert_eq!(first["volume"], "demo");
    assert_eq!(first["branch"], BRANCH);
    assert_eq!(first["head"], s.branch_head().to_string());
    let status = s.local().refname_to_id("refs/agentfs/demo/status").unwrap();
    assert_eq!(first["status"], status.to_string());
    assert_eq!(
        changes(first),
        [
            change(1, "none", "none", true),
            change(2, "none", "held", false)
        ]
    );
    assert_eq!(
        first["changes"][0]["patch"],
        s.local().refname_to_id(&applied(1)).unwrap().to_string()
    );
    assert_eq!(first["changes"][1]["to"]["reason"], "conflict");

    // Nothing new: no notice.
    let out = s.agentfs(&["patch", "apply"]);
    assert_eq!(out.status.code(), Some(1), "{}", text(&out.stderr));
    assert_eq!(s.notices().len(), 1);

    // 2 replaced by one that merges: it lands, and the notice says so.
    s.ok(&["push"]);
    let b2 = s.file("b2", "b-from-workload\nand iac\n");
    s.submit_ok(
        &p,
        &[
            "--replace",
            "2",
            "--put",
            &format!("b.txt={b2}"),
            "-m",
            "two again",
        ],
    );
    let out = s.agentfs(&["patch", "apply"]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    let notices = s.notices();
    assert_eq!(notices.len(), 2, "{notices:?}");
    assert_eq!(changes(&notices[1]), [change(2, "held", "none", true)]);
    assert_eq!(notices[1]["apply"]["kind"], "landed");
    assert_eq!(notices[1]["apply"]["landed"], serde_json::json!([2]));
}

/// Acceptance 2: a hook that fails raises `NotifyFailed`, and the next run
/// sends the same changes again; once delivered, nothing more.
#[test]
fn a_failing_hook_retries_at_the_next_run() {
    if !have_git() {
        return;
    }
    let s = Setup::new();
    s.notify_hook();
    s.one_lands_two_conflicts();
    s.notify_mode(1);
    let out = s.agentfs(&["patch", "apply"]);
    let err = text(&out.stderr);
    assert_eq!(out.status.code(), Some(1), "{err}");
    assert!(
        err.contains("alarm: telling the integrator of the patch queue's changes failed (hook)"),
        "{err}"
    );
    assert!(err.contains("exited with 1"), "{err}");

    s.notify_mode(0);
    let out = s.agentfs(&["patch", "apply"]);
    let err = text(&out.stderr);
    assert!(!err.contains("alarm: telling"), "{err}");
    let notices = s.notices();
    assert_eq!(notices.len(), 2, "{notices:?}");
    assert_eq!(changes(&notices[1]), changes(&notices[0]));
    assert_eq!(
        changes(&notices[1]),
        [
            change(1, "none", "none", true),
            change(2, "none", "held", false)
        ]
    );

    let out = s.agentfs(&["patch", "apply"]);
    assert_eq!(out.status.code(), Some(1), "{}", text(&out.stderr));
    assert_eq!(s.notices().len(), 2);
}

/// A request the webhook listener saw.
struct Posted {
    path: String,
    headers: std::collections::BTreeMap<String, String>,
    body: serde_json::Value,
}

/// An HTTP listener on 127.0.0.1: `/ok` answers 200, `/fail` 500.
fn webhook_listener() -> (u16, std::sync::Arc<std::sync::Mutex<Vec<Posted>>>) {
    use std::io::{BufRead as _, BufReader, Read as _, Write as _};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let log = seen.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut line = String::new();
            if reader.read_line(&mut line).is_err() {
                continue;
            }
            let path = line.split_whitespace().nth(1).unwrap_or("").to_owned();
            let mut headers = std::collections::BTreeMap::new();
            loop {
                let mut h = String::new();
                if reader.read_line(&mut h).unwrap_or(0) == 0 || h.trim().is_empty() {
                    break;
                }
                if let Some((k, v)) = h.split_once(':') {
                    headers.insert(k.trim().to_ascii_lowercase(), v.trim().to_owned());
                }
            }
            let len = headers
                .get("content-length")
                .and_then(|l| l.parse().ok())
                .unwrap_or(0);
            let mut body = vec![0; len];
            let _ = reader.read_exact(&mut body);
            let status = if path == "/ok" {
                "200 OK"
            } else {
                "500 Internal Server Error"
            };
            log.lock().unwrap().push(Posted {
                path,
                headers,
                body: serde_json::from_slice(&body).unwrap_or(serde_json::Value::Null),
            });
            let _ = stream.write_all(
                format!("HTTP/1.1 {status}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                    .as_bytes(),
            );
        }
    });
    (port, seen)
}

/// Acceptance 3: the webhook gets the notice with `Idempotency-Key:
/// <volume>:<status>` and the bearer token; a 500 is `NotifyFailed`, and
/// the next run sends it again.
#[test]
fn the_webhook_is_told_each_change() {
    if !have_git() {
        return;
    }
    let s = Setup::new();
    let (port, seen) = webhook_listener();
    let token = s.file("token", "s3cret\n");
    let mut config = git2::Config::open(&s.state().join("git/config")).unwrap();
    config
        .set_str(
            "agentfs.patchNotifyUrl",
            &format!("http://127.0.0.1:{port}/fail"),
        )
        .unwrap();
    config
        .set_str("agentfs.patchNotifyCredentialFile", &token)
        .unwrap();
    s.one_lands_two_conflicts();

    let out = s.agentfs(&["patch", "apply"]);
    let err = text(&out.stderr);
    assert!(err.contains("failed (webhook)"), "{err}");
    assert!(err.contains("answered 500"), "{err}");
    config
        .set_str(
            "agentfs.patchNotifyUrl",
            &format!("http://127.0.0.1:{port}/ok"),
        )
        .unwrap();
    let out = s.agentfs(&["patch", "apply", "--no-fetch"]);
    let err = text(&out.stderr);
    assert!(!err.contains("failed (webhook)"), "{err}");
    let status = s.local().refname_to_id("refs/agentfs/demo/status").unwrap();
    {
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 2);
        let ok = &seen[1];
        assert_eq!(ok.path, "/ok");
        assert_eq!(
            ok.headers.get("idempotency-key").map(String::as_str),
            Some(format!("demo:{status}").as_str())
        );
        assert_eq!(
            ok.headers.get("authorization").map(String::as_str),
            Some("Bearer s3cret")
        );
        assert_eq!(
            ok.headers.get("content-type").map(String::as_str),
            Some("application/json")
        );
        assert_eq!(ok.body["status"], status.to_string());
        assert_eq!(changes(&ok.body), changes(&seen[0].body));
        assert_eq!(changes(&ok.body).len(), 2);
    }
    let out = s.agentfs(&["patch", "apply", "--no-fetch"]);
    assert_eq!(out.status.code(), Some(1), "{}", text(&out.stderr));
    assert_eq!(seen.lock().unwrap().len(), 2, "nothing new, nothing sent");
}

/// Acceptance 4: while `agentfs run` holds the volume, `patch apply`
/// lands nothing: it judges, notifies with `apply.kind = attached`, and
/// answers "workload attached; lands at next mount" with exit 75. With the
/// daemon gone, the next apply lands it.
#[test]
fn apply_under_a_daemon_answers_attached() {
    if !have_git() {
        return;
    }
    let s = Setup::new();
    s.notify_hook();
    s.record("a.txt", "a0\n");
    s.ok(&["push"]);
    let head = s.branch_head();
    let p = s.producer("p");
    let a1 = s.file("a1", "a1\n");
    s.submit_ok(&p, &["--put", &format!("a.txt={a1}"), "-m", "one"]);

    let mut run = s
        .agentfs_command(&["run"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let socket = s.state().join("meta/control/agentfs.sock");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    while !socket.exists() {
        if std::time::Instant::now() > deadline {
            let _ = run.kill();
            let _ = run.wait();
            panic!("run never listened");
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    let out = s.agentfs(&["patch", "apply"]);
    let _ = run.kill();
    let _ = run.wait();
    let err = text(&out.stderr);
    assert_eq!(out.status.code(), Some(75), "{err}");
    assert!(
        err.contains("patches: workload attached; lands at next mount"),
        "{err}"
    );
    assert!(err.contains("patch 1 ready"), "{err}");
    assert_eq!(
        s.read("a.txt").as_deref(),
        Some("a0\n"),
        "the tree is unchanged"
    );
    assert_eq!(s.branch_head(), head, "nothing landed");
    assert_eq!(s.inbox_commits(1), 0);
    let notices = s.notices();
    assert_eq!(notices.len(), 1, "{notices:?}");
    assert_eq!(notices[0]["apply"]["kind"], "attached");
    assert_eq!(
        notices[0]["apply"]["message"],
        "workload attached; lands at next mount"
    );
    assert_eq!(changes(&notices[0]), [change(1, "none", "ready", false)]);
    let json = s.ok(&["patch", "status", "--json"]);
    let doc: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!(doc["status"]["queue"][0]["verdict"]["kind"], "ready");

    let out = s.agentfs(&["patch", "apply"]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    assert_eq!(s.read("a.txt").as_deref(), Some("a1\n"));
    let notices = s.notices();
    assert_eq!(notices.len(), 2, "{notices:?}");
    assert_eq!(changes(&notices[1]), [change(1, "ready", "none", true)]);
}

/// Acceptance 5: with no daemon, `patch apply` lands as before and the
/// notice answers the call: `apply.kind = landed`.
#[test]
fn apply_with_no_daemon_lands() {
    if !have_git() {
        return;
    }
    let s = Setup::new();
    s.notify_hook();
    s.record("a.txt", "a0\n");
    s.ok(&["push"]);
    let p = s.producer("p");
    let a1 = s.file("a1", "a1\n");
    s.submit_ok(&p, &["--put", &format!("a.txt={a1}"), "-m", "one"]);
    let out = s.agentfs(&["patch", "apply"]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    assert_eq!(s.read("a.txt").as_deref(), Some("a1\n"));
    let notices = s.notices();
    assert_eq!(notices.len(), 1, "{notices:?}");
    assert_eq!(notices[0]["apply"]["kind"], "landed");
    assert_eq!(notices[0]["apply"]["landed"], serde_json::json!([1]));
    assert_eq!(changes(&notices[0]), [change(1, "none", "none", true)]);
}
