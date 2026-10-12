//! The state-area lock from the command line (design §2.3): `mount`, `run`,
//! `record`, `flush` and `push` refuse a state area another process holds,
//! with exit status 75, unless `--wait-lock` lets them wait; `status` is
//! read-only and reports the holder.

use std::ffi::OsStr;
use std::path::PathBuf;
use std::process::{Command, Output};
use std::time::Duration;

use agentfs_core::lock::StateLock;
use agentfs_core::volume::StateDir;

struct Volume {
    dir: tempfile::TempDir,
}

impl Volume {
    fn new() -> Self {
        let v = Self {
            dir: tempfile::tempdir().unwrap(),
        };
        std::fs::create_dir_all(v.tree()).unwrap();
        std::fs::write(v.tree().join("a"), "1").unwrap();
        let out = v.run(["init", "--volume", "demo"]);
        assert!(out.status.success(), "{}", stderr(&out));
        v
    }

    fn state(&self) -> StateDir {
        StateDir::new(self.dir.path().join("state"))
    }

    fn tree(&self) -> PathBuf {
        self.dir.path().join("tree")
    }

    fn run<I, S>(&self, args: I) -> Output
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_agentfs"));
        for (name, _) in std::env::vars_os() {
            let name_str = name.to_string_lossy();
            if name_str.starts_with("OTEL_") || name_str.starts_with("AGENTFS_") {
                cmd.env_remove(name);
            }
        }
        cmd.arg("--state")
            .arg(self.state().root())
            .arg("--root")
            .arg(self.tree())
            .args(args)
            .env("AGENTFS_REPLICA", "test-0")
            .output()
            .unwrap()
    }
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

#[test]
fn commands_refuse_a_held_state_area() {
    let v = Volume::new();
    let _lock = StateLock::acquire(&v.state(), "test-holder", Duration::ZERO).unwrap();
    for args in [
        vec!["flush".to_owned()],
        vec!["record".to_owned(), "a".to_owned()],
        vec!["push".to_owned()],
        vec!["run".to_owned()],
    ] {
        let out = v.run(&args);
        assert_eq!(out.status.code(), Some(75), "{args:?}: {}", stderr(&out));
        let err = stderr(&out);
        assert!(err.contains("is in use"), "{args:?}: {err}");
        assert!(err.contains("command=test-holder"), "{args:?}: {err}");
    }
    // Nothing was recorded behind the holder's back.
    let out = v.run(["status"]);
    assert!(out.status.success(), "{}", stderr(&out));
    let text = stdout(&out);
    assert!(text.contains("backlog:   0 record(s)"), "{text}");
    assert!(text.contains("lock:      held by pid="), "{text}");
    assert!(text.contains("command=test-holder"), "{text}");
}

#[test]
fn status_shows_a_free_lock() {
    let v = Volume::new();
    let out = v.run(["status"]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(stdout(&out).contains("lock:      free"), "{}", stdout(&out));
}

#[test]
fn wait_lock_waits_for_the_holder() {
    let v = Volume::new();
    let lock = StateLock::acquire(&v.state(), "test-holder", Duration::ZERO).unwrap();
    let releaser = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(500));
        drop(lock);
    });
    let out = v.run(["--wait-lock", "5s", "record", "a"]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(stdout(&out).contains("recorded a"), "{}", stdout(&out));
    releaser.join().unwrap();
}
