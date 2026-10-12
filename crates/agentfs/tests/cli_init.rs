//! `agentfs init` from the command line (design §2.3, §4.3, §17.1): safe to
//! re-run for the same volume, refusing another volume or branch, setting
//! `include.path` from `--include`, and creating `--root`.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::Duration;

use agentfs_core::lock::StateLock;
use agentfs_core::volume::StateDir;

struct Dir {
    dir: tempfile::TempDir,
}

impl Dir {
    fn new() -> Self {
        Self {
            dir: tempfile::tempdir().unwrap(),
        }
    }

    fn p(&self, rel: &str) -> PathBuf {
        self.dir.path().join(rel)
    }

    fn state(&self) -> StateDir {
        StateDir::new(self.p("state"))
    }

    fn config(&self) -> String {
        std::fs::read_to_string(self.p("state/git/config")).unwrap()
    }

    fn run<I, S>(&self, args: I) -> Output
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        self.command().args(args).output().unwrap()
    }

    fn command(&self) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_agentfs"));
        for (name, _) in std::env::vars_os() {
            let n = name.to_string_lossy();
            if n.starts_with("OTEL_") || n.starts_with("AGENTFS_") {
                cmd.env_remove(name);
            }
        }
        cmd.arg("--state").arg(self.state().root());
        cmd
    }

    fn ok<I, S>(&self, args: I) -> String
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let out = self.run(args);
        assert!(out.status.success(), "{}{}", stdout(&out), stderr(&out));
        stdout(&out)
    }

    fn fragment(&self, name: &str, content: &str) -> PathBuf {
        let path = self.p(name);
        std::fs::write(&path, content).unwrap();
        path
    }
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

fn s(p: &Path) -> &str {
    p.to_str().unwrap()
}

#[test]
fn a_rerun_for_the_same_volume_succeeds_and_changes_nothing() {
    let d = Dir::new();
    let out = d.ok(["init", "--volume", "demo"]);
    assert!(out.contains("initialised volume demo in"), "{out}");
    let before = d.config();
    let out = d.ok(["init", "--volume", "demo"]);
    assert!(out.contains("volume demo already initialised in"), "{out}");
    let out = d.ok(["init", "--volume", "demo", "--branch", "agentfs/demo"]);
    assert!(out.contains("already initialised"), "{out}");
    assert_eq!(d.config(), before);
    // The volume can come from the environment (the downward API).
    let out = d
        .command()
        .arg("init")
        .env("AGENTFS_VOLUME", "demo")
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", stderr(&out));
}

#[test]
fn another_volume_or_branch_is_refused_and_nothing_changes() {
    let d = Dir::new();
    d.ok(["init", "--volume", "demo"]);
    let before = d.config();
    let out = d.run(["init", "--volume", "other"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(
        stderr(&out).contains("belongs to volume \"demo\", not \"other\""),
        "{}",
        stderr(&out)
    );
    let out = d.run(["init", "--volume", "demo", "--branch", "elsewhere"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(
        stderr(&out).contains("branch is \"agentfs/demo\", not \"elsewhere\""),
        "{}",
        stderr(&out)
    );
    assert_eq!(d.config(), before);
}

#[test]
fn a_rerun_takes_the_state_area_lock() {
    let d = Dir::new();
    d.ok(["init", "--volume", "demo"]);
    let lock = StateLock::acquire(&d.state(), "test", Duration::ZERO).unwrap();
    let out = d.run(["init", "--volume", "demo"]);
    assert_eq!(out.status.code(), Some(75), "{}", stderr(&out));
    drop(lock);
    d.ok(["init", "--volume", "demo"]);
}

#[test]
fn includes_are_set_once_and_replaced() {
    let d = Dir::new();
    let a = d.fragment(
        "a.conf",
        "[agentfs]\n\tpushInterval = 7s\n[remote \"origin\"]\n\turl = /srv/history.git\n",
    );
    let b = d.fragment("b.conf", "[agentfs]\n\tcommitWindow = 1s\n");
    d.ok(["init", "--volume", "demo", "--include", s(&a)]);
    d.ok(["init", "--volume", "demo", "--include", s(&a)]);
    assert_eq!(d.config().matches("path = ").count(), 1, "{}", d.config());
    // A remote defined only in the fragment.
    let status = d.ok(["status"]);
    assert!(
        status.contains("remote:    origin /srv/history.git"),
        "{status}"
    );
    // Given again with another list: exactly that list.
    d.ok(["init", "--volume", "demo", "--include", s(&b)]);
    let config = d.config();
    assert!(!config.contains("a.conf"), "{config}");
    assert_eq!(config.matches("path = ").count(), 1, "{config}");
    let status = d.ok(["status"]);
    assert!(!status.contains("origin"), "{status}");
    // Both, in order. Without --include, a re-run keeps them.
    d.ok([
        "init",
        "--volume",
        "demo",
        "--include",
        s(&a),
        "--include",
        s(&b),
    ]);
    d.ok(["init", "--volume", "demo"]);
    let config = d.config();
    let (ia, ib) = (
        config.find("a.conf").unwrap(),
        config.find("b.conf").unwrap(),
    );
    assert!(ia < ib, "{config}");
}

#[test]
fn a_relative_include_is_made_absolute() {
    let d = Dir::new();
    d.fragment("a.conf", "[agentfs]\n\tpushInterval = 7s\n");
    let out = d
        .command()
        .current_dir(d.dir.path())
        .args(["init", "--volume", "demo", "--include", "a.conf"])
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(d.config().contains(s(&d.p("a.conf"))), "{}", d.config());
}

#[test]
fn a_missing_include_fails() {
    let d = Dir::new();
    let out = d.run([
        "init",
        "--volume",
        "demo",
        "--include",
        s(&d.p("missing.conf")),
    ]);
    assert_eq!(out.status.code(), Some(1));
    assert!(stderr(&out).contains("cannot be read"), "{}", stderr(&out));
    // Repaired by a re-run with a file that exists.
    let good = d.fragment("good.conf", "");
    d.ok(["init", "--volume", "demo", "--include", s(&good)]);
    d.ok(["status"]);
}

#[test]
fn root_is_created() {
    let d = Dir::new();
    let root = d.p("data/backing");
    let out = d.ok([
        "--root".as_ref(),
        root.as_os_str(),
        "init".as_ref(),
        "--volume".as_ref(),
        "demo".as_ref(),
    ]);
    assert!(out.contains("created the backing tree"), "{out}");
    assert!(root.is_dir());
    use std::os::unix::fs::PermissionsExt;
    let mode = std::fs::metadata(&root).unwrap().permissions().mode() & 0o7777;
    // 0755 less the umask.
    assert_eq!(mode & !0o755, 0, "{mode:o}");
    // Existing: left alone.
    let out = d.ok([
        "--root".as_ref(),
        root.as_os_str(),
        "init".as_ref(),
        "--volume".as_ref(),
        "demo".as_ref(),
    ]);
    assert!(!out.contains("created"), "{out}");
}

#[test]
fn existing_rule_files_are_kept() {
    let d = Dir::new();
    let info = d.p("state/git/info");
    std::fs::create_dir_all(&info).unwrap();
    std::fs::write(info.join("exclude"), "*.log\n").unwrap();
    d.ok(["init", "--volume", "demo"]);
    assert_eq!(
        std::fs::read_to_string(info.join("exclude")).unwrap(),
        "*.log\n"
    );
}
