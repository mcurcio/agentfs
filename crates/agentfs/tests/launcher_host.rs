//! `agentfs host` driven as its launcher drives it (docs/library/filesystem.md,
//! "The view host command"): the real binary, a Unix socket pair as its
//! standard input, one JSON line per request and reply. Without root: the
//! control channel's check, the hello, keyed forks and their refusals,
//! reconcile, the drain at EOF, a busy volume, the name guard and the
//! not-built log. With root and /dev/fuse (ignored by default; run by
//! ci/fuse-tests.sh with sudo): views opened with a pod context, closed,
//! landed from a fork, rebound and stacked after a kill, a fork's one
//! writer, and a close held open by a file waiting alone while the requests
//! that name nothing it holds are answered by id.

use std::io::{BufRead, BufReader, Read as _, Write as _};
use std::os::fd::OwnedFd;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::thread::JoinHandle;

use agentfs_contracts::host::{
    Envelope, HOST_PROTOCOL, Hello, Refusal, RefusalKind, Reply, Request, TreeRef, ViewRequest,
};
use agentfs_contracts::name::{ClientLabel, ForkKey, Label};
use agentfs_contracts::pod::PodContext;
use agentfs_contracts::tree::TreeId;
use agentfs_contracts::trees::{ForkSettings, Landing};
use agentfs_core::file_lock::{FileLock, Mode};
use agentfs_fuse::stale::{MountState, mount_state};

struct Volume {
    dir: tempfile::TempDir,
    /// The id `init` printed.
    id: String,
}

impl Volume {
    /// A tree volume `t`, made by `agentfs init`.
    fn new() -> Self {
        Self::with_init(&[])
    }

