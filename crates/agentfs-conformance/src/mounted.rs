//! The shared calls on a real mount: `open(2)`, `pwrite(2)`, `ftruncate(2)`,
//! `close(2)`, `rename(2)` and `unlink(2)` against one process serving the root
//! and at most one sandbox. Linux only, with `/dev/fuse` and `fusermount3`.
//!
//! The process is a failpoints build with `AGENTFS_TAKE_GATE` set: it takes
//! nothing by itself. Each take runs at the trace's point and answers on a fifo,
//! so a take crosses FUSE at that point and needs no poll. Cost: this replay never
//! exercises automatic takes (R1); the mounted P1 and P2 cases cover those.

use crate::engine_names::EngineNames;
use crate::library::{fail, seed};
use agentfs_core::tree_actor::TreeId as EngineTree;
use agentfs_model::calls::{CallError, Caller, Took, TreeCalls, TreeId};
use std::collections::{BTreeMap, VecDeque};
use std::ffi::CString;
use std::fs::{File, OpenOptions};
use std::io::{self, BufRead, BufReader, Write};
use std::os::fd::RawFd;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// How long a mount may take to appear. A mount has no event of its own to wait on.
const MOUNT_WAITS: Duration = Duration::from_secs(30);

fn io_fail(what: &str, e: &io::Error) -> CallError {
    CallError {
        errno: e.raw_os_error(),
        message: format!("{what}: {e}"),
    }
}

fn last_error(what: &str) -> CallError {
    io_fail(what, &io::Error::last_os_error())
}

fn c_path(p: &Path) -> Result<CString, CallError> {
    CString::new(p.as_os_str().as_bytes()).map_err(|e| CallError::new(e.to_string()))
}

/// Discards the events: this harness consumes none.
struct NoConsumers;

impl agentfs_contracts::ports::EventSink for NoConsumers {
    fn publish(&mut self, _: agentfs_contracts::ports::Event) {}
}

/// The sandbox fork's key: `mount` finds the fork under it at every mount,
/// the remount after a crash included.
const FORK_KEY: &str = "conformance";

/// The sandbox's fork, made as the trial CLI makes one: through the Trees
/// domain on the store adapter, under [`FORK_KEY`], so the fork's row is on
/// its chain and the mount finds it.
fn sandbox_fork(
    recording: &std::sync::Arc<agentfs_core::tree_actor::Recording>,
) -> Result<EngineTree, CallError> {
    use agentfs_contracts::caps::{LaunchCap, LaunchScope, Minter};
    use agentfs_contracts::name::{ForkKey, LauncherId};
    use agentfs_contracts::trees::{ForkSettings, Landing, Trees};
    let store = agentfs_core::store::TreeStore::new(std::sync::Arc::clone(recording));
    let mut trees = agentfs_trees::TreesService::restore(store.clone(), store, NoConsumers)
        .map_err(|e| CallError::new(e.to_string()))?;
    #[allow(
        clippy::disallowed_methods,
        reason = "the harness is its own composition root"
    )]
    let minter = Minter::new();
    let launcher = LauncherId::parse("conformance").map_err(|e| CallError::new(e.to_string()))?;
    let cap = LaunchCap::mint(&minter, launcher, LaunchScope::Root);
    let settings = ForkSettings {
        landing: Landing::Automatic,
    };
    let key = ForkKey::parse(FORK_KEY).map_err(|e| CallError::new(e.to_string()))?;
    let fork = trees
        .create(EngineTree::Root, Some(key), settings, &cap)
        .map_err(|e| CallError::new(e.to_string()))?;
    Ok(fork.into())
}

/// A mount of the root and at most one sandbox, and the opens this harness holds.
pub struct Mounted {
    bin: PathBuf,
    state: PathBuf,
    root: PathBuf,
    sandbox: PathBuf,
    gate: PathBuf,
    /// The root's name, and the sandbox's fork when there is one.
    names: EngineNames,
    fork_used: bool,
    child: Option<Child>,
    go: File,
    took: BufReader<File>,
    log: PathBuf,
    opens: BTreeMap<RawFd, Caller>,
    appends: VecDeque<bool>,
}

