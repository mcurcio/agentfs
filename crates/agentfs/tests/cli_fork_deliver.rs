//! End-to-end tests of `agentfs fork deliver` (design §11.5 "Delivery")
//! against `file://` bare remotes: a fork of `src` is published to
//! `origin`, and a receiver's clone delivers its change by each method:
//! inbox and branch with the real `git`, command with a receiver script,
//! webhook to an in-process HTTP listener.
//!
//! Skipped, with a message, when `git` is not installed. The CI `test` job
//! runs them against the runner's git.

use std::collections::BTreeMap;
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use git2::{Oid, Repository};

mod fork_setup;

use fork_setup::{Setup, have_git, text};

/// A fork of `src` published to `origin`: `a.txt` at base, `b.txt` added
/// by the fork, and a receiver's clone of `origin`.
struct Published {
    s: Setup,
    fork: String,
    base: Oid,
    change: Oid,
    finish: Oid,
    receiver: PathBuf,
}

fn published() -> Option<Published> {
    if !have_git() {
        return None;
    }
    let s = Setup::new(false);
    s.record("state", "tree", "a.txt", "a\n");
    s.ok("state", "tree", &["push"]);
    let base = s.source_head();
    let fork = s.create_fork();
    s.record("fork", "fork-tree", "b.txt", "from the fork\n");
    s.ok("fork", "fork-tree", &["fork", "finish"]);
    let at = |kind: &str| {
        s.remote_ref("origin.git", &format!("refs/agentfs/{fork}/{kind}"))
            .unwrap()
    };
    let (change, finish) = (at("change"), at("finish"));
    let receiver = s.path("receiver");
    let out = s.git(
        s.dir.path(),
        &["clone", "--quiet", &s.url("origin.git"), "receiver"],
    );
    assert!(out.status.success(), "{}", text(&out.stderr));
    Some(Published {
        s,
        fork,
        base,
        change,
        finish,
        receiver,
    })
}

impl Published {
    /// `agentfs fork deliver <fork> --repo <receiver> args`, run in the
    /// test's directory, with `env` added.
    fn deliver_env(&self, args: &[&str], env: &[(&str, &str)]) -> Output {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_agentfs"));
        cmd.current_dir(self.s.dir.path())
            .args(["fork", "deliver", &self.fork, "--repo"])
            .arg(&self.receiver)
            .args(args);
        self.s.hermetic(&mut cmd);
        cmd.envs(env.iter().copied());
        cmd.output().unwrap()
    }

    fn deliver(&self, args: &[&str]) -> Output {
        self.deliver_env(args, &[])
    }

    fn origin(&self) -> Repository {
        Repository::open_bare(self.s.path("origin.git")).unwrap()
    }

    fn origin_ref(&self, name: &str) -> Option<Oid> {
        self.s.remote_ref("origin.git", name)
    }

    /// A commit on `origin` at `name`: base's tree with `files` written.
    fn branch_on_origin(&self, name: &str, files: &[(&str, &str)]) -> Oid {
        let repo = self.origin();
        let base = repo.find_commit(self.base).unwrap();
        let mut tree = repo.treebuilder(Some(&base.tree().unwrap())).unwrap();
        for (path, content) in files {
            let blob = repo.blob(content.as_bytes()).unwrap();
            tree.insert(path, blob, 0o100644).unwrap();
        }
        let tree = repo.find_tree(tree.write().unwrap()).unwrap();
        let sig = git2::Signature::now("Upstream", "up@example.com").unwrap();
        repo.commit(
            Some(&format!("refs/heads/{name}")),
            &sig,
            &sig,
            name,
            &tree,
            &[&base],
        )
        .unwrap()
    }

    fn file_at(&self, commit: Oid, path: &str) -> Option<String> {
        let repo = self.origin();
        let tree = repo.find_commit(commit).unwrap().tree().unwrap();
        let entry = tree.get_path(Path::new(path)).ok()?;
        let blob = repo.find_blob(entry.id()).unwrap();
        Some(String::from_utf8_lossy(blob.content()).into_owned())
    }
}

fn assert_exit(out: &Output, code: i32) {
    assert_eq!(
        out.status.code(),
        Some(code),
        "stdout: {}\nstderr: {}",
        text(&out.stdout),
        text(&out.stderr)
    );
}

