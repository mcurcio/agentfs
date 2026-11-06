//! Git plumbing for tree actors (target architecture §§3, 12).
//!
//! The repository is in Git's files format, and its refs are a copy: the
//! actor's journal is the commit point and `rebuild_refs` remakes them after a
//! crash. Objects are durable before a record names them. agentfs writes
//! `config` and `HEAD` itself, atomically, before any git command: git replaces
//! `config` with no fsync, and an emptied one silently loses `core.fsync`.

use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::io::{self, Write};
use std::os::fd::{AsRawFd, BorrowedFd};
use std::os::unix::ffi::OsStringExt;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::Arc;

use crate::file_lock::FileLock;

use git2::Oid;
use thiserror::Error;

/// Errors from the Git executable or its storage.
#[derive(Debug, Error)]
pub enum Error {
    #[error("I/O: {0}")]
    Io(#[from] io::Error),
    #[error("state area full: {0}")]
    NoSpace(String),
    #[error("git failed: {0}")]
    Git(String),
    #[error("invalid git output: {0}")]
    Invalid(String),
}

type Result<T> = std::result::Result<T, Error>;

/// What git's stderr says went wrong. A full state area must reach the call as
/// `ENOSPC`, and git's exit status does not say (children run under `LC_ALL=C`).
fn classify(stderr: &[u8]) -> Error {
    let text = String::from_utf8_lossy(stderr).trim().to_owned();
    if text.contains("No space left on device") || text.contains("Disk quota exceeded") {
        Error::NoSpace(text)
    } else {
        Error::Git(text)
    }
}

// The repository's whole configuration; nothing else writes it.
const CONFIG: &str = "[core]\n\trepositoryformatversion = 0\n\tfilemode = true\n\tbare = true\n";
const HEAD: &str = "ref: refs/heads/main\n";

/// A single tree entry. Names are raw Unix filename bytes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TreeEntry {
    pub name: Vec<u8>,
    pub mode: u32,
    pub oid: Oid,
}

/// The tree, parents, and raw message of a commit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Commit {
    pub tree: Oid,
    pub parents: Vec<Oid>,
    pub message: Vec<u8>,
}

/// An atomic compare-and-swap of one reference. None means absent/deleted.
#[derive(Clone, Debug)]
pub struct RefUpdate {
    pub refname: String,
    pub new: Option<Oid>,
    pub expected_old: Option<Oid>,
}

/// A bare Git repository whose refs are a copy of the journal's pointers.
#[derive(Clone, Debug)]
pub struct TreeGit {
    path: PathBuf,
    lease: Option<Arc<FileLock>>,
}

impl TreeGit {
    /// Initialize a bare repository. For an actor, use `init_with_lease`.
    pub fn init(path: impl AsRef<Path>) -> Result<Self> {
        Self::init_inner(path.as_ref(), None)
    }

    /// Initialize while retaining the actor's exclusive lock across Git children.
    pub fn init_with_lease(path: impl AsRef<Path>, lease: Arc<FileLock>) -> Result<Self> {
        Self::init_inner(path.as_ref(), Some(lease))
    }

    fn init_inner(path: &Path, lease: Option<Arc<FileLock>>) -> Result<Self> {
        std::fs::create_dir_all(path)?;
        let repo = Self {
            path: std::fs::canonicalize(path)?,
            lease,
        };
        repo.run(&["init", "--bare"], None, false)?;
        sync_initialized_repository(&repo.path)?;
        Ok(repo)
    }

