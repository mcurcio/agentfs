//! Git LFS through the binary (design §7): `record` stores large files as
//! pointers with a managed `.gitattributes` block, `push` uploads the
//! objects to local and `file://` remotes before their refs, a failed
//! upload pushes no ref, `restore` brings LFS content back from the cache
//! or a remote, and `status` shows each remote's LFS endpoint.
//!
//! Ordinary clones are checked with the `git` binary. When `git-lfs` is on
//! PATH (it is in CI) a clone also runs `git lfs pull` and compares the
//! bytes; otherwise that part is skipped with a message.

use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const LARGE: &str = "large content, over the sixteen-byte threshold\n";

struct Volume {
    dir: tempfile::TempDir,
}

impl Volume {
    fn new() -> Self {
        let v = Self {
            dir: tempfile::tempdir().unwrap(),
        };
        fs::create_dir_all(v.tree()).unwrap();
        fs::create_dir_all(v.home()).unwrap();
        v.ok(&["init", "--volume", "demo", "--local-only"]);
        v.config(&[("agentfs.lfsThreshold", "16")]);
        fs::write(
            v.state().join("git/info/attributes"),
            "*.psd filter=lfs\nkeep.bin agentfs.store=blob\n",
        )
        .unwrap();
        v
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

    fn config(&self, pairs: &[(&str, &str)]) {
        let mut config = git2::Config::open(&self.state().join("git/config")).unwrap();
        for (k, v) in pairs {
            config.set_str(k, v).unwrap();
            // A remote section declares the volume's remotes: drop `--local-only` (F8 S3).
            if k.starts_with("remote.") {
                let _ = config.remove("agentfs.remotes");
            }
        }
    }

    fn run(&self, args: &[&str]) -> Output {
        let home = self.home();
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_agentfs"));
        cmd.arg("--state")
            .arg(self.state())
            .arg("--root")
            .arg(self.tree())
            .args(args)
            .env_remove("AGENTFS_STATE")
            .env_remove("AGENTFS_ROOT")
            .env("AGENTFS_REPLICA", "test-0")
            .env("HOME", &home)
            .env("XDG_CONFIG_HOME", home.join(".config"));
        for (name, _) in std::env::vars_os() {
            if name.to_string_lossy().starts_with("OTEL_") {
                cmd.env_remove(name);
            }
        }
        cmd.output().unwrap()
    }

    fn ok(&self, args: &[&str]) -> String {
        let out = self.run(args);
        assert!(
            out.status.success(),
            "agentfs {args:?}: {}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    fn write(&self, path: &str, content: &str, executable: bool) {
        let p = self.tree().join(path);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(&p, content).unwrap();
        let mode = if executable { 0o755 } else { 0o644 };
        fs::set_permissions(&p, fs::Permissions::from_mode(mode)).unwrap();
    }

    /// The cache path of an object.
    fn cached(&self, oid: &str) -> PathBuf {
        self.state()
            .join(format!("lfs/objects/{}/{}/{oid}", &oid[..2], &oid[2..4]))
    }
}

/// Runs git hermetically in `dir`.
fn git(dir: &Path, args: &[&str]) -> Output {
    Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("HOME", dir)
        .env("GIT_LFS_SKIP_SMUDGE", "1")
        .output()
        .expect("running git")
}

fn git_ok(dir: &Path, args: &[&str]) -> String {
    let out = git(dir, args);
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn have(program: &str, arg: &str) -> bool {
    Command::new(program)
        .arg(arg)
        .output()
        .is_ok_and(|o| o.status.success())
}

/// `version …\noid sha256:<oid>\nsize <n>\n` → `(oid, n)`.
fn pointer(text: &str) -> (String, u64) {
    let mut lines = text.lines();
    assert_eq!(
        lines.next(),
        Some("version https://git-lfs.github.com/spec/v1"),
        "{text:?}"
    );
    let oid = lines.next().unwrap().strip_prefix("oid sha256:").unwrap();
    let size = lines.next().unwrap().strip_prefix("size ").unwrap();
    (oid.to_owned(), size.parse().unwrap())
}

fn bare(path: &Path) {
    git2::Repository::init_bare(path).unwrap();
}

/// Records the standard set of files: LFS by threshold, by attribute, with
/// a space and an executable bit, and blobs.
fn record_files(v: &Volume) {
    v.write("small.txt", "small\n", false);
    v.write("big.bin", LARGE, false);
    v.write("art.psd", "tiny psd", false);
    v.write("keep.bin", LARGE, false);
    v.write("dir with space/large file.bin", LARGE, true);
    v.ok(&[
        "record",
        "small.txt",
        "big.bin",
        "art.psd",
        "keep.bin",
        "dir with space/large file.bin",
    ]);
}

const LFS: [&str; 3] = ["art.psd", "big.bin", "dir with space/large file.bin"];

#[test]
fn large_files_reach_local_and_file_remotes_as_lfs() {
    if !have("git", "--version") {
        eprintln!("skipped: git is not installed");
        return;
    }
    let v = Volume::new();
    let remotes = v.dir.path().join("remotes");
    let local = remotes.join("local.git");
    let file = remotes.join("file.git");
    bare(&local);
    bare(&file);
    v.config(&[
        ("remote.local.url", local.to_str().unwrap()),
        ("remote.file.url", &format!("file://{}", file.display())),
    ]);
    record_files(&v);
    let out = v.ok(&["push"]);
    assert!(out.contains("local: pushed"), "{out}");
    assert!(out.contains("file: pushed"), "{out}");

    let state_git = v.state().join("git");
    let attributes = git_ok(&state_git, &["show", "agentfs/demo:.gitattributes"]);
    assert_eq!(
        attributes,
        "# agentfs-lfs-begin: maintained by agentfs (Git LFS paths); edits are replaced\n\
         /art.psd filter=lfs diff=lfs merge=lfs -text\n\
         /big.bin filter=lfs diff=lfs merge=lfs -text\n\
         /dir[[:space:]]with[[:space:]]space/large[[:space:]]file.bin filter=lfs diff=lfs merge=lfs -text\n\
         # agentfs-lfs-end\n"
    );
    // The backing tree never gets it.
    assert!(!v.tree().join(".gitattributes").exists());
    for remote in [&local, &file] {
        for path in LFS {
            let text = git_ok(remote, &["show", &format!("agentfs/demo:{path}")]);
            let (oid, size) = pointer(&text);
            let content = fs::read(v.tree().join(path)).unwrap();
            assert_eq!(size, content.len() as u64, "{path}");
            let object = remote.join(format!("lfs/objects/{}/{}/{oid}", &oid[..2], &oid[2..4]));
            assert_eq!(fs::read(&object).unwrap(), content, "{path}");
            assert!(v.cached(&oid).is_file());
        }
        for (path, content) in [("small.txt", "small\n"), ("keep.bin", LARGE)] {
            assert_eq!(
                git_ok(remote, &["show", &format!("agentfs/demo:{path}")]),
                content
            );
        }
        let mode = git_ok(
            remote,
            &["ls-tree", "agentfs/demo", "dir with space/large file.bin"],
        );
        assert!(mode.starts_with("100755 "), "{mode}");
    }

    // An ordinary clone sees the attributes; with git-lfs, the content.
    let clone = v.dir.path().join("clone");
    let out = git(
        v.dir.path(),
        &[
            "clone",
            "-q",
            "-b",
            "agentfs/demo",
            &format!("file://{}", file.display()),
            clone.to_str().unwrap(),
        ],
    );
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let check = git_ok(
        &clone,
        &["check-attr", "filter", "--", "big.bin", "small.txt"],
    );
    assert_eq!(
        check,
        "big.bin: filter: lfs\nsmall.txt: filter: unspecified\n"
    );
    if !have("git-lfs", "version") {
        eprintln!("skipped `git lfs pull`: git-lfs is not installed");
        return;
    }
    git_ok(&clone, &["lfs", "install", "--local"]);
    git_ok(&clone, &["lfs", "pull"]);
    for path in LFS.iter().chain(&["small.txt", "keep.bin"]) {
        assert_eq!(
            fs::read(clone.join(path)).unwrap(),
            fs::read(v.tree().join(path)).unwrap(),
            "{path}"
        );
    }
    let files = git_ok(&clone, &["lfs", "ls-files", "-n"]);
    let mut listed: Vec<&str> = files.lines().collect();
    listed.sort();
    assert_eq!(listed, LFS);
    assert_eq!(git_ok(&clone, &["status", "--porcelain"]), "");
}

#[test]
fn restore_brings_lfs_content_back_from_the_cache_or_a_remote() {
    let v = Volume::new();
    let remote = v.dir.path().join("remote.git");
    bare(&remote);
    v.config(&[("remote.origin.url", remote.to_str().unwrap())]);
    v.write("big.bin", LARGE, false);
    v.ok(&["record", "big.bin"]);
    v.ok(&["push"]);
    let c1 = git_ok(&v.state().join("git"), &["rev-parse", "agentfs/demo"]);
    let c1 = c1.trim();
    fs::remove_file(v.tree().join("big.bin")).unwrap();
    v.ok(&["record", "big.bin"]);

    let out = v.ok(&["restore", "big.bin", c1]);
    assert!(out.contains("write"), "{out}");
    assert_eq!(fs::read_to_string(v.tree().join("big.bin")).unwrap(), LARGE);

    // Again, with the cache lost: the object comes from the remote.
    fs::remove_file(v.tree().join("big.bin")).unwrap();
    v.ok(&["record", "big.bin"]);
    fs::remove_dir_all(v.state().join("lfs/objects")).unwrap();
    // A dry run downloads nothing, and says what a real run would.
    let out = v.ok(&["restore", "--dry-run", "big.bin", c1]);
    assert!(
        out.contains(&format!(
            "1 LFS object(s), {} bytes, not in the local cache",
            LARGE.len()
        )),
        "{out}"
    );
    assert!(out.contains("nothing written"), "{out}");
    let cached = fs::read_dir(v.state().join("lfs/objects"))
        .map(|d| d.count())
        .unwrap_or(0);
    assert_eq!(cached, 0, "a dry run downloaded objects");
    assert!(!v.tree().join("big.bin").exists());
    v.ok(&["restore", "big.bin", c1]);
    assert_eq!(fs::read_to_string(v.tree().join("big.bin")).unwrap(), LARGE);

    // Nowhere to get it from: refused before anything is written.
    fs::remove_file(v.tree().join("big.bin")).unwrap();
    v.ok(&["record", "big.bin"]);
    fs::remove_dir_all(v.state().join("lfs/objects")).unwrap();
    fs::remove_dir_all(remote.join("lfs")).unwrap();
    let out = v.run(&["restore", "big.bin", c1]);
    assert_eq!(out.status.code(), Some(1));
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("LFS object"), "{err}");
    assert!(!v.tree().join("big.bin").exists());
}

/// An HTTP server that answers every request with 401, counting them.
fn refusing_server() -> (u16, std::sync::Arc<std::sync::atomic::AtomicUsize>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let count = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let seen = count.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            seen.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut length = 0usize;
            loop {
                let mut line = String::new();
                if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                    break;
                }
                if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                    length = v.trim().parse().unwrap_or(0);
                }
            }
            let mut body = vec![0; length];
            let _ = reader.read_exact(&mut body);
            let reply = "{\"message\":\"Credentials needed\"}";
            let _ = write!(
                stream,
                "HTTP/1.1 401 Unauthorized\r\nContent-Type: application/vnd.git-lfs+json\r\n\
                 Content-Length: {}\r\nConnection: close\r\n\r\n{reply}",
                reply.len()
            );
        }
    });
    (port, count)
}