impl Mounted {
    /// Seeds a volume under `work`, forks if the trace does (`mountable` puts that
    /// fork before any client call) and mounts it, its trees named by `names`.
    /// `appends`: for each open the trace makes, in order, whether it appends
    /// ([`crate::opens_that_append`]).
    pub fn new(
        bin: &Path,
        work: &Path,
        mut names: EngineNames,
        with_sandbox: bool,
        appends: Vec<bool>,
    ) -> Result<Self, CallError> {
        let (state, root, sandbox, gate) = (
            work.join("state"),
            work.join("root"),
            work.join("sandbox"),
            work.join("gate"),
        );
        for dir in [&root, &sandbox, &gate] {
            std::fs::create_dir_all(dir).map_err(|e| io_fail("mkdir", &e))?;
        }
        {
            let recording = agentfs_core::tree_actor::Recording::create(
                &state,
                &crate::library::identity(),
                &agentfs_contracts::pod::PodContext::default(),
            )
            .map_err(fail)?;
            seed(
                &mut *recording.actor().map_err(fail)?,
                names.name(TreeId::ROOT)?,
            )
            .map_err(fail)?;
            if with_sandbox {
                names.forked(sandbox_fork(&recording)?)?;
            }
            // Every record durable and built, and the lease free, at the drop.
        }
        for fifo in ["go", "took"] {
            let path = c_path(&gate.join(fifo))?;
            // SAFETY: `path` is a valid C string for the call's duration.
            if unsafe { libc::mkfifo(path.as_ptr(), 0o600) } != 0 {
                return Err(last_error("mkfifo"));
            }
        }
        // Read and write: neither side ever sees end of file, nor blocks opening.
        let fifo = |name: &str| {
            OpenOptions::new()
                .read(true)
                .write(true)
                .open(gate.join(name))
                .map_err(|e| io_fail("open fifo", &e))
        };
        let (go, took) = (fifo("go")?, BufReader::new(fifo("took")?));
        let mut mounted = Self {
            bin: bin.to_owned(),
            log: work.join("mount.log"),
            state,
            root,
            sandbox,
            gate,
            names,
            fork_used: false,
            child: None,
            go,
            took,
            opens: BTreeMap::new(),
            appends: appends.into(),
        };
        mounted.mount()?;
        Ok(mounted)
    }

    /// The bare repository the invariants read.
    pub fn repo(&self) -> PathBuf {
        self.state.join("history.git")
    }

    fn mountpoints(&self) -> Vec<PathBuf> {
        let mut points = vec![self.root.clone()];
        if self.sandbox_fork().is_some() {
            points.push(self.sandbox.clone());
        }
        points
    }