    /// Open a repository. For an actor, use `open_with_lease`.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        Self::open_inner(path.as_ref(), None)
    }

    /// Keeps the path and the actor's lock across Git children. Runs no git and
    /// checks no format: after a crash `write_config` comes first.
    pub fn open_with_lease(path: impl AsRef<Path>, lease: Arc<FileLock>) -> Result<Self> {
        Self::open_inner(path.as_ref(), Some(lease))
    }

    fn open_inner(path: &Path, lease: Option<Arc<FileLock>>) -> Result<Self> {
        Ok(Self {
            path: std::fs::canonicalize(path)?,
            lease,
        })
    }

    /// Replaces `config` and `HEAD`, each atomically (temporary file, fsync,
    /// rename, directory fsync). Runs no git, and leaves what git needs to call
    /// this a repository, `refs/` included: a cut rebuild can leave none.
    /// `upstreams` maps a fork's name to its parent's.
    pub fn write_config(&self, upstreams: &BTreeMap<String, String>) -> Result<()> {
        let mut config = CONFIG.to_owned();
        for (fork, parent) in upstreams {
            let section = fork.replace('\\', "\\\\").replace('"', "\\\"");
            config.push_str(&format!(
                "[branch \"{section}\"]\n\tremote = .\n\tmerge = refs/heads/{parent}\n"
            ));
        }
        replace_file(&self.path, "config", config.as_bytes())?;
        replace_file(&self.path, "HEAD", HEAD.as_bytes())?;
        std::fs::create_dir_all(self.path.join("refs"))?;
        Ok(())
    }

    /// Removes `refs/` and `packed-refs`, makes `refs/`, and creates every
    /// pointer in one `update-ref --stdin`. No fsync: this is a copy.
    pub fn rebuild_refs(&self, pointers: &[RefUpdate]) -> Result<()> {
        let refs = self.path.join("refs");
        match std::fs::remove_dir_all(&refs) {
            Err(err) if err.kind() != io::ErrorKind::NotFound => return Err(err.into()),
            _ => {}
        }
        match std::fs::remove_file(self.path.join("packed-refs")) {
            Err(err) if err.kind() != io::ErrorKind::NotFound => return Err(err.into()),
            _ => {}
        }
        std::fs::create_dir(&refs)?;
        self.ref_transaction(pointers)
    }

    /// What is lost of each tip's commit, tree and blobs, parents not walked.
    pub fn missing_objects(&self, tips: &[Oid]) -> Result<Vec<Oid>> {
        let mut input = String::new();
        for tip in tips {
            input.push_str(&format!("{tip}\n"));
        }
        let out = self.run(
            &[
                "rev-list",
                "--objects",
                "--no-walk",
                "--missing=print",
                "--stdin",
            ],
            Some(input.as_bytes()),
            true,
        )?;
        out.stdout
            .split(|b| *b == b'\n')
            .filter_map(|line| line.strip_prefix(b"?"))
            .map(parse_oid)
            .collect()
    }

    fn command(&self, bare: bool) -> Command {
        let mut cmd = Command::new("git");
        cmd.arg("-c").arg("core.hooksPath=/dev/null");
        if let Some(lease) = &self.lease {
            attach_lease(&mut cmd, lease);
        }
        cmd.env_clear()
            .env("PATH", crate::git_cli::GIT_PATH)
            .env("LC_ALL", "C")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_TERMINAL_PROMPT", "0")
            .current_dir(&self.path);
        if bare {
            cmd.env("GIT_DIR", &self.path);
        }
        cmd
    }

    fn run(&self, args: &[&str], input: Option<&[u8]>, bare: bool) -> Result<Output> {
        let mut cmd = self.command(bare);
        cmd.args(args);
        run_command(cmd, input)
    }

    /// Return a reference's value, or None when it does not exist.
    pub fn head(&self, name: &str) -> Result<Option<Oid>> {
        self.run(&["check-ref-format", name], None, true)?;
        let out = self
            .command(true)
            .args(["rev-parse", "--verify", "--quiet", name])
            .output()?;
        if out.status.success() {
            Ok(Some(parse_oid(trim(&out.stdout))?))
        } else if out.status.code() == Some(1) {
            Ok(None)
        } else {
            Err(classify(&out.stderr))
        }
    }

    /// List references beneath a prefix, in Git's reference order.
    pub fn list_refs(&self, prefix: &str) -> Result<Vec<(String, Oid)>> {
        let out = self.run(
            &[
                "for-each-ref",
                "--format=%(refname)%00%(objectname)",
                prefix,
            ],
            None,
            true,
        )?;
        out.stdout
            .split(|b| *b == b'\n')
            .filter(|line| !line.is_empty())
            .map(|line| {
                let index = line
                    .iter()
                    .position(|b| *b == 0)
                    .ok_or_else(|| Error::Invalid("reference record".into()))?;
                let (name, oid) = (&line[..index], &line[index + 1..]);
                Ok((
                    String::from_utf8(name.to_vec())
                        .map_err(|_| Error::Invalid("reference name".into()))?,
                    parse_oid(oid)?,
                ))
            })
            .collect()
    }

    /// Store raw blob bytes.
    pub fn write_blob(&self, bytes: &[u8]) -> Result<Oid> {
        let oid = parse_oid(trim(
            &self
                .run(&["hash-object", "-w", "--stdin"], Some(bytes), true)?
                .stdout,
        ))?;
        self.sync_object_dirs(oid)?;
        Ok(oid)
    }
    /// Read raw blob bytes without filters.
    pub fn read_blob(&self, oid: Oid) -> Result<Vec<u8>> {
        Ok(self
            .run(&["cat-file", "blob", &oid.to_string()], None, true)?
            .stdout)
    }
    /// Read immediate tree entries with raw names.
    pub fn list_tree(&self, oid: Oid) -> Result<Vec<TreeEntry>> {
        let out = self.run(&["ls-tree", "-z", &oid.to_string()], None, true)?;
        out.stdout
            .split(|b| *b == 0)
            .filter(|record| !record.is_empty())
            .map(|record| {
                let index = record
                    .iter()
                    .position(|b| *b == b'\t')
                    .ok_or_else(|| Error::Invalid("tree entry".into()))?;
                let (header, name) = (&record[..index], &record[index + 1..]);
                let fields: Vec<_> = header.split(|b| *b == b' ').collect();
                if fields.len() != 3 {
                    return Err(Error::Invalid("tree header".into()));
                }
                let mode = u32::from_str_radix(
                    std::str::from_utf8(fields[0])
                        .map_err(|_| Error::Invalid("tree mode".into()))?,
                    8,
                )
                .map_err(|_| Error::Invalid("tree mode".into()))?;
                Ok(TreeEntry {
                    name: name.to_vec(),
                    mode,
                    oid: parse_oid(fields[2])?,
                })
            })
            .collect()
    }
    /// Store immediate entries. Git checks the tree's format.
    ///
    /// Through `hash-object`, not `git mktree`: mktree reads no configuration,
    /// so core.fsync never reaches it and its object can be an empty file after
    /// a cut. Writing that tree again then finds the name taken and keeps the
    /// empty file. ci/check-tree-git.sh checks that the writers used here sync.
    pub fn write_tree(&self, entries: &[TreeEntry]) -> Result<Oid> {
        let mut sorted = Vec::with_capacity(entries.len());
        for e in entries {
            if !matches!(e.mode, 0o40000 | 0o100644 | 0o100755 | 0o120000 | 0o160000) {
                return Err(Error::Invalid("tree mode".into()));
            }
            if e.name.is_empty() || e.name.contains(&0) || e.name.contains(&b'/') {
                return Err(Error::Invalid("tree entry name".into()));
            }
            // Git orders a directory as if its name ended in a slash.
            let mut key = e.name.clone();
            if e.mode == 0o40000 {
                key.push(b'/');
            }
            sorted.push((key, e));
        }
        sorted.sort_by(|a, b| a.0.cmp(&b.0));
        let mut data = Vec::new();
        for (_, e) in sorted {
            write!(&mut data, "{:o} ", e.mode)?;
            data.extend_from_slice(&e.name);
            data.push(0);
            data.extend_from_slice(e.oid.as_bytes());
        }
        let oid = parse_oid(trim(
            &self
                .run(
                    &["hash-object", "-t", "tree", "-w", "--stdin"],
                    Some(&data),
                    true,
                )?
                .stdout,
        ))?;
        self.sync_object_dirs(oid)?;
        Ok(oid)
    }
    /// Read a commit's tree, parents, and message.
    pub fn read_commit(&self, oid: Oid) -> Result<Commit> {
        let bytes = self
            .run(&["cat-file", "commit", &oid.to_string()], None, true)?
            .stdout;
        let boundary = bytes
            .windows(2)
            .position(|w| w == b"\n\n")
            .ok_or_else(|| Error::Invalid("commit header".into()))?;
        let mut tree = None;
        let mut parents = Vec::new();
        for line in bytes[..boundary].split(|b| *b == b'\n') {
            if let Some(value) = line.strip_prefix(b"tree ") {
                tree = Some(parse_oid(value)?);
            }
            if let Some(value) = line.strip_prefix(b"parent ") {
                parents.push(parse_oid(value)?);
            }
        }
        Ok(Commit {
            tree: tree.ok_or_else(|| Error::Invalid("commit tree".into()))?,
            parents,
            message: bytes[boundary + 2..].to_vec(),
        })
    }
    /// Write a commit with the fixed service identity.
    pub fn write_commit(&self, tree: Oid, parents: &[Oid], message: &str) -> Result<Oid> {
        let mut cmd = self.command(true);
        cmd.arg("commit-tree").arg(tree.to_string());
        for parent in parents {
            cmd.arg("-p").arg(parent.to_string());
        }
        cmd.env("GIT_AUTHOR_NAME", "agentfs")
            .env("GIT_AUTHOR_EMAIL", "agentfs@localhost")
            .env("GIT_COMMITTER_NAME", "agentfs")
            .env("GIT_COMMITTER_EMAIL", "agentfs@localhost");
        let out = run_command(cmd, Some(message.as_bytes()))?;
        let oid = parse_oid(trim(&out.stdout))?;
        self.sync_object_dirs(oid)?;
        Ok(oid)
    }

    // Git fsyncs loose object data under core.fsync=objects. Persist the
    // fan-out directory entry and any newly created fan-out directory before
    // a subsequent transaction can publish this object.
    fn sync_object_dirs(&self, oid: Oid) -> Result<()> {
        let objects = self.path.join("objects");
        let name = oid.to_string();
        let fanout = objects.join(&name[..2]);
        match std::fs::File::open(fanout) {
            Ok(dir) => dir.sync_all()?,
            Err(err) if err.kind() == io::ErrorKind::NotFound => {} // Already packed.
            Err(err) => return Err(err.into()),
        }
        std::fs::File::open(objects)?.sync_all()?;
        Ok(())
    }

    /// Atomically apply all reference changes, checking every old value.
    pub fn ref_transaction(&self, updates: &[RefUpdate]) -> Result<()> {
        let mut data = Vec::new();
        data.extend_from_slice(b"start\0");
        for u in updates {
            self.run(&["check-ref-format", &u.refname], None, true)?;
            let zero = Oid::ZERO_SHA1;
            let new = u.new.unwrap_or(zero);
            let old = u.expected_old.unwrap_or(zero);
            write!(&mut data, "update {}\0{}\0{}\0", u.refname, new, old)?;
        }
        data.extend_from_slice(b"prepare\0commit\0");
        self.run(&["update-ref", "--stdin", "-z"], Some(&data), true)?;
        Ok(())
    }
    /// Whether the first commit is an ancestor of the second.
    pub fn is_ancestor(&self, ancestor: Oid, descendant: Oid) -> Result<bool> {
        let out = self
            .command(true)
            .args([
                "merge-base",
                "--is-ancestor",
                &ancestor.to_string(),
                &descendant.to_string(),
            ])
            .output()?;
        match out.status.code() {
            Some(0) => Ok(true),
            Some(1) => Ok(false),
            _ => Err(classify(&out.stderr)),
        }
    }
    /// Replace a private cache directory with the exact tree, without Git filters.
    pub fn checkout_tree(&self, tree: Oid, dest: &Path) -> Result<()> {
        if dest.exists() {
            std::fs::remove_dir_all(dest)?;
        }
        std::fs::create_dir_all(dest)?;
        self.checkout_entries(tree, dest)
    }
    fn checkout_entries(&self, tree: Oid, dest: &Path) -> Result<()> {
        for entry in self.list_tree(tree)? {
            let name = OsString::from_vec(entry.name);
            if name == OsStr::new(".") || name == OsStr::new("..") {
                return Err(Error::Invalid("tree entry name".into()));
            }
            let path = dest.join(name);
            match entry.mode {
                0o40000 => {
                    std::fs::create_dir(&path)?;
                    self.checkout_entries(entry.oid, &path)?;
                }
                0o100644 | 0o100755 => {
                    std::fs::write(&path, self.read_blob(entry.oid)?)?;
                    std::fs::set_permissions(
                        &path,
                        std::fs::Permissions::from_mode(entry.mode & 0o777),
                    )?;
                }
                0o120000 => symlink(OsString::from_vec(self.read_blob(entry.oid)?), path)?,
                _ => return Err(Error::Invalid("unsupported checkout mode".into())),
            }
        }
        Ok(())
    }
}