/// C1: the published change commit lands in the inbox of the volume the
/// flag names, whatever the change's source says; a repeat is already
/// delivered and queues nothing.
#[test]
fn inbox_queues_the_checked_change_once_in_the_named_volume() {
    let Some(p) = published() else { return };
    // Another volume whose branch also holds base.
    let spec = format!("{}:refs/heads/agentfs/other", p.base);
    let out = p.s.git(&p.receiver, &["push", "--quiet", "origin", &spec]);
    assert!(out.status.success(), "{}", text(&out.stderr));

    let out = p.deliver(&["--to", "inbox", "--volume", "other"]);
    assert_exit(&out, 0);
    assert!(
        text(&out.stdout).contains("as inbox 1"),
        "{}",
        text(&out.stdout)
    );
    assert_eq!(
        p.origin_ref("refs/agentfs/other/inbox/0000000001"),
        Some(p.change)
    );
    let src_inbox = |p: &Published| {
        p.s.refs("origin.git")
            .into_iter()
            .filter(|(name, _)| name.starts_with("refs/agentfs/src/inbox/"))
            .count()
    };
    assert_eq!(src_inbox(&p), 0, "it landed in the change's source");

    let out = p.deliver(&["--to", "inbox", "--volume", "src"]);
    assert_exit(&out, 0);
    assert_eq!(
        p.origin_ref("refs/agentfs/src/inbox/0000000001"),
        Some(p.change)
    );
    let before = p.s.refs("origin.git");
    let out = p.deliver(&["--to", "inbox", "--volume", "src"]);
    assert_exit(&out, 0);
    assert!(
        text(&out.stdout).contains("already delivered"),
        "{}",
        text(&out.stdout)
    );
    assert_eq!(p.s.refs("origin.git"), before, "the repeat queued again");
    assert_eq!(src_inbox(&p), 1);

    // Not published: exit 1.
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_agentfs"));
    cmd.args(["fork", "deliver", "src-nope", "--to", "inbox", "--volume"])
        .args(["src", "--repo"])
        .arg(&p.receiver);
    p.s.hermetic(&mut cmd);
    let out = cmd.output().unwrap();
    assert_exit(&out, 1);
    assert!(
        text(&out.stderr).contains("is not published"),
        "{}",
        text(&out.stderr)
    );
}

/// C2: a squash branch is one commit with the change's tree on base; a
/// repeat is already delivered; a ref at another commit (even one carrying
/// the change's trailer) is refused and never moved; `--onto` merges a
/// clean change and refuses a conflicting one with its paths.
#[test]
fn branch_squash_is_created_once_and_never_moved() {
    let Some(p) = published() else { return };
    let ref_name = format!("refs/heads/agentfs-review/{}", p.fork);
    let out = p.deliver(&["--to", "branch"]);
    assert_exit(&out, 0);
    assert!(
        text(&out.stdout).contains(&ref_name),
        "{}",
        text(&out.stdout)
    );
    let squash = p.origin_ref(&ref_name).unwrap();
    let repo = p.origin();
    let commit = repo.find_commit(squash).unwrap();
    let change = repo.find_commit(p.change).unwrap();
    assert_eq!(commit.tree_id(), change.tree_id());
    assert_eq!(commit.parent_ids().collect::<Vec<_>>(), [p.base]);
    assert_eq!(commit.author().email(), change.author().email());
    let message = commit.message().unwrap().to_owned();
    assert_eq!(
        message,
        format!(
            "change of fork {}\n\nAgentfs-Change: {}\n",
            p.fork, p.change
        )
    );

    let out = p.deliver(&["--to", "branch"]);
    assert_exit(&out, 0);
    assert!(
        text(&out.stdout).contains("already delivered"),
        "{}",
        text(&out.stdout)
    );
    assert_eq!(p.origin_ref(&ref_name), Some(squash));

    // A forged ref: the same message and trailer over base's tree.
    let base = repo.find_commit(p.base).unwrap();
    let sig = commit.author();
    let forged_ref = format!("refs/heads/forged/{}", p.fork);
    let forged = repo
        .commit(
            Some(&forged_ref),
            &sig,
            &sig,
            &message,
            &base.tree().unwrap(),
            &[&base],
        )
        .unwrap();
    let out = p.deliver(&["--to", "branch", "--prefix", "forged/"]);
    assert_exit(&out, 1);
    assert!(
        text(&out.stderr).contains("never moved"),
        "{}",
        text(&out.stderr)
    );
    assert_eq!(
        p.origin_ref(&forged_ref),
        Some(forged),
        "it was overwritten"
    );

    // Onto a branch that moved on without conflict.
    let main = p.branch_on_origin("main", &[("c.txt", "c\n")]);
    let out = p.deliver(&["--to", "branch", "--prefix", "onto/", "--onto", "main"]);
    assert_exit(&out, 0);
    let onto = p
        .origin_ref(&format!("refs/heads/onto/{}", p.fork))
        .unwrap();
    let origin = p.origin();
    let merged = origin.find_commit(onto).unwrap();
    assert_eq!(merged.parent_ids().collect::<Vec<_>>(), [main]);
    assert_eq!(p.file_at(onto, "b.txt").as_deref(), Some("from the fork\n"));
    assert_eq!(p.file_at(onto, "c.txt").as_deref(), Some("c\n"));
    assert_eq!(p.file_at(onto, "a.txt").as_deref(), Some("a\n"));

    // Onto a branch that wrote the same path differently.
    p.branch_on_origin("clash", &[("b.txt", "upstream\n")]);
    let out = p.deliver(&["--to", "branch", "--prefix", "clash/", "--onto", "clash"]);
    assert_exit(&out, 1);
    assert!(text(&out.stderr).contains("b.txt"), "{}", text(&out.stderr));
    assert_eq!(p.origin_ref(&format!("refs/heads/clash/{}", p.fork)), None);
}

