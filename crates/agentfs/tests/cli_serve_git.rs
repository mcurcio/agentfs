//! End-to-end tests of `agentfs serve-git` (design §12, §19.2 item 11;
//! bead agentfs-mia.9): the real binary serving a state area over git's
//! smart HTTP on 127.0.0.1, `git http-backend` behind it, and the real
//! `git` and `agentfs patch` as pushers.
//!
//! Skipped, with a message, when `git` is not installed. The CI `test` job
//! runs them against the runner's git.

use std::collections::BTreeSet;
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

use git2::{Oid, Repository};

const BRANCH: &str = "refs/heads/agentfs/demo";

fn have_git() -> bool {
    let found = Command::new("git").arg("--version").output().is_ok();
    if !found {
        eprintln!("skipping: git is not installed");
    }
    found
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

fn inbox(n: u64) -> String {
    format!("refs/agentfs/demo/inbox/{n:010}")
}

fn approved(n: u64) -> String {
    format!("refs/agentfs/demo/approved/{n:010}")
}

/// A volume `demo` with no remote and `agentfs.patchRemote = none`, `a.txt`
/// recorded, and pushers `iac` (token `iac-token`) and `ops`.
struct Setup {
    dir: tempfile::TempDir,
}

impl Setup {
    fn new() -> Self {
        let s = Self {
            dir: tempfile::tempdir().unwrap(),
        };
        fs::create_dir_all(s.tree()).unwrap();
        fs::create_dir_all(s.dir.path().join("home")).unwrap();
        let out = s.agentfs(&["init", "--volume", "demo"]);
        assert!(out.status.success(), "{}", text(&out.stderr));
        s.config().set_str("agentfs.patchRemote", "none").unwrap();
        s.record("a.txt", "a0\n");
        s
    }

    fn state(&self) -> PathBuf {
        self.dir.path().join("state")
    }

    fn tree(&self) -> PathBuf {
        self.dir.path().join("tree")
    }

    fn config(&self) -> git2::Config {
        git2::Config::open(&self.state().join("git/config")).unwrap()
    }

    /// Adds pusher `name` with `rights` and token `<name>-token`.
    fn pusher(&self, name: &str, rights: &[&str]) {
        let token = self.dir.path().join(format!("{name}.token"));
        fs::write(&token, format!("{name}-token\n")).unwrap();
        let mut config = self.config();
        config
            .set_str(
                &format!("agentfsPusher.{name}.tokenFile"),
                &token.display().to_string(),
            )
            .unwrap();
        for right in rights {
            config
                .set_multivar(&format!("agentfsPusher.{name}.right"), "^$", right)
                .unwrap();
        }
    }

    fn hermetic(&self, cmd: &mut Command) {
        let home = self.dir.path().join("home");
        cmd.env("HOME", &home)
            .env("XDG_CONFIG_HOME", home.join(".config"))
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_AUTHOR_NAME", "Producer")
            .env("GIT_AUTHOR_EMAIL", "producer@example.com")
            .env("GIT_COMMITTER_NAME", "Producer")
            .env("GIT_COMMITTER_EMAIL", "producer@example.com")
            .env("GIT_TERMINAL_PROMPT", "0")
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

    fn read(&self, name: &str) -> Option<String> {
        fs::read_to_string(self.tree().join(name)).ok()
    }

    fn local(&self) -> Repository {
        Repository::open_bare(self.state().join("git")).unwrap()
    }

    fn local_ref(&self, name: &str) -> Option<Oid> {
        self.local().refname_to_id(name).ok()
    }

    fn branch_head(&self) -> Oid {
        self.local_ref(BRANCH).unwrap()
    }

    /// Starts `serve-git` on a free port and waits for its address.
    fn serve(&self) -> Served {
        let mut child = self
            .agentfs_command(&["serve-git", "--listen", "127.0.0.1:0"])
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        let mut line = String::new();
        BufReader::new(child.stdout.take().unwrap())
            .read_line(&mut line)
            .unwrap();
        let address = line
            .trim()
            .strip_prefix("serve-git: listening on http://")
            .and_then(|a| a.strip_suffix('/'))
            .unwrap_or_else(|| {
                let _ = child.kill();
                panic!("serve-git did not start: {line:?}")
            })
            .to_owned();
        Served { child, address }
    }

    /// An empty producer repository.
    fn producer(&self, name: &str) -> PathBuf {
        let dir = self.dir.path().join(name);
        let out = self.git(
            self.dir.path(),
            &["init", "--quiet", &dir.display().to_string()],
        );
        assert!(out.status.success(), "{}", text(&out.stderr));
        dir
    }

    fn git(&self, dir: &Path, args: &[&str]) -> Output {
        let mut cmd = Command::new("git");
        cmd.arg("-C").arg(dir).args(args);
        self.hermetic(&mut cmd);
        cmd.output().unwrap()
    }

    fn git_ok(&self, dir: &Path, args: &[&str]) -> String {
        let out = self.git(dir, args);
        assert!(
            out.status.success(),
            "git {args:?}: {}{}",
            text(&out.stdout),
            text(&out.stderr)
        );
        text(&out.stdout).trim().to_owned()
    }

    /// A producer with the volume's branch checked out from `url`.
    fn producer_on_branch(&self, name: &str, url: &str) -> PathBuf {
        let p = self.producer(name);
        self.git_ok(
            &p,
            &[
                "fetch",
                "--quiet",
                url,
                &format!("{BRANCH}:refs/heads/base"),
            ],
        );
        self.git_ok(&p, &["checkout", "--quiet", "base"]);
        p
    }

    /// Commits `content` to `a.txt` in `producer` with `message`.
    fn commit(&self, producer: &Path, content: &str, message: &str) -> Oid {
        fs::write(producer.join("a.txt"), content).unwrap();
        self.git_ok(producer, &["commit", "--quiet", "-am", message]);
        Oid::from_str(&self.git_ok(producer, &["rev-parse", "HEAD"])).unwrap()
    }

    fn submit(&self, producer: &Path, url: &str, args: &[&str]) -> Output {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_agentfs"));
        cmd.args(["patch", "submit", "--volume", "demo", "--repo"])
            .arg(producer)
            .args(["--remote", url])
            .args(args);
        self.hermetic(&mut cmd);
        cmd.output().unwrap()
    }

    fn file(&self, name: &str, content: &str) -> String {
        let path = self.dir.path().join(name);
        fs::write(&path, content).unwrap();
        path.display().to_string()
    }

    /// Installs `$GIT_DIR/hooks/patch-notify`, appending each notice as a
    /// JSON line to `notices.jsonl`.
    fn notify_hook(&self) {
        let log = self.dir.path().join("notices.jsonl");
        let hook = self.state().join("git/hooks/patch-notify");
        fs::create_dir_all(hook.parent().unwrap()).unwrap();
        fs::write(
            &hook,
            format!(
                "#!/bin/sh\ncat >> '{log}'\necho >> '{log}'\n",
                log = log.display()
            ),
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt as _;
        fs::set_permissions(&hook, fs::Permissions::from_mode(0o755)).unwrap();
    }

    fn notices(&self) -> Vec<serde_json::Value> {
        fs::read_to_string(self.dir.path().join("notices.jsonl"))
            .unwrap_or_default()
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }

    /// No quarantine directory a refused push left in the objects.
    fn no_quarantine_left(&self) {
        let left: Vec<String> = fs::read_dir(self.state().join("git/objects"))
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|n| n.contains("incoming") || n.starts_with("tmp_objdir"))
            .collect();
        assert!(left.is_empty(), "{left:?}");
    }
}

/// A running `serve-git`, stopped when dropped.
struct Served {
    child: Child,
    address: String,
}

impl Served {
    fn url(&self, user: &str, token: &str) -> String {
        format!("http://{user}:{token}@{}/", self.address)
    }

    /// One HTTP/1.1 request; the status code and body.
    fn http(&self, method: &str, path: &str, authorization: Option<&str>) -> (u16, String) {
        let mut stream = TcpStream::connect(&self.address).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(60)))
            .unwrap();
        let auth = authorization
            .map(|a| format!("Authorization: {a}\r\n"))
            .unwrap_or_default();
        write!(
            stream,
            "{method} {path} HTTP/1.1\r\nHost: {}\r\n{auth}Content-Length: 0\r\nConnection: \
             close\r\n\r\n",
            self.address
        )
        .unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).unwrap();
        let status = response
            .split_whitespace()
            .nth(1)
            .and_then(|s| s.parse().ok())
            .unwrap_or_else(|| panic!("{response:?}"));
        let body = response
            .split_once("\r\n\r\n")
            .map(|(_, b)| b.to_owned())
            .unwrap_or_default();
        (status, body)
    }
}

