//! Edges of `agentfs serve-git` and `agentfs patch apply` (design §12;
//! beads agentfs-5sg, agentfs-g09): what hiding a ref does not hide, and
//! the answer of a daemon from before the control socket's `apply` and
//! `judge`.
//!
//! The real binary serves a state area on 127.0.0.1 with `git
//! http-backend` behind it; the real `git` and `agentfs patch` push to it.
//! An old daemon is a fake control socket in the test that answers every
//! request the way a daemon from before `apply` and `judge` does. Skipped,
//! with a message, when `git` is not installed.

use std::collections::BTreeSet;
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use agentfs_core::listening_socket::Listener;
use git2::{Oid, Repository};

const BRANCH: &str = "refs/heads/agentfs/demo";
const INBOX_1: &str = "refs/agentfs/demo/inbox/0000000001";

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

/// A volume `demo` with `agentfs.patchRemote = none`, `a.txt` recorded,
/// and pusher `iac` (token `iac-token`) with every right.
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
        s.ok(&["init", "--volume", "demo"]);
        let mut config = s.config();
        config.set_str("agentfs.patchRemote", "none").unwrap();
        let token = s.dir.path().join("iac.token");
        fs::write(&token, "iac-token\n").unwrap();
        config
            .set_str("agentfsPusher.iac.tokenFile", &token.display().to_string())
            .unwrap();
        for right in ["offer", "integrator"] {
            config
                .set_multivar("agentfsPusher.iac.right", "^$", right)
                .unwrap();
        }
        fs::write(s.tree().join("a.txt"), "a0\n").unwrap();
        s.ok(&["record", "a.txt"]);
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

    fn local(&self) -> Repository {
        Repository::open_bare(self.state().join("git")).unwrap()
    }

    fn read(&self, name: &str) -> String {
        fs::read_to_string(self.tree().join(name)).unwrap()
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

    fn git(&self, dir: &Path, args: &[&str]) -> Output {
        let mut cmd = Command::new("git");
        cmd.arg("-C").arg(dir).args(args);
        self.hermetic(&mut cmd);
        cmd.output().unwrap()
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

    /// Pushes `a.txt = a1` on the branch to `served` as `iac`'s
    /// `inbox/1`, with plain git, which relays the hooks' messages.
    fn submit(&self, served: &Served) -> Output {
        let producer = self.dir.path().join("p");
        let p = producer.to_str().unwrap();
        let url = served.url();
        for args in [
            vec!["init", "--quiet", p],
            vec!["-C", p, "fetch", "--quiet", &url, BRANCH],
            vec!["-C", p, "checkout", "--quiet", "FETCH_HEAD"],
        ] {
            let out = self.git(self.dir.path(), &args);
            assert!(out.status.success(), "{args:?}: {}", text(&out.stderr));
        }
        fs::write(producer.join("a.txt"), "a1\n").unwrap();
        let out = self.git(&producer, &["commit", "--quiet", "-am", "one"]);
        assert!(out.status.success(), "{}", text(&out.stderr));
        let out = self.git(&producer, &["push", &url, &format!("HEAD:{INBOX_1}")]);
        assert!(
            out.status.success(),
            "{}{}",
            text(&out.stdout),
            text(&out.stderr)
        );
        assert!(self.local().refname_to_id(INBOX_1).is_ok(), "queued");
        out
    }
}

/// A running `serve-git`, stopped when dropped.
struct Served {
    child: Child,
    address: String,
}

impl Served {
    fn url(&self) -> String {
        format!("http://iac:iac-token@{}/", self.address)
    }

    /// `POST /agentfs/apply` as `iac`: the status code and body.
    fn apply(&self) -> (u16, String) {
        let mut stream = TcpStream::connect(&self.address).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(120)))
            .unwrap();
        write!(
            stream,
            "POST /agentfs/apply HTTP/1.1\r\nHost: {}\r\nAuthorization: Bearer \
             iac-token\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
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

/// A daemon from before the control socket's `apply` and `judge`: it
/// listens on the volume's control socket and answers every request with
/// the error its protocol parser gives an op it does not know. It does not
/// hold the state-area lock, so a client's lock path runs to the end.
struct OldDaemon {
    /// The ops it was asked, in order.
    asked: Arc<Mutex<Vec<String>>>,
}

impl OldDaemon {
    fn start(state: &Path) -> Self {
        let dir = state.join("meta/control");
        fs::create_dir_all(&dir).unwrap();
        let listener = Listener::bind(dir.join("agentfs.sock")).unwrap();
        let asked = Arc::new(Mutex::new(Vec::new()));
        let log = asked.clone();
        std::thread::spawn(move || {
            while let Ok(Some(mut stream)) = listener.accept() {
                let mut line = String::new();
                if BufReader::new(&stream).read_line(&mut line).is_err() {
                    continue;
                }
                let request: serde_json::Value = serde_json::from_str(&line).unwrap_or_default();
                let op = request["op"].as_str().unwrap_or_default().to_owned();
                log.lock().unwrap().push(op.clone());
                let reply = serde_json::json!({
                    "error": format!(
                        "malformed request: unknown variant `{op}`, expected one of \
                         `restore`, `revert`, `push`, `receive_fork` at line 1 column 20"
                    ),
                });
                let _ = writeln!(stream, "{reply}");
            }
        });
        Self { asked }
    }

    fn asked(&self) -> Vec<String> {
        self.asked.lock().unwrap().clone()
    }
}

/// An old daemon's "unknown variant" answer to `judge` leaves the arrival
/// to its next run, and to `apply` sends `agentfs patch apply` down its
/// lock path, which lands the queue here (no lock is held). Without that
/// fallback the command fails with the daemon's error.
#[test]
fn patch_apply_falls_back_when_an_old_daemon_cannot_apply() {
    if !have_git() {
        return;
    }
    let s = Setup::new();
    let daemon = OldDaemon::start(&s.state());
    let served = s.serve();
    let submitted = s.submit(&served);
    let err = text(&submitted.stderr);
    assert!(
        err.contains("the running daemon cannot judge yet; its next run judges the push"),
        "{err}"
    );
    assert_eq!(daemon.asked(), ["judge"]);

    let out = s.agentfs(&["patch", "apply"]);
    assert!(
        out.status.success(),
        "{}{}",
        text(&out.stdout),
        text(&out.stderr)
    );
    assert_eq!(daemon.asked(), ["judge", "apply"], "the daemon was asked");
    assert_eq!(s.read("a.txt"), "a1\n", "landed by the lock path");
}

/// `POST /agentfs/apply` against an old daemon runs `agentfs patch apply`,
/// which falls back the same way and lands the queue: 200 with its run.
#[test]
fn serve_git_apply_falls_back_when_an_old_daemon_cannot_apply() {
    if !have_git() {
        return;
    }
    let s = Setup::new();
    let daemon = OldDaemon::start(&s.state());
    let served = s.serve();
    s.submit(&served);
    let (status, body) = served.apply();
    assert_eq!(status, 200, "{body}");
    let run: serde_json::Value = serde_json::from_str(body.trim()).unwrap();
    assert_eq!(run["ran"]["exit"], 0, "{body}");
    // serve-git asked, then the `patch apply` it ran.
    assert_eq!(daemon.asked(), ["judge", "apply", "apply"]);
    assert_eq!(s.read("a.txt"), "a1\n", "{body}");
}

/// `transfer.hideRefs` hides ref names, not objects: protocol v2's
/// upload-pack serves any object a client names by id, while v0 refuses
/// one no advertised ref points at (git-config(1) `transfer.hideRefs`;
/// gitnamespaces(7) SECURITY). This pins what design §12 says hiding
/// gives (bead agentfs-5sg).
#[test]
fn hidden_refs_hide_names_not_objects() {
    if !have_git() {
        return;
    }
    let s = Setup::new();
    let repo = s.local();
    let head = repo
        .find_commit(repo.refname_to_id(BRANCH).unwrap())
        .unwrap();
    let blob = repo.blob(b"held\n").unwrap();
    let mut tree = repo.treebuilder(Some(&head.tree().unwrap())).unwrap();
    tree.insert("held.txt", blob, 0o100644).unwrap();
    let tree = repo.find_tree(tree.write().unwrap()).unwrap();
    let sig = git2::Signature::now("t", "t@example.com").unwrap();
    let hidden: Oid = repo
        .commit(None, &sig, &sig, "held", &tree, &[&head])
        .unwrap();
    let held = "refs/agentfs/demo/held/0000000001";
    repo.reference(held, hidden, true, "test").unwrap();
    let served = s.serve();

    let listed: BTreeSet<String> = text(&s.git(s.dir.path(), &["ls-remote", &served.url()]).stdout)
        .lines()
        .filter_map(|l| l.split_whitespace().nth(1).map(str::to_owned))
        .collect();
    assert!(!listed.contains(held), "{listed:?}");
    assert!(listed.contains(BRANCH), "{listed:?}");

    let fetch = |version: &str| {
        let into = s.dir.path().join(format!("fetch-v{version}"));
        Repository::init_bare(&into).unwrap();
        let protocol = format!("protocol.version={version}");
        let out = s.git(
            &into,
            &["-c", &protocol, "fetch", &served.url(), &hidden.to_string()],
        );
        let has = Repository::open_bare(&into)
            .unwrap()
            .find_commit(hidden)
            .is_ok();
        (out, has)
    };
    let (out, has) = fetch("0");
    assert!(
        !out.status.success(),
        "v0 refuses it: {}",
        text(&out.stderr)
    );
    assert!(!has);
    let (out, has) = fetch("2");
    assert!(out.status.success(), "v2 serves it: {}", text(&out.stderr));
    assert!(has, "the hidden ref's commit was fetched by id");
}