/// C3: the history shape pushes the fork's history as published (its
/// finish commit); `--onto` with it is refused before anything is pushed.
#[test]
fn branch_history_pushes_the_published_history() {
    let Some(p) = published() else { return };
    let out = p.deliver(&["--to", "branch", "--shape", "history", "--prefix", "hist/"]);
    assert_exit(&out, 0);
    let ref_name = format!("refs/heads/hist/{}", p.fork);
    assert_eq!(p.origin_ref(&ref_name), Some(p.finish));
    assert_eq!(
        p.origin_ref(&format!("refs/heads/agentfs/{}", p.fork)),
        Some(p.finish)
    );
    let out = p.deliver(&["--to", "branch", "--shape", "history", "--prefix", "hist/"]);
    assert_exit(&out, 0);
    assert!(text(&out.stdout).contains("already delivered"));

    p.branch_on_origin("main", &[("c.txt", "c\n")]);
    let before = p.s.refs("origin.git");
    let out = p.deliver(&[
        "--to", "branch", "--shape", "history", "--prefix", "hist2/", "--onto", "main",
    ]);
    assert_exit(&out, 1);
    assert!(
        text(&out.stderr).contains("--onto"),
        "{}",
        text(&out.stderr)
    );
    assert_eq!(p.s.refs("origin.git"), before);

    // A flag of another method is refused, not ignored.
    let out = p.deliver(&["--to", "branch", "--url", "http://127.0.0.1:1/"]);
    assert_exit(&out, 1);
    assert!(text(&out.stderr).contains("--url"), "{}", text(&out.stderr));
}

/// The receiver script: writes the payload, its arguments and its
/// environment into `$1`, then exits with `$2` (or sleeps when `$2` is
/// `sleep`, through a child of its own).
const RECEIVER: &str = r#"#!/bin/sh
out="$1"
code="$2"
shift 2
cat > "$out/payload.json"
for a in "$@"; do printf '%s\n' "$a"; done > "$out/args"
env > "$out/env"
if [ "$code" = sleep ]; then
  sleep 30
  exit 0
fi
exit "$code"
"#;