#[test]
fn a_refused_lfs_upload_pushes_no_ref() {
    let v = Volume::new();
    let remote = v.dir.path().join("remote.git");
    bare(&remote);
    let (port, requests) = refusing_server();
    let lfs_url = format!("http://bob:hunter2@127.0.0.1:{port}/acme/lfs");
    v.config(&[
        ("remote.origin.url", remote.to_str().unwrap()),
        ("remote.origin.lfsurl", &lfs_url),
    ]);
    let status = v.ok(&["status"]);
    assert!(
        status.contains(&format!(
            "lfs: http://***@127.0.0.1:{port}/acme/lfs (remote.origin.lfsurl, credentials from \
             the URL)"
        )),
        "{status}"
    );
    assert!(
        status.contains("files over 16 byte(s) are stored as LFS"),
        "{status}"
    );
    assert!(!status.contains("hunter2"), "{status}");

    // Nothing stored as LFS yet: the endpoint is never contacted.
    v.write("small.txt", "small\n", false);
    v.ok(&["record", "small.txt"]);
    v.ok(&["push"]);
    assert_eq!(requests.load(std::sync::atomic::Ordering::SeqCst), 0);
    let pushed = git_ok_or_none(&remote);

    v.write("big.bin", LARGE, false);
    v.ok(&["record", "big.bin"]);
    let out = v.run(&["push"]);
    assert_eq!(out.status.code(), Some(1));
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stdout.contains("origin: failed (auth, stage lfs)"),
        "{stdout}"
    );
    assert!(stderr.contains("failed (auth, stage lfs)"), "{stderr}");
    assert!(!stdout.contains("hunter2") && !stderr.contains("hunter2"));
    assert!(requests.load(std::sync::atomic::Ordering::SeqCst) > 0);
    // The remote branch did not move.
    assert_eq!(git_ok_or_none(&remote), pushed);
    let status = v.ok(&["status"]);
    assert!(status.contains("last error (auth, stage lfs)"), "{status}");
}

