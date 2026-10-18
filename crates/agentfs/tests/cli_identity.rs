//! Committer and author identities from the `[agentfs "identity"]`
//! templates, end to end through the command line (design §9, §18 test
//! 10): every variable from the pod flags, the `{replica}` and `{podUid}`
//! fallbacks, the refusal of a bad template, fronts of a history service
//! committing as their own pods, and a template change across a restart
//! of the service. No FUSE; the mounts are in `tests/mount.rs`
//! (`identity_templates_through_mount`).

use std::fs;
use std::path::PathBuf;
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

use agentfs_core::volume::StateDir;

const BRANCH: &str = "refs/heads/agentfs/demo";

const TEMPLATES: [(&str, &str); 4] = [
    (
        "agentfs.identity.committerName",
        "{replica} of {pod} on {node}",
    ),
    (
        "agentfs.identity.committerEmail",
        "{podUid}@{namespace}.{volume}.test.invalid",
    ),
    ("agentfs.identity.authorName", "writer {uid}:{gid}"),
    (
        "agentfs.identity.authorEmail",
        "uid-{uid}.gid-{gid}@{pod}.{volume}.test.invalid",
    ),
];

/// One commit's identities and its `Agentfs-Pod` trailer.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Who {
    committer: (String, String),
    author: (String, String),
    pod: Option<String>,
}

struct Volume {
    dir: tempfile::TempDir,
}

fn text(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

fn uid() -> u32 {
    rustix::process::getuid().as_raw()
}

fn gid() -> u32 {
    rustix::process::getgid().as_raw()
}

impl Volume {
    fn new() -> Self {
        let v = Self {
            dir: tempfile::tempdir().unwrap(),
        };
        fs::create_dir_all(v.tree()).unwrap();
        let out = v
            .command()
            .args(["init", "--volume", "demo"])
            .output()
            .unwrap();
        assert!(out.status.success(), "{}", text(&out.stderr));
        v.config("agentfs.commitWindow", "10ms");
        v
    }

    fn state(&self) -> StateDir {
        StateDir::new(self.dir.path().join("state"))
    }

    fn tree(&self) -> PathBuf {
        self.dir.path().join("tree")
    }

    fn config(&self, key: &str, value: &str) {
        git2::Config::open(&self.state().git().join("config"))
            .unwrap()
            .set_str(key, value)
            .unwrap();
    }

    fn templates(&self) {
        for (key, value) in TEMPLATES {
            self.config(key, value);
        }
    }

    /// `agentfs` with no pod flags from the environment.
    fn command(&self) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_agentfs"));
        for (name, _) in std::env::vars_os() {
            let n = name.to_string_lossy();
            if n.starts_with("OTEL_") || n.starts_with("AGENTFS_") {
                cmd.env_remove(name);
            }
        }
        cmd.arg("--state")
            .arg(self.state().root())
            .arg("--root")
            .arg(self.tree());
        cmd
    }

    /// `agentfs record` of `path` (written with `content` first) with the
    /// pod variables `env`.
    fn record(&self, path: &str, content: &str, env: &[(&str, &str)]) -> Output {
        fs::write(self.tree().join(path), content).unwrap();
        let mut cmd = self.command();
        cmd.args(["record", path]);
        for (k, v) in env {
            cmd.env(k, v);
        }
        cmd.output().unwrap()
    }

    /// `record --history-service` as pod `pod` (namespace `ns`).
    fn front_record(&self, pod: &str, ns: &str, path: &str, content: &str) -> Output {
        fs::write(self.tree().join(path), content).unwrap();
        self.command()
            .args(["record", "--history-service"])
            .arg(self.state().history_socket())
            .args(["--history-wait", "10s", path])
            .env("AGENTFS_POD", pod)
            .env("AGENTFS_POD_UID", format!("{pod}-uid"))
            .env("AGENTFS_NAMESPACE", ns)
            .env("AGENTFS_NODE", "node-1")
            .output()
            .unwrap()
    }

    fn serve(&self) -> Service {
        let log = self.dir.path().join(format!(
            "serve-{}.log",
            fs::read_dir(self.dir.path()).unwrap().count()
        ));
        let child = self
            .command()
            .arg("serve-history")
            .env("AGENTFS_REPLICA", "service-0")
            .env("AGENTFS_POD", "service-pod")
            .env("AGENTFS_POD_UID", "service-uid")
            .env("AGENTFS_NAMESPACE", "system")
            .env("AGENTFS_NODE", "node-1")
            .stdout(fs::File::create(&log).unwrap())
            .stderr(fs::File::create(log.with_extension("err")).unwrap())
            .stdin(Stdio::null())
            .spawn()
            .unwrap();
        let service = Service {
            child: Some(child),
            log,
        };
        service.wait_for("serving history for volume demo");
        service
    }

    /// The commit that last changed `path`.
    fn who(&self, path: &str) -> Who {
        self.all()
            .into_iter()
            .rev()
            .find(|(paths, _)| paths.iter().any(|p| p == path))
            .unwrap_or_else(|| panic!("no commit changed {path}"))
            .1
    }

    /// Every commit, oldest first: the paths it changed and who made it.
    fn all(&self) -> Vec<(Vec<String>, Who)> {
        let repo = git2::Repository::open_bare(self.state().git()).unwrap();
        let mut walk = repo.revwalk().unwrap();
        walk.push_ref(BRANCH).unwrap();
        walk.set_sorting(git2::Sort::TOPOLOGICAL | git2::Sort::REVERSE)
            .unwrap();
        walk.map(|id| {
            let commit = repo.find_commit(id.unwrap()).unwrap();
            let parent = commit.parent(0).ok().map(|p| p.tree().unwrap());
            let diff = repo
                .diff_tree_to_tree(parent.as_ref(), Some(&commit.tree().unwrap()), None)
                .unwrap();
            let paths = diff
                .deltas()
                .map(|d| d.new_file().path().unwrap().to_string_lossy().into_owned())
                .collect();
            let sig =
                |s: git2::Signature| (s.name().unwrap().to_owned(), s.email().unwrap().to_owned());
            let who = Who {
                committer: sig(commit.committer()),
                author: sig(commit.author()),
                pod: commit
                    .message()
                    .unwrap()
                    .lines()
                    .find_map(|l| l.strip_prefix("Agentfs-Pod: "))
                    .map(str::to_owned),
            };
            (paths, who)
        })
        .collect()
    }
}