/// C4: the program gets the v1 payload on stdin, its arguments literally
/// (no shell), and an environment of PATH alone; its exit status decides;
/// one that outlives --timeout is killed and counts unavailable.
#[test]
fn command_runs_the_program_without_a_shell() {
    let Some(p) = published() else { return };
    let script = p.s.path("receive.sh");
    fs::write(&script, RECEIVER).unwrap();
    {
        use std::os::unix::fs::PermissionsExt as _;
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
    }
    let out_dir = p.s.path("received");
    fs::create_dir_all(&out_dir).unwrap();
    let script = script.display().to_string();
    let out_arg = out_dir.display().to_string();
    let run = |code: &str, extra: &[&str]| {
        let mut args = vec!["--to", "command"];
        args.extend_from_slice(extra);
        args.extend(["--", script.as_str(), out_arg.as_str(), code, "$(touch x)"]);
        p.deliver(&args)
    };

    let out = run("0", &[]);
    assert_exit(&out, 0);
    assert!(
        text(&out.stdout).contains("accepted"),
        "{}",
        text(&out.stdout)
    );
    let payload: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(out_dir.join("payload.json")).unwrap()).unwrap();
    let fork = &p.fork;
    assert_eq!(
        payload,
        serde_json::json!({
            "schema": 1,
            "fork": fork,
            "source": "src",
            "base": p.base.to_string(),
            "change": p.change.to_string(),
            "finish": p.finish.to_string(),
            "fetch": {
                "remote": "origin",
                "change": format!("refs/agentfs/{fork}/change"),
                "finish": format!("refs/agentfs/{fork}/finish"),
                "history": format!("refs/heads/agentfs/{fork}"),
            },
        })
    );
    assert_eq!(
        fs::read_to_string(out_dir.join("args")).unwrap(),
        "$(touch x)\n"
    );
    assert!(!p.s.path("x").exists(), "a shell expanded an argument");
    let env = fs::read_to_string(out_dir.join("env")).unwrap();
    assert!(env.lines().any(|l| l.starts_with("PATH=")), "{env}");
    for var in ["HOME=", "AGENTFS_REPLICA=", "GIT_AUTHOR_NAME="] {
        assert!(
            !env.lines().any(|l| l.starts_with(var)),
            "{var} reached it: {env}"
        );
    }

    assert_exit(&run("75", &[]), 75);
    assert_exit(&run("3", &[]), 1);

    let started = Instant::now();
    let out = run("sleep", &["--timeout", "1s"]);
    let took = started.elapsed();
    assert_exit(&out, 75);
    assert!(took < Duration::from_secs(20), "took {took:?}");
    assert!(
        text(&out.stderr).contains("killed"),
        "{}",
        text(&out.stderr)
    );
}

/// A request the fake receiver saw.
#[derive(Debug, Clone)]
struct Seen {
    method: String,
    path: String,
    headers: BTreeMap<String, String>,
    body: Vec<u8>,
}

/// An HTTP/1.1 receiver on a loopback port that answers by path and
/// records every request.
struct Receiver {
    port: u16,
    seen: Arc<Mutex<Vec<Seen>>>,
}

impl Receiver {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let log = seen.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut line = String::new();
                if reader.read_line(&mut line).is_err() {
                    continue;
                }
                let mut parts = line.split_whitespace();
                let method = parts.next().unwrap_or("").to_owned();
                let path = parts.next().unwrap_or("").to_owned();
                let mut headers = BTreeMap::new();
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
                let (status, extra) = match path.as_str() {
                    "/ok" | "/second" => ("200 OK", String::new()),
                    "/dup" => ("409 Conflict", String::new()),
                    "/busy" => ("503 Service Unavailable", String::new()),
                    "/bad" => ("400 Bad Request", String::new()),
                    "/redirect" => (
                        "302 Found",
                        format!("Location: http://127.0.0.1:{port}/second\r\n"),
                    ),
                    _ => ("404 Not Found", String::new()),
                };
                log.lock().unwrap().push(Seen {
                    method,
                    path,
                    headers,
                    body,
                });
                let _ = write!(
                    stream,
                    "HTTP/1.1 {status}\r\nContent-Length: 0\r\nConnection: close\r\n{extra}\r\n"
                );
            }
        });
        Self { port, seen }
    }

    fn url(&self, path: &str) -> String {
        format!("http://127.0.0.1:{}{path}", self.port)
    }

    fn seen(&self) -> Vec<Seen> {
        self.seen.lock().unwrap().clone()
    }
}