// Git creates its skeleton before agentfs writes the config, with no
// core.fsync. Flush the files first, then every containing
// directory from the leaves up, including the state directory entry naming
// the repository. Symlinks are entries in their parent, not files to follow.
fn sync_initialized_repository(path: &Path) -> io::Result<()> {
    fn sync_tree(dir: &Path) -> io::Result<()> {
        for entry in std::fs::read_dir(dir)? {
            let entry = entry?;
            let kind = entry.file_type()?;
            if kind.is_dir() {
                sync_tree(&entry.path())?;
            } else if kind.is_file() {
                std::fs::File::open(entry.path())?.sync_all()?;
            }
        }
        std::fs::File::open(dir)?.sync_all()
    }
    sync_tree(path)?;
    // The actor may have created the state directory immediately before the
    // repository. Flush both its contents and its entry in its parent.
    for dir in path.ancestors().skip(1).take(2) {
        std::fs::File::open(dir)?.sync_all()?;
    }
    Ok(())
}

// Spawn, feed stdin and wait. The exit status wins over a failed stdin write: a
// git that died of a full disk also breaks the pipe, and stderr says why.
fn run_command(mut cmd: Command, input: Option<&[u8]>) -> Result<Output> {
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    if input.is_some() {
        cmd.stdin(Stdio::piped());
    } else {
        cmd.stdin(Stdio::null());
    }
    let mut child = cmd.spawn()?;
    let (output, write_result) = std::thread::scope(|scope| {
        let writer = input.map(|bytes| {
            let mut stdin = child.stdin.take().expect("piped stdin");
            scope.spawn(move || stdin.write_all(bytes))
        });
        let output = child.wait_with_output();
        let write_result = writer
            .map(|writer| {
                writer
                    .join()
                    .map_err(|_| io::Error::other("git stdin writer panicked"))
            })
            .transpose();
        (output, write_result)
    });
    let output = output?;
    let write_result = write_result?;
    if !output.status.success() {
        return Err(classify(&output.stderr));
    }
    if let Some(result) = write_result {
        result?;
    }
    Ok(output)
}