fn pair(name: &str, email: &str) -> (String, String) {
    (name.to_owned(), email.to_owned())
}

/// A running `agentfs serve-history`.
struct Service {
    child: Option<Child>,
    log: PathBuf,
}

impl Service {
    fn log(&self) -> String {
        format!(
            "{}{}",
            fs::read_to_string(&self.log).unwrap_or_default(),
            fs::read_to_string(self.log.with_extension("err")).unwrap_or_default()
        )
    }

    fn wait_for(&self, needle: &str) {
        let deadline = Instant::now() + Duration::from_secs(20);
        while !self.log().contains(needle) {
            assert!(
                Instant::now() < deadline,
                "never logged {needle:?}:\n{}",
                self.log()
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    fn signal(&mut self, signal: rustix::process::Signal) -> String {
        let mut child = self.child.take().unwrap();
        let pid = rustix::process::Pid::from_child(&child);
        rustix::process::kill_process(pid, signal).unwrap();
        let status = child.wait().unwrap();
        if signal == rustix::process::Signal::TERM {
            assert!(status.success(), "{status:?}:\n{}", self.log());
        }
        self.log()
    }

    fn stop(mut self) -> String {
        self.signal(rustix::process::Signal::TERM)
    }

    fn kill(mut self) -> String {
        self.signal(rustix::process::Signal::KILL)
    }
}

impl Drop for Service {
    fn drop(&mut self) {
        if let Some(child) = &mut self.child {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// Every template variable, from the pod flags and from their environment
/// variables, renders into the commit exactly; the author is the caller's
/// uid and gid.
#[test]
fn every_variable_renders_from_flags_and_environment() {
    let v = Volume::new();
    v.templates();

    let out = v.record(
        "env.txt",
        "1",
        &[
            ("AGENTFS_NAMESPACE", "agents"),
            ("AGENTFS_POD", "web-7c9d8-x2k4q"),
            ("AGENTFS_POD_UID", "0b6f-11"),
            ("AGENTFS_NODE", "hawking"),
            ("AGENTFS_REPLICA", "web-0"),
        ],
    );
    assert!(out.status.success(), "{}", text(&out.stderr));
    let (u, g) = (uid(), gid());
    assert_eq!(
        v.who("env.txt"),
        Who {
            committer: pair(
                "web-0 of web-7c9d8-x2k4q on hawking",
                "0b6f-11@agents.demo.test.invalid"
            ),
            author: pair(
                &format!("writer {u}:{g}"),
                &format!("uid-{u}.gid-{g}@web-7c9d8-x2k4q.demo.test.invalid")
            ),
            pod: Some("web-7c9d8-x2k4q".into()),
        }
    );

    // The flags, which win over the environment.
    fs::write(v.tree().join("flags.txt"), "2").unwrap();
    let out = v
        .command()
        .args([
            "record",
            "--namespace",
            "ns2",
            "--pod",
            "api-1",
            "--pod-uid",
            "uid-2",
            "--node",
            "feynman",
            "--replica",
            "api-r",
            "flags.txt",
        ])
        .env("AGENTFS_POD", "ignored")
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", text(&out.stderr));
    let who = v.who("flags.txt");
    assert_eq!(
        who.committer,
        pair("api-r of api-1 on feynman", "uid-2@ns2.demo.test.invalid")
    );
    assert_eq!(
        who.author.1,
        format!("uid-{u}.gid-{g}@api-1.demo.test.invalid")
    );
}

/// `{replica}` falls back to the pod name, and `{podUid}` to the replica
/// (design §9; docs/configuration.md); the defaults render as documented.
#[test]
fn replica_and_pod_uid_fall_back_as_documented() {
    let v = Volume::new();

    // The defaults: committer {replica}, {podUid}@{volume}.agentfs.invalid.
    let out = v.record(
        "a",
        "a",
        &[
            ("AGENTFS_POD", "web-1"),
            ("AGENTFS_POD_UID", "u-1"),
            ("AGENTFS_REPLICA", "web-r"),
        ],
    );
    assert!(out.status.success(), "{}", text(&out.stderr));
    let u = uid();
    assert_eq!(
        v.who("a"),
        Who {
            committer: pair("web-r", "u-1@demo.agentfs.invalid"),
            author: pair(
                &format!("uid-{u}"),
                &format!("uid-{u}@demo.agentfs.invalid")
            ),
            pod: Some("web-1".into()),
        }
    );

    // No replica: the pod name; no pod uid: the replica.
    let out = v.record("b", "b", &[("AGENTFS_POD", "web-2")]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    assert_eq!(
        v.who("b").committer,
        pair("web-2", "web-2@demo.agentfs.invalid")
    );

    // An empty replica counts as none.
    let out = v.record(
        "c",
        "c",
        &[("AGENTFS_POD", "web-3"), ("AGENTFS_REPLICA", "")],
    );
    assert!(out.status.success(), "{}", text(&out.stderr));
    assert_eq!(v.who("c").committer.0, "web-3");
}

/// A template with an unknown variable or an unclosed `{`, or one that
/// renders empty, refuses to start with the documented error, and records
/// nothing.
#[test]
fn a_bad_template_is_refused_at_startup() {
    let cases = [
        (
            "agentfs.identity.committerName",
            "{replica} {cluster}",
            "unknown template variable {cluster}",
        ),
        (
            "agentfs.identity.authorEmail",
            "uid-{uid@x.invalid",
            "unclosed `{` in template",
        ),
        (
            "agentfs.identity.committerName",
            "{namespace}",
            "is empty or contains `<`, `>` or a newline",
        ),
        (
            "agentfs.identity.committerEmail",
            "<{pod}>",
            "is empty or contains `<`, `>` or a newline",
        ),
    ];
    for (key, value, want) in cases {
        let v = Volume::new();
        v.config(key, value);
        let out = v.record("a", "a", &[("AGENTFS_POD", "p")]);
        let err = text(&out.stderr);
        assert_eq!(out.status.code(), Some(1), "{key}={value}: {err}");
        assert!(err.contains(want), "{key}={value}: {err}");
        assert!(
            git2::Repository::open_bare(v.state().git())
                .unwrap()
                .find_reference(BRANCH)
                .is_err(),
            "{key}={value} committed"
        );
        let mut serve = v.command();
        let out = serve
            .arg("serve-history")
            .env("AGENTFS_POD", "p")
            .output()
            .unwrap();
        let err = text(&out.stderr);
        assert_eq!(
            out.status.code(),
            Some(1),
            "serve-history {key}={value}: {err}"
        );
        assert!(err.contains(want), "serve-history {key}={value}: {err}");
    }
}

/// Fronts of one history service commit under their own pods' rendered
/// identities, not the service's; a front whose pod does not render is
/// refused when it connects.
#[test]
fn fronts_commit_under_their_own_pods() {
    let v = Volume::new();
    v.templates();
    let service = v.serve();

    let out = v.front_record("pod-a", "team-a", "a", "from a");
    assert!(out.status.success(), "{}", text(&out.stderr));
    let out = v.front_record("pod-b", "team-b", "b", "from b");
    assert!(out.status.success(), "{}", text(&out.stderr));

    // A pod name with `<` cannot be in a committer: refused.
    let out = v.front_record("pod<c>", "team-c", "c", "from c");
    assert!(!out.status.success());
    let err = text(&out.stderr);
    assert!(
        err.contains("identity templates do not render for pod \"pod<c>\""),
        "{err}"
    );

    service.stop();
    let (u, g) = (uid(), gid());
    for (path, pod, ns) in [("a", "pod-a", "team-a"), ("b", "pod-b", "team-b")] {
        assert_eq!(
            v.who(path),
            Who {
                // A front's replica defaults to its pod.
                committer: pair(
                    &format!("{pod} of {pod} on node-1"),
                    &format!("{pod}-uid@{ns}.demo.test.invalid")
                ),
                author: pair(
                    &format!("writer {u}:{g}"),
                    &format!("uid-{u}.gid-{g}@{pod}.demo.test.invalid")
                ),
                pod: Some(pod.into()),
            }
        );
    }
    assert!(
        v.all()
            .iter()
            .all(|(paths, _)| !paths.contains(&"c".into()))
    );
}

/// A template change across a restart of the service: commits already
/// made keep their identities, new ones take the new template, and a
/// journaled record whose pod no longer renders is committed with the
/// service's own committer, keeping its pod trailer, with the
/// `IdentityFallback` alarm.
#[test]
fn a_template_change_across_a_restart() {
    let v = Volume::new();
    v.templates();
    let service = v.serve();
    let out = v.front_record("pod-a", "team-a", "a", "1");
    assert!(out.status.success(), "{}", text(&out.stderr));
    service.stop();
    let before = v.who("a");
    assert_eq!(before.committer.0, "pod-a of pod-a on node-1");

    // A record journaled and not committed when the service dies; then
    // the templates change so that pod-a's committer no longer renders.
    v.config("agentfs.commitWindow", "1h");
    let service = v.serve();
    let out = v.front_record("pod-a", "team-a", "b", "2");
    assert!(out.status.success(), "{}", text(&out.stderr));
    service.kill();
    v.config("agentfs.commitWindow", "10ms");
    v.config(
        "agentfs.identity.committerName",
        "{namespace}-{pod} (renamed)",
    );
    v.config(
        "agentfs.identity.committerEmail",
        "{podUid}@{namespace}.{volume}.test.invalid",
    );
    // pod-a's journaled origin has namespace team-a: it still renders,
    // under the new template.
    let service = v.serve();
    let deadline = Instant::now() + Duration::from_secs(20);
    while v.all().iter().all(|(p, _)| !p.contains(&"b".into())) {
        assert!(
            Instant::now() < deadline,
            "b never committed:\n{}",
            service.log()
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    assert_eq!(v.who("a"), before, "an old commit changed");
    assert_eq!(
        v.who("b").committer,
        pair(
            "team-a-pod-a (renamed)",
            "pod-a-uid@team-a.demo.test.invalid"
        )
    );
    service.stop();

    // Now a journaled record whose pod cannot render: a front with no
    // namespace, while the templates did not need one.
    v.config("agentfs.identity.committerName", "{pod}");
    v.config(
        "agentfs.identity.committerEmail",
        "{podUid}@{volume}.test.invalid",
    );
    v.config("agentfs.commitWindow", "1h");
    let service = v.serve();
    let out = v.front_record("pod-n", "", "n", "3");
    assert!(out.status.success(), "{}", text(&out.stderr));
    service.kill();
    v.config("agentfs.commitWindow", "10ms");
    v.config("agentfs.identity.committerName", "{namespace}");
    let service = v.serve();
    let deadline = Instant::now() + Duration::from_secs(20);
    while v.all().iter().all(|(p, _)| !p.contains(&"n".into())) {
        assert!(
            Instant::now() < deadline,
            "n never committed:\n{}",
            service.log()
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    let log = service.stop();
    assert!(
        log.contains("the committer of front pod-n no longer renders"),
        "{log}"
    );
    let who = v.who("n");
    // The service's own committer, the pod's trailer.
    assert_eq!(
        who.committer,
        pair("system", "service-uid@demo.test.invalid")
    );
    assert_eq!(who.pod.as_deref(), Some("pod-n"));
}