    fn mount(&mut self) -> Result<(), CallError> {
        let log = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.log)
            .map_err(|e| io_fail("mount log", &e))?;
        let mut command = Command::new(&self.bin);
        command
            .arg("--state")
            .arg(&self.state)
            .args(["trees", "mount"])
            .arg(&self.root)
            .env("AGENTFS_TAKE_GATE", &self.gate)
            .stdin(Stdio::null())
            .stdout(log.try_clone().map_err(|e| io_fail("mount log", &e))?)
            .stderr(log);
        if self.sandbox_fork().is_some() {
            command
                .arg("--fork-mount")
                .arg(&self.sandbox)
                .args(["--fork-key", FORK_KEY]);
        }
        self.child = Some(command.spawn().map_err(|e| io_fail("spawn", &e))?);
        let started = Instant::now();
        loop {
            let table =
                std::fs::read("/proc/self/mountinfo").map_err(|e| io_fail("mountinfo", &e))?;
            let text = String::from_utf8_lossy(&table);
            let mounted = |p: &PathBuf| {
                text.lines()
                    .any(|l| l.split(' ').nth(4) == Some(&*p.to_string_lossy()))
            };
            if self.mountpoints().iter().all(mounted) {
                return Ok(());
            }
            if started.elapsed() > MOUNT_WAITS {
                return Err(CallError::new(format!(
                    "the mount did not appear: {}",
                    self.log_text()
                )));
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// What the mount process printed.
    pub fn log_text(&self) -> String {
        std::fs::read_to_string(&self.log).unwrap_or_default()
    }

    /// The sandbox's fork, the one `mountable` allows, if `new` made it.
    fn sandbox_fork(&self) -> Option<EngineTree> {
        self.names.name(TreeId::new(1)).ok()
    }

    fn root(&self, t: TreeId) -> &Path {
        match t {
            TreeId::ROOT => &self.root,
            _ => &self.sandbox,
        }
    }

    fn at(&self, c: &Caller, path: &str) -> PathBuf {
        match c {
            Caller::Root(_) => self.root.join(path),
            Caller::Fork(_) => self.sandbox.join(path),
        }
    }

    fn open(&mut self, c: &Caller, path: &str, flags: i32) -> Result<RawFd, CallError> {
        let append = self
            .appends
            .pop_front()
            .ok_or_else(|| CallError::new("more opens than the trace counted"))?;
        let flags = flags | libc::O_RDWR | if append { libc::O_APPEND } else { 0 };
        let file = c_path(&self.at(c, path))?;
        // SAFETY: `file` is a valid C string; the mode argument is read for O_CREAT.
        let fd = unsafe { libc::open(file.as_ptr(), flags, 0o644 as libc::c_uint) };
        if fd < 0 {
            return Err(last_error("open"));
        }
        self.opens.insert(fd, c.clone());
        self.settle()?;
        Ok(fd)
    }

    /// `RELEASE` is a background request, sent in order before `close` returns.
    /// This mount's one serving thread answers in order, so a synchronous request
    /// answered means the release ran. Events only: no poll.
    fn released(&self) -> Result<(), CallError> {
        for root in self.mountpoints() {
            std::fs::metadata(&root).map_err(|e| io_fail("stat", &e))?;
        }
        Ok(())
    }

    fn close_fd(&mut self, fd: RawFd) -> Result<(), CallError> {
        self.opens.remove(&fd);
        // SAFETY: `fd` is an open this harness holds and closes once.
        if unsafe { libc::close(fd) } != 0 {
            return Err(last_error("close"));
        }
        self.released()?;
        self.settle()
    }

    /// Asks the gate to wait until history is built through every record so
    /// far: the invariants read git after each call, and the builder makes
    /// history behind the call path.
    fn settle(&mut self) -> Result<(), CallError> {
        writeln!(self.go, "settle").map_err(|e| io_fail("ask the gate", &e))?;
        let mut answer = String::new();
        self.took
            .read_line(&mut answer)
            .map_err(|e| io_fail("read the gate", &e))?;
        match answer.trim_end() {
            "settled" => Ok(()),
            other => Err(CallError::new(format!("the gate answered {other:?}"))),
        }
    }

    fn unmount(&self, lazy: bool) -> Result<(), CallError> {
        for point in self.mountpoints() {
            let mut command = Command::new("fusermount3");
            command.arg(if lazy { "-uz" } else { "-u" }).arg(&point);
            let status = command.status().map_err(|e| io_fail("fusermount3", &e))?;
            if !status.success() {
                return Err(CallError::new(format!("fusermount3 {status}")));
            }
        }
        Ok(())
    }

    /// Unmounts and lets the process finish. Every open must be closed by now, as a
    /// trace closes them; one left is closed here.
    pub fn finish(&mut self) -> Result<(), CallError> {
        let held: Vec<RawFd> = self.opens.keys().copied().collect();
        for fd in held {
            self.close_fd(fd)?;
        }
        self.unmount(false)?;
        // An empty line ends the gate (failpoints::TakeGate).
        self.go
            .write_all(b"\n")
            .map_err(|e| io_fail("end the gate", &e))?;
        if let Some(mut child) = self.child.take() {
            let status = child.wait().map_err(|e| io_fail("wait", &e))?;
            if !status.success() {
                return Err(CallError::new(format!(
                    "the mount exited {status}: {}",
                    self.log_text()
                )));
            }
        }
        Ok(())
    }
}

impl Drop for Mounted {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
            let _ = self.unmount(true);
        }
    }
}

impl TreeCalls for Mounted {
    type Open = RawFd;

    /// `new` made the one fork `mountable` allows.
    fn fork(&mut self) -> Result<TreeId, CallError> {
        if self.sandbox_fork().is_none() || self.fork_used {
            return Err(CallError::new("not a mountable trace"));
        }
        self.fork_used = true;
        Ok(TreeId::new(1))
    }

    fn open_write(&mut self, c: &Caller, path: &str) -> Result<RawFd, CallError> {
        self.open(c, path, 0)
    }

    fn open_trunc(&mut self, c: &Caller, path: &str) -> Result<RawFd, CallError> {
        self.open(c, path, libc::O_TRUNC)
    }

    fn create(&mut self, c: &Caller, path: &str) -> Result<RawFd, CallError> {
        self.open(c, path, libc::O_CREAT | libc::O_EXCL)
    }

    fn pwrite(&mut self, h: RawFd, offset: u64, bytes: &[u8]) -> Result<(), CallError> {
        let mut done = 0;
        while done < bytes.len() {
            // SAFETY: the pointer and length name the rest of `bytes`.
            let n = unsafe {
                libc::pwrite(
                    h,
                    bytes[done..].as_ptr().cast(),
                    bytes.len() - done,
                    (offset + done as u64) as libc::off_t,
                )
            };
            if n < 0 {
                return Err(last_error("pwrite"));
            }
            done += n as usize;
        }
        Ok(())
    }