fn replace_file(dir: &Path, name: &str, bytes: &[u8]) -> io::Result<()> {
    let temporary = dir.join(format!("{name}.agentfs-tmp"));
    let mut file = std::fs::File::create(&temporary)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    std::fs::rename(&temporary, dir.join(name))?;
    std::fs::File::open(dir)?.sync_all()
}

fn trim(bytes: &[u8]) -> &[u8] {
    bytes.strip_suffix(b"\n").unwrap_or(bytes)
}
fn parse_oid(bytes: &[u8]) -> Result<Oid> {
    let text = std::str::from_utf8(bytes).map_err(|_| Error::Invalid("object id".into()))?;
    Oid::from_str(text).map_err(|_| Error::Invalid("object id".into()))
}

// Only tree Git subprocesses inherit this one descriptor. The Arc keeps the
// FileLock alive through synchronous waits, so its Drop cannot explicitly
// unlock while a child is active. On a crash the kernel retains the lock on
// the shared open file until the child exits, even before PDEATHSIG arrives.
fn attach_lease(cmd: &mut Command, lease: &FileLock) {
    attach_lease_signal(cmd, lease, libc::SIGKILL);
}

fn attach_lease_signal(cmd: &mut Command, lease: &FileLock, signal: i32) {
    let fd = lease.file().as_raw_fd();
    #[cfg(target_os = "linux")]
    let parent = unsafe { libc::getpid() };
    // SAFETY: pre_exec runs after fork, before exec; it uses only async-signal-
    // safe syscalls and the fd remains open because the caller owns the lease.
    unsafe {
        cmd.pre_exec(move || {
            #[cfg(target_os = "linux")]
            {
                if libc::prctl(libc::PR_SET_PDEATHSIG, signal) == -1 {
                    return Err(io::Error::last_os_error());
                }
                if libc::getppid() != parent {
                    return Err(io::Error::from_raw_os_error(libc::ECHILD));
                }
            }
            let borrowed = BorrowedFd::borrow_raw(fd);
            let mut flags = rustix::io::fcntl_getfd(borrowed)?;
            flags.remove(rustix::io::FdFlags::CLOEXEC);
            rustix::io::fcntl_setfd(borrowed, flags)?;
            Ok(())
        });
    }
}

