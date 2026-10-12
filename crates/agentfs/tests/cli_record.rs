//! End-to-end tests of `agentfs record` and its path rules (design §4, §5).

use std::fs;
use std::path::PathBuf;
use std::process::{Command, Output};

use agentfs_core::history::History;

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
        let out = v.run(&["init", "--volume", "demo"]);
        assert!(out.status.success(), "{}", stderr(&out));
        v
    }

    fn state(&self) -> PathBuf {
        self.dir.path().join("state")
    }

    fn tree(&self) -> PathBuf {
        self.dir.path().join("tree")
    }

    /// A home directory standing in for the user's, so global git config
    /// can be planted without touching the real one.
    fn home(&self) -> PathBuf {
        self.dir.path().join("home")
    }

    fn file(&self, rel: &str, content: &str) {
        let path = self.tree().join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }

    fn run(&self, args: &[&str]) -> Output {
        let mut root: Vec<std::ffi::OsString> = vec!["--root".into(), self.tree().into()];
        root.extend(args.iter().map(Into::into));
        self.run_raw(&root)
    }

    /// Runs with `--state` only.
    fn run_raw(&self, args: &[std::ffi::OsString]) -> Output {
        let home = self.home();
        Command::new(env!("CARGO_BIN_EXE_agentfs"))
            .arg("--state")
            .arg(self.state())
            .args(args)
            .env_remove("AGENTFS_STATE")
            .env_remove("AGENTFS_ROOT")
            .env("AGENTFS_REPLICA", "test-0")
            .env("HOME", &home)
            .env("XDG_CONFIG_HOME", home.join(".config"))
            .env("GIT_CONFIG_GLOBAL", home.join(".gitconfig"))
            .env("GIT_CONFIG_SYSTEM", home.join("system-gitconfig"))
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .output()
            .unwrap()
    }

    fn record(&self, paths: &[&str]) -> String {
        let mut args = vec!["record"];
        args.extend_from_slice(paths);
        let out = self.run(&args);
        assert!(out.status.success(), "{}", stderr(&out));
        String::from_utf8(out.stdout).unwrap()
    }

    /// Paths in the branch head's tree.
    fn head_paths(&self) -> Vec<String> {
        let history = History::open(&self.state().join("git")).unwrap();
        let Some(head) = history.head().unwrap() else {
            return Vec::new();
        };
        let mut out = Vec::new();
        collect(history.repo(), &head.tree().unwrap(), "", &mut out);
        out.sort();
        out
    }
}

fn collect(repo: &git2::Repository, tree: &git2::Tree<'_>, prefix: &str, out: &mut Vec<String>) {
    for entry in tree.iter() {
        let name = format!("{prefix}{}", entry.name().unwrap());
        match entry.to_object(repo).unwrap().into_tree() {
            Ok(sub) => collect(repo, &sub, &format!("{name}/"), out),
            Err(_) => out.push(name),
        }
    }
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

#[test]
fn excluded_paths_are_reported_and_not_recorded() {
    let v = Volume::new();
    fs::write(v.state().join("git/info/exclude"), "*.log\n").unwrap();
    v.file("a.txt", "a");
    v.file("b.log", "b");
    let stdout = v.record(&["a.txt", "b.log"]);
    assert!(stdout.contains("recorded a.txt as seq 0"), "{stdout}");
    assert!(
        stdout.contains("skipped b.log: ignored by info/exclude (*.log)"),
        "{stdout}"
    );
    assert!(stdout.contains("1 record(s) into 1 commit(s)"), "{stdout}");
    assert_eq!(v.head_paths(), ["a.txt"]);
}

#[test]
fn global_and_system_git_config_are_ignored() {
    let v = Volume::new();
    let home = v.home();
    let everything = home.join("everything");
    fs::write(&everything, "*\n").unwrap();
    let no_record = home.join("no-record");
    fs::write(&no_record, "* -agentfs\n").unwrap();
    let config = format!(
        "[core]\n\texcludesFile = {}\n\tattributesFile = {}\n\tignoreCase = true\n",
        everything.display(),
        no_record.display()
    );
    fs::write(home.join(".gitconfig"), &config).unwrap();
    fs::write(home.join("system-gitconfig"), &config).unwrap();
    let xdg = home.join(".config/git");
    fs::create_dir_all(&xdg).unwrap();
    fs::write(xdg.join("config"), &config).unwrap();
    fs::write(xdg.join("ignore"), "*\n").unwrap();
    fs::write(xdg.join("attributes"), "* -agentfs\n").unwrap();
    // ignoreCase would make this exclude A.TXT if it were honoured.
    fs::write(v.state().join("git/info/exclude"), "a.txt\n").unwrap();

    v.file("A.TXT", "a");
    let stdout = v.record(&["A.TXT"]);
    assert!(stdout.contains("recorded A.TXT as seq 0"), "{stdout}");
    assert_eq!(v.head_paths(), ["A.TXT"]);
}

#[test]
fn dotgit_paths_and_nested_repositories_are_skipped() {
    let v = Volume::new();
    v.file("sub/.git/config", "[core]\n");
    v.file("sub/file", "x");
    v.file("top", "t");
    let stdout = v.record(&["sub/.git/config", "sub/file", "top"]);
    assert!(stdout.contains("skipped sub/.git/config: "), "{stdout}");
    assert!(
        stdout.contains("skipped sub/file: inside nested repository sub (.git directory)"),
        "{stdout}"
    );
    assert!(stdout.contains("recorded top as seq 0"), "{stdout}");
    assert_eq!(v.head_paths(), ["top"]);
}

#[test]
fn tracked_paths_stay_recorded() {
    let v = Volume::new();
    v.file("dir/a.log", "1");
    v.record(&["dir/a.log"]);
    fs::write(v.state().join("git/info/exclude"), "dir/\n").unwrap();
    v.file("dir/a.log", "2");
    v.file("dir/b.log", "3");
    let stdout = v.record(&["dir/a.log", "dir/b.log"]);
    assert!(stdout.contains("recorded dir/a.log as seq 1"), "{stdout}");
    assert!(
        stdout.contains("skipped dir/b.log: ignored by info/exclude (dir/)"),
        "{stdout}"
    );
    // Deleting it is still recorded.
    fs::remove_file(v.tree().join("dir/a.log")).unwrap();
    let stdout = v.record(&["dir/a.log", "gone"]);
    assert!(stdout.contains("recorded dir/a.log as seq 2"), "{stdout}");
    assert!(
        stdout.contains("skipped gone: nothing tracked at this path"),
        "{stdout}"
    );
    assert!(v.head_paths().is_empty());
}

#[test]
fn status_accepts_the_global_root() {
    let v = Volume::new();
    let out = v.run(&["status"]);
    assert!(out.status.success(), "{}", stderr(&out));
}

#[test]
fn record_needs_an_explicit_root() {
    let v = Volume::new();
    v.file("a.txt", "1");
    v.record(&["a.txt"]);
    // Run from elsewhere without --root, a.txt would look removed.
    let out = v.run_raw(&["record".into(), "a.txt".into()]);
    assert!(!out.status.success());
    assert!(stderr(&out).contains("--root"), "{}", stderr(&out));
    assert_eq!(v.head_paths(), ["a.txt"]);
    // Commands that do not record still default to the current directory.
    let out = v.run_raw(&["status".into()]);
    assert!(out.status.success(), "{}", stderr(&out));
}