/// The remote's branch head, if any.
fn git_ok_or_none(remote: &Path) -> Option<git2::Oid> {
    git2::Repository::open_bare(remote)
        .unwrap()
        .refname_to_id("refs/heads/agentfs/demo")
        .ok()
}

#[test]
fn status_shows_derived_endpoints_and_why_there_is_none() {
    let v = Volume::new();
    let remote = v.dir.path().join("remote.git");
    bare(&remote);
    v.config(&[
        ("remote.local.url", remote.to_str().unwrap()),
        ("remote.web.url", "https://git.example/acme/r"),
        ("remote.web.agentfsCredentials", "/run/secrets/git"),
        ("remote.git.url", "git://git.example/acme/r.git"),
    ]);
    let status = v.ok(&["status"]);
    assert!(
        status.contains(&format!(
            "lfs: {} (derived from remote.local.url)",
            remote.display()
        )),
        "{status}"
    );
    if cfg!(feature = "https") {
        assert!(
            status.contains(
                "lfs: https://git.example/acme/r.git/info/lfs (derived from remote.web.url, \
                 credentials from agentfsCredentials)"
            ),
            "{status}"
        );
    }
    assert!(
        status.contains("lfs: none (remote git pushes over git://"),
        "{status}"
    );
    assert!(!status.contains("proxy:"), "{status}");
    if cfg!(feature = "ssh") {
        // An SSH remote's endpoint is the one git-lfs-authenticate names.
        v.config(&[
            ("remote.ssh.url", "ssh://git@git.example:2222/acme/r.git"),
            ("remote.ssh.agentfsSshKey", "/run/secrets/key"),
            ("remote.ssh.agentfsKnownHosts", "/run/secrets/known_hosts"),
        ]);
        let status = v.ok(&["status"]);
        assert!(
            status.contains(
                "lfs: git-lfs-authenticate over ssh://git@git.example:2222/acme/r.git \
                 (derived from remote.ssh.url; credentials from git-lfs-authenticate)"
            ),
            "{status}"
        );
    }
    // Proxies: http.proxy for every remote, unless a remote sets its own.
    v.config(&[
        ("http.proxy", "proxy.example:3128"),
        ("remote.git.proxy", ""),
        ("remote.web.proxy", "http://web-proxy.example:8080"),
    ]);
    let status = v.ok(&["status"]);
    assert!(
        status.contains("proxy: http://proxy.example:3128 (http.proxy), LFS only"),
        "{status}"
    );
    assert!(
        status.contains("proxy: http://web-proxy.example:8080 (remote.web.proxy), git and LFS"),
        "{status}"
    );
    // local and web (and ssh, in an ssh build); git turns it off.
    let proxied = if cfg!(feature = "ssh") { 3 } else { 2 };
    assert_eq!(status.matches("proxy: ").count(), proxied, "{status}");
}

