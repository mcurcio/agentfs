//! Real FUSE mounts (design §18 tests 1, 2, 4, 5, 6, 7, 11 and 15, plus SQLite
//! in and out of the fsync class (design §8),
//! git on the mount, restores through the mount, the daemon's lifecycle, and
//! `probe`, `--on-stale-mount hold` and `recover` after a crash (design §17.1), two
//! front mounts of one backing tree feeding one history service (design §14), and
//! writes through a shared mapping after the file's last close (design §3.1, bead
//! agentfs-dyu).
//!
//! Every test needs `/dev/fuse` and root, so all are `#[ignore]`d; the
//! `fuse` CI job runs them through `ci/fuse-tests.sh`:
//!
//! ```sh
//! sudo AGENTFS_TEST_PASSTHROUGH=auto target/debug/deps/mount-<hash> --ignored --test-threads=1
//! ```
//!
//! - `AGENTFS_TEST_PASSTHROUGH` (`auto`, `on`, `off`) is passed to every
//!   mount.
//! - `AGENTFS_TEST_UNPRIV_USER` names a user to run an unprivileged daemon
//!   as (needs `user_allow_other` in `/etc/fuse.conf`); without it that test
//!   is skipped.
//! - Daemon logs go to `$RUNNER_TEMP/agentfs-mount-logs` when set.
//!
//! Each test mounts its own volume; history is read with git2 straight from
//! the state area while the daemon holds it (reading needs no lock).

use std::collections::BTreeMap;
use std::fs;
use std::io::Write as _;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Output, Stdio};
use std::time::{Duration, Instant};

const UID: u32 = 10000;

#[derive(Debug, Clone, PartialEq, Eq)]
enum Item {
    File(String),
    Exec(String),
    Link(String),
}

use Item::{Exec, File, Link};

fn f(s: &str) -> Item {
    File(s.into())
}

type Tree = BTreeMap<String, Item>;

fn tree(items: &[(&str, Item)]) -> Tree {
    items
        .iter()
        .map(|(p, i)| ((*p).to_owned(), i.clone()))
        .collect()
}

fn require_root() {
    assert!(
        rustix::process::geteuid().is_root(),
        "the mount tests run as root (ci/fuse-tests.sh)"
    );
}

fn passthrough() -> String {
    std::env::var("AGENTFS_TEST_PASSTHROUGH").unwrap_or_else(|_| "auto".into())
}

fn log_dir() -> PathBuf {
    let dir = match std::env::var_os("RUNNER_TEMP") {
        Some(t) => PathBuf::from(t).join("agentfs-mount-logs"),
        None => std::env::temp_dir().join("agentfs-mount-logs"),
    };
    fs::create_dir_all(&dir).unwrap();
    dir
}

/// A volume: `backing/`, `state/` and `mnt/` side by side.
struct Vol {
    dir: tempfile::TempDir,
    name: &'static str,
}