/// C5: one POST of the v1 payload, keyed by the change, with the bearer
/// token from the file; the status decides; no redirect is followed and
/// no proxy from the environment is used.
#[test]
fn webhook_posts_the_payload_keyed_by_the_change() {
    let Some(p) = published() else { return };
    let hook = Receiver::start();
    let token = p.s.path("token");
    fs::write(&token, "s3cret\n").unwrap();
    let token = token.display().to_string();
    // A proxy from the environment would make every request fail.
    let proxies = [
        ("http_proxy", "http://127.0.0.1:1"),
        ("HTTP_PROXY", "http://127.0.0.1:1"),
        ("ALL_PROXY", "http://127.0.0.1:1"),
    ];
    let url = hook.url("/ok");
    let out = p.deliver_env(
        &[
            "--to",
            "webhook",
            "--url",
            &url,
            "--credential-file",
            &token,
        ],
        &proxies,
    );
    assert_exit(&out, 0);
    let seen = hook.seen();
    assert_eq!(seen.len(), 1, "{seen:?}");
    let first = &seen[0];
    assert_eq!(first.method, "POST");
    assert_eq!(first.path, "/ok");
    assert_eq!(
        first.headers.get("authorization").map(String::as_str),
        Some("Bearer s3cret")
    );
    assert_eq!(
        first.headers.get("content-type").map(String::as_str),
        Some("application/json")
    );
    let payload: serde_json::Value = serde_json::from_slice(&first.body).unwrap();
    assert_eq!(payload["schema"], 1);
    assert_eq!(payload["change"], p.change.to_string());
    assert_eq!(payload["fork"], p.fork.as_str());

    let webhook = |path: &str| p.deliver(&["--to", "webhook", "--url", &hook.url(path)]);
    let out = webhook("/dup");
    assert_exit(&out, 0);
    assert!(text(&out.stdout).contains("already delivered"));
    assert_exit(&webhook("/busy"), 75);
    assert_exit(&webhook("/bad"), 1);
    let out = webhook("/redirect");
    assert_exit(&out, 1);
    assert!(
        text(&out.stderr).contains("never followed"),
        "{}",
        text(&out.stderr)
    );

    let seen = hook.seen();
    let paths: Vec<&str> = seen.iter().map(|s| s.path.as_str()).collect();
    assert_eq!(
        paths,
        ["/ok", "/dup", "/busy", "/bad", "/redirect"],
        "a redirect was followed"
    );
    for s in &seen {
        assert_eq!(
            s.headers.get("idempotency-key").map(String::as_str),
            Some(p.change.to_string().as_str()),
            "{s:?}"
        );
    }
    assert!(
        seen[1..]
            .iter()
            .all(|s| !s.headers.contains_key("authorization"))
    );
}

/// A webhook URL carrying a password is refused before any request, and
/// the refusal never prints it: the token belongs in `--credential-file`
/// (bead agentfs-5sg, fix round).
#[test]
fn a_webhook_url_with_a_password_is_refused_unprinted() {
    let Some(p) = published() else { return };
    let hook = Receiver::start();
    let url = format!("http://u:s3cret@127.0.0.1:{}/ok", hook.port);
    let out = p.deliver(&["--to", "webhook", "--url", &url]);
    assert!(!out.status.success(), "{}", text(&out.stdout));
    let shown = format!("{}{}", text(&out.stdout), text(&out.stderr));
    assert!(!shown.contains("s3cret"), "{shown}");
    assert!(shown.contains("password"), "{shown}");
    assert!(hook.seen().is_empty(), "{:?}", hook.seen());
}

/// C7: the CSI driver never delivers (gate answer "2 a for CSI").
#[test]
fn the_csi_driver_never_delivers() {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("../agentfs-csi/src");
    let mut files = vec![src];
    let mut checked = 0;
    while let Some(path) = files.pop() {
        if path.is_dir() {
            files.extend(fs::read_dir(&path).unwrap().map(|e| e.unwrap().path()));
            continue;
        }
        if path.extension().is_none_or(|e| e != "rs") {
            continue;
        }
        checked += 1;
        let source = fs::read_to_string(&path).unwrap();
        for needle in [
            "fork::deliver",
            "deliver::",
            "Delivery",
            "{deliver",
            "\"deliver\"",
        ] {
            assert!(
                !source.contains(needle),
                "{} mentions {needle:?}: the CSI driver only publishes",
                path.display()
            );
        }
    }
    assert!(checked > 5, "found only {checked} CSI sources");
}