impl Drop for Served {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn basic(user: &str, token: &str) -> String {
    use std::fmt::Write as _;
    // base64 by hand: the test crate has no base64 dependency.
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let input = format!("{user}:{token}").into_bytes();
    let mut out = String::from("Basic ");
    for chunk in input.chunks(3) {
        let b = [
            chunk[0],
            chunk.get(1).copied().unwrap_or(0),
            chunk.get(2).copied().unwrap_or(0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        for i in 0..4 {
            if i <= chunk.len() {
                let _ = out.write_char(ALPHABET[(n >> (18 - 6 * i) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// Acceptance 1: `agentfs patch submit` against serve-git queues
/// `inbox/1`, `patch approve` records the approval, and `patch apply`
/// with no daemon lands it.
#[test]
fn submit_against_serve_git() {
    if !have_git() {
        return;
    }
    let s = Setup::new();
    s.pusher("iac", &["offer", "approve"]);
    let served = s.serve();
    let url = served.url("iac", "iac-token");
    let p = s.producer("p");
    let a1 = s.file("a1", "a1\n");
    let out = s.submit(&p, &url, &["--put", &format!("a.txt={a1}"), "-m", "one"]);
    assert!(
        out.status.success(),
        "{}{}",
        text(&out.stdout),
        text(&out.stderr)
    );
    let tip = s.local_ref(&inbox(1)).expect("queued as inbox/1");
    assert_eq!(s.read("a.txt").as_deref(), Some("a0\n"), "not landed yet");

    let mut approve = Command::new(env!("CARGO_BIN_EXE_agentfs"));
    approve
        .args(["patch", "approve", "1", "--volume", "demo", "--repo"])
        .arg(&p)
        .args(["--remote", &url]);
    s.hermetic(&mut approve);
    let out = approve.output().unwrap();
    assert!(out.status.success(), "{}", text(&out.stderr));
    assert_eq!(s.local_ref(&approved(1)), Some(tip));

    s.ok(&["patch", "apply"]);
    assert_eq!(s.read("a.txt").as_deref(), Some("a1\n"));
    let repo = s.local();
    let landed = repo.find_commit(s.branch_head()).unwrap();
    assert!(
        landed.message().unwrap().contains("Agentfs-Inbox: 1\n"),
        "{:?}",
        landed.message()
    );
}

/// Acceptance 2: a push is admitted whole by the pusher's rights or not
/// at all, and keeps nothing when refused; a wrong token is a 401.
#[test]
fn rights_are_enforced() {
    if !have_git() {
        return;
    }
    let s = Setup::new();
    s.pusher("iac", &["offer"]);
    let served = s.serve();
    let url = served.url("iac", "iac-token");
    let p = s.producer_on_branch("p", &url);
    let head = s.branch_head();

    // An offer the pusher may make, with an approval it may not: neither
    // is kept, nor the pushed commit.
    let c = s.commit(&p, "a1\n", "one");
    let out = s.git(
        &p,
        &[
            "push",
            &url,
            &format!("HEAD:{}", inbox(1)),
            &format!("HEAD:{}", approved(1)),
        ],
    );
    let err = text(&out.stderr);
    assert!(!out.status.success(), "{err}");
    assert!(err.contains("iac may not approve patch 1"), "{err}");
    assert_eq!(s.local_ref(&inbox(1)), None, "{err}");
    assert_eq!(s.local_ref(&approved(1)), None);
    assert!(
        s.local().find_commit(c).is_err(),
        "the pushed objects are gone"
    );
    s.no_quarantine_left();

    // A barrier needs the barrier right.
    s.git_ok(
        &p,
        &[
            "commit",
            "--quiet",
            "--amend",
            "-m",
            "one\n\nAgentfs-Barrier: yes",
        ],
    );
    let out = s.git(&p, &["push", &url, &format!("HEAD:{}", inbox(1))]);
    let err = text(&out.stderr);
    assert!(!out.status.success(), "{err}");
    assert!(err.contains("iac may not offer barrier patch 1"), "{err}");
    assert_eq!(s.local_ref(&inbox(1)), None);

    // The branch is never pushed to.
    let out = s.git(&p, &["push", &url, &format!("HEAD:{BRANCH}")]);
    assert!(!out.status.success(), "{}", text(&out.stderr));
    assert_eq!(s.branch_head(), head);

    // The same offer without the approval and the trailer is kept.
    s.git_ok(&p, &["commit", "--quiet", "--amend", "-m", "one"]);
    s.git_ok(
        &p,
        &["push", "--quiet", &url, &format!("HEAD:{}", inbox(1))],
    );
    assert!(s.local_ref(&inbox(1)).is_some());

    // Credentials.
    let path = "/info/refs?service=git-upload-pack";
    assert_eq!(served.http("GET", path, None).0, 401);
    assert_eq!(
        served.http("GET", path, Some(&basic("iac", "wrong"))).0,
        401
    );
    assert_eq!(
        served
            .http("GET", path, Some(&basic("iac", "iac-token-and-more")))
            .0,
        401
    );
    assert_eq!(served.http("GET", path, Some("Bearer wrong")).0, 401);
    assert_eq!(
        served.http("GET", path, Some(&basic("iac", "iac-token"))).0,
        200
    );
    assert_eq!(served.http("GET", path, Some("Bearer iac-token")).0, 200);
    let out = s.git(s.dir.path(), &["ls-remote", &served.url("iac", "wrong")]);
    assert!(!out.status.success(), "{}", text(&out.stdout));
    // Dumb HTTP, which would hand out every object, is not served.
    assert_eq!(
        served
            .http("GET", "/info/refs", Some(&basic("iac", "iac-token")))
            .0,
        404
    );
    assert_eq!(
        served
            .http(
                "GET",
                "/objects/info/packs",
                Some(&basic("iac", "iac-token"))
            )
            .0,
        404
    );
}

/// Acceptance 3: a tip whose history never meets the branch is refused
/// with the queue's own reason, and not kept.
#[test]
fn an_unlandable_patch_is_not_kept() {
    if !have_git() {
        return;
    }
    let s = Setup::new();
    s.pusher("iac", &["offer"]);
    let served = s.serve();
    let url = served.url("iac", "iac-token");
    let p = s.producer("p");
    fs::write(p.join("b.txt"), "b\n").unwrap();
    s.git_ok(&p, &["add", "b.txt"]);
    s.git_ok(&p, &["commit", "--quiet", "-m", "unrelated"]);
    fs::write(p.join("b.txt"), "b1\n").unwrap();
    s.git_ok(&p, &["commit", "--quiet", "-am", "on it"]);
    let out = s.git(&p, &["push", &url, &format!("HEAD:{}", inbox(1))]);
    let err = text(&out.stderr);
    assert!(!out.status.success(), "{err}");
    assert!(err.contains("patch 1: parent_not_in_history"), "{err}");
    assert_eq!(s.local_ref(&inbox(1)), None);
    s.no_quarantine_left();
}

/// Acceptance 4: with `agentfs run` holding the volume, a push is judged
/// and the integrator told it is ready; nothing lands. The integrator's
/// apply is answered 409, attached.
#[test]
fn arrival_judges_and_notifies_never_lands() {
    if !have_git() {
        return;
    }
    let s = Setup::new();
    s.pusher("iac", &["offer", "integrator"]);
    s.notify_hook();
    let head = s.branch_head();
    let mut run = s
        .agentfs_command(&["run"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let socket = s.state().join("meta/control/agentfs.sock");
    let deadline = Instant::now() + Duration::from_secs(20);
    while !socket.exists() {
        if Instant::now() > deadline {
            let _ = run.kill();
            let _ = run.wait();
            panic!("run never listened");
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let served = s.serve();
    let url = served.url("iac", "iac-token");
    let p = s.producer("p");
    let a1 = s.file("a1", "a1\n");
    let out = s.submit(&p, &url, &["--put", &format!("a.txt={a1}"), "-m", "one"]);
    let (apply, body) = served.http("POST", "/agentfs/apply", Some("Bearer iac-token"));
    let _ = run.kill();
    let _ = run.wait();
    assert!(out.status.success(), "{}", text(&out.stderr));
    let notices = s.notices();
    assert_eq!(notices.len(), 1, "{notices:?}");
    let change = &notices[0]["changes"][0];
    assert_eq!(change["inbox"], 1);
    assert_eq!(change["to"]["kind"], "ready");
    assert!(notices[0].get("apply").is_none(), "{notices:?}");
    assert_eq!(
        s.read("a.txt").as_deref(),
        Some("a0\n"),
        "the tree is unchanged"
    );
    assert_eq!(s.branch_head(), head, "nothing landed");
    assert_eq!(apply, 409, "{body}");
    let reply: serde_json::Value = serde_json::from_str(body.trim()).unwrap();
    assert_eq!(
        reply["attached"]["message"],
        "workload attached; lands at next mount"
    );
}

/// `POST /agentfs/apply` needs the Integrator right: no credentials is a
/// 401, a pusher with only `offer` a 403 that lands nothing, and one with
/// the right lands the queue (nothing attached).
#[test]
fn apply_needs_the_integrator_right() {
    if !have_git() {
        return;
    }
    let s = Setup::new();
    s.pusher("iac", &["offer"]);
    s.pusher("ops", &["integrator"]);
    let head = s.branch_head();
    let served = s.serve();
    let url = served.url("iac", "iac-token");
    let p = s.producer("p");
    let a1 = s.file("a1", "a1\n");
    let out = s.submit(&p, &url, &["--put", &format!("a.txt={a1}"), "-m", "one"]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    assert!(s.local_ref(&inbox(1)).is_some());

    assert_eq!(served.http("POST", "/agentfs/apply", None).0, 401);
    let (status, body) = served.http("POST", "/agentfs/apply", Some("Bearer iac-token"));
    assert_eq!(status, 403, "{body}");
    assert!(body.contains("iac may not apply the queue"), "{body}");
    assert_eq!(s.read("a.txt").as_deref(), Some("a0\n"), "nothing landed");
    assert_eq!(s.branch_head(), head, "nothing landed");

    let (status, body) = served.http("POST", "/agentfs/apply", Some("Bearer ops-token"));
    assert_eq!(status, 200, "{body}");
    assert_eq!(s.read("a.txt").as_deref(), Some("a1\n"), "{body}");
}

/// Acceptance 5: serve-git is the patch source, so a patch remote refuses
/// it, naming the key.
#[test]
fn refuses_with_a_patch_remote() {
    if !have_git() {
        return;
    }
    let s = Setup::new();
    let remote = s.dir.path().join("remote.git");
    Repository::init_bare(&remote).unwrap();
    let mut config = s.config();
    config
        .set_str("remote.origin.url", &format!("file://{}", remote.display()))
        .unwrap();
    // Unset: the only remote is the patch remote.
    config.remove("agentfs.patchRemote").unwrap();
    let out = s.agentfs(&["serve-git", "--listen", "127.0.0.1:0"]);
    let err = text(&out.stderr);
    assert_eq!(out.status.code(), Some(1), "{err}");
    assert!(err.contains("agentfs.patchRemote"), "{err}");
    s.config().set_str("agentfs.patchRemote", "origin").unwrap();
    let out = s.agentfs(&["serve-git", "--listen", "127.0.0.1:0"]);
    assert_eq!(out.status.code(), Some(1), "{}", text(&out.stderr));
}

/// Acceptance 6: fetchers and pushers see only the branch and the queue's
/// shared refs.
#[test]
fn hidden_refs() {
    if !have_git() {
        return;
    }
    let s = Setup::new();
    s.pusher("reader", &[]);
    let head = s.branch_head();
    let repo = s.local();
    let visible = [
        BRANCH.to_owned(),
        inbox(1),
        format!("refs/agentfs/demo/applied/{:010}", 1),
        approved(1),
        "refs/agentfs/demo/status".to_owned(),
    ];
    for name in visible.iter().skip(1).chain(
        [
            "refs/agentfs/demo/held/0000000001".to_owned(),
            "refs/agentfs/other/inbox/0000000001".to_owned(),
            "refs/agentfs/demo-two/inbox/0000000001".to_owned(),
            "refs/heads/other".to_owned(),
            "refs/heads/agentfs/demo-two".to_owned(),
            "refs/tags/t".to_owned(),
        ]
        .iter(),
    ) {
        repo.reference(name, head, true, "test").unwrap();
    }
    let served = s.serve();
    let listed: BTreeSet<String> = s
        .git_ok(
            s.dir.path(),
            &["ls-remote", &served.url("reader", "reader-token")],
        )
        .lines()
        .filter_map(|l| l.split_whitespace().nth(1).map(str::to_owned))
        .collect();
    assert_eq!(listed, visible.into_iter().collect::<BTreeSet<_>>());
}