impl Vol {
    fn new(name: &'static str) -> Self {
        require_root();
        let dir = tempfile::tempdir().unwrap();
        // Other users (uid 10000) must reach the mount point.
        fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o755)).unwrap();
        let v = Self { dir, name };
        for d in ["backing", "mnt"] {
            fs::create_dir_all(v.p(d)).unwrap();
            fs::set_permissions(v.p(d), fs::Permissions::from_mode(0o755)).unwrap();
        }
        let out = v.agentfs(&["init", "--volume", "demo"]);
        assert!(out.status.success(), "{}", text(&out.stderr));
        v.config("agentfs.commitWindow", "50ms");
        v
    }

    fn p(&self, rel: &str) -> PathBuf {
        self.dir.path().join(rel)
    }

    fn backing(&self) -> PathBuf {
        self.p("backing")
    }

    fn mnt(&self) -> PathBuf {
        self.p("mnt")
    }

    fn state(&self) -> PathBuf {
        self.p("state")
    }

    fn config(&self, key: &str, value: &str) {
        let mut c = git2::Config::open(&self.state().join("git/config")).unwrap();
        c.set_str(key, value).unwrap();
    }

    fn command(&self) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_agentfs"));
        for (name, _) in std::env::vars_os() {
            let n = name.to_string_lossy();
            if n.starts_with("OTEL_") || n.starts_with("AGENTFS_") {
                cmd.env_remove(name);
            }
        }
        cmd.arg("--state")
            .arg(self.state())
            .env("AGENTFS_REPLICA", "test-0");
        cmd
    }

    fn agentfs(&self, args: &[&str]) -> Output {
        self.command().args(args).output().unwrap()
    }

    fn log_path(&self) -> PathBuf {
        log_dir().join(format!("{}.log", self.name))
    }

    /// Starts `agentfs mount` and waits until the kernel lists the mount.
    fn mount(&self, extra: &[&str]) -> Mounted {
        self.mount_with(extra, |c| c)
    }

    /// `agentfs mount` with `extra` options, logging to the test's log.
    fn mount_command(&self, extra: &[&str]) -> Command {
        let mut log = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.log_path())
            .unwrap();
        writeln!(log, "---- mount {extra:?}").unwrap();
        let mut cmd = self.command();
        cmd.args([
            "mount",
            "--passthrough",
            &passthrough(),
            "--unmount-grace",
            "3s",
        ])
        .args(extra)
        .arg(self.backing())
        .arg(self.mnt())
        .stdout(log.try_clone().unwrap())
        .stderr(log)
        .stdin(Stdio::null());
        cmd
    }

    fn mount_with(&self, extra: &[&str], wrap: impl FnOnce(Command) -> Command) -> Mounted {
        let mut cmd = wrap(self.mount_command(extra));
        // The log is appended to across mounts: wait for this one's line.
        let before = startup_lines(&fs::read_to_string(self.log_path()).unwrap_or_default());
        let child = cmd.spawn().unwrap();
        let mounted = Mounted {
            child: Some(child),
            mnt: self.mnt(),
            log: self.log_path(),
        };
        mounted.wait_mounted(before);
        mounted
    }

    /// The branch's tree: path to content, mode.
    fn history(&self) -> Tree {
        let mut out = Tree::new();
        let Ok(repo) = git2::Repository::open_bare(self.state().join("git")) else {
            return out;
        };
        let Ok(reference) = repo.find_reference("refs/heads/agentfs/demo") else {
            return out;
        };
        let commit = reference.peel_to_commit().unwrap();
        commit
            .tree()
            .unwrap()
            .walk(git2::TreeWalkMode::PreOrder, |root, e| {
                if e.kind() == Some(git2::ObjectType::Blob) {
                    let blob = repo.find_blob(e.id()).unwrap();
                    let content = String::from_utf8_lossy(blob.content()).into_owned();
                    let item = match e.filemode() {
                        0o100755 => Exec(content),
                        0o120000 => Link(content),
                        _ => File(content),
                    };
                    out.insert(format!("{root}{}", e.name().unwrap()), item);
                }
                git2::TreeWalkResult::Ok
            })
            .unwrap();
        out
    }

    /// The head commit's author name and message.
    fn head(&self) -> (String, String) {
        let repo = git2::Repository::open_bare(self.state().join("git")).unwrap();
        let commit = repo
            .find_reference("refs/heads/agentfs/demo")
            .unwrap()
            .peel_to_commit()
            .unwrap();
        (
            commit.author().name().unwrap_or("").to_owned(),
            commit.message().unwrap_or("").to_owned(),
        )
    }

    /// The branch head commit id.
    fn head_id(&self) -> String {
        let repo = git2::Repository::open_bare(self.state().join("git")).unwrap();
        repo.refname_to_id("refs/heads/agentfs/demo")
            .unwrap()
            .to_string()
    }

    /// The id of the head commit's first parent.
    fn parent_id(&self) -> String {
        let repo = git2::Repository::open_bare(self.state().join("git")).unwrap();
        let head = repo
            .find_reference("refs/heads/agentfs/demo")
            .unwrap()
            .peel_to_commit()
            .unwrap();
        head.parent_id(0).unwrap().to_string()
    }

    /// Runs `agentfs` and expects success; its stdout.
    fn ok(&self, args: &[&str]) -> String {
        let out = self.agentfs(args);
        assert!(
            out.status.success(),
            "{args:?}: {}{}\nlog:\n{}",
            text(&out.stdout),
            text(&out.stderr),
            fs::read_to_string(self.log_path()).unwrap_or_default()
        );
        text(&out.stdout)
    }

    /// Waits until history is exactly `want`.
    fn expect(&self, want: &Tree) {
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            let got = self.history();
            if &got == want {
                return;
            }
            if Instant::now() > deadline {
                panic!(
                    "history never became {want:#?}\ngot {got:#?}\ndaemon log:\n{}",
                    fs::read_to_string(self.log_path()).unwrap_or_default()
                );
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    /// Runs a shell command in the mount as root; it must succeed.
    fn sh(&self, script: &str) -> String {
        self.sh_as(None, script, true)
    }

    /// Runs a shell command in the mount as `uid` (and gid) if given.
    fn sh_as(&self, uid: Option<u32>, script: &str, must_succeed: bool) -> String {
        let mut cmd = match uid {
            None => Command::new("sh"),
            Some(uid) => {
                let mut c = Command::new("setpriv");
                c.args([
                    format!("--reuid={uid}"),
                    format!("--regid={uid}"),
                    "--clear-groups".into(),
                    "--".into(),
                    "sh".into(),
                ]);
                c
            }
        };
        let out = cmd
            .arg("-c")
            .arg(script)
            .current_dir(self.mnt())
            .output()
            .unwrap();
        if must_succeed {
            assert!(
                out.status.success(),
                "`{script}` failed: {}{}",
                text(&out.stdout),
                text(&out.stderr)
            );
        }
        text(&out.stdout)
    }
}

fn text(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

/// How many startup lines (`mounted ...`) a daemon log holds.
fn startup_lines(log: &str) -> usize {
    log.lines().filter(|l| l.starts_with("mounted ")).count()
}

fn is_mounted(mnt: &Path) -> bool {
    let info = fs::read_to_string("/proc/self/mountinfo").unwrap();
    let mnt = mnt.to_str().unwrap();
    info.lines()
        .any(|l| l.split(' ').nth(4).is_some_and(|m| m == mnt))
}

/// A running `agentfs mount`.
struct Mounted {
    child: Option<Child>,
    mnt: PathBuf,
    log: PathBuf,
}

impl Mounted {
    fn log(&self) -> String {
        fs::read_to_string(&self.log).unwrap_or_default()
    }

    /// Waits until the kernel lists the mount and the daemon has printed
    /// a startup line beyond the `before` already in the log.
    fn wait_mounted(&self, before: usize) {
        let deadline = Instant::now() + Duration::from_secs(20);
        while !(is_mounted(&self.mnt) && startup_lines(&self.log()) > before) {
            if Instant::now() > deadline {
                panic!("never mounted; log:\n{}", self.log());
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    fn pid(&self) -> rustix::process::Pid {
        rustix::process::Pid::from_child(self.child.as_ref().unwrap())
    }

    fn signal(&self, sig: rustix::process::Signal) {
        rustix::process::kill_process(self.pid(), sig).unwrap();
    }

    fn wait_exit(&mut self, timeout: Duration) -> ExitStatus {
        let deadline = Instant::now() + timeout;
        let child = self.child.as_mut().unwrap();
        loop {
            if let Some(status) = child.try_wait().unwrap() {
                self.child = None;
                return status;
            }
            if Instant::now() > deadline {
                panic!("the daemon did not exit; log:\n{}", self.log());
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// SIGTERM; the daemon must exit 0 and leave nothing mounted.
    fn stop(mut self) -> String {
        self.signal(rustix::process::Signal::TERM);
        let status = self.wait_exit(Duration::from_secs(30));
        let log = self.log();
        assert!(status.success(), "exit {status:?}; log:\n{log}");
        assert!(!is_mounted(&self.mnt), "still mounted; log:\n{log}");
        // No open fell back to the daemon: with passthrough on, every
        // regular file's I/O really went to its backing file.
        assert!(
            !log.contains("passthrough refused"),
            "passthrough fell back; log:\n{log}"
        );
        log
    }
}

impl Drop for Mounted {
    fn drop(&mut self) {
        if let Some(child) = &mut self.child {
            let _ = child.kill();
            let _ = child.wait();
        }
        if is_mounted(&self.mnt) {
            let _ = Command::new("umount").arg("-l").arg(&self.mnt).status();
        }
    }
}

// ---- design §18 test 1: no gaps ----

#[test]
#[ignore = "needs /dev/fuse and root: ci/fuse-tests.sh"]
fn no_gaps() {
    let v = Vol::new("no_gaps");
    let m = v.mount(&[]);
    let mut want = Tree::new();

    // An editor's save.
    v.sh("printf old > f && printf new > .f.swp && mv .f.swp f");
    want.insert("f".into(), f("new"));
    v.expect(&want);

    v.sh("printf more >> f");
    want.insert("f".into(), f("newmore"));
    v.expect(&want);

    v.sh("truncate -s 3 f");
    want.insert("f".into(), f("new"));
    v.expect(&want);

    v.sh("mkdir -p dir/sub && printf x > dir/sub/x && printf y > dir/y && mv dir dir2");
    want.insert("dir2/sub/x".into(), f("x"));
    want.insert("dir2/y".into(), f("y"));
    v.expect(&want);

    v.sh("rm -r dir2");
    want.retain(|k, _| !k.starts_with("dir2/"));
    v.expect(&want);

    v.sh("ln f hard");
    want.insert("hard".into(), f("new"));
    v.expect(&want);

    v.sh("chmod +x f");
    want.insert("f".into(), Exec("new".into()));
    want.insert("hard".into(), Exec("new".into()));
    v.expect(&want);

    v.sh("ln -s f sym");
    want.insert("sym".into(), Link("f".into()));
    v.expect(&want);

    // Unlink of an open file.
    v.sh("printf g > g");
    want.insert("g".into(), f("g"));
    v.expect(&want);
    v.sh("exec 3>>g; rm g; echo x >&3; exec 3>&-");
    want.remove("g");
    v.expect(&want);

    // Rename over an open file.
    v.sh("printf old > h && printf fresh > h2");
    v.sh("exec 3>>h; mv h2 h; echo late >&3; exec 3>&-");
    want.insert("h".into(), f("fresh"));
    v.expect(&want);

    // Log rotation while the writer is open.
    v.sh(
        "i=0; while [ $i -lt 300 ]; do echo line$i; i=$((i+1)); sleep 0.002; done >> app.log & \
         sleep 0.3; mv app.log app.log.1; printf 'fresh\\n' > app.log; wait",
    );
    let rotated = fs::read_to_string(v.backing().join("app.log.1")).unwrap();
    assert_eq!(rotated.lines().count(), 300, "{rotated}");
    want.insert("app.log".into(), f("fresh\n"));
    want.insert("app.log.1".into(), File(rotated));
    v.expect(&want);

    // A writer other than root.
    v.sh("mkdir u && chown 10000:10000 u");
    v.sh_as(Some(UID), "printf mine > u/by-10000", true);
    want.insert("u/by-10000".into(), f("mine"));
    v.expect(&want);
    let (author, message) = v.head();
    assert_eq!(author, "uid-10000", "{message}");

    // The backing tree has the same content.
    assert_eq!(fs::read_to_string(v.backing().join("f")).unwrap(), "new");
    m.stop();
}

// ---- design §18 test 2: before return ----

#[test]
#[ignore = "needs /dev/fuse and root: ci/fuse-tests.sh"]
fn recorded_before_close_returns() {
    let v = Vol::new("before_return");
    // Nothing is committed before the kill, so only the journal can carry
    // the record over.
    v.config("agentfs.commitWindow", "10m");
    let mut m = v.mount(&[]);
    v.sh("printf data > f");
    // close has returned: kill the daemon at once.
    m.signal(rustix::process::Signal::KILL);
    m.wait_exit(Duration::from_secs(10));

    // A stale mount point is refused with the way out.
    let err = rustix::fs::stat(v.mnt()).unwrap_err();
    assert_eq!(err, rustix::io::Errno::NOTCONN);
    let out = v.agentfs(&[
        "mount",
        v.backing().to_str().unwrap(),
        v.mnt().to_str().unwrap(),
    ]);
    assert_eq!(out.status.code(), Some(1));
    assert!(
        text(&out.stderr).contains("stale FUSE mount"),
        "{}",
        text(&out.stderr)
    );
    // The kernel released the lock with the process.
    let status = text(&v.agentfs(&["status"]).stdout);
    assert!(status.contains("lock:      free"), "{status}");
    assert!(
        Command::new("umount")
            .arg("-l")
            .arg(v.mnt())
            .status()
            .unwrap()
            .success()
    );

    assert!(v.history().is_empty(), "committed before the kill");

    // Remount: recovery finds the record made before close returned in
    // the journal, and commits it.
    v.config("agentfs.commitWindow", "50ms");
    let m = v.mount(&[]);
    let log = m.log();
    let line = log.lines().rfind(|l| l.starts_with("mounted ")).unwrap();
    // The shell's redirection closes the file twice (dup2, then close), so
    // there may be a record of the empty file before the one with data.
    let recovered: usize = line
        .split(", ")
        .find_map(|p| p.strip_suffix(" uncommitted record(s) recovered"))
        .and_then(|n| n.parse().ok())
        .unwrap_or_else(|| panic!("{line}"));
    assert!(recovered >= 1, "{line}");
    v.expect(&tree(&[("f", f("data"))]));
    assert_eq!(fs::read_to_string(v.backing().join("f")).unwrap(), "data");
    m.stop();
}

// ---- design §18 test 4: any user ----

#[test]
#[ignore = "needs /dev/fuse and root: ci/fuse-tests.sh"]
fn any_user() {
    let v = Vol::new("any_user");
    let m = v.mount(&[]);
    v.sh("mkdir u && chown 10000:10000 u");
    v.sh_as(Some(UID), "printf a > u/f", true);
    let meta = fs::metadata(v.backing().join("u/f")).unwrap();
    assert_eq!((meta.uid(), meta.gid()), (UID, UID), "owned by its creator");
    v.expect(&tree(&[("u/f", f("a"))]));
    assert_eq!(v.head().0, "uid-10000");

    // Ordinary permission semantics.
    v.sh("printf r > rootfile && chmod 644 rootfile");
    v.sh_as(Some(UID), "printf x >> rootfile", false);
    assert_eq!(
        fs::read_to_string(v.backing().join("rootfile")).unwrap(),
        "r"
    );
    let denied = Command::new("setpriv")
        .args(["--reuid=10000", "--regid=10000", "--clear-groups", "--"])
        .args(["sh", "-c", "printf x >> rootfile"])
        .current_dir(v.mnt())
        .output()
        .unwrap();
    assert!(!denied.status.success());
    assert!(
        text(&denied.stderr).contains("Permission denied"),
        "{}",
        text(&denied.stderr)
    );

    // Root reads a 0600 file of uid 10000; chmod 600 persists.
    v.sh_as(Some(UID), "printf s > u/secret && chmod 600 u/secret", true);
    assert_eq!(v.sh("cat u/secret"), "s");
    let mode = |p: &Path| fs::metadata(p).unwrap().permissions().mode() & 0o7777;
    assert_eq!(mode(&v.backing().join("u/secret")), 0o600);
    assert_eq!(mode(&v.mnt().join("u/secret")), 0o600);

    // chown and utimens pass through.
    v.sh("chown 10000 rootfile && TZ=UTC touch -d '2001-02-03 04:05:06' rootfile");
    let meta = fs::metadata(v.backing().join("rootfile")).unwrap();
    assert_eq!(meta.uid(), UID);
    assert_eq!(meta.mtime(), 981_173_106);

    // A setgid directory's group is inherited.
    v.sh("mkdir sg && chown 10000:4242 sg && chmod 2775 sg");
    v.sh_as(Some(UID), "printf z > sg/f && mkdir sg/d", true);
    assert_eq!(fs::metadata(v.backing().join("sg/f")).unwrap().gid(), 4242);
    let d = fs::metadata(v.backing().join("sg/d")).unwrap();
    assert_eq!(d.gid(), 4242);
    assert_eq!(d.permissions().mode() & 0o2000, 0o2000, "setgid kept");
    m.stop();

    // chmod 600 survives a remount.
    let m = v.mount(&[]);
    assert_eq!(mode(&v.mnt().join("u/secret")), 0o600);
    m.stop();
}

// A daemon running as root creates each object as its caller (design §6):
// it is the caller's from the instant it exists, never root's first, and
// a directory the caller reaches through a supplementary group works.
#[test]
#[ignore = "needs /dev/fuse and root: ci/fuse-tests.sh"]
fn new_objects_are_the_callers_from_the_start() {
    let v = Vol::new("callers_from_the_start");
    let m = v.mount(&[]);
    v.sh("mkdir u && chown 10000:10000 u && mkdir g && chown 0:4242 g && chmod 770 g");
    let out = Command::new("setpriv")
        .args(["--reuid=10000", "--regid=10000", "--groups=4242", "--"])
        .args([
            "sh",
            "-c",
            "umask 0 && printf a > g/f && mkdir g/d && ln -s f g/l && mkfifo g/p",
        ])
        .current_dir(v.mnt())
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", text(&out.stderr));
    for (name, mode) in [
        ("f", Some(0o666)),
        ("d", Some(0o777)),
        ("l", None),
        ("p", Some(0o666)),
    ] {
        let meta = fs::symlink_metadata(v.backing().join("g").join(name)).unwrap();
        assert_eq!((meta.uid(), meta.gid()), (UID, UID), "g/{name}");
        if let Some(mode) = mode {
            assert_eq!(meta.mode() & 0o7777, mode, "g/{name}");
        }
    }
    v.expect(&tree(&[("g/f", f("a")), ("g/l", Link("f".into()))]));

    // Watch the backing tree while the caller creates and removes one
    // name through the mount: the file is never root's.
    let mut creator = Command::new("setpriv")
        .args(["--reuid=10000", "--regid=10000", "--clear-groups", "--"])
        .args([
            "sh",
            "-c",
            "for i in $(seq 1 1500); do printf x > race; rm race; done",
        ])
        .current_dir(v.mnt().join("u"))
        .spawn()
        .unwrap();
    let race = v.backing().join("u/race");
    let (mut seen, mut roots) = (0u64, 0u64);
    while creator.try_wait().unwrap().is_none() {
        if let Ok(meta) = fs::symlink_metadata(&race) {
            seen += 1;
            if meta.uid() != UID {
                roots += 1;
            }
        }
    }
    assert!(creator.wait().unwrap().success());
    assert!(seen > 0, "never saw the file");
    assert_eq!(
        roots, 0,
        "the file was someone else's {roots} of {seen} times"
    );
    m.stop();
}

// ---- design §18 test 5: invisible ----

fn walk(root: &Path) -> Vec<String> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for e in fs::read_dir(&dir).unwrap() {
            let e = e.unwrap();
            let rel = e
                .path()
                .strip_prefix(root)
                .unwrap()
                .to_string_lossy()
                .into_owned();
            if e.file_type().unwrap().is_dir() {
                stack.push(e.path());
            }
            out.push(rel);
        }
    }
    out.sort();
    out
}

#[test]
#[ignore = "needs /dev/fuse and root: ci/fuse-tests.sh"]
fn nothing_of_the_state_area_is_visible() {
    let v = Vol::new("invisible");
    let creds = v.p("creds");
    fs::write(&creds, "username=bot\npassword=CRED-SECRET-4711\n").unwrap();
    v.config(
        "remote.origin.url",
        &format!("file://{}", v.p("remote.git").display()),
    );
    v.config("remote.origin.agentfsCredentials", creds.to_str().unwrap());
    git2::Repository::init_bare(v.p("remote.git")).unwrap();
    let m = v.mount(&[]);
    v.sh("mkdir -p a/b && printf one > a/b/one && printf two > two && ln -s two l");
    v.expect(&tree(&[
        ("a/b/one", f("one")),
        ("l", Link("two".into())),
        ("two", f("two")),
    ]));
    assert_eq!(walk(&v.mnt()), walk(&v.backing()));
    let grep = Command::new("grep")
        .args(["-r", "-l", "CRED-SECRET"])
        .arg(v.mnt())
        .output()
        .unwrap();
    assert!(grep.stdout.is_empty(), "{}", text(&grep.stdout));
    for name in ["git", "journal", "pending", "meta", "HEAD", "objects"] {
        assert!(!v.mnt().join(name).exists(), "{name} visible");
    }
    // `..` from the mount root is the mount point's parent.
    let up = fs::metadata(v.mnt().join("..")).unwrap();
    let parent = fs::metadata(v.dir.path()).unwrap();
    assert_eq!((up.dev(), up.ino()), (parent.dev(), parent.ino()));
    // Nothing reachable through the mount is the state area.
    let state = fs::metadata(v.state()).unwrap();
    for rel in walk(&v.mnt()) {
        let meta = fs::symlink_metadata(v.mnt().join(&rel)).unwrap();
        assert_ne!(
            (meta.dev(), meta.ino()),
            (state.dev(), state.ino()),
            "{rel}"
        );
    }
    m.stop();
}

// ---- design §18 test 6: rules ----

#[test]
#[ignore = "needs /dev/fuse and root: ci/fuse-tests.sh"]
fn rules_through_the_mount() {
    let v = Vol::new("rules");
    let exclude = v.state().join("git/info/exclude");
    fs::create_dir_all(exclude.parent().unwrap()).unwrap();
    fs::write(&exclude, "*.log\n").unwrap();
    let m = v.mount(&[]);
    v.sh("printf a > a.log && printf t > t.txt");
    v.expect(&tree(&[("t.txt", f("t"))]));
    m.stop();

    // Ignored after it was recorded: still recorded.
    fs::write(&exclude, "*.log\n*.txt\n").unwrap();
    let m = v.mount(&[]);
    v.sh("printf t2 > t.txt && printf n > new.txt && printf x > x.dat");
    v.expect(&tree(&[("t.txt", f("t2")), ("x.dat", f("x"))]));

    // Tracked rule files cannot turn recording off by default, and are
    // content themselves.
    v.sh("printf '*\\n' > .gitignore && printf '* -agentfs\\n' > .gitattributes");
    v.sh("printf z > z.dat");
    v.expect(&tree(&[
        (".gitattributes", f("* -agentfs\n")),
        (".gitignore", f("*\n")),
        ("t.txt", f("t2")),
        ("x.dat", f("x")),
        ("z.dat", f("z")),
    ]));
    m.stop();
}

// ---- design §18 test 7: nested repositories ----

fn source_repo(v: &Vol) -> PathBuf {
    let src = v.p("src");
    let git = |args: &[&str]| {
        let ok = Command::new("git")
            .args(["-c", "user.name=t", "-c", "user.email=t@example.com"])
            .args(["-c", "init.defaultBranch=main"])
            .args(args)
            .current_dir(v.dir.path())
            .status()
            .unwrap()
            .success();
        assert!(ok, "git {args:?}");
    };
    git(&["init", "-q", "src"]);
    fs::write(src.join("README"), "hello").unwrap();
    fs::create_dir_all(src.join("lib")).unwrap();
    fs::write(src.join("lib/code.rs"), "fn main() {}").unwrap();
    git(&["-C", "src", "add", "."]);
    git(&["-C", "src", "commit", "-q", "-m", "init"]);
    src
}

#[test]
#[ignore = "needs /dev/fuse and root: ci/fuse-tests.sh"]
fn nested_repositories() {
    let v = Vol::new("nested");
    let src = source_repo(&v);
    let m = v.mount(&[]);
    v.sh("printf k > keep");
    v.expect(&tree(&[("keep", f("k"))]));
    let src = src.to_str().unwrap();
    v.sh(&format!("git clone -q {src} work"));
    v.sh(&format!("git clone -q --bare {src} b.git"));
    v.sh("git init -q .");
    v.sh("printf n > new");
    v.expect(&tree(&[("keep", f("k")), ("new", f("n"))]));
    // A clone that stops being one (its `.git` removed): its paths are
    // new paths like any other, recorded at their next change.
    v.sh("rm -rf work/.git && printf h2 > work/README");
    v.expect(&tree(&[
        ("keep", f("k")),
        ("new", f("n")),
        ("work/README", f("h2")),
    ]));
    let log = m.stop();
    assert!(!log.contains("alarm:"), "{log}");

    v.config("agentfs.nested", "working-tree");
    let m = v.mount(&[]);
    v.sh(&format!("git clone -q {src} wt"));
    v.expect(&tree(&[
        ("keep", f("k")),
        ("new", f("n")),
        ("work/README", f("h2")),
        ("wt/README", f("hello")),
        ("wt/lib/code.rs", f("fn main() {}")),
    ]));
    let log = m.stop();
    assert!(!log.contains("alarm:"), "{log}");
}

/// Paths ever deleted on the branch's history (design §5: nothing is
/// deleted from history by nesting).
fn deleted_paths(v: &Vol) -> String {
    let out = Command::new("git")
        .arg("--git-dir")
        .arg(v.state().join("git"))
        .args([
            "log",
            "--diff-filter=D",
            "--name-only",
            "--format=",
            "refs/heads/agentfs/demo",
        ])
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", text(&out.stderr));
    text(&out.stdout)
}

/// Design §5 and §18 test 7: tracked paths stay tracked when a directory
/// around them becomes a nested repository (`git init`, `git init
/// --bare`, a `git worktree add` gitfile), only new paths under it are
/// left out (or, with `nested = working-tree`, its working files are
/// recorded), and nothing is ever deleted from history: under
/// `exclude`, then `working-tree`, then `exclude` again. Through a
/// single-process mount, or a front of a history service.
fn nothing_is_deleted_by_nesting_in(name: &'static str, front: bool) {
    let v = Vol::new(name);
    let start = |v: &Vol| -> Vec<Mounted> {
        if front {
            let service = start_history_service(v);
            let front = start_front(v, &v.mnt(), "pod-n");
            vec![front, service]
        } else {
            vec![v.mount(&[])]
        }
    };
    let stop = |ms: Vec<Mounted>| {
        for m in ms {
            let log = m.stop();
            assert!(!log.contains("alarm:"), "{log}");
        }
    };
    let git = "git -c user.name=t -c user.email=t@example.com -c init.defaultBranch=main";

    let ms = start(&v);
    v.sh("mkdir -p a/sub t && printf x1 > a/x && printf y > a/sub/y && printf z > t/z");
    let mut want = tree(&[("a/x", f("x1")), ("a/sub/y", f("y")), ("t/z", f("z"))]);
    v.expect(&want);

    // Nest: a working tree at a/, a bare repository inside it, and a
    // linked worktree (a `.git` file) in the tracked t/.
    v.sh(&format!(
        "{git} -C a init -q && {git} init -q --bare a/sub/bare.git && \
         {git} -C a commit -q --allow-empty -m init && {git} -C a worktree add -q ../t/wt"
    ));
    v.sh("printf x2 > a/x && printf new > a/new.txt && printf new > t/wt/new.txt");
    v.sh("printf b > a/sub/bare.git/description && printf z2 > t/z");
    want.insert("a/x".into(), f("x2"));
    want.insert("t/z".into(), f("z2"));
    v.expect(&want);
    assert_eq!(deleted_paths(&v), "");
    stop(ms);

    // Working files of nested repositories are recorded; still nothing
    // tracked goes.
    v.config("agentfs.nested", "working-tree");
    let ms = start(&v);
    v.sh("printf x3 > a/x && printf wt > a/wt.txt && printf wt > t/wt/wt.txt");
    want.insert("a/x".into(), f("x3"));
    want.insert("a/wt.txt".into(), f("wt"));
    want.insert("t/wt/wt.txt".into(), f("wt"));
    v.expect(&want);
    assert_eq!(deleted_paths(&v), "");
    stop(ms);

    // Back to `exclude`: what was recorded stays tracked, new paths under
    // the nested repositories are left out.
    v.config("agentfs.nested", "exclude");
    let ms = start(&v);
    v.sh("printf x4 > a/x && printf wt2 > a/wt.txt && printf later > a/later.txt");
    want.insert("a/x".into(), f("x4"));
    want.insert("a/wt.txt".into(), f("wt2"));
    v.expect(&want);
    assert_eq!(deleted_paths(&v), "");
    let head = v.history();
    assert!(
        head.contains_key("a/sub/y") && head.contains_key("t/z"),
        "{head:#?}"
    );
    stop(ms);
}

#[test]
#[ignore = "needs /dev/fuse and root: ci/fuse-tests.sh"]
fn nothing_is_deleted_by_nesting() {
    nothing_is_deleted_by_nesting_in("nesting", false);
}

#[test]
#[ignore = "needs /dev/fuse and root: ci/fuse-tests.sh"]
fn nothing_is_deleted_by_nesting_through_a_front() {
    nothing_is_deleted_by_nesting_in("nesting-front", true);
}

// ---- workloads ----

fn sqlite(v: &Vol, db: &str, mode: &str) {
    let mut sql =
        format!("PRAGMA journal_mode={mode};\nCREATE TABLE t(id INTEGER PRIMARY KEY, v TEXT);\n");
    for tx in 0..10 {
        sql.push_str("BEGIN;\n");
        for i in 0..100 {
            sql.push_str(&format!("INSERT INTO t(v) VALUES ('row {tx}-{i}');\n"));
        }
        sql.push_str("COMMIT;\n");
    }
    let script = v.p(&format!("{db}.sql"));
    fs::write(&script, sql).unwrap();
    fs::set_permissions(&script, fs::Permissions::from_mode(0o644)).unwrap();
    v.sh_as(
        Some(UID),
        &format!("sqlite3 u/{db} < {} > /dev/null", script.display()),
        true,
    );
    check_sqlite(v, db);
}

fn check_sqlite(v: &Vol, db: &str) {
    let out = v.sh_as(
        Some(UID),
        &format!("sqlite3 u/{db} 'PRAGMA integrity_check; SELECT count(*) FROM t;'"),
        true,
    );
    assert_eq!(out, "ok\n1000\n", "{db}");
}

#[test]
#[ignore = "needs /dev/fuse and root: ci/fuse-tests.sh"]
fn sqlite_on_the_mount() {
    let v = Vol::new("sqlite");
    let m = v.mount(&[]);
    v.sh("mkdir u && chown 10000:10000 u");
    sqlite(&v, "wal.db", "WAL");
    sqlite(&v, "delete.db", "DELETE");
    m.stop();
    let m = v.mount(&[]);
    check_sqlite(&v, "wal.db");
    check_sqlite(&v, "delete.db");
    m.stop();
}

// ---- the fsync class (design §8) ----

/// The attribute line that puts SQLite databases in the fsync class.
const SQLITE_CLASS: &str = "*.db* agentfs.commit=fsync agentfs.group=sqlite agentfs.coalesce=1s\n";

/// Writes `db` in `mode` from one long-lived `sqlite3` process: `txs`
/// transactions of 50 rows, `pause` apart, so captures land while the
/// database (and its WAL) is open and in use.
fn sqlite_slowly(v: &Vol, db: &str, mode: &str, txs: u32, pause: &str) {
    v.sh_as(
        Some(UID),
        &format!(
            "( echo 'PRAGMA journal_mode={mode};'; echo 'PRAGMA synchronous=NORMAL;'; \
               echo 'CREATE TABLE t(id INTEGER PRIMARY KEY, v TEXT);'; \
               i=0; while [ $i -lt {txs} ]; do \
                 echo 'BEGIN;'; \
                 j=0; while [ $j -lt 50 ]; do echo \"INSERT INTO t(v) VALUES ('$i-$j');\"; j=$((j+1)); done; \
                 echo 'COMMIT;'; sleep {pause}; i=$((i+1)); \
               done ) | sqlite3 u/{db} > /dev/null"
        ),
        true,
    );
}

/// Every version of `u/<db>` in the branch's history, with its `-wal` and
/// `-journal` from the same commit, restored into a scratch directory and
/// checked with `PRAGMA integrity_check`. Returns the distinct row counts
/// seen, each a whole number of transactions.
fn check_every_version(v: &Vol, db: &str) -> std::collections::BTreeSet<u64> {
    let repo = git2::Repository::open_bare(v.state().join("git")).unwrap();
    let mut walk = repo.revwalk().unwrap();
    walk.push_ref("refs/heads/agentfs/demo").unwrap();
    let mut seen = std::collections::BTreeSet::new();
    let mut versions = std::collections::BTreeSet::new();
    for id in walk {
        let tree = repo.find_commit(id.unwrap()).unwrap().tree().unwrap();
        let mut files = Vec::new();
        for suffix in ["", "-wal", "-journal", "-shm"] {
            let path = format!("u/{db}{suffix}");
            if let Ok(entry) = tree.get_path(Path::new(&path)) {
                assert_ne!(suffix, "-shm", "{path} was recorded");
                files.push((
                    suffix,
                    repo.find_blob(entry.id()).unwrap().content().to_vec(),
                ));
            }
        }
        if files.first().is_none_or(|(s, _)| !s.is_empty()) {
            continue;
        };
        if !versions.insert(files.clone()) {
            continue;
        }
        let scratch = tempfile::tempdir().unwrap();
        for (suffix, content) in &files {
            fs::write(scratch.path().join(format!("{db}{suffix}")), content).unwrap();
        }
        let out = Command::new("sqlite3")
            .arg(scratch.path().join(db))
            .arg("PRAGMA integrity_check; SELECT count(*) FROM t;")
            .output()
            .unwrap();
        let stdout = text(&out.stdout);
        let mut lines = stdout.lines();
        assert_eq!(
            lines.next(),
            Some("ok"),
            "{db} at {id:?}: {stdout}{}",
            text(&out.stderr),
            id = tree.id()
        );
        // A brand new database may not have its table yet.
        if let Some(count) = lines.next().and_then(|c| c.parse::<u64>().ok()) {
            assert_eq!(count % 50, 0, "{db}: a torn transaction ({count} rows)");
            seen.insert(count);
        }
    }
    seen
}

#[test]
#[ignore = "needs /dev/fuse and root: ci/fuse-tests.sh"]
fn sqlite_in_the_fsync_class() {
    let v = Vol::new("sqlite-fsync");
    fs::write(v.state().join("git/info/attributes"), SQLITE_CLASS).unwrap();
    let m = v.mount(&[]);
    v.sh("mkdir u && chown 10000:10000 u");
    sqlite_slowly(&v, "wal.db", "WAL", 12, "0.3");
    sqlite_slowly(&v, "delete.db", "DELETE", 12, "0.3");
    check_sqlite_rows(&v, "wal.db", 600);
    check_sqlite_rows(&v, "delete.db", 600);
    let log = m.stop();
    assert!(!log.contains("alarm:"), "{log}");

    for db in ["wal.db", "delete.db"] {
        let counts = check_every_version(&v, db);
        // The final version is whole, and captures landed mid-run.
        assert_eq!(counts.last(), Some(&600), "{db}: {counts:?}");
        assert!(counts.len() >= 2, "{db}: only {counts:?}");
    }
    // -shm never reaches the history; the journal and WAL of a closed
    // database are gone from the head.
    let head = v.history();
    assert!(head.contains_key("u/wal.db") && head.contains_key("u/delete.db"));
    assert!(head.keys().all(|p| !p.ends_with("-shm")), "{head:?}");
    assert!(!head.contains_key("u/wal.db-wal"), "{head:?}");
    assert!(!head.contains_key("u/delete.db-journal"), "{head:?}");
}

/// With a coalesce longer than the run, fsyncs after the first capture
/// nothing, yet closing the database records its final state whole.
#[test]
#[ignore = "needs /dev/fuse and root: ci/fuse-tests.sh"]
fn sqlite_fsync_class_records_on_close() {
    let v = Vol::new("sqlite-fsync-close");
    fs::write(
        v.state().join("git/info/attributes"),
        "*.db* agentfs.commit=fsync agentfs.group=sqlite agentfs.coalesce=1h\n",
    )
    .unwrap();
    let m = v.mount(&[]);
    v.sh("mkdir u && chown 10000:10000 u");
    sqlite_slowly(&v, "wal.db", "WAL", 3, "0");
    m.stop();
    let counts = check_every_version(&v, "wal.db");
    assert_eq!(counts.last(), Some(&150), "{counts:?}");
    let m = v.mount(&[]);
    check_sqlite_rows(&v, "wal.db", 150);
    m.stop();
}

fn check_sqlite_rows(v: &Vol, db: &str, rows: u64) {
    let out = v.sh_as(
        Some(UID),
        &format!("sqlite3 u/{db} 'PRAGMA integrity_check; SELECT count(*) FROM t;'"),
        true,
    );
    assert_eq!(out, format!("ok\n{rows}\n"), "{db}");
}
#[test]
#[ignore = "needs /dev/fuse and root: ci/fuse-tests.sh"]
fn git_works_on_the_mount() {
    let v = Vol::new("git");
    let m = v.mount(&[]);
    let script = "set -e
        git init -q proj
        cd proj
        git config user.name t
        git config user.email t@example.com
        mkdir -p src
        for i in $(seq 1 50); do echo \"line $i\" > src/f$i.txt; done
        git add .
        git commit -q -m one
        for i in $(seq 1 50); do echo more >> src/f$i.txt; done
        git commit -q -am two
        git mv src/f1.txt src/g1.txt
        git commit -q -m three
        git gc -q
        git fsck --no-progress
        git status --porcelain
        git log --oneline | wc -l";
    let out = v.sh(script);
    assert!(out.trim_end().ends_with('3'), "{out}");
    // proj is a nested repository: nothing of it is recorded.
    v.sh("printf r > recorded");
    v.expect(&tree(&[("recorded", f("r"))]));
    m.stop();
}

// ---- the daemon ----

#[test]
#[ignore = "needs /dev/fuse and root: ci/fuse-tests.sh"]
fn sigterm_with_a_file_open_for_writing() {
    let v = Vol::new("sigterm_open");
    let mut m = v.mount(&[]);
    // A writer that never closes its file after writing. (`printf >&3`
    // would close a duplicate of it afterwards: a FLUSH that records the
    // data, leaving the final sweep nothing to record, design §3.1.)
    let mut writer = Command::new("sh")
        .args(["-c", "exec 3>held; exec 1>&3; printf data; exec sleep 60"])
        .current_dir(v.mnt())
        .spawn()
        .unwrap();
    std::thread::sleep(Duration::from_millis(500));
    m.signal(rustix::process::Signal::TERM);
    let status = m.wait_exit(Duration::from_secs(30));
    let log = m.log();
    assert!(status.success(), "{log}");
    assert!(log.contains("detached lazily"), "{log}");
    assert!(
        log.contains("final sweep: 1 open file(s) recorded"),
        "{log}"
    );
    assert!(!is_mounted(&v.mnt()));
    let _ = writer.kill();
    let _ = writer.wait();
    v.expect(&tree(&[("held", f("data"))]));
    let status = text(&v.agentfs(&["status"]).stdout);
    assert!(status.contains("backlog:   0 record(s)"), "{status}");
}

#[test]
#[ignore = "needs /dev/fuse and root: ci/fuse-tests.sh"]
fn an_external_unmount_stops_the_daemon() {
    let v = Vol::new("external_umount");
    let mut m = v.mount(&[]);
    v.sh("printf a > a");
    v.expect(&tree(&[("a", f("a"))]));
    assert!(
        Command::new("umount")
            .arg(v.mnt())
            .status()
            .unwrap()
            .success()
    );
    let status = m.wait_exit(Duration::from_secs(30));
    assert!(status.success(), "{}", m.log());
    assert!(m.log().contains("unmounted from outside"), "{}", m.log());
}

#[test]
#[ignore = "needs /dev/fuse and root: ci/fuse-tests.sh"]
fn the_lock_holds_while_mounted() {
    let v = Vol::new("lock");
    // A remote, so that `push` has work to hand the mount.
    git2::Repository::init_bare(v.p("remote.git")).unwrap();
    v.config(
        "remote.origin.url",
        &format!("file://{}", v.p("remote.git").display()),
    );
    let m = v.mount(&[]);
    let root = v.backing().to_str().unwrap().to_owned();
    for args in [&["run"][..], &["flush"], &["--root", &root, "record", "f"]] {
        let out = v.agentfs(args);
        assert_eq!(
            out.status.code(),
            Some(75),
            "{args:?}: {}",
            text(&out.stderr)
        );
    }
    // `push` is not refused: the mount pushes for it (design §2.3).
    let out = v.agentfs(&["push", "--timeout", "10s"]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    assert!(
        text(&out.stdout).contains("through the running daemon"),
        "{}",
        text(&out.stdout)
    );
    let out = v.agentfs(&["status"]);
    assert!(out.status.success());
    assert!(
        text(&out.stdout).contains("command=mount"),
        "{}",
        text(&out.stdout)
    );
    m.stop();
}

#[test]
#[ignore = "needs /dev/fuse and root: ci/fuse-tests.sh"]
fn the_startup_line_reports_passthrough() {
    let v = Vol::new("passthrough");
    let m = v.mount(&[]);
    v.sh("printf data > f && cat f > /dev/null");
    v.expect(&tree(&[("f", f("data"))]));
    let log = m.stop();
    let line = log
        .lines()
        .find(|l| l.starts_with("mounted "))
        .unwrap()
        .to_owned();
    // The CI job's log shows what the runner's kernel negotiated.
    println!("startup ({}): {line}", passthrough());
    match passthrough().as_str() {
        "off" => assert!(
            line.contains("passthrough: off (disabled by --passthrough off)"),
            "{line}"
        ),
        "on" => assert!(line.contains("passthrough: on,"), "{line}"),
        _ => assert!(line.contains("passthrough: "), "{line}"),
    }
}

// ---- bead agentfs-dyu: a shared mapping that outlives its file's last close ----

/// Maps `path` shared and writable, closes the file (its last close: FLUSH,
/// then RELEASE) and says `closed`. On a `write` line from stdin it writes
/// `new` through the mapping and says `written`; on `finish`, `msync` mode
/// msyncs (`MS_SYNC`) and munmaps, `exit` mode exits at once and leaves the
/// dirty pages to the kernel.
const MMAP_WRITER: &str = r#"
import ctypes, mmap, os, sys
path, new, mode = sys.argv[1], sys.argv[2].encode(), sys.argv[3]
libc = ctypes.CDLL(None, use_errno=True)
libc.mmap.restype = ctypes.c_void_p
libc.mmap.argtypes = [ctypes.c_void_p, ctypes.c_size_t, ctypes.c_int, ctypes.c_int, ctypes.c_int, ctypes.c_long]
libc.msync.argtypes = [ctypes.c_void_p, ctypes.c_size_t, ctypes.c_int]
libc.munmap.argtypes = [ctypes.c_void_p, ctypes.c_size_t]
def check(ok, what):
    if not ok:
        e = ctypes.get_errno()
        raise OSError(e, what + ": " + os.strerror(e))
def say(word):
    print(word)
    sys.stdout.flush()
def expect(word):
    got = sys.stdin.readline().strip()
    if got != word:
        raise SystemExit("expected " + word + ", got " + repr(got))
n = len(new)
fd = os.open(path, os.O_RDWR)
addr = libc.mmap(None, n, mmap.PROT_READ | mmap.PROT_WRITE, mmap.MAP_SHARED, fd, 0)
check(addr not in (None, ctypes.c_void_p(-1).value), "mmap")
os.close(fd)
say("closed")
expect("write")
ctypes.memmove(addr, new, n)
say("written")
expect("finish")
if mode == "msync":
    check(libc.msync(addr, n, 4) == 0, "msync")  # MS_SYNC
    check(libc.munmap(addr, n) == 0, "munmap")
    say("msync, munmap")
else:
    say("exiting without msync")
    os._exit(0)
"#;

/// Whether process `pid` holds `path` open for writing.
fn holds_for_writing(pid: rustix::process::Pid, path: &Path) -> bool {
    let proc = PathBuf::from(format!("/proc/{}", pid.as_raw_nonzero()));
    let Ok(fds) = fs::read_dir(proc.join("fd")) else {
        return false;
    };
    fds.flatten().any(|fd| {
        if fs::read_link(fd.path()).ok().as_deref() != Some(path) {
            return false;
        }
        let info = fs::read_to_string(proc.join("fdinfo").join(fd.file_name()));
        info.ok()
            .and_then(|i| {
                let flags = i.lines().find_map(|l| l.strip_prefix("flags:"))?.trim();
                u32::from_str_radix(flags, 8).ok()
            })
            .is_some_and(|flags| flags & 0o3 != 0)
    })
}

/// Waits up to `limit` for the daemon to hold `path` open for writing no
/// more: its RELEASE of the last handle has been served (the handle's
/// descriptor is closed after RELEASE's record). Whether it did.
fn released(m: &Mounted, path: &Path, limit: Duration) -> bool {
    let deadline = Instant::now() + limit;
    while holds_for_writing(m.pid(), path) {
        if Instant::now() > deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    true
}

/// A write through a shared mapping after the last close is recorded late
/// but recorded (design §3.1). Under FUSE passthrough the kernel's
/// `backing_file_mmap` points the mapping at the backing file
/// (`vma_set_file`), so the mapping does not hold the FUSE file open:
/// RELEASE was served at the last close, before the write, and the sweep
/// looks only at open files (agentfs-dyu, red under passthrough on and
/// auto in CI run 40993499943). So a handle that can write is never given
/// passthrough (design §17.1): its mapping holds the FUSE file, and
/// RELEASE waits for munmap in every mode. Should a writer get passthrough
/// again, the writer here writes only once the daemon has served that
/// RELEASE, so the write never races it.
#[test]
#[ignore = "needs /dev/fuse and root: ci/fuse-tests.sh"]
fn a_shared_mapping_written_after_the_last_close_is_recorded() {
    use std::io::{BufRead as _, BufReader};

    let v = Vol::new("mmap_after_close");
    let m = v.mount(&[]);
    let startup = m
        .log()
        .lines()
        .find(|l| l.starts_with("mounted "))
        .unwrap()
        .to_owned();
    let active = startup.contains("passthrough: on,");
    println!(
        "agentfs-dyu: kernel {}; --passthrough {}; startup: {startup}",
        text(&Command::new("uname").arg("-r").output().unwrap().stdout).trim(),
        passthrough()
    );
    if passthrough() == "on" {
        assert!(active, "--passthrough on but not active: {startup}");
    }

    // Same length as the new content: a mapping cannot grow a file.
    v.sh("printf 'old-msync-bytes!' > synced && printf 'old-exit-bytes!!' > exited");
    v.expect(&tree(&[
        ("exited", f("old-exit-bytes!!")),
        ("synced", f("old-msync-bytes!")),
    ]));

    let cases = [
        ("synced", "old-msync-bytes!", "NEW-MSYNC-BYTES!", "msync"),
        ("exited", "old-exit-bytes!!", "NEW-EXIT-BYTES!!", "exit"),
    ];
    let mut evidence = Vec::new();
    for (name, old, new, mode) in cases {
        let backing = fs::canonicalize(v.backing().join(name)).unwrap();
        // The shell's own RELEASE first.
        assert!(released(&m, &backing, Duration::from_secs(10)));
        let mut writer = Command::new("python3")
            .arg("-c")
            .arg(MMAP_WRITER)
            .arg(v.mnt().join(name))
            .arg(new)
            .arg(mode)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let mut stdin = writer.stdin.take().unwrap();
        let mut stdout = BufReader::new(writer.stdout.take().unwrap()).lines();
        let mut said = |want: &str| {
            let got = stdout.next().map(Result::unwrap);
            assert_eq!(got.as_deref(), Some(want), "the writer of {name}");
        };
        said("closed");
        // With the mapping holding the FUSE file open (no passthrough) no
        // RELEASE comes before munmap: under passthrough this waits its
        // whole timeout, and only a writer given passthrough would end it.
        let release_first = active && released(&m, &backing, Duration::from_secs(10));
        writeln!(stdin, "write").unwrap();
        said("written");
        // Under passthrough the mapping is the backing file's page cache:
        // the backing file shows the write before any msync. Without it
        // the write waits in the FUSE inode's pages until written back.
        let before_msync = fs::read_to_string(&backing).unwrap();
        let mapped_backing = before_msync == new;
        assert!(
            mapped_backing || before_msync == old,
            "{name}: {before_msync:?}"
        );
        writeln!(stdin, "finish").unwrap();
        drop(stdin);
        let rest: Vec<String> = stdout.map(Result::unwrap).collect();
        let status = writer.wait().unwrap();
        assert!(
            status.success(),
            "the writer of {name}: {status:?} {rest:?}"
        );
        println!(
            "agentfs-dyu: {name} ({mode}): RELEASE served before the write: {release_first}; \
             backing file before msync {before_msync:?}, so the mapping is the {}; then {rest:?}",
            if mapped_backing {
                "backing file (passthrough)"
            } else {
                "FUSE file (through the daemon)"
            }
        );
        evidence.push((name, new, release_first, mapped_backing));
    }

    // Shutdown is the event after which nothing more is recorded: every
    // RELEASE has been served, the final sweep has run and the commit
    // loop has drained (backlog 0 below).
    m.stop();
    let status = v.ok(&["status"]);
    assert!(status.contains("backlog:   0 record(s)"), "{status}");
    let history = v.history();
    let mut gaps = Vec::new();
    for (name, new, _, _) in &evidence {
        let on_disk = fs::read_to_string(v.backing().join(name)).unwrap();
        assert_eq!(&on_disk, new, "the write never reached the backing file");
        let recorded = history.get(*name);
        println!("agentfs-dyu: {name}: backing file {on_disk:?}; history {recorded:?}");
        if recorded != Some(&f(new)) {
            gaps.push(*name);
        }
    }
    println!(
        "agentfs-dyu: RESULT (--passthrough {}, active {active}): {}",
        passthrough(),
        if gaps.is_empty() {
            "every write through a mapping after the last close was recorded".to_owned()
        } else {
            format!("NOT RECORDED: {gaps:?}")
        }
    );

    // A writer's mapping is on the FUSE file in every mode: a handle that
    // can write never gets passthrough (design §17.1).
    for (name, _, _, mapped_backing) in &evidence {
        assert!(
            !*mapped_backing,
            "{name}: passthrough active {active}, mapping on the backing file"
        );
    }
    assert!(
        gaps.is_empty(),
        "writes through a shared mapping after the last close went unrecorded: {gaps:?}"
    );
}

#[test]
#[ignore = "needs /dev/fuse and root: ci/fuse-tests.sh"]
fn an_unprivileged_daemon() {
    let Ok(user) = std::env::var("AGENTFS_TEST_UNPRIV_USER") else {
        println!("AGENTFS_TEST_UNPRIV_USER not set: skipped");
        return;
    };
    let id = |flag: &str| -> u32 {
        let out = Command::new("id").arg(flag).arg(&user).output().unwrap();
        text(&out.stdout).trim().parse().unwrap()
    };
    let (uid, gid) = (id("-u"), id("-g"));
    let v = Vol::new("unprivileged");
    let ok = Command::new("chown")
        .arg("-R")
        .arg(format!("{uid}:{gid}"))
        .arg(v.dir.path())
        .status()
        .unwrap()
        .success();
    assert!(ok);
    let log_path = v.log_path();
    let as_user = move |cmd: Command| {
        let mut wrapped = Command::new("setpriv");
        wrapped
            .arg(format!("--reuid={uid}"))
            .arg(format!("--regid={gid}"))
            .arg("--init-groups")
            .arg("--")
            .arg(cmd.get_program())
            .args(cmd.get_args());
        for (k, val) in cmd.get_envs() {
            match val {
                Some(val) => wrapped.env(k, val),
                None => wrapped.env_remove(k),
            };
        }
        let log = fs::OpenOptions::new().append(true).open(&log_path).unwrap();
        wrapped
            .stdout(log.try_clone().unwrap())
            .stderr(log)
            .stdin(Stdio::null());
        wrapped
    };
    if passthrough() == "on" {
        // Without CAP_SYS_ADMIN, `on` is refused before anything is
        // mounted.
        let status = as_user(v.mount_command(&[])).status().unwrap();
        let log = fs::read_to_string(v.log_path()).unwrap();
        assert_eq!(status.code(), Some(1), "{log}");
        assert!(
            log.contains("--passthrough on: the daemon lacks CAP_SYS_ADMIN"),
            "{log}"
        );
        assert!(!is_mounted(&v.mnt()));
        return;
    }
    let m = v.mount_with(&[], as_user);
    // Root writes through another user's allow_other mount.
    v.sh("printf r > by-root");
    v.expect(&tree(&[("by-root", f("r"))]));
    let meta = fs::metadata(v.backing().join("by-root")).unwrap();
    assert_eq!(meta.uid(), uid, "a non-root daemon creates files as itself");
    let log = m.stop();
    let line = log.lines().find(|l| l.starts_with("mounted ")).unwrap();
    println!("unprivileged startup: {line}");
    if passthrough() != "off" {
        assert!(
            line.contains("passthrough: off (the daemon lacks CAP_SYS_ADMIN"),
            "{line}"
        );
    }
}

// ---- extended attributes and stray .git directories ----

#[test]
#[ignore = "needs /dev/fuse and root: ci/fuse-tests.sh"]
fn xattrs_through_the_mount() {
    let v = Vol::new("xattrs");
    let m = v.mount(&[]);
    v.sh("printf x > f && mkdir d");
    for target in ["f", "d"] {
        v.sh(&format!("setfattr -n user.agentfs -v hello {target}"));
        let got = v.sh(&format!("getfattr --only-values -n user.agentfs {target}"));
        assert_eq!(got, "hello", "{target}");
        // It is the backing inode's attribute.
        let mut buf = [0u8; 64];
        let n =
            rustix::fs::getxattr(v.backing().join(target), "user.agentfs", &mut buf[..]).unwrap();
        assert_eq!(&buf[..n], b"hello", "{target}");
        let listed = v.sh(&format!("getfattr -d -m - {target}"));
        assert!(listed.contains("user.agentfs=\"hello\""), "{listed}");
        v.sh(&format!("setfattr -x user.agentfs {target}"));
        assert!(
            rustix::fs::getxattr(v.backing().join(target), "user.agentfs", &mut buf[..]).is_err()
        );
    }
    // Extended attributes are not content: nothing new is recorded.
    v.expect(&tree(&[("f", f("x"))]));
    m.stop();
}

#[test]
#[ignore = "needs /dev/fuse and root: ci/fuse-tests.sh"]
fn a_dot_git_that_is_no_repository_alarms() {
    let v = Vol::new("stray_dot_git");
    let m = v.mount(&[]);
    v.sh("mkdir -p notes/.git && printf r > notes/.git/report.txt && printf k > keep");
    v.expect(&tree(&[("keep", f("k"))]));
    let log = m.stop();
    let alarms: Vec<&str> = log.lines().filter(|l| l.starts_with("alarm:")).collect();
    assert_eq!(alarms.len(), 1, "{log}");
    assert!(alarms[0].contains("notes/.git/report.txt"), "{log}");
}

// ---- design §15, §18 test 11: recovery ----

#[test]
#[ignore = "needs /dev/fuse and root: ci/fuse-tests.sh"]
fn restore_through_the_mount() {
    let v = Vol::new("restore_through_the_mount");
    let m = v.mount(&[]);
    v.sh("chown 10000:10000 .");
    v.sh_as(
        Some(UID),
        "printf v1 > f && mkdir d && printf x1 > d/x",
        true,
    );
    v.expect(&tree(&[("d/x", f("x1")), ("f", f("v1"))]));
    let c1 = v.head_id();
    v.sh_as(Some(UID), "printf v2 > f && printf n > d/new", true);
    v.expect(&tree(&[
        ("d/new", f("n")),
        ("d/x", f("x1")),
        ("f", f("v2")),
    ]));
    let c2 = v.head_id();

    // A file, while mounted: the mount shows it at once, history has one
    // new commit, authored by the operator (root here).
    let out = v.ok(&["restore", "f", &c1]);
    assert!(out.contains("  write f\n"), "{out}");
    assert!(
        out.contains("restored 1 path(s), removed 0: commit "),
        "{out}"
    );
    assert_eq!(fs::read_to_string(v.mnt().join("f")).unwrap(), "v1");
    v.expect(&tree(&[
        ("d/new", f("n")),
        ("d/x", f("x1")),
        ("f", f("v1")),
    ]));
    assert_eq!(v.parent_id(), c2, "one commit");
    let (author, message) = v.head();
    assert_eq!(author, "uid-0");
    assert!(message.starts_with("restore f from "), "{message}");
    assert!(message.contains("Agentfs-Trigger: restore\n"), "{message}");
    assert!(
        message.contains(&format!("Agentfs-Restore: {c1}\n")),
        "{message}"
    );
    assert!(message.contains("Agentfs-Uid: 0\n"), "{message}");
    assert!(!message.contains("Agentfs-Uid: 10000"), "{message}");
    assert!(
        out.contains(&v.head_id()),
        "the reply names the commit: {out}"
    );
    // Rewritten in place: still the workload's file.
    assert_eq!(fs::metadata(v.mnt().join("f")).unwrap().uid(), UID);

    // A directory: a removal too. Then a dry run and a no-op.
    let out = v.ok(&["restore", "--dry-run", "d", &c1]);
    assert!(
        out.contains("remove d/new") && out.contains("dry run"),
        "{out}"
    );
    assert!(v.mnt().join("d/new").exists());
    v.ok(&["restore", "d", &c1]);
    assert!(!v.mnt().join("d/new").exists());
    v.expect(&tree(&[("d/x", f("x1")), ("f", f("v1"))]));
    let out = v.ok(&["restore", ".", &c1]);
    assert!(out.contains("already matches"), "{out}");
    // A refusal says why and exits 1 (an unrecorded write through the
    // mount: restore_refuses_an_unrecorded_write_through_the_mount).
    let out = v.agentfs(&["restore", "nothing-here", &c1]);
    assert_eq!(out.status.code(), Some(1), "{}", text(&out.stderr));
    assert!(text(&out.stderr).contains("nothing at nothing-here"));
    m.stop();
}

#[test]
#[ignore = "needs /dev/fuse and root: ci/fuse-tests.sh"]
fn revert_through_the_mount() {
    let v = Vol::new("revert_through_the_mount");
    let m = v.mount(&[]);
    v.sh("chown 10000:10000 .");
    v.sh_as(
        Some(UID),
        "mkdir d && printf y > d/y && chmod 750 d/y && printf k > keep",
        true,
    );
    v.expect(&tree(&[("d/y", Exec("y".into())), ("keep", f("k"))]));
    // One open, one write, one close: one record, so the head is the commit
    // that added x. A shell `printf x > x` would not do: dash opens x, dups it
    // onto stdout and closes the original before printf writes, and that
    // close's FLUSH records x empty (design §3.1). When the write's FLUSH
    // comes more than a commit window later, the head adds nothing and only
    // writes x, and reverting it writes x back to empty.
    v.sh_as(Some(UID), "printf x | tee x >/dev/null", true);
    v.expect(&tree(&[
        ("d/y", Exec("y".into())),
        ("keep", f("k")),
        ("x", f("x")),
    ]));
    let add = v.head_id();
    v.sh_as(Some(UID), "rm d/y", true);
    v.expect(&tree(&[("keep", f("k")), ("x", f("x"))]));
    let delete = v.head_id();

    // Revert the add: x is removed.
    let out = v.ok(&["revert", &add]);
    assert!(out.contains("  remove x\n"), "{out}");
    assert!(!v.mnt().join("x").exists());
    v.expect(&tree(&[("keep", f("k"))]));
    let (_, message) = v.head();
    assert!(
        message.contains(&format!("Agentfs-Revert: {add}\n")),
        "{message}"
    );
    assert!(message.contains("Agentfs-Trigger: restore\n"), "{message}");

    // Revert the delete: d/y comes back, owned like its directory, with
    // its exec bit.
    v.ok(&["revert", &delete]);
    v.expect(&tree(&[("d/y", Exec("y".into())), ("keep", f("k"))]));
    let meta = fs::metadata(v.mnt().join("d/y")).unwrap();
    assert_eq!((meta.uid(), meta.gid()), (UID, UID), "the parent's owner");
    assert_eq!(meta.mode() & 0o777, 0o755);
    assert_eq!(fs::read_to_string(v.mnt().join("d/y")).unwrap(), "y");
    m.stop();
}

#[test]
#[ignore = "needs /dev/fuse and root: ci/fuse-tests.sh"]
fn restore_refuses_a_file_open_for_writing() {
    let v = Vol::new("restore_open_for_writing");
    let m = v.mount(&[]);
    v.sh("printf v1 > f");
    v.expect(&tree(&[("f", f("v1"))]));
    let c1 = v.head_id();
    v.sh("printf v2 > f");
    v.expect(&tree(&[("f", f("v2"))]));

    // A writer holding f open (without writing).
    let mut holder = Command::new("sh")
        .arg("-c")
        .arg("exec 3>>f; echo ready; exec sleep 60")
        .current_dir(v.mnt())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    {
        use std::io::BufRead as _;
        let mut line = String::new();
        std::io::BufReader::new(holder.stdout.as_mut().unwrap())
            .read_line(&mut line)
            .unwrap();
        assert_eq!(line.trim(), "ready");
    }
    let out = v.agentfs(&["restore", "f", &c1]);
    let err = text(&out.stderr);
    assert_eq!(out.status.code(), Some(1), "{err}");
    assert!(
        err.contains("open for writing through the mount: f"),
        "{err}"
    );
    assert!(err.contains("--force"), "{err}");
    assert_eq!(fs::read_to_string(v.mnt().join("f")).unwrap(), "v2");

    v.ok(&["restore", "--force", "f", &c1]);
    assert_eq!(fs::read_to_string(v.mnt().join("f")).unwrap(), "v1");
    v.expect(&tree(&[("f", f("v1"))]));
    let _ = holder.kill();
    let _ = holder.wait();
    m.stop();
}

#[test]
#[ignore = "needs /dev/fuse and root: ci/fuse-tests.sh"]
fn restore_with_the_workload_stopped() {
    let v = Vol::new("restore_stopped");
    let m = v.mount(&[]);
    v.sh("chown 10000:10000 .");
    v.sh_as(Some(UID), "printf v1 > f && printf o > other", true);
    v.expect(&tree(&[("f", f("v1")), ("other", f("o"))]));
    let c1 = v.head_id();
    v.sh_as(Some(UID), "printf v2 > f && rm other", true);
    v.expect(&tree(&[("f", f("v2"))]));
    m.stop();

    // No mount: direct mode writes the backing tree and records it.
    let backing = v.backing().to_str().unwrap().to_owned();
    let out = v.ok(&["--root", &backing, "restore", ".", &c1]);
    assert!(
        out.contains("restored 2 path(s), removed 0: commit "),
        "{out}"
    );
    v.expect(&tree(&[("f", f("v1")), ("other", f("o"))]));
    let (author, message) = v.head();
    assert_eq!(author, "uid-0");
    assert!(message.contains("Agentfs-Restore: "), "{message}");
    // The rewritten file keeps its owner; the recreated one gets its
    // directory's.
    assert_eq!(fs::metadata(v.backing().join("f")).unwrap().uid(), UID);
    assert_eq!(fs::metadata(v.backing().join("other")).unwrap().uid(), UID);

    // Remounted, the tree and history agree.
    let m = v.mount(&[]);
    assert_eq!(fs::read_to_string(v.mnt().join("f")).unwrap(), "v1");
    assert_eq!(fs::read_to_string(v.mnt().join("other")).unwrap(), "o");
    v.sh("printf v3 > f");
    v.expect(&tree(&[("f", f("v3")), ("other", f("o"))]));
    m.stop();
}

/// A file a workload has written through the mount and still holds open
/// is a change not recorded yet: refused even with --force, and kept. The
/// check's own read records it, so a retry finds it in history.
#[test]
#[ignore = "needs /dev/fuse and root: ci/fuse-tests.sh"]
fn restore_refuses_an_unrecorded_write_through_the_mount() {
    let v = Vol::new("restore_unrecorded_write");
    let m = v.mount(&[]);
    v.sh("printf v1 > f");
    v.expect(&tree(&[("f", f("v1"))]));
    let c1 = v.head_id();
    v.sh("printf v2 > f");
    v.expect(&tree(&[("f", f("v2"))]));
    let head = v.head_id();

    // A writer that has written f and holds it open: nothing recorded.
    // Every close of a descriptor for f is a change boundary the mount
    // records, including a copy closed by fork and exec (or by a shell
    // redirection), so the writer is a process that never forks.
    let mut holder = Command::new("perl")
        .arg("-e")
        .arg(
            r#"open(my $f, ">", "f") or die "$!"; syswrite($f, "dirty") == 5 or die "$!";
               $| = 1; print "ready\n"; sleep 60;"#,
        )
        .current_dir(v.mnt())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    {
        use std::io::BufRead as _;
        let mut line = String::new();
        std::io::BufReader::new(holder.stdout.as_mut().unwrap())
            .read_line(&mut line)
            .unwrap();
        assert_eq!(line.trim(), "ready");
    }
    let out = v.agentfs(&["restore", "--force", "f", &c1]);
    let err = text(&out.stderr);
    assert_eq!(out.status.code(), Some(1), "{err}");
    assert!(err.contains("f differs from history"), "{err}");
    assert!(err.contains("--force does not override"), "{err}");
    assert_eq!(fs::read_to_string(v.mnt().join("f")).unwrap(), "dirty");
    // The check's own read of f through the mount was a change boundary
    // (its close is a FLUSH of a changed inode), so the write is in
    // history now: nothing unrecorded is left to lose, and only the
    // open handle remains, which --force overrides.
    v.expect(&tree(&[("f", f("dirty"))]));
    assert_ne!(v.head_id(), head);
    let out = v.agentfs(&["restore", "f", &c1]);
    let err = text(&out.stderr);
    assert_eq!(out.status.code(), Some(1), "{err}");
    assert!(
        err.contains("open for writing through the mount: f"),
        "{err}"
    );
    v.ok(&["restore", "--force", "f", &c1]);
    assert_eq!(fs::read_to_string(v.mnt().join("f")).unwrap(), "v1");
    v.expect(&tree(&[("f", f("v1"))]));
    // The commit before the restore holds the workload's write.
    let repo = git2::Repository::open_bare(v.state().join("git")).unwrap();
    let parent = repo
        .find_commit(git2::Oid::from_str(&v.parent_id()).unwrap())
        .unwrap();
    let blob = parent.tree().unwrap().get_name("f").unwrap().id();
    assert_eq!(repo.find_blob(blob).unwrap().content(), b"dirty");
    let _ = holder.kill();
    let _ = holder.wait();
    m.stop();
}

/// Only root and the daemon's user may use the control socket: another
/// uid is refused by its peer credentials when it can reach the socket
/// (CAP_DAC_OVERRIDE), and cannot reach it otherwise.
#[test]
#[ignore = "needs /dev/fuse and root: ci/fuse-tests.sh"]
fn restore_by_another_user_is_refused() {
    let v = Vol::new("restore_other_user");
    let m = v.mount(&[]);
    v.sh("printf v1 > f");
    v.expect(&tree(&[("f", f("v1"))]));
    let c1 = v.head_id();
    v.sh("printf v2 > f");
    v.expect(&tree(&[("f", f("v2"))]));
    let head = v.head_id();
    // A copy of the binary uid 10000 can execute wherever the build is.
    let bin = v.p("agentfs-bin");
    fs::copy(env!("CARGO_BIN_EXE_agentfs"), &bin).unwrap();
    fs::set_permissions(&bin, fs::Permissions::from_mode(0o755)).unwrap();
    let as_other = |caps: &[&str]| {
        let mut cmd = Command::new("setpriv");
        cmd.args([
            format!("--reuid={UID}"),
            format!("--regid={UID}"),
            "--clear-groups".into(),
        ])
        .args(caps)
        .arg("--")
        .arg(&bin)
        .arg("--state")
        .arg(v.state())
        .args(["restore", "f", c1.as_str()])
        .env("AGENTFS_REPLICA", "test-0")
        .env_remove("AGENTFS_ROOT");
        cmd.output().unwrap()
    };
    // Through the socket: the peer's uid is refused.
    let out = as_other(&["--inh-caps=+dac_override", "--ambient-caps=+dac_override"]);
    let err = text(&out.stderr);
    assert_eq!(out.status.code(), Some(1), "{err}");
    assert!(
        err.contains(&format!(
            "uid {UID} may not use this mount's control socket"
        )),
        "{err}"
    );
    // Without it: the socket's directory is closed to it.
    let out = as_other(&[]);
    let err = text(&out.stderr);
    assert_eq!(out.status.code(), Some(1), "{err}");
    assert!(
        err.contains("cannot be used") || err.contains("ermission denied"),
        "{err}"
    );
    assert_eq!(fs::read_to_string(v.mnt().join("f")).unwrap(), "v2");
    assert_eq!(v.head_id(), head);
    m.stop();
}

// ---- design §17.1: probes, and a sidecar restarted after a crash ----

/// The mountinfo line for `mnt`, as the runner's kernel reports it.
fn mountinfo_line(mnt: &Path) -> Option<String> {
    let info = fs::read_to_string("/proc/self/mountinfo").unwrap();
    let mnt = mnt.to_str().unwrap();
    info.lines()
        .rfind(|l| l.split(' ').nth(4).is_some_and(|m| m == mnt))
        .map(str::to_owned)
}

#[test]
#[ignore = "needs /dev/fuse and root: ci/fuse-tests.sh"]
fn probe_hold_and_detach_after_a_crash() {
    let v = Vol::new("probe_hold");
    let mnt = v.mnt();
    let mnt_s = mnt.to_str().unwrap().to_owned();
    let mut m = v.mount(&[]);
    // The kernel's view, for the CI log.
    println!("mountinfo: {:?}", mountinfo_line(&mnt));
    let out = v.agentfs(&["probe", &mnt_s]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    assert_eq!(text(&out.stdout), "live agentfs mount (agentfs:demo)\n");
    v.sh("printf before > before");
    v.expect(&tree(&[("before", f("before"))]));

    // The daemon dies: the mount point is stale.
    m.signal(rustix::process::Signal::KILL);
    m.wait_exit(Duration::from_secs(10));
    assert_eq!(
        rustix::fs::stat(&mnt).unwrap_err(),
        rustix::io::Errno::NOTCONN
    );
    let out = v.agentfs(&["probe", &mnt_s]);
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(text(&out.stdout), "stale agentfs mount (daemon gone)\n");
    let out = v.agentfs(&["probe", "--allow-stale", &mnt_s]);
    assert_eq!(out.status.code(), Some(0));

    // The default still refuses, with the advice.
    let out = v.agentfs(&["mount", v.backing().to_str().unwrap(), &mnt_s]);
    assert_eq!(out.status.code(), Some(1));
    assert!(
        text(&out.stderr).contains("stale FUSE mount"),
        "{}",
        text(&out.stderr)
    );

    // Hold: mounts nothing, takes no lock, keeps the mount point stale.
    let child = v
        .mount_command(&["--on-stale-mount", "hold"])
        .spawn()
        .unwrap();
    let mut held = Mounted {
        child: Some(child),
        mnt: mnt.clone(),
        log: v.log_path(),
    };
    let deadline = Instant::now() + Duration::from_secs(20);
    while !held.log().contains("Holding until SIGTERM") {
        assert!(
            Instant::now() < deadline,
            "never held; log:\n{}",
            held.log()
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    std::thread::sleep(Duration::from_millis(300));
    assert!(is_mounted(&mnt), "{}", held.log());
    assert_eq!(
        rustix::fs::stat(&mnt).unwrap_err(),
        rustix::io::Errno::NOTCONN
    );
    let status = text(&v.agentfs(&["status"]).stdout);
    assert!(status.contains("lock:      free"), "{status}");
    let out = v.agentfs(&["probe", "--allow-stale", &mnt_s]);
    assert_eq!(out.status.code(), Some(0));

    // SIGTERM: the stale mount is detached and the hold exits 1.
    held.signal(rustix::process::Signal::TERM);
    let status = held.wait_exit(Duration::from_secs(20));
    let log = held.log();
    assert_eq!(status.code(), Some(1), "{log}");
    assert!(
        log.contains(&format!("detached stale mount {mnt_s}")),
        "{log}"
    );
    assert!(!is_mounted(&mnt), "{log}");
    let out = v.agentfs(&["probe", &mnt_s]);
    assert_eq!(text(&out.stdout), "not mounted\n");

    // The next mount recovers what was recorded before the kill.
    let m = v.mount(&["--on-stale-mount", "hold"]);
    v.expect(&tree(&[("before", f("before"))]));
    v.sh("printf after > after");
    v.expect(&tree(&[("after", f("after")), ("before", f("before"))]));
    m.stop();
}

/// Runs `cmd` under `setpriv` with CAP_SYS_ADMIN dropped from the bounding
/// set, so root's exec gets every capability but that one.
fn without_sys_admin(cmd: Command) -> Command {
    let mut wrapped = Command::new("setpriv");
    wrapped
        .args(["--bounding-set=-sys_admin", "--inh-caps=-sys_admin", "--"])
        .arg(cmd.get_program())
        .args(cmd.get_args());
    for (name, value) in cmd.get_envs() {
        match value {
            Some(value) => wrapped.env(name, value),
            None => wrapped.env_remove(name),
        };
    }
    wrapped
}

/// How many agentfs mounts the kernel lists at `mnt` (stacked layers).
fn layers(mnt: &Path) -> usize {
    let info = fs::read_to_string("/proc/self/mountinfo").unwrap();
    let mnt = mnt.to_str().unwrap();
    info.lines()
        .filter(|l| l.split(' ').nth(4).is_some_and(|m| m == mnt) && l.contains(" agentfs:"))
        .count()
}

/// Kills the daemon with SIGKILL and checks the mount point went stale.
fn crash(m: &mut Mounted) {
    m.signal(rustix::process::Signal::KILL);
    m.wait_exit(Duration::from_secs(10));
    assert_eq!(
        rustix::fs::stat(&m.mnt).unwrap_err(),
        rustix::io::Errno::NOTCONN
    );
}

/// `--on-stale-mount recover` (design §17.1): a daemon killed with -9 is
/// replaced by one stacked over its stale mount, which every copy of the
/// stale mount then reaches: a bind of it (as a kubelet subPath) and a
/// slave mount namespace (as a workload container). Refused without
/// CAP_SYS_ADMIN and for fronts; a hold detaches every stale layer.
#[test]
#[ignore = "needs /dev/fuse and root: ci/fuse-tests.sh"]
fn recover_stacks_over_a_crashed_mount() {
    let v = Vol::new("recover");
    let mnt = v.mnt();
    let mnt_s = mnt.to_str().unwrap().to_owned();
    let subpath = v.p("subpath");
    fs::create_dir(&subpath).unwrap();
    let mut first = v.mount(&[]);
    // Shared, as under a Bidirectional emptyDir, so copies are peers and
    // slaves of it.
    let ok = |cmd: &mut Command| {
        let out = cmd.output().unwrap();
        assert!(out.status.success(), "{cmd:?}: {}", text(&out.stderr));
    };
    ok(Command::new("mount").arg("--make-shared").arg(&mnt));
    ok(Command::new("mount").arg("--bind").arg(&mnt).arg(&subpath));
    let mut holder = Command::new("unshare")
        .args(["-m", "--propagation", "slave", "sleep", "600"])
        .spawn()
        .unwrap();
    let holder_pid = holder.id().to_string();
    v.sh("printf before > before");
    v.expect(&tree(&[("before", f("before"))]));
    crash(&mut first);
    assert_eq!(
        rustix::fs::stat(&subpath).unwrap_err(),
        rustix::io::Errno::NOTCONN
    );

    // Refused without CAP_SYS_ADMIN, and for fronts; the stale mount is
    // left as it is.
    let out = without_sys_admin(v.mount_command(&["--on-stale-mount", "recover"]))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1), "{}", text(&out.stderr));
    assert!(
        text(&out.stderr).contains("CAP_SYS_ADMIN"),
        "{}",
        text(&out.stderr)
    );
    let out = v
        .mount_command(&[
            "--on-stale-mount",
            "recover",
            "--history-service",
            "unix:/nonexistent.sock",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1), "{}", text(&out.stderr));
    assert!(
        text(&out.stderr).contains("fronts"),
        "{}",
        text(&out.stderr)
    );
    assert_eq!(layers(&mnt), 1);

    // Recover: stacked over the stale mount, it serves every copy.
    let mut second = v.mount(&["--on-stale-mount", "recover"]);
    let log = second.log();
    assert!(
        log.contains(&format!(
            "stacked over a stale mount left by a crashed daemon at {mnt_s} (1 stale layer(s) below)"
        )),
        "{log}"
    );
    assert_eq!(layers(&mnt), 2);
    let out = v.agentfs(&["probe", &mnt_s]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stdout));
    assert_eq!(fs::read_to_string(mnt.join("before")).unwrap(), "before");
    v.sh("printf after > after");
    v.expect(&tree(&[("after", f("after")), ("before", f("before"))]));
    assert_eq!(fs::read_to_string(subpath.join("after")).unwrap(), "after");
    fs::write(subpath.join("via-subpath"), "s").unwrap();
    let out = Command::new("nsenter")
        .args(["-m", "-t", &holder_pid, "cat"])
        .arg(mnt.join("after"))
        .output()
        .unwrap();
    assert_eq!(text(&out.stdout), "after", "{}", text(&out.stderr));
    let out = Command::new("nsenter")
        .args(["-m", "-t", &holder_pid, "sh", "-c"])
        .arg(format!("printf ns > {mnt_s}/via-namespace"))
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", text(&out.stderr));
    v.expect(&tree(&[
        ("after", f("after")),
        ("before", f("before")),
        ("via-namespace", f("ns")),
        ("via-subpath", f("s")),
    ]));

    // A second crash leaves two stale layers; a hold detaches both.
    crash(&mut second);
    assert_eq!(layers(&mnt), 2);
    let child = v
        .mount_command(&["--on-stale-mount", "hold"])
        .spawn()
        .unwrap();
    let mut held = Mounted {
        child: Some(child),
        mnt: mnt.clone(),
        log: v.log_path(),
    };
    let deadline = Instant::now() + Duration::from_secs(20);
    while !held.log().contains("Holding until SIGTERM") {
        assert!(
            Instant::now() < deadline,
            "never held; log:\n{}",
            held.log()
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    held.signal(rustix::process::Signal::TERM);
    assert_eq!(held.wait_exit(Duration::from_secs(20)).code(), Some(1));
    assert!(
        held.log()
            .contains(&format!("detached stale mount {mnt_s} (2 layer(s))")),
        "{}",
        held.log()
    );
    assert_eq!(layers(&mnt), 0, "{}", held.log());

    // Recover again, stop: the stop detaches the stale layer under it.
    let mut third = v.mount(&[]);
    crash(&mut third);
    let fourth = v.mount(&["--on-stale-mount", "recover"]);
    v.sh("printf last > last");
    let log = fourth.stop();
    assert!(
        log.contains(&format!("detached 1 stale layer(s) at {mnt_s}")),
        "{log}"
    );
    assert_eq!(layers(&mnt), 0, "{log}");

    let _ = holder.kill();
    let _ = holder.wait();
    let _ = Command::new("umount").arg("-l").arg(&subpath).status();
    let _ = Command::new("umount").arg("-l").arg(&subpath).status();
}

/// With nothing stale, `recover` is a normal mount; files left in the bare
/// mount point are warned about (design §17.1).
#[test]
#[ignore = "needs /dev/fuse and root: ci/fuse-tests.sh"]
fn recover_on_a_healthy_mount_point_is_a_normal_mount() {
    let v = Vol::new("recover_healthy");
    fs::write(v.mnt().join("left-behind"), "x").unwrap();
    let m = v.mount(&["--on-stale-mount", "recover"]);
    let log = m.log();
    assert!(!log.contains("stacked over"), "{log}");
    assert!(log.contains("is not empty"), "{log}");
    assert_eq!(layers(&v.mnt()), 1);
    m.stop();
    fs::remove_file(v.mnt().join("left-behind")).unwrap();
}

#[test]
#[ignore = "needs /dev/fuse and root: ci/fuse-tests.sh"]
fn mount_creates_a_missing_mount_point() {
    let v = Vol::new("create_mnt");
    fs::remove_dir(v.mnt()).unwrap();
    let m = v.mount(&[]);
    assert!(m.log().contains("created the mount point"), "{}", m.log());
    v.sh("printf x > x");
    v.expect(&tree(&[("x", f("x"))]));
    m.stop();
    assert!(v.mnt().is_dir());
}

/// `--seal-mountpoint` (design §17.1): the bare mount point is root's with
/// mode 0000, so between mounts a non-root writer is refused; the mount on
/// top is unaffected, and a sealed mount point mounts again.
#[test]
#[ignore = "needs /dev/fuse and root: ci/fuse-tests.sh"]
fn a_sealed_mount_point_refuses_writes_between_mounts() {
    use std::os::unix::fs::MetadataExt as _;

    let v = Vol::new("seal");
    let m = v.mount(&["--seal-mountpoint"]);
    v.sh("printf x > x");
    v.expect(&tree(&[("x", f("x"))]));
    m.stop();
    let meta = fs::metadata(v.mnt()).unwrap();
    assert_eq!((meta.mode() & 0o7777, meta.uid()), (0, 0));
    let probe = "if printf y > y; then echo wrote; else echo refused; fi 2>/dev/null";
    assert_eq!(v.sh_as(Some(10000), probe, false), "refused\n");
    assert!(!v.mnt().join("y").exists());
    let m = v.mount(&["--seal-mountpoint"]);
    assert_eq!(v.sh("cat x"), "x");
    m.stop();
}

// ---- design §11.2: the final push at shutdown ----

/// Stopping `agentfs mount` pushes what the push interval had not yet sent
/// (design §11.2), here a whole hour of it.
#[test]
#[ignore = "needs /dev/fuse and root: ci/fuse-tests.sh"]
fn mount_pushes_its_final_commit_at_shutdown() {
    let v = Vol::new("final_push");
    let remote = v.p("remote.git");
    git2::Repository::init_bare(&remote).unwrap();
    v.config("remote.origin.url", &format!("file://{}", remote.display()));
    v.config("agentfs.pushInterval", "1h");
    let m = v.mount(&["--push-grace", "30s"]);
    v.sh("printf late > late.txt");
    v.expect(&tree(&[("late.txt", f("late"))]));
    let remote_head = || {
        git2::Repository::open_bare(&remote)
            .unwrap()
            .refname_to_id("refs/heads/agentfs/demo")
            .ok()
            .map(|id| id.to_string())
    };
    // Committed, but the next push is an hour away.
    assert_eq!(remote_head(), None);
    let log = m.stop();
    let head = v.head_id();
    assert!(
        log.contains(&format!("final push: origin: pushed none..{head}")),
        "{log}"
    );
    assert_eq!(remote_head(), Some(head));
}

// ---- design §18 test 15: the patch queue at mount (design §12) ----

/// Queues patch `n` in the volume's own repository: `parent` with `path`
/// set to `content`.
fn queue_patch(v: &Vol, n: u64, parent: &str, path: &str, content: &str) {
    let repo = git2::Repository::open_bare(v.state().join("git")).unwrap();
    let parent = repo
        .find_commit(git2::Oid::from_str(parent).unwrap())
        .unwrap();
    let mut index = git2::Index::new().unwrap();
    index.read_tree(&parent.tree().unwrap()).unwrap();
    let blob = repo.blob(content.as_bytes()).unwrap();
    index
        .add(&git2::IndexEntry {
            ctime: git2::IndexTime::new(0, 0),
            mtime: git2::IndexTime::new(0, 0),
            dev: 0,
            ino: 0,
            mode: 0o100644,
            uid: 0,
            gid: 0,
            file_size: 0,
            id: blob,
            flags: 0,
            flags_extended: 0,
            path: path.as_bytes().to_vec(),
        })
        .unwrap();
    let tree = repo.find_tree(index.write_tree_to(&repo).unwrap()).unwrap();
    let sig = git2::Signature::now("Ada", "ada@example.com").unwrap();
    let oid = repo
        .commit(None, &sig, &sig, &format!("patch {n}"), &tree, &[&parent])
        .unwrap();
    repo.reference(
        &format!("refs/agentfs/demo/inbox/{n:010}"),
        oid,
        true,
        "test",
    )
    .unwrap();
}

#[test]
#[ignore = "needs /dev/fuse and root: ci/fuse-tests.sh"]
fn patches_apply_before_the_mount_serves() {
    let v = Vol::new("patches_apply_before_the_mount_serves");
    let m = v.mount(&[]);
    v.sh("printf a0 > a.txt && printf b0 > b.txt");
    v.expect(&tree(&[("a.txt", f("a0")), ("b.txt", f("b0"))]));
    let base = v.head_id();
    v.sh("printf workload > b.txt");
    v.expect(&tree(&[("a.txt", f("a0")), ("b.txt", f("workload"))]));
    m.stop();
    queue_patch(&v, 1, &base, "a.txt", "a1");
    queue_patch(&v, 2, &base, "b.txt", "iac");
    queue_patch(&v, 3, &base, "c.txt", "c");

    // Patches 1 and 3 are there from the first read; 2 conflicts and is
    // held whole (bead agentfs-mia.2); the volume mounts anyway.
    let m = v.mount(&[]);
    assert_eq!(fs::read_to_string(v.mnt().join("a.txt")).unwrap(), "a1");
    assert_eq!(
        fs::read_to_string(v.mnt().join("b.txt")).unwrap(),
        "workload"
    );
    assert_eq!(fs::read_to_string(v.mnt().join("c.txt")).unwrap(), "c");
    let (author, message) = v.head();
    assert_eq!(author, "Ada");
    assert!(message.contains("Agentfs-Inbox: 3\n"), "{message}");
    let log = m.log();
    assert!(log.contains("patch 2 held (conflict)"), "{log}");
    assert!(log.contains("alarm: "), "{log}");
    let status = v.ok(&["patch", "status", "--json"]);
    let doc: serde_json::Value = serde_json::from_str(&status).unwrap();
    assert_eq!(doc["status"]["blocked"], serde_json::Value::Null);
    let queue = doc["status"]["queue"].as_array().unwrap();
    assert_eq!(queue.len(), 1);
    assert_eq!(queue[0]["verdict"]["reason"], "conflict");
    // The workload goes on.
    v.sh("printf more > d.txt");
    v.expect(&tree(&[
        ("a.txt", f("a1")),
        ("b.txt", f("workload")),
        ("c.txt", f("c")),
        ("d.txt", f("more")),
    ]));
    m.stop();
}

// ---- design §14: two fronts, one history service ----

/// Starts `agentfs serve-history` for `v`, logging to its own log, and waits
/// for it to serve.
fn start_history_service(v: &Vol) -> Mounted {
    // A log of its own, apart from the fronts'.
    let path = log_dir().join(format!("{}-service.log", v.name));
    let mut log = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .unwrap();
    writeln!(log, "---- serve-history").unwrap();
    let before = fs::read_to_string(&path)
        .unwrap_or_default()
        .matches("serving history for volume")
        .count();
    let child = v
        .command()
        .arg("--root")
        .arg(v.backing())
        .arg("serve-history")
        .env("AGENTFS_REPLICA", "history-0")
        .stdout(log.try_clone().unwrap())
        .stderr(log)
        .stdin(Stdio::null())
        .spawn()
        .unwrap();
    // Not a mount: `mnt` is never mounted, so Drop unmounts nothing.
    let service = Mounted {
        child: Some(child),
        mnt: v.p("no-mount"),
        log: path,
    };
    let deadline = Instant::now() + Duration::from_secs(20);
    while service.log().matches("serving history for volume").count() <= before {
        assert!(
            Instant::now() < deadline,
            "never served; log:\n{}",
            service.log()
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    service
}

/// Starts a front mount of `v`'s backing tree at `mnt` as pod `pod`.
fn start_front(v: &Vol, mnt: &Path, pod: &str) -> Mounted {
    // A log of its own: its alarms are its own.
    let path = log_dir().join(format!("{}-{pod}.log", v.name));
    let socket = v.state().join("meta/history/history.sock");
    let mut log = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .unwrap();
    writeln!(log, "---- front {pod}").unwrap();
    let before = startup_lines(&fs::read_to_string(&path).unwrap_or_default());
    let child = v
        .command()
        .args([
            "mount",
            "--passthrough",
            &passthrough(),
            "--unmount-grace",
            "3s",
        ])
        .arg("--history-service")
        .arg(&socket)
        .args(["--history-timeout", "3s", "--history-wait", "10s"])
        .arg(v.backing())
        .arg(mnt)
        .env("AGENTFS_REPLICA", pod)
        .env("AGENTFS_POD", pod)
        .env("AGENTFS_POD_UID", format!("{pod}-uid"))
        .env("AGENTFS_NAMESPACE", "agents")
        .stdout(log.try_clone().unwrap())
        .stderr(log)
        .stdin(Stdio::null())
        .spawn()
        .unwrap();
    let front = Mounted {
        child: Some(child),
        mnt: mnt.to_owned(),
        log: path,
    };
    front.wait_mounted(before);
    front
}

/// Writes `content` to `path` and returns what close(2) said.
fn write_and_close(path: &Path, content: &str) -> std::io::Result<()> {
    use std::os::fd::IntoRawFd as _;
    let mut file = fs::File::create(path)?;
    file.write_all(content.as_bytes())?;
    let fd = file.into_raw_fd();
    // SAFETY: `fd` came from `into_raw_fd`, so this is its only owner.
    if unsafe { libc::close(fd) } == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

/// Every commit with a pod trailer, oldest first: committer and pod.
fn pod_commits(v: &Vol) -> Vec<(String, String)> {
    let repo = git2::Repository::open_bare(v.state().join("git")).unwrap();
    let mut walk = repo.revwalk().unwrap();
    walk.push_ref("refs/heads/agentfs/demo").unwrap();
    walk.set_sorting(git2::Sort::TOPOLOGICAL | git2::Sort::REVERSE)
        .unwrap();
    walk.filter_map(|id| {
        let commit = repo.find_commit(id.unwrap()).unwrap();
        let pod = commit
            .message()
            .unwrap_or("")
            .lines()
            .find_map(|l| l.strip_prefix("Agentfs-Pod: "))?
            .to_owned();
        Some((commit.committer().name().unwrap_or("").to_owned(), pod))
    })
    .collect()
}

#[test]
#[ignore = "needs /dev/fuse and root: ci/fuse-tests.sh"]
fn two_fronts_one_service() {
    let v = Vol::new("two-fronts");
    // The fronts' repairs are retried at each sweep.
    v.config("agentfs.sweepInterval", "1s");
    let mnt2 = v.p("mnt2");
    fs::create_dir_all(&mnt2).unwrap();
    fs::set_permissions(&mnt2, fs::Permissions::from_mode(0o755)).unwrap();

    // No service: a front exits without mounting.
    let out = v
        .command()
        .args(["mount", "--history-service"])
        .arg(v.state().join("meta/history/history.sock"))
        .args(["--history-wait", "1s"])
        .arg(v.backing())
        .arg(v.mnt())
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1), "{}", text(&out.stderr));
    assert!(!is_mounted(&v.mnt()));

    let service = start_history_service(&v);
    let a = start_front(&v, &v.mnt(), "pod-a");
    let b = start_front(&v, &mnt2, "pod-b");
    write_and_close(&v.mnt().join("from-a.txt"), "a").unwrap();
    write_and_close(&mnt2.join("from-b.txt"), "b").unwrap();
    // One backing tree: each front sees the other's file.
    assert_eq!(fs::read_to_string(mnt2.join("from-a.txt")).unwrap(), "a");
    assert_eq!(fs::read_to_string(v.mnt().join("from-b.txt")).unwrap(), "b");
    v.expect(&tree(&[("from-a.txt", f("a")), ("from-b.txt", f("b"))]));
    let status = v.ok(&["status"]);
    assert!(status.contains("history:   serving fronts at"), "{status}");
    assert!(status.contains("command=mount pod=pod-a"), "{status}");
    assert!(status.contains("command=mount pod=pod-b"), "{status}");

    // A file open in A that B renames, or removes: A's close records it
    // under its current name, or nowhere (design §14).
    {
        write_and_close(&v.mnt().join("app.log"), "1").unwrap();
        write_and_close(&v.mnt().join("gone.log"), "1").unwrap();
        let mut moved = fs::OpenOptions::new()
            .append(true)
            .open(v.mnt().join("app.log"))
            .unwrap();
        let mut removed = fs::OpenOptions::new()
            .append(true)
            .open(v.mnt().join("gone.log"))
            .unwrap();
        fs::rename(mnt2.join("app.log"), mnt2.join("app.log.1")).unwrap();
        fs::remove_file(mnt2.join("gone.log")).unwrap();
        moved.write_all(b"2").unwrap();
        removed.write_all(b"2").unwrap();
        drop(moved);
        drop(removed);
        v.expect(&tree(&[
            ("from-a.txt", f("a")),
            ("from-b.txt", f("b")),
            ("app.log.1", f("12")),
        ]));
        fs::remove_file(mnt2.join("app.log.1")).unwrap();
    }

    // The service down: the boundary fails with EIO and an alarm.
    let mut service = service;
    service.signal(rustix::process::Signal::KILL);
    service.wait_exit(Duration::from_secs(10));
    let err = write_and_close(&v.mnt().join("lost.txt"), "x").unwrap_err();
    assert_eq!(err.raw_os_error(), Some(libc::EIO), "{err}");
    // A rename is a boundary too: EIO, though the tree has the new name.
    let err = fs::rename(v.mnt().join("from-a.txt"), v.mnt().join("moved.txt")).unwrap_err();
    assert_eq!(err.raw_os_error(), Some(libc::EIO), "{err}");
    assert!(v.backing().join("moved.txt").exists());
    // Front A's own log: the alarm is A's, not the service's or B's.
    let log = a.log();
    assert!(log.contains("alarm: "), "{log}");
    assert!(log.contains("is unavailable"), "{log}");

    // Back up: the fronts carry on without a restart.
    let service = start_history_service(&v);
    write_and_close(&mnt2.join("after.txt"), "after").unwrap();
    // A's repairs (the failed close and rename) reach history at its next
    // sweep: HEAD follows the tree again.
    let want = tree(&[
        ("from-b.txt", f("b")),
        ("lost.txt", f("x")),
        ("moved.txt", f("a")),
        ("after.txt", f("after")),
    ]);
    let deadline = Instant::now() + Duration::from_secs(30);
    while v.history() != want {
        assert!(
            Instant::now() < deadline,
            "history never caught up: {:?}\nfront a:\n{}",
            v.history(),
            a.log()
        );
        std::thread::sleep(Duration::from_millis(100));
    }

    a.stop();
    b.stop();
    service.stop();
    let commits = pod_commits(&v);
    assert!(
        commits.contains(&("pod-a".to_owned(), "pod-a".to_owned())),
        "{commits:?}"
    );
    assert!(
        commits.contains(&("pod-b".to_owned(), "pod-b".to_owned())),
        "{commits:?}"
    );
    assert!(
        commits.iter().all(|(c, p)| c == p),
        "a commit mixes pods: {commits:?}"
    );
}

/// `restore` and `revert` while the history service serves two fronts
/// (design §15): the service settles both, writes the backing tree, and
/// each front's kernel drops what it cached, so both mounts show the
/// restored content at once.
#[test]
#[ignore = "needs /dev/fuse and root: ci/fuse-tests.sh"]
fn restores_through_the_service_reach_every_front() {
    let v = Vol::new("live-restore");
    // Only a close or a settle records a write; the sweep never runs.
    v.config("agentfs.sweepInterval", "1h");
    let mnt2 = v.p("mnt2");
    fs::create_dir_all(&mnt2).unwrap();
    fs::set_permissions(&mnt2, fs::Permissions::from_mode(0o755)).unwrap();
    let service = start_history_service(&v);
    let a = start_front(&v, &v.mnt(), "pod-a");
    let b = start_front(&v, &mnt2, "pod-b");
    let read = |mnt: &Path, p: &str| fs::read_to_string(mnt.join(p)).ok();

    write_and_close(&v.mnt().join("f.txt"), "v1").unwrap();
    write_and_close(&v.mnt().join("g.txt"), "g").unwrap();
    v.expect(&tree(&[("f.txt", f("v1")), ("g.txt", f("g"))]));
    write_and_close(&mnt2.join("f.txt"), "v2-longer").unwrap();
    fs::remove_file(v.mnt().join("g.txt")).unwrap();
    v.expect(&tree(&[("f.txt", f("v2-longer"))]));
    // Both kernels cache f.txt's content and attributes, and g.txt's
    // absence.
    for mnt in [v.mnt(), mnt2.clone()] {
        assert_eq!(read(&mnt, "f.txt").as_deref(), Some("v2-longer"));
        assert_eq!(read(&mnt, "g.txt"), None);
    }

    // HEAD~2 still had g.txt and f.txt = v1.
    let out = v.ok(&["restore", ".", "HEAD~2"]);
    assert!(out.contains("restored 2 path(s)"), "{out}");
    assert_eq!(fs::read_to_string(v.backing().join("f.txt")).unwrap(), "v1");
    for mnt in [v.mnt(), mnt2.clone()] {
        assert_eq!(
            read(&mnt, "f.txt").as_deref(),
            Some("v1"),
            "{}",
            mnt.display()
        );
        assert_eq!(
            read(&mnt, "g.txt").as_deref(),
            Some("g"),
            "{}",
            mnt.display()
        );
    }
    assert!(
        v.head().1.contains("Agentfs-Trigger: restore"),
        "{}",
        v.head().1
    );
    v.expect(&tree(&[("f.txt", f("v1")), ("g.txt", f("g"))]));

    // Undone by a revert: both fronts again.
    v.ok(&["revert", "HEAD"]);
    for mnt in [v.mnt(), mnt2.clone()] {
        assert_eq!(read(&mnt, "f.txt").as_deref(), Some("v2-longer"));
        assert_eq!(read(&mnt, "g.txt"), None);
    }
    v.expect(&tree(&[("f.txt", f("v2-longer"))]));

    // A file open for writing through a front: refused, unless --force.
    let held = fs::OpenOptions::new()
        .append(true)
        .open(mnt2.join("f.txt"))
        .unwrap();
    let out = v.agentfs(&["restore", "f.txt", "HEAD~1"]);
    assert_eq!(out.status.code(), Some(1), "{}", text(&out.stderr));
    assert!(
        text(&out.stderr).contains("open for writing"),
        "{}",
        text(&out.stderr)
    );
    drop(held);
    // The front's own writes go on being recorded after a restore.
    v.ok(&["restore", "f.txt", "HEAD~1"]);
    assert_eq!(read(&mnt2, "f.txt").as_deref(), Some("v1"));
    write_and_close(&mnt2.join("f.txt"), "after").unwrap();
    v.expect(&tree(&[("f.txt", f("after"))]));

    // Fronts settle first (design §15): a write through a front that is
    // not closed yet, which only its sweep would record (an hour away
    // here), is recorded and committed before the restore's commit.
    write_and_close(&mnt2.join("k.txt"), "k1").unwrap();
    v.expect(&tree(&[("f.txt", f("after")), ("k.txt", f("k1"))]));
    let k1 = v.head_id();
    write_and_close(&mnt2.join("k.txt"), "k2").unwrap();
    v.expect(&tree(&[("f.txt", f("after")), ("k.txt", f("k2"))]));
    let mut armed = fs::File::create(v.mnt().join("h.txt")).unwrap();
    armed.write_all(b"armed").unwrap();
    let out = v.ok(&["restore", "k.txt", &k1]);
    assert!(out.contains("restored 1 path(s)"), "{out}");
    let repo = git2::Repository::open_bare(v.state().join("git")).unwrap();
    let head = repo
        .find_reference("refs/heads/agentfs/demo")
        .unwrap()
        .peel_to_commit()
        .unwrap();
    assert!(
        head.message()
            .unwrap_or("")
            .contains("Agentfs-Trigger: restore"),
        "{:?}",
        head.message()
    );
    let before = head.parent(0).unwrap().tree().unwrap();
    let entry = before.get_path(Path::new("h.txt")).unwrap();
    assert_eq!(
        repo.find_blob(entry.id()).unwrap().content(),
        b"armed",
        "the open file's write is committed before the restore"
    );
    drop(armed);

    a.stop();
    b.stop();
    service.stop();
}

// ---- design §3.1: one record per close, directory moves ----

#[test]
#[ignore = "needs /dev/fuse and root: ci/fuse-tests.sh"]
fn a_close_is_recorded_once() {
    // A file written and closed within one timestamp tick is recorded at
    // the close, racily; its RELEASE finds the same bytes and records
    // nothing more (design §3.1, §19.2 item 4).
    let v = Vol::new("one_record");
    v.config("agentfs.commitWindow", "10m");
    let src = v.p("src");
    fs::create_dir_all(&src).unwrap();
    let mut want = Vec::new();
    for i in 0..20 {
        fs::write(src.join(format!("f{i}")), format!("content {i}")).unwrap();
        want.push((format!("f{i}"), f(&format!("content {i}"))));
    }
    let m = v.mount(&[]);
    // `tee` opens with O_TRUNC, writes and closes once: one FLUSH, one
    // RELEASE. (`dd` truncates with ftruncate, a record of its own.)
    v.sh(&format!(
        "for i in $(seq 0 19); do tee f$i < {}/f$i > /dev/null; done",
        src.display()
    ));
    // Every RELEASE has arrived (they follow the closes asynchronously).
    std::thread::sleep(Duration::from_secs(2));
    let log = m.stop();
    let line = log
        .lines()
        .find(|l| l.starts_with("final commit: "))
        .unwrap_or_else(|| panic!("{log}"));
    assert!(
        line.starts_with("final commit: 20 record(s)"),
        "one record per file: {line}\n{log}"
    );
    let want: Vec<(&str, Item)> = want.iter().map(|(p, i)| (p.as_str(), i.clone())).collect();
    v.expect(&tree(&want));
}

#[test]
#[ignore = "needs /dev/fuse and root: ci/fuse-tests.sh"]
fn directory_renames_are_one_move() {
    let v = Vol::new("mvdir");
    let exclude = v.state().join("git/info/exclude");
    fs::create_dir_all(exclude.parent().unwrap()).unwrap();
    fs::write(&exclude, "ignored/\n").unwrap();
    let m = v.mount(&[]);
    v.sh("mkdir -p d/e ignored && printf a > d/a && printf b > d/e/b && printf i > ignored/i");
    v.expect(&tree(&[("d/a", f("a")), ("d/e/b", f("b"))]));
    v.sh("mv d m");
    v.expect(&tree(&[("m/a", f("a")), ("m/e/b", f("b"))]));
    let (_, message) = v.head();
    assert!(message.starts_with("rename: d -> m\n"), "{message}");
    // Into an ignored directory: dropped. Out of one: recorded.
    v.sh("mv m ignored/m");
    v.expect(&tree(&[]));
    v.sh("mv ignored kept");
    v.expect(&tree(&[
        ("kept/i", f("i")),
        ("kept/m/a", f("a")),
        ("kept/m/e/b", f("b")),
    ]));
    // A file held open across its directory's rename is recorded at the
    // new path when it is closed.
    v.sh("exec 3>>kept/m/a; mv kept k2; printf 2 >&3; exec 3>&-");
    v.expect(&tree(&[
        ("k2/i", f("i")),
        ("k2/m/a", f("a2")),
        ("k2/m/e/b", f("b")),
    ]));
    m.stop();
}

// ---- design §5: absolute gitdir targets ----

#[test]
#[ignore = "needs /dev/fuse and root: ci/fuse-tests.sh"]
fn absolute_gitdirs_map_exactly_through_the_views() {
    let v = Vol::new("views");
    let m = v.mount(&["--workload-mount", "/workspace"]);
    let script = "set -e
        git init -q main
        cd main
        git -c user.name=t -c user.email=t@example.com commit -q --allow-empty -m one
        git worktree add -q ../wt
        cd ..
        printf w > wt/f
        mkdir -p seen elsewhere
        printf 'gitdir: /workspace/main/.git/worktrees/wt\\n' > seen/.git
        printf s > seen/f
        printf 'gitdir: /workspace/x/main/.git/worktrees/wt\\n' > elsewhere/.git
        printf e > elsewhere/f
        head -c 7 wt/.git";
    assert_eq!(v.sh(script), "gitdir:");
    // `wt` (gitdir under the mount point) and `seen` (under the workload
    // mount) are worktrees of `main`: nested. `elsewhere` names a path
    // under /workspace that is no git directory: not nested, although a
    // suffix of its target is one.
    v.expect(&tree(&[("elsewhere/f", f("e"))]));
    m.stop();
}

/// The head commit's committer and author, each `name <email>`.
fn head_identities(v: &Vol) -> (String, String) {
    let repo = git2::Repository::open_bare(v.state().join("git")).unwrap();
    let commit = repo
        .find_reference("refs/heads/agentfs/demo")
        .unwrap()
        .peel_to_commit()
        .unwrap();
    let sig =
        |s: git2::Signature| format!("{} <{}>", s.name().unwrap_or(""), s.email().unwrap_or(""));
    (sig(commit.committer()), sig(commit.author()))
}

/// Design §9 and §18 test 10: custom identity templates through a
/// single-process mount and through a front of a history service, with
/// a uid 10000 writer: the committer is the mount's (or the front's)
/// pod, the author the writer's uid and gid.
#[test]
#[ignore = "needs /dev/fuse and root: ci/fuse-tests.sh"]
fn identity_templates_through_mount() {
    let v = Vol::new("identity");
    v.config("agentfs.identity.committerName", "{pod} in {namespace}");
    v.config(
        "agentfs.identity.committerEmail",
        "{podUid}@{volume}.mount.invalid",
    );
    v.config("agentfs.identity.authorName", "writer {uid}:{gid}");
    v.config(
        "agentfs.identity.authorEmail",
        "uid-{uid}.gid-{gid}@{pod}.invalid",
    );

    let m = v.mount_with(&[], |mut c| {
        c.env("AGENTFS_POD", "solo")
            .env("AGENTFS_POD_UID", "solo-uid")
            .env("AGENTFS_NAMESPACE", "ns-1");
        c
    });
    v.sh("mkdir u && chown 10000:10000 u");
    v.sh_as(Some(UID), "printf 1 > u/solo.txt", true);
    v.expect(&tree(&[("u/solo.txt", f("1"))]));
    assert_eq!(
        head_identities(&v),
        (
            "solo in ns-1 <solo-uid@demo.mount.invalid>".to_owned(),
            "writer 10000:10000 <uid-10000.gid-10000@solo.invalid>".to_owned()
        )
    );
    m.stop();

    let service = start_history_service(&v);
    let front = start_front(&v, &v.mnt(), "pod-f");
    v.sh_as(Some(UID), "printf 2 > u/front.txt", true);
    v.expect(&tree(&[("u/solo.txt", f("1")), ("u/front.txt", f("2"))]));
    assert_eq!(
        head_identities(&v),
        (
            "pod-f in agents <pod-f-uid@demo.mount.invalid>".to_owned(),
            "writer 10000:10000 <uid-10000.gid-10000@pod-f.invalid>".to_owned()
        )
    );
    front.stop();
    service.stop();
}