/// The objects in `remote`'s standalone-file layout.
fn remote_object(remote: &Path, oid: &str) -> PathBuf {
    remote.join(format!("lfs/objects/{}/{}/{oid}", &oid[..2], &oid[2..4]))
}

/// The oid of `path`'s pointer at `rev` in the volume's history.
fn oid_at(v: &Volume, rev: &str, path: &str) -> String {
    let text = git_ok(&v.state().join("git"), &["show", &format!("{rev}:{path}")]);
    pointer(&text).0
}

fn cached_count(v: &Volume) -> usize {
    fn walk(dir: &Path) -> usize {
        fs::read_dir(dir)
            .map(|d| {
                d.flatten()
                    .map(|e| {
                        if e.file_type().unwrap().is_dir() {
                            walk(&e.path())
                        } else {
                            1
                        }
                    })
                    .sum()
            })
            .unwrap_or(0)
    }
    walk(&v.state().join("lfs/objects"))
}

#[test]
fn lfs_fetch_brings_back_what_commits_reference() {
    let v = Volume::new();
    let a = v.dir.path().join("a.git");
    let b = v.dir.path().join("b.git");
    bare(&a);
    bare(&b);
    v.config(&[
        ("remote.a.url", a.to_str().unwrap()),
        ("remote.b.url", b.to_str().unwrap()),
    ]);
    v.write("one.bin", &format!("{LARGE} one"), false);
    v.ok(&["record", "one.bin"]);
    let first = git_ok(&v.state().join("git"), &["rev-parse", "agentfs/demo"]);
    let first = first.trim().to_owned();
    // `one.bin` is replaced: the head no longer references its object.
    v.write("one.bin", &format!("{LARGE} one, again"), false);
    v.write("two.bin", &format!("{LARGE} two"), false);
    v.ok(&["record", "one.bin", "two.bin"]);
    v.ok(&["push"]);
    let old_one = oid_at(&v, &first, "one.bin");
    let new_one = oid_at(&v, "agentfs/demo", "one.bin");
    let two = oid_at(&v, "agentfs/demo", "two.bin");
    // Remote a loses two.bin's object: b provides it.
    fs::remove_file(remote_object(&a, &two)).unwrap();
    fs::remove_dir_all(v.state().join("lfs/objects")).unwrap();

    // A dry run downloads nothing.
    let out = v.ok(&["lfs", "fetch", "--dry-run"]);
    assert!(out.contains("2 object(s)"), "{out}");
    assert!(out.contains("would fetch 2 object(s)"), "{out}");
    assert_eq!(cached_count(&v), 0);

    // The head: its two objects, from a and then b.
    let out = v.ok(&["lfs", "fetch"]);
    assert!(out.contains("a: fetched 1 object(s)"), "{out}");
    assert!(out.contains("b: fetched 1 object(s)"), "{out}");
    assert!(v.cached(&new_one).is_file() && v.cached(&two).is_file());
    assert!(!v.cached(&old_one).exists());
    // Again: nothing to do.
    let out = v.ok(&["lfs", "fetch"]);
    assert!(out.contains("2 already cached"), "{out}");

    // A revision: the first commit's tree.
    let out = v.ok(&["lfs", "fetch", &first]);
    assert!(out.contains("fetched 1 object(s)"), "{out}");
    assert!(v.cached(&old_one).is_file());

    // --all, from b only, which lacks nothing.
    fs::remove_dir_all(v.state().join("lfs/objects")).unwrap();
    let out = v.ok(&["lfs", "fetch", "--all", "--remote", "b"]);
    assert!(out.contains("3 object(s)"), "{out}");
    assert!(!out.contains("a: "), "{out}");
    assert_eq!(cached_count(&v), 3);

    // Only from a, which lacks two.bin's object: exit 1, naming it.
    fs::remove_dir_all(v.state().join("lfs/objects")).unwrap();
    let out = v.run(&["lfs", "fetch", "--remote", "a"]);
    assert_eq!(out.status.code(), Some(1));
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains(&two) && err.contains("two.bin"), "{err}");
    // Unknown remotes and revisions are errors.
    assert_eq!(
        v.run(&["lfs", "fetch", "--remote", "nope"]).status.code(),
        Some(1)
    );
    assert_eq!(
        v.run(&["lfs", "fetch", "no-such-rev"]).status.code(),
        Some(1)
    );
    // A restore after a fetch downloads nothing more: the objects are
    // cached, and a's are gone too.
    v.ok(&["lfs", "fetch"]);
    fs::remove_dir_all(a.join("lfs")).unwrap();
    fs::remove_dir_all(b.join("lfs")).unwrap();
    fs::remove_file(v.tree().join("two.bin")).unwrap();
    v.ok(&["record", "two.bin"]);
    v.ok(&["restore", "two.bin", "agentfs/demo~1"]);
    assert_eq!(
        fs::read_to_string(v.tree().join("two.bin")).unwrap(),
        format!("{LARGE} two")
    );
}