    fn with_init(extra: &[&str]) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let mut v = Self {
            dir,
            id: String::new(),
        };
        let out = v
            .command()
            .args(["init", "--volume", "t"])
            .args(extra)
            .output()
            .unwrap();
        assert!(out.status.success(), "{}", text(&out.stderr));
        let printed = text(&out.stdout);
        let fields: Vec<&str> = printed.split_whitespace().collect();
        assert_eq!(fields.first(), Some(&"t"), "{printed}");
        v.id = fields[1].to_owned();
        v
    }

    /// A tree volume `t` for the root tests, or `None` (with a message)
    /// without root and FUSE.
    fn mounted() -> Option<Self> {
        if !rustix::process::geteuid().is_root() || !Path::new("/dev/fuse").exists() {
            eprintln!("skipped: needs root and /dev/fuse (ci/fuse-tests.sh)");
            return None;
        }
        Some(Self::new())
    }

    fn p(&self, rel: &str) -> PathBuf {
        self.dir.path().join(rel)
    }

    fn state(&self) -> PathBuf {
        self.p("state")
    }

    fn command(&self) -> Command {
        self.command_at(&self.state())
    }

    /// The binary, its OTEL and AGENTFS variables removed, at `state`.
    fn command_at(&self, state: &Path) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_agentfs"));
        for (name, _) in std::env::vars_os() {
            let n = name.to_string_lossy();
            if n.starts_with("OTEL_") || n.starts_with("AGENTFS_") {
                cmd.env_remove(name);
            }
        }
        cmd.arg("--state").arg(state);
        cmd
    }

    /// `agentfs host --launcher csi --volume t [EXTRA]`, its standard input
    /// one end of a socket pair whose other end this test holds.
    fn host(&self, extra: &[&str]) -> Host {
        let (ours, theirs) = UnixStream::pair().unwrap();
        let mut cmd = self.command();
        cmd.args(["host", "--launcher", "csi", "--volume", "t"])
            .args(extra)
            .stdin(Stdio::from(OwnedFd::from(theirs)))
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        let mut child = cmd.spawn().unwrap();
        let mut stderr = child.stderr.take().unwrap();
        let log = std::thread::spawn(move || {
            let mut all = String::new();
            let _ = stderr.read_to_string(&mut all);
            all
        });
        Host {
            child,
            reader: BufReader::new(ours.try_clone().unwrap()),
            control: Some(ours),
            log: Some(log),
            next: 1,
        }
    }

    /// The volume's lease (`actor.lock`), taken once whoever holds it lets
    /// go. A killed host's Git children keep its lease until each has
    /// exited, which `wait` on the host does not wait for: a host started
    /// before then exits 75, as the launcher's contract says.
    fn lease(&self) -> FileLock {
        let file = FileLock::open(&self.p("state/actor.lock")).unwrap();
        FileLock::wait(file, Mode::Exclusive).unwrap()
    }

    /// SIGKILL to `host`, which leaves its views stale, and the lease free:
    /// the next host of this volume is not busy.
    fn kill(&self, mut host: Host) {
        host.kill();
        drop(self.lease());
    }

    /// Every file under the state area with its size and modification time.
    fn snapshot(&self) -> Vec<(PathBuf, u64, std::time::SystemTime)> {
        fn walk(dir: &Path, out: &mut Vec<(PathBuf, u64, std::time::SystemTime)>) {
            let Ok(entries) = std::fs::read_dir(dir) else {
                return;
            };
            for entry in entries {
                let entry = entry.unwrap();
                let meta = entry.metadata().unwrap();
                out.push((entry.path(), meta.len(), meta.modified().unwrap()));
                if meta.is_dir() {
                    walk(&entry.path(), out);
                }
            }
        }
        let mut out = Vec::new();
        walk(&self.state(), &mut out);
        out.sort();
        out
    }

    /// Every commit message of the root's history that touched `path`.
    fn messages(&self, path: &str) -> String {
        let hash = Command::new("git")
            .args(["hash-object", "--stdin"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .and_then(|mut git| {
                git.stdin.take().unwrap().write_all(path.as_bytes())?;
                git.wait_with_output()
            })
            .unwrap();
        let hex = text(&hash.stdout).trim().to_owned();
        let out = Command::new("git")
            .arg("--git-dir")
            .arg(self.p("state/history.git"))
            .args(["log", "--format=%B", "root", "--"])
            .arg(format!("files/{}/{}/{path}", &hex[..2], &hex[2..4]))
            .output()
            .unwrap();
        assert!(out.status.success(), "{}", text(&out.stderr));
        text(&out.stdout)
    }
}

/// A running host and this test's end of its control channel.
struct Host {
    child: Child,
    control: Option<UnixStream>,
    reader: BufReader<UnixStream>,
    log: Option<JoinHandle<String>>,
    /// The next request's id.
    next: u64,
}

impl Host {
    /// The next line, or `None` at EOF.
    fn line(&mut self) -> Option<String> {
        let mut line = String::new();
        let n = self.reader.read_line(&mut line).unwrap();
        (n > 0).then_some(line)
    }

    /// The host's hello. A host that ended before it wrote one fails the
    /// test with its exit status and what it logged.
    fn hello(&mut self) -> Hello {
        let Some(line) = self.line() else {
            let status = self.child.wait().unwrap();
            let log = self.log.take().map(|log| log.join().unwrap());
            panic!(
                "the host wrote no hello: {status}: {}",
                log.unwrap_or_default()
            );
        };
        serde_json::from_str(&line).unwrap()
    }

    /// Sends `request` and reads the next line: its reply.
    fn ask(&mut self, request: &Request) -> Reply {
        let id = self.send(request);
        self.reply_to(id)
    }

    /// Sends `request` with the next id, and returns the id.
    fn send(&mut self, request: &Request) -> u64 {
        let id = self.next;
        self.next += 1;
        self.send_as(id, request);
        id
    }

    fn send_as(&mut self, id: u64, request: &Request) {
        let line = serde_json::to_string(&Envelope {
            id,
            body: request.clone(),
        })
        .unwrap();
        let control = self.control.as_mut().expect("the channel is open");
        writeln!(control, "{line}").unwrap();
    }

    /// The next line: a reply and the id it answers.
    fn reply(&mut self) -> Envelope<Reply> {
        let line = self.line().expect("the host ended before it replied");
        serde_json::from_str(&line).unwrap()
    }

    /// The next line, which must answer `id`.
    fn reply_to(&mut self, id: u64) -> Reply {
        let reply = self.reply();
        assert_eq!(reply.id, id, "the next reply answers another request");
        reply.body
    }

    /// Shuts this test's end for writing (EOF; the reader is a clone of
    /// the same socket): the host drains and exits.
    fn eof(&mut self) -> (ExitStatus, String) {
        if let Some(control) = self.control.take() {
            control.shutdown(std::net::Shutdown::Write).unwrap();
        }
        let status = self.child.wait().unwrap();
        (status, self.log.take().unwrap().join().unwrap())
    }

    /// SIGKILL: the host's views are left stale, and its lease is held
    /// until its Git children have exited ([`Volume::kill`] waits for that).
    fn kill(&mut self) {
        self.child.kill().unwrap();
        self.child.wait().unwrap();
    }
}

impl Drop for Host {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

fn key(text: &str) -> ForkKey {
    ForkKey::parse(text).unwrap()
}

fn fork_create(upstream: TreeRef, k: &str, landing: Landing) -> Request {
    Request::ForkCreate {
        upstream,
        key: key(k),
        settings: ForkSettings { landing },
    }
}

fn kind(reply: &Reply) -> Option<RefusalKind> {
    match reply {
        Reply::Refused(Refusal { kind, .. }) => Some(*kind),
        _ => None,
    }
}

fn forked(reply: &Reply) -> TreeId {
    match reply {
        Reply::Forked { tree } => *tree,
        other => panic!("not forked: {other:?}"),
    }
}

fn pod(namespace: &str, name: &str, uid: &str) -> PodContext {
    PodContext {
        namespace: Some(Label::parse(namespace).unwrap()),
        pod: Some(Label::parse(name).unwrap()),
        pod_uid: Some(Label::parse(uid).unwrap()),
        node: None,
    }
}

fn open(tree: TreeRef, at: &Path, label: &str, pod: PodContext) -> Request {
    Request::Open(ViewRequest {
        tree,
        at: at.to_path_buf(),
        label: ClientLabel::parse(label).unwrap(),
        read_only: false,
        pod,
    })
}

fn close(at: &Path) -> Request {
    Request::Close {
        at: at.to_path_buf(),
    }
}

fn mkdir(v: &Volume, name: &str) -> PathBuf {
    let at = v.p(name);
    std::fs::create_dir(&at).unwrap();
    at
}

/// (a) A pipe as the standard input exits 2, and the state area is
/// untouched: one never made stays absent, and a volume's files are as
/// they were.
#[test]
fn a_pipe_is_no_control_channel() {
    let v = Volume::new();
    let before = v.snapshot();
    for state in [v.state(), v.p("none")] {
        let out = v
            .command_at(&state)
            .args(["host", "--launcher", "csi", "--volume", "t"])
            .stdin(Stdio::piped())
            .output()
            .unwrap();
        assert_eq!(out.status.code(), Some(2), "{}", text(&out.stderr));
        assert!(
            text(&out.stderr).contains(
                "error: agentfs host is run by a launcher that controls it over its standard \
                 input, a Unix socket"
            ),
            "{}",
            text(&out.stderr)
        );
    }
    assert!(!v.p("none").exists());
    assert_eq!(v.snapshot(), before, "the state area was touched");
}

/// (b), (c), (d), (e): the hello; keyed forks and their refusals; reconcile;
/// EOF drains the host to exit 0, and the volume is free.
#[test]
fn forks_by_key_and_the_drain() {
    let v = Volume::new();
    let mut host = v.host(&[]);
    let hello = host.hello();
    assert_eq!(hello.v, HOST_PROTOCOL);
    assert_eq!(hello.volume.as_str(), "t");
    assert_eq!(hello.id.0.hyphenated().to_string(), v.id);

    let one = forked(&host.ask(&fork_create(TreeRef::Root, "k1", Landing::Automatic)));
    assert!(matches!(one, TreeId::Fork(_)), "{one}");
    assert_eq!(
        host.ask(&fork_create(TreeRef::Root, "k1", Landing::Automatic)),
        Reply::Forked { tree: one },
        "the same key, the same fork"
    );
    let nested = host.ask(&fork_create(
        TreeRef::Fork(key("k1")),
        "k2",
        Landing::Automatic,
    ));
    assert_eq!(
        kind(&nested),
        Some(RefusalKind::NestedNotBuilt),
        "{nested:?}"
    );
    let approval = host.ask(&fork_create(TreeRef::Root, "k3", Landing::Approval));
    assert_eq!(
        kind(&approval),
        Some(RefusalKind::ApprovalNotBuilt),
        "{approval:?}"
    );
    let two = forked(&host.ask(&fork_create(TreeRef::Root, "k4", Landing::Automatic)));

    // The same steps on another volume, without the approval: the same
    // numbers, so the refused create wrote nothing.
    let control = Volume::new();
    let mut other = control.host(&[]);
    other.hello();
    assert_eq!(
        forked(&other.ask(&fork_create(TreeRef::Root, "k1", Landing::Automatic))),
        one
    );
    other.ask(&fork_create(
        TreeRef::Fork(key("k1")),
        "k2",
        Landing::Automatic,
    ));
    assert_eq!(
        forked(&other.ask(&fork_create(TreeRef::Root, "k4", Landing::Automatic))),
        two,
        "the refused approval took a fork number"
    );
    let (status, log) = other.eof();
    assert!(status.success(), "{log}");

    assert_eq!(host.ask(&Request::ForkEnd { key: key("k1") }), Reply::Ended);
    assert_eq!(
        host.ask(&Request::ForkEnd { key: key("k1") }),
        Reply::Ended,
        "idempotent"
    );
    let unknown = host.ask(&Request::ForkEnd { key: key("nope") });
    assert_eq!(kind(&unknown), Some(RefusalKind::KeyUnknown), "{unknown:?}");
    let ended = host.ask(&fork_create(TreeRef::Root, "k1", Landing::Automatic));
    assert_eq!(kind(&ended), Some(RefusalKind::KeyEnded), "{ended:?}");
    let at = mkdir(&v, "nowhere");
    let missing = host.ask(&open(
        TreeRef::Fork(key("nope")),
        &at,
        "w",
        PodContext::default(),
    ));
    assert_eq!(kind(&missing), Some(RefusalKind::KeyUnknown), "{missing:?}");
    assert_eq!(mount_state(&at).unwrap(), MountState::NotMounted);
    let missing = host.ask(&open(
        TreeRef::Fork(key("nope")),
        &at,
        "w",
        PodContext::default(),
    ));
    assert_eq!(
        kind(&missing),
        Some(RefusalKind::KeyUnknown),
        "an open never makes a fork"
    );

    assert_eq!(
        host.ask(&Request::Reconcile),
        Reply::Reconciled { ended: 0 }
    );

    let (status, log) = host.eof();
    assert_eq!(status.code(), Some(0), "{log}");
    let out = v
        .command()
        .args(["fork", "end", "--fork-key", "k4"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
}

/// (f2) A killed host's lease outlives it while a process that shares it
/// lives (a Git child of its builder). A host started then exits 75 before
/// its hello, and one started once the lease is free serves.
#[test]
fn a_host_started_under_a_killed_hosts_lease_is_busy() {
    let v = Volume::new();
    let mut killed = v.host(&[]);
    killed.hello();
    killed.kill();
    // The lease as a child that outlived the kill holds it.
    let held = v.lease();
    let mut early = v.host(&[]);
    let (status, log) = early.eof();
    assert_eq!(status.code(), Some(75), "{log}");
    drop(held);
    let mut later = v.host(&[]);
    later.hello();
    let (status, log) = later.eof();
    assert_eq!(status.code(), Some(0), "{log}");
}

/// (f) A second host while one serves the volume exits 75.
#[test]
fn a_second_host_is_busy() {
    let v = Volume::new();
    let mut first = v.host(&[]);
    first.hello();
    let mut second = v.host(&[]);
    let (status, log) = second.eof();
    assert_eq!(status.code(), Some(75), "{log}");
    assert_eq!(
        first.ask(&Request::Reconcile),
        Reply::Reconciled { ended: 0 }
    );
    let (status, log) = first.eof();
    assert_eq!(status.code(), Some(0), "{log}");
}

/// (g) Another volume's name exits 1 before the hello, writing nothing.
#[test]
fn another_name_is_refused_before_any_write() {
    let v = Volume::new();
    let before = v.snapshot();
    let (ours, theirs) = UnixStream::pair().unwrap();
    let out = v
        .command()
        .args(["host", "--launcher", "csi", "--volume", "other"])
        .stdin(Stdio::from(OwnedFd::from(theirs)))
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1), "{}", text(&out.stderr));
    assert!(
        text(&out.stderr).contains("nothing was written"),
        "{}",
        text(&out.stderr)
    );
    let mut written = String::new();
    BufReader::new(ours).read_line(&mut written).unwrap();
    assert_eq!(written, "", "a hello was written");
    assert_eq!(v.snapshot(), before, "the state area was touched");
}

/// (h) A remote in the operator settings is logged as not built at boot.
#[test]
fn not_built_settings_are_logged() {
    let dir = tempfile::tempdir().unwrap();
    let include = dir.path().join("operator.conf");
    std::fs::write(
        &include,
        "[remote \"origin\"]\n\turl = https://example.com/r.git\n",
    )
    .unwrap();
    let v = Volume::with_init(&["--include", include.to_str().unwrap()]);
    let mut host = v.host(&[]);
    host.hello();
    let (status, log) = host.eof();
    assert_eq!(status.code(), Some(0), "{log}");
    assert!(
        log.contains("not built: remote.origin.url (push): "),
        "{log}"
    );
}

/// (i) A view of the root opened at a directory with a pod context: a live
/// mount of `agentfs:t`; `close` answers once the view has ended and its
/// client is detached, so another view may open there at once; the host
/// serves on with no view; the commit holding the write names the pod.
#[test]
#[ignore = "needs root and /dev/fuse: ci/fuse-tests.sh"]
fn a_view_opens_with_its_pod_and_closes() {
    let Some(v) = Volume::mounted() else { return };
    let a = mkdir(&v, "a");
    let mut host = v.host(&[]);
    host.hello();
    assert_eq!(
        host.ask(&open(TreeRef::Root, &a, "a", pod("ns", "p1", "u1"))),
        Reply::Opened {
            tree: TreeId::Root,
            stacked: false
        }
    );
    assert_eq!(
        mount_state(&a).unwrap(),
        MountState::Live {
            source: "agentfs:t".into()
        }
    );
    // The kernel names the filesystem by its subtype, as libfuse's mounts do.
    let mountinfo = std::fs::read_to_string("/proc/self/mountinfo").unwrap();
    let entry = mountinfo
        .lines()
        .find(|line| line.split(' ').nth(4) == Some(a.to_str().unwrap()))
        .unwrap_or_else(|| panic!("no mount at {a:?} in {mountinfo}"));
    assert!(entry.contains(" - fuse.agentfs agentfs:t "), "{entry}");
    assert_eq!(
        host.ask(&open(TreeRef::Root, &a, "a", pod("ns", "p1", "u1"))),
        Reply::Opened {
            tree: TreeId::Root,
            stacked: false
        },
        "the same view again"
    );
    let occupied = host.ask(&open(TreeRef::Root, &a, "other", PodContext::default()));
    assert_eq!(kind(&occupied), Some(RefusalKind::Occupied), "{occupied:?}");
    std::fs::write(a.join("f"), "f\n").unwrap();
    // A file held open across the close keeps the view's session alive
    // after its mount has left the namespace: `closed` waits for its end.
    let held = std::fs::File::open(a.join("f")).unwrap();
    let id = host.send(&close(&a));
    drop(held);
    assert_eq!(host.reply_to(id), Reply::Closed);
    assert_eq!(mount_state(&a).unwrap(), MountState::NotMounted);
    // `closed` came once the view had ended and its client was detached:
    // the place is free for another view at once.
    assert_eq!(
        host.ask(&open(TreeRef::Root, &a, "a2", pod("ns", "p3", "u3"))),
        Reply::Opened {
            tree: TreeId::Root,
            stacked: false
        }
    );
    assert_eq!(host.ask(&close(&a)), Reply::Closed);
    assert_eq!(host.ask(&close(&a)), Reply::Closed, "closed already");
    // The last view gone, the host serves on.
    assert_eq!(
        host.ask(&Request::Reconcile),
        Reply::Reconciled { ended: 0 }
    );
    let (status, log) = host.eof();
    assert_eq!(status.code(), Some(0), "{log}");
    let messages = v.messages("f");
    assert!(messages.contains("Agentfs-Pod: p1"), "{messages}");
    assert!(messages.contains("Agentfs-Namespace: ns"), "{messages}");
}

/// (j) A fork's view, opened beside the root's with its own pod: its write
/// lands in the root, with that pod's trailers. The drain at EOF (no view
/// open) releases only with every queue empty, so the exit is the landing's
/// event.
#[test]
#[ignore = "needs root and /dev/fuse: ci/fuse-tests.sh"]
fn a_fork_view_lands_in_the_root() {
    let Some(v) = Volume::mounted() else { return };
    let (a, b) = (mkdir(&v, "a"), mkdir(&v, "b"));
    let mut host = v.host(&[]);
    host.hello();
    assert!(matches!(
        host.ask(&open(TreeRef::Root, &a, "a", pod("ns", "p1", "u1"))),
        Reply::Opened { .. }
    ));
    let fork = forked(&host.ask(&fork_create(TreeRef::Root, "k", Landing::Automatic)));
    assert_eq!(
        host.ask(&open(
            TreeRef::Fork(key("k")),
            &b,
            "b",
            pod("ns", "p2", "u2")
        )),
        Reply::Opened {
            tree: fork,
            stacked: false
        }
    );
    assert_eq!(
        mount_state(&b).unwrap(),
        MountState::Live {
            source: format!("agentfs:t:{fork}")
        }
    );
    std::fs::write(b.join("g"), "g\n").unwrap();
    assert_eq!(host.ask(&close(&b)), Reply::Closed);
    assert_eq!(host.ask(&close(&a)), Reply::Closed);
    let (status, log) = host.eof();
    assert_eq!(status.code(), Some(0), "{log}");
    let messages = v.messages("g");
    assert!(messages.contains("Agentfs-Pod: p2"), "{messages}");
    // Through the root's view, after a restart.
    let mut host = v.host(&[]);
    host.hello();
    assert!(matches!(
        host.ask(&open(TreeRef::Root, &a, "a", pod("ns", "p1", "u1"))),
        Reply::Opened { .. }
    ));
    assert_eq!(std::fs::read_to_string(a.join("g")).unwrap(), "g\n");
    assert_eq!(host.ask(&close(&a)), Reply::Closed);
    let (status, log) = host.eof();
    assert_eq!(status.code(), Some(0), "{log}");
}

/// (k) After a kill, a new host opens both views with the same labels: each
/// rebinds its client (so the fork's one writer is not taken) and stacks
/// over its stale mount; reconcile ends nothing; closed, nothing is left
/// mounted.
#[test]
#[ignore = "needs root and /dev/fuse: ci/fuse-tests.sh"]
fn a_restarted_host_rebinds_and_stacks() {
    let Some(v) = Volume::mounted() else { return };
    let (a, b) = (mkdir(&v, "a"), mkdir(&v, "b"));
    let mut host = v.host(&[]);
    host.hello();
    assert!(matches!(
        host.ask(&open(TreeRef::Root, &a, "a", pod("ns", "p1", "u1"))),
        Reply::Opened { .. }
    ));
    let fork = forked(&host.ask(&fork_create(TreeRef::Root, "k", Landing::Automatic)));
    assert!(matches!(
        host.ask(&open(
            TreeRef::Fork(key("k")),
            &b,
            "b",
            pod("ns", "p2", "u2")
        )),
        Reply::Opened { .. }
    ));
    std::fs::write(a.join("f"), "f\n").unwrap();
    std::fs::write(b.join("g"), "g\n").unwrap();
    v.kill(host);
    assert!(matches!(mount_state(&a).unwrap(), MountState::Stale { .. }));
    assert!(matches!(mount_state(&b).unwrap(), MountState::Stale { .. }));

    let mut host = v.host(&[]);
    host.hello();
    assert_eq!(
        host.ask(&open(TreeRef::Root, &a, "a", pod("ns", "p1", "u1"))),
        Reply::Opened {
            tree: TreeId::Root,
            stacked: true
        }
    );
    assert_eq!(
        host.ask(&open(
            TreeRef::Fork(key("k")),
            &b,
            "b",
            pod("ns", "p2", "u2")
        )),
        Reply::Opened {
            tree: fork,
            stacked: true
        }
    );
    assert_eq!(
        host.ask(&Request::Reconcile),
        Reply::Reconciled { ended: 0 }
    );
    assert_eq!(std::fs::read_to_string(a.join("f")).unwrap(), "f\n");
    assert_eq!(std::fs::read_to_string(b.join("g")).unwrap(), "g\n");
    std::fs::write(b.join("h"), "after the restart\n").unwrap();
    assert_eq!(host.ask(&close(&a)), Reply::Closed);
    assert_eq!(host.ask(&close(&b)), Reply::Closed);
    let (status, log) = host.eof();
    assert_eq!(status.code(), Some(0), "{log}");
    assert!(
        log.contains(&format!(
            "stacked over a stale mount at {} left by a crashed daemon",
            a.display()
        )),
        "{log}"
    );
    for at in [&a, &b] {
        assert_eq!(mount_state(at).unwrap(), MountState::NotMounted, "{at:?}");
        assert_eq!(agentfs_fuse::stale::agentfs_layers(at).unwrap(), 0);
    }
}

/// (m) A stop with views open (the parent gone: EOF) drains nothing: the
/// host exits 3, each mount left stale and its client attached, as at a
/// crash. The next host rebinds both clients, so the fork claim keeps its
/// one writer, and stacks over both mounts; what the views wrote is there.
#[test]
#[ignore = "needs root and /dev/fuse: ci/fuse-tests.sh"]
fn a_stop_with_views_open_leaves_them_for_the_next_host() {
    let Some(v) = Volume::mounted() else { return };
    let (a, b) = (mkdir(&v, "a"), mkdir(&v, "b"));
    let mut host = v.host(&[]);
    host.hello();
    assert!(matches!(
        host.ask(&open(TreeRef::Root, &a, "a", pod("ns", "p1", "u1"))),
        Reply::Opened { .. }
    ));
    let fork = forked(&host.ask(&fork_create(TreeRef::Root, "k", Landing::Automatic)));
    assert!(matches!(
        host.ask(&open(
            TreeRef::Fork(key("k")),
            &b,
            "b",
            pod("ns", "p2", "u2")
        )),
        Reply::Opened { .. }
    ));
    std::fs::write(a.join("f"), "f\n").unwrap();
    std::fs::write(b.join("g"), "g\n").unwrap();
    let (status, log) = host.eof();
    assert_eq!(status.code(), Some(3), "{log}");
    assert!(log.contains("stopped with 2 view(s) open"), "{log}");
    for at in [&a, &b] {
        assert!(
            matches!(mount_state(at).unwrap(), MountState::Stale { .. }),
            "{at:?}"
        );
    }

    let mut host = v.host(&[]);
    host.hello();
    assert_eq!(
        host.ask(&open(TreeRef::Root, &a, "a", pod("ns", "p1", "u1"))),
        Reply::Opened {
            tree: TreeId::Root,
            stacked: true
        }
    );
    assert_eq!(
        host.ask(&open(
            TreeRef::Fork(key("k")),
            &b,
            "b",
            pod("ns", "p2", "u2")
        )),
        Reply::Opened {
            tree: fork,
            stacked: true
        },
        "the fork claim's writer is rebound, not taken"
    );
    assert_eq!(
        host.ask(&Request::Reconcile),
        Reply::Reconciled { ended: 0 }
    );
    assert_eq!(std::fs::read_to_string(a.join("f")).unwrap(), "f\n");
    assert_eq!(std::fs::read_to_string(b.join("g")).unwrap(), "g\n");
    assert_eq!(host.ask(&close(&a)), Reply::Closed);
    assert_eq!(host.ask(&close(&b)), Reply::Closed);
    let (status, log) = host.eof();
    assert_eq!(status.code(), Some(0), "{log}");
    for at in [&a, &b] {
        assert_eq!(mount_state(at).unwrap(), MountState::NotMounted, "{at:?}");
        assert_eq!(agentfs_fuse::stale::agentfs_layers(at).unwrap(), 0);
    }
    assert!(v.messages("f").contains("Agentfs-Pod: p1"));
}

/// (n) A host killed with a view open leaves its client attached. A new
/// host stopped before any open or reconcile still releases: its drain ends
/// every client of its launcher, not only its views'.
#[test]
#[ignore = "needs root and /dev/fuse: ci/fuse-tests.sh"]
fn a_drain_ends_a_crashed_hosts_clients() {
    let Some(v) = Volume::mounted() else { return };
    let a = mkdir(&v, "a");
    let mut host = v.host(&[]);
    host.hello();
    assert!(matches!(
        host.ask(&open(TreeRef::Root, &a, "a", pod("ns", "p1", "u1"))),
        Reply::Opened { .. }
    ));
    v.kill(host);
    let mut host = v.host(&[]);
    host.hello();
    let (status, log) = host.eof();
    assert_eq!(status.code(), Some(0), "{log}");
    assert!(!log.contains("DrainBlocked"), "{log}");
    agentfs_fuse::stale::detach_stale_layers(&a, 4).unwrap();
}

/// (l) A fork has one writer for life: once its view closed cleanly, a view
/// with a new label is refused.
#[test]
#[ignore = "needs root and /dev/fuse: ci/fuse-tests.sh"]
fn a_fork_has_one_writer() {
    let Some(v) = Volume::mounted() else { return };
    let b = mkdir(&v, "b");
    let mut host = v.host(&[]);
    host.hello();
    forked(&host.ask(&fork_create(TreeRef::Root, "k", Landing::Automatic)));
    assert!(matches!(
        host.ask(&open(
            TreeRef::Fork(key("k")),
            &b,
            "w1",
            PodContext::default()
        )),
        Reply::Opened { .. }
    ));
    std::fs::write(b.join("x"), "x\n").unwrap();
    assert_eq!(host.ask(&close(&b)), Reply::Closed);
    let taken = host.ask(&open(
        TreeRef::Fork(key("k")),
        &b,
        "w2",
        PodContext::default(),
    ));
    assert_eq!(kind(&taken), Some(RefusalKind::ForkTaken), "{taken:?}");
    assert_eq!(mount_state(&b).unwrap(), MountState::NotMounted);
    let (status, log) = host.eof();
    assert_eq!(status.code(), Some(0), "{log}");
}

/// (o) A file held open keeps fork view a's close pending. Requests that
/// name nothing a holds are answered by id before it: an open elsewhere,
/// another view's close, a reconcile. An open at a's mount, the end of a's
/// fork, and a reconcile behind that open wait, each in arrival order, and
/// are answered once a has ended and its `closed` is written. So is an open
/// at d that names nothing a holds but shares a label with the waiting open
/// at a's mount: a resource's requests are answered in arrival order. Each
/// check is the next line read: the socket's read timeout only guards a
/// hang.
#[test]
#[ignore = "needs root and /dev/fuse: ci/fuse-tests.sh"]
fn a_held_close_waits_alone() {
    let Some(v) = Volume::mounted() else { return };
    let (a, b, c, d) = (
        mkdir(&v, "a"),
        mkdir(&v, "b"),
        mkdir(&v, "c"),
        mkdir(&v, "d"),
    );
    let mut host = v.host(&[]);
    host.reader
        .get_ref()
        .set_read_timeout(Some(std::time::Duration::from_secs(60)))
        .unwrap();
    host.hello();
    forked(&host.ask(&fork_create(TreeRef::Root, "k", Landing::Automatic)));
    assert!(matches!(
        host.ask(&open(
            TreeRef::Fork(key("k")),
            &a,
            "a",
            PodContext::default()
        )),
        Reply::Opened { .. }
    ));
    assert!(matches!(
        host.ask(&open(TreeRef::Root, &b, "b", PodContext::default())),
        Reply::Opened { .. }
    ));
    std::fs::write(a.join("f"), "f\n").unwrap();
    let held = std::fs::File::open(a.join("f")).unwrap();
    let close_a = host.send(&close(&a));
    assert!(matches!(
        host.ask(&open(TreeRef::Root, &c, "c", PodContext::default())),
        Reply::Opened { .. }
    ));
    assert_eq!(host.ask(&close(&b)), Reply::Closed);
    assert!(matches!(
        host.ask(&Request::Reconcile),
        Reply::Reconciled { .. }
    ));
    // Each of these names something a holds, or waits behind one that does.
    let reopen = host.send(&open(TreeRef::Root, &a, "a2", PodContext::default()));
    // Behind the open at a's mount only: they share the label a2.
    let chained = host.send(&open(TreeRef::Root, &d, "a2", PodContext::default()));
    let end = host.send(&Request::ForkEnd { key: key("k") });
    let reconcile = host.send(&Request::Reconcile);
    assert_eq!(
        host.ask(&close(&v.p("nowhere"))),
        Reply::Closed,
        "answered before every waiting request"
    );
    drop(held);
    assert_eq!(host.reply_to(close_a), Reply::Closed, "a's close first");
    assert_eq!(
        host.reply_to(reopen),
        Reply::Opened {
            tree: TreeId::Root,
            stacked: false
        }
    );
    let at_d = host.reply_to(chained);
    assert_eq!(host.reply_to(end), Reply::Ended);
    assert!(matches!(host.reply_to(reconcile), Reply::Reconciled { .. }));
    assert_eq!(host.ask(&close(&a)), Reply::Closed);
    assert_eq!(host.ask(&close(&c)), Reply::Closed);
    if matches!(at_d, Reply::Opened { .. }) {
        assert_eq!(host.ask(&close(&d)), Reply::Closed);
    }
    let (status, log) = host.eof();
    assert_eq!(status.code(), Some(0), "{log}");
}

/// (p) An id already in flight is a malformed line: the host stops with
/// exit 2 (and, with a view open, leaves it stale).
#[test]
#[ignore = "needs root and /dev/fuse: ci/fuse-tests.sh"]
fn a_duplicate_id_in_flight_is_malformed() {
    let Some(v) = Volume::mounted() else { return };
    let a = mkdir(&v, "a");
    let mut host = v.host(&[]);
    // Only guards a hang: a host that does not stop answers nothing here.
    host.reader
        .get_ref()
        .set_read_timeout(Some(std::time::Duration::from_secs(60)))
        .unwrap();
    host.hello();
    assert!(matches!(
        host.ask(&open(TreeRef::Root, &a, "a", PodContext::default())),
        Reply::Opened { .. }
    ));
    std::fs::write(a.join("f"), "f\n").unwrap();
    let held = std::fs::File::open(a.join("f")).unwrap();
    host.send_as(5, &close(&a));
    host.send_as(5, &Request::Reconcile);
    assert_eq!(host.line(), None, "no reply: the host stopped");
    let status = host.child.wait().unwrap();
    drop(held);
    let log = host.log.take().unwrap().join().unwrap();
    assert_eq!(status.code(), Some(2), "{log}");
    assert!(log.contains("id 5 is already in flight"), "{log}");
    agentfs_fuse::stale::detach_stale_layers(&a, 4).unwrap();
}

/// (p) An id a waiting request holds is in flight too: the host stops with
/// exit 2, so no two replies carry one id.
#[test]
#[ignore = "needs root and /dev/fuse: ci/fuse-tests.sh"]
fn a_duplicate_of_a_waiting_id_is_malformed() {
    let Some(v) = Volume::mounted() else { return };
    let a = mkdir(&v, "a");
    let mut host = v.host(&[]);
    // Only guards a hang: a host that does not stop answers nothing here.
    host.reader
        .get_ref()
        .set_read_timeout(Some(std::time::Duration::from_secs(60)))
        .unwrap();
    host.hello();
    assert!(matches!(
        host.ask(&open(TreeRef::Root, &a, "a", PodContext::default())),
        Reply::Opened { .. }
    ));
    std::fs::write(a.join("f"), "f\n").unwrap();
    let held = std::fs::File::open(a.join("f")).unwrap();
    host.send_as(5, &close(&a));
    // Waits behind a's close: it names a's mount.
    host.send_as(6, &open(TreeRef::Root, &a, "a2", PodContext::default()));
    host.send_as(6, &Request::Reconcile);
    assert_eq!(host.line(), None, "no reply: the host stopped");
    let status = host.child.wait().unwrap();
    drop(held);
    let log = host.log.take().unwrap().join().unwrap();
    assert_eq!(status.code(), Some(2), "{log}");
    assert!(log.contains("id 6 is already in flight"), "{log}");
    agentfs_fuse::stale::detach_stale_layers(&a, 4).unwrap();
}

/// (q) An id may be used again once its reply is written; a missing id is
/// a malformed line (exit 2).
#[test]
fn ids_are_answered_and_a_line_without_one_is_malformed() {
    let v = Volume::new();
    let mut host = v.host(&[]);
    host.hello();
    for _ in 0..2 {
        host.send_as(9, &Request::Reconcile);
        assert_eq!(host.reply_to(9), Reply::Reconciled { ended: 0 });
    }
    let control = host.control.as_mut().unwrap();
    writeln!(control, r#"{{"op":"reconcile"}}"#).unwrap();
    assert_eq!(host.line(), None);
    let (status, log) = host.eof();
    assert_eq!(status.code(), Some(2), "{log}");
}