    fn append(&mut self, h: RawFd, bytes: &[u8]) -> Result<(), CallError> {
        let mut done = 0;
        while done < bytes.len() {
            // SAFETY: the pointer and length name the rest of `bytes`.
            let n = unsafe { libc::write(h, bytes[done..].as_ptr().cast(), bytes.len() - done) };
            if n < 0 {
                return Err(last_error("write"));
            }
            done += n as usize;
        }
        Ok(())
    }

    fn truncate(&mut self, h: RawFd, size: u64) -> Result<(), CallError> {
        // SAFETY: a plain call on a descriptor this harness holds.
        if unsafe { libc::ftruncate(h, size as libc::off_t) } != 0 {
            return Err(last_error("ftruncate"));
        }
        Ok(())
    }

    fn close(&mut self, h: RawFd) -> Result<(), CallError> {
        self.close_fd(h)
    }

    fn unlink(&mut self, c: &Caller, path: &str) -> Result<(), CallError> {
        std::fs::remove_file(self.at(c, path)).map_err(|e| io_fail("unlink", &e))?;
        self.settle()
    }

    fn rename(&mut self, c: &Caller, from: &str, to: &str) -> Result<(), CallError> {
        std::fs::rename(self.at(c, from), self.at(c, to)).map_err(|e| io_fail("rename", &e))?;
        self.settle()
    }

    fn end_client(&mut self, c: &Caller) -> Result<(), CallError> {
        let mine: Vec<RawFd> = self
            .opens
            .iter()
            .filter(|(_, o)| *o == c)
            .map(|(fd, _)| *fd)
            .collect();
        for fd in mine {
            self.close_fd(fd)?;
        }
        Ok(())
    }

    fn take_next(&mut self, t: TreeId) -> Result<Took, CallError> {
        let tree = self.names.name(t)?;
        writeln!(self.go, "{tree}").map_err(|e| io_fail("ask the gate", &e))?;
        let mut answer = String::new();
        self.took
            .read_line(&mut answer)
            .map_err(|e| io_fail("read the gate", &e))?;
        let took = match answer.trim_end() {
            "taken" => Took::Taken,
            "waiting" => Took::Waiting,
            "empty" => Took::Empty,
            other => return Err(CallError::new(format!("the gate answered {other:?}"))),
        };
        self.settle()?;
        Ok(took)
    }

    /// `SIGKILL`, wait on the pid, drop what the harness holds, `fusermount3 -uz`,
    /// mount. The new process's recovery closes every open once.
    fn crash(&mut self) -> Result<(), CallError> {
        if let Some(mut child) = self.child.take() {
            child.kill().map_err(|e| io_fail("kill", &e))?;
            child.wait().map_err(|e| io_fail("wait", &e))?;
            // Its tree Git children share its lease and die of their
            // parent-death signal after it: taking the lease, blocking, is
            // the event that every holder is gone.
            agentfs_core::tree_actor::wait_for_lease(&self.state)
                .map_err(|e| io_fail("lease", &e))?;
        }
        for (fd, _) in std::mem::take(&mut self.opens) {
            // SAFETY: an open this harness holds; its connection is gone, so the
            // result says nothing.
            unsafe { libc::close(fd) };
        }
        self.unmount(true)?;
        self.mount()?;
        // Open never waits for history; the invariants read it after each call.
        self.settle()
    }

    fn read(&self, t: TreeId, path: &str) -> Result<Option<Vec<u8>>, CallError> {
        match std::fs::read(self.root(t).join(path)) {
            Ok(b) => Ok(Some(b)),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(io_fail("read", &e)),
        }
    }

    fn paths(&self, t: TreeId) -> Result<Vec<String>, CallError> {
        let mut paths = Vec::new();
        for entry in std::fs::read_dir(self.root(t)).map_err(|e| io_fail("readdir", &e))? {
            let entry = entry.map_err(|e| io_fail("readdir", &e))?;
            paths.push(entry.file_name().to_string_lossy().into_owned());
        }
        paths.sort();
        Ok(paths)
    }
}

impl crate::Objects for Mounted {
    /// Each file's inode: the view numbers a file's object `object + 2`.
    fn objects(&self, t: TreeId) -> Result<Vec<(String, u64)>, CallError> {
        use std::os::unix::fs::MetadataExt;
        let mut objects = Vec::new();
        for path in self.paths(t)? {
            let meta = std::fs::symlink_metadata(self.root(t).join(&path))
                .map_err(|e| io_fail("stat", &e))?;
            objects.push((path, meta.ino()));
        }
        Ok(objects)
    }
}