#[test]
fn lfs_prune_keeps_what_a_remote_lacks_and_fetch_brings_objects_back() {
    let v = Volume::new();
    let a = v.dir.path().join("a.git");
    let b = v.dir.path().join("b.git");
    bare(&a);
    bare(&b);
    v.config(&[
        ("remote.a.url", a.to_str().unwrap()),
        ("remote.b.url", b.to_str().unwrap()),
    ]);
    v.write("one.bin", &format!("{LARGE} one"), false);
    v.write("two.bin", &format!("{LARGE} two"), false);
    v.ok(&["record", "one.bin", "two.bin"]);
    v.ok(&["push"]);
    let one = oid_at(&v, "agentfs/demo", "one.bin");
    let two = oid_at(&v, "agentfs/demo", "two.bin");
    // b loses two.bin's object: only one.bin's is on every remote.
    fs::remove_file(remote_object(&b, &two)).unwrap();

    let out = v.ok(&["lfs", "prune", "--dry-run"]);
    assert!(out.contains("would prune 1 object(s)"), "{out}");
    assert!(out.contains("b lacks 1"), "{out}");
    assert!(v.cached(&one).is_file(), "a dry run deleted an object");
    // --older-than keeps what was cached recently.
    let out = v.ok(&["lfs", "prune", "--older-than", "1h"]);
    assert!(out.contains("pruned 0 object(s)"), "{out}");
    assert!(v.cached(&one).is_file());
    let out = v.ok(&["lfs", "prune"]);
    assert!(out.contains("pruned 1 object(s)"), "{out}");
    assert!(!v.cached(&one).exists());
    assert!(v.cached(&two).is_file());

    // A restore of a pruned object downloads it again.
    fs::remove_file(v.tree().join("one.bin")).unwrap();
    v.ok(&["record", "one.bin"]);
    v.ok(&["restore", "one.bin", "agentfs/demo~1"]);
    assert!(v.cached(&one).is_file());
    v.ok(&["lfs", "prune"]);
    assert!(!v.cached(&one).exists());

    // A new remote after a prune: its first push needs objects the cache
    // no longer has, and says how to get them back.
    let c = v.dir.path().join("c.git");
    bare(&c);
    v.config(&[("remote.c.url", c.to_str().unwrap())]);
    let out = v.run(&["push", "--remote", "c"]);
    assert_eq!(out.status.code(), Some(1));
    let all = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(all.contains("c: failed (local, stage lfs)"), "{all}");
    assert!(all.contains("agentfs lfs fetch --all"), "{all}");
    assert!(all.contains("one.bin"), "{all}");
    assert_eq!(git_ok_or_none(&c), None);
    v.ok(&["lfs", "fetch", "--all"]);
    let out = v.ok(&["push", "--remote", "c"]);
    assert!(out.contains("c: pushed"), "{out}");

    // A misconfigured remote refuses the prune.
    v.config(&[
        ("remote.bad.url", "/nowhere"),
        ("remote.bad.agentfsMaxBlobSize", "0"),
    ]);
    let out = v.run(&["lfs", "prune"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("remote bad is misconfigured"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn lfs_prune_waits_for_the_cache_lock() {
    let v = Volume::new();
    let a = v.dir.path().join("a.git");
    bare(&a);
    v.config(&[("remote.a.url", a.to_str().unwrap())]);
    v.write("one.bin", LARGE, false);
    v.ok(&["record", "one.bin"]);
    v.ok(&["push"]);
    let one = oid_at(&v, "agentfs/demo", "one.bin");
    // A restore (here, the test) holds the cache lock shared.
    let lock = fs::File::create(v.state().join("lfs/lock")).unwrap();
    rustix::fs::flock(&lock, rustix::fs::FlockOperation::LockShared).unwrap();
    let out = v.run(&["--wait-lock", "300ms", "lfs", "prune"]);
    assert_eq!(out.status.code(), Some(75));
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("in use"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(v.cached(&one).is_file());
    drop(lock);
    v.ok(&["lfs", "prune"]);
    assert!(!v.cached(&one).exists());
}

#[test]
fn lfs_objects_over_the_smallest_lfs_limit_are_refused_when_recorded() {
    let v = Volume::new();
    let a = v.dir.path().join("a.git");
    bare(&a);
    v.config(&[
        ("remote.a.url", a.to_str().unwrap()),
        ("remote.a.agentfsMaxLfsObjectSize", "64"),
    ]);
    v.write("small.bin", LARGE, false);
    v.write("huge.bin", &LARGE.repeat(3), false);
    let out = v.run(&["record", "small.bin", "huge.bin"]);
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("path refused at record: huge.bin"), "{err}");
    assert!(err.contains("LFS object limit of remote a"), "{err}");
    let state_git = v.state().join("git");
    pointer(&git_ok(&state_git, &["show", "agentfs/demo:small.bin"]));
    assert!(
        !git(&state_git, &["show", "agentfs/demo:huge.bin"])
            .status
            .success()
    );
    // status shows a bad value as a misconfiguration.
    v.config(&[("remote.a.agentfsMaxLfsObjectSize", "0")]);
    let status = v.ok(&["status"]);
    assert!(
        status.contains("a: misconfigured, not pushed: remote.a.agentfsMaxLfsObjectSize"),
        "{status}"
    );
}
