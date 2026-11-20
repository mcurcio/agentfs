//! `agentfs host` driven as its launcher drives it (docs/library/filesystem.md,
//! "The view host command"): the real binary, a Unix socket pair as its
//! standard input, one JSON line per request and reply. Without root: the
//! control channel's check, the hello, keyed forks and their refusals,
//! reconcile, the drain at EOF, a busy volume, the name guard and the
//! not-built log. With root and /dev/fuse (ignored by default; run by
//! ci/fuse-tests.sh with sudo): views opened with a pod context, closed,
//! landed from a fork, rebound and stacked after a kill, and a fork's one
//! writer.

use std::io::{BufRead, BufReader, Read as _, Write as _};
use std::os::fd::OwnedFd;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::thread::JoinHandle;

use agentfs_contracts::host::{
    HOST_PROTOCOL, Hello, Refusal, RefusalKind, Reply, Request, TreeRef, ViewRequest,
};
use agentfs_contracts::name::{ClientLabel, ForkKey, Label};
use agentfs_contracts::pod::PodContext;
use agentfs_contracts::tree::TreeId;
use agentfs_contracts::trees::{ForkSettings, Landing};
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
        }
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
}

impl Host {
    /// The next line, or `None` at EOF.
    fn line(&mut self) -> Option<String> {
        let mut line = String::new();
        let n = self.reader.read_line(&mut line).unwrap();
        (n > 0).then_some(line)
    }

    fn hello(&mut self) -> Hello {
        let line = self.line().expect("the host wrote no hello");
        serde_json::from_str(&line).unwrap()
    }

    fn ask(&mut self, request: &Request) -> Reply {
        self.send(request);
        self.reply()
    }

    fn send(&mut self, request: &Request) {
        let control = self.control.as_mut().expect("the channel is open");
        writeln!(control, "{}", serde_json::to_string(request).unwrap()).unwrap();
    }

    fn reply(&mut self) -> Reply {
        let line = self.line().expect("the host ended before it replied");
        serde_json::from_str(&line).unwrap()
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

    /// SIGKILL: the host's views are left stale.
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
    host.send(&close(&a));
    drop(held);
    assert_eq!(host.reply(), Reply::Closed);
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
/// lands in the root, with that pod's trailers. The drain at EOF releases
/// only with every queue empty, so the exit is the landing's event.
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
    let (status, log) = host.eof();
    assert_eq!(status.code(), Some(0), "{log}");
}

/// (k) After a kill, a new host opens both views with the same labels: each
/// rebinds its client (so the fork's one writer is not taken) and stacks
/// over its stale mount; reconcile ends nothing; EOF leaves nothing mounted.
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
    host.kill();
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