#[cfg(test)]
mod classify_tests {
    use super::*;

    #[test]
    fn git_no_space_is_enospc() {
        for stderr in [
            &b"error: unable to create temporary file: No space left on device\nfatal: bad\n"[..],
            b"fatal: write error: Disk quota exceeded\n",
        ] {
            let error = crate::tree_actor::Error::from(classify(stderr));
            assert_eq!(error.errno(), libc::ENOSPC);
        }
        let error = crate::tree_actor::Error::from(classify(b"fatal: not a git repository\n"));
        assert_eq!(error.errno(), libc::EIO);
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use crate::file_lock::Mode;
    use std::time::{Duration, Instant};

    // Invoked as a separate test process by the lifecycle test below.
    #[test]
    fn lease_child_process() {
        let Some(root) = std::env::var_os("AGENTFS_TREE_LEASE_TEST") else {
            return;
        };
        let root = PathBuf::from(root);
        let lease = FileLock::try_take(&root.join("actor.lock"), Mode::Exclusive)
            .unwrap()
            .unwrap();
        let mut command = Command::new("git");
        command
            .env_clear()
            .env("PATH", crate::git_cli::GIT_PATH)
            .env("LC_ALL", "C")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_TERMINAL_PROMPT", "0")
            .args(["upload-pack", "--stateless-rpc"])
            .arg(root.join("repo.git"))
            // The test driver retains the write end even after this helper is
            // killed, so Git cannot exit on EOF before the death signal.
            .stdin(Stdio::inherit())
            .stdout(Stdio::null())
            .stderr(Stdio::from(
                std::fs::File::create(root.join("git.stderr")).unwrap(),
            ));
        let signal = if std::env::var_os("AGENTFS_TREE_LEASE_STOP").is_some() {
            libc::SIGSTOP
        } else {
            libc::SIGKILL
        };
        attach_lease_signal(&mut command, &lease, signal);
        let mut child = command.spawn().unwrap();
        std::fs::write(root.join("pid.tmp"), child.id().to_string()).unwrap();
        std::fs::rename(root.join("pid.tmp"), root.join("pid")).unwrap();
        child.wait().unwrap();
    }

    fn run_death_case(stop: bool) {
        let temp = tempfile::tempdir().unwrap();
        TreeGit::init(temp.path().join("repo.git")).unwrap();
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args([
                "--exact",
                "tree_git::tests::lease_child_process",
                "--nocapture",
            ])
            .env("AGENTFS_TREE_LEASE_TEST", temp.path())
            .env_remove("AGENTFS_TREE_LEASE_STOP")
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::from(
                std::fs::File::create(temp.path().join("helper.stderr")).unwrap(),
            ));
        if stop {
            command.env("AGENTFS_TREE_LEASE_STOP", "1");
        }
        let mut parent = command.spawn().unwrap();
        let keep_stdin = parent.stdin.take().unwrap();
        let diagnostic = || {
            format!(
                "helper stderr: {}; git stderr: {}",
                std::fs::read_to_string(temp.path().join("helper.stderr")).unwrap_or_default(),
                std::fs::read_to_string(temp.path().join("git.stderr")).unwrap_or_default(),
            )
        };
        let pidfile = temp.path().join("pid");
        let deadline = Instant::now() + Duration::from_secs(10);
        while !pidfile.exists() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        let pid: i32 = std::fs::read_to_string(pidfile)
            .unwrap_or_else(|err| panic!("helper did not publish Git pid: {err}; {}", diagnostic()))
            .parse()
            .unwrap();
        // Wait until Git has entered its blocking read; merely spawning it
        // does not prove it survived startup or is still attached to stdin.
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let status = std::fs::read_to_string(format!("/proc/{pid}/status"))
                .unwrap_or_else(|err| panic!("Git exited before ready: {err}; {}", diagnostic()));
            if status.lines().any(|line| line.starts_with("State:\tS")) {
                break;
            }
            assert!(
                !status.lines().any(|line| line.starts_with("State:\tZ")),
                "Git exited before ready; {}",
                diagnostic()
            );
            assert!(
                Instant::now() < deadline,
                "Git did not become ready; {}",
                diagnostic()
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        parent.kill().unwrap();
        parent.wait().unwrap();
        if stop {
            let deadline = Instant::now() + Duration::from_secs(10);
            loop {
                let status =
                    std::fs::read_to_string(format!("/proc/{pid}/status")).unwrap_or_else(|err| {
                        panic!("Git exited before lease check: {err}; {}", diagnostic())
                    });
                if status.lines().any(|line| line.starts_with("State:\tT")) {
                    break;
                }
                assert!(
                    Instant::now() < deadline,
                    "Git did not receive parent death signal"
                );
                std::thread::sleep(Duration::from_millis(10));
            }
            assert!(
                FileLock::try_take(&temp.path().join("actor.lock"), Mode::Exclusive)
                    .unwrap()
                    .is_none()
            );
            assert_eq!(unsafe { libc::kill(pid, libc::SIGKILL) }, 0);
        }
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if FileLock::try_take(&temp.path().join("actor.lock"), Mode::Exclusive)
                .unwrap()
                .is_some()
            {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "Git retained lease after parent death"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        drop(keep_stdin);
        // A zombie has exited even if the system has not yet reaped it.
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let status = std::fs::read_to_string(format!("/proc/{pid}/status"));
            if status.is_err()
                || status
                    .unwrap()
                    .lines()
                    .any(|line| line.starts_with("State:\tZ"))
            {
                break;
            }
            assert!(Instant::now() < deadline, "Git did not exit");
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    #[test]
    fn killed_parent_leaves_lease_until_git_exits() {
        // Stop on parent death to inspect the lease, then exercise the actual
        // SIGKILL production path separately.
        run_death_case(true);
        run_death_case(false);
    }
}
