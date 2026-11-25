//! The binary names itself `agentfs`, though its package is `agentfs-cli`:
//! `--version` on the real binary, which the upgrade runbook reads, and the
//! usage line of `--help`. No FUSE.

use std::process::Command;

/// What `--version` prints: the package version, `+failpoints` in a build
/// with failpoints compiled in.
fn version() -> String {
    let features = if cfg!(feature = "failpoints") {
        "+failpoints"
    } else {
        ""
    };
    format!("agentfs {}{features}\n", env!("CARGO_PKG_VERSION"))
}

fn run(arg: &str) -> String {
    let out = Command::new(env!("CARGO_BIN_EXE_agentfs"))
        .arg(arg)
        .output()
        .unwrap();
    assert!(out.status.success(), "{arg}: {out:?}");
    String::from_utf8(out.stdout).unwrap()
}

#[test]
fn the_binary_names_itself_agentfs() {
    assert_eq!(run("--version"), version());
    let help = run("--help");
    assert!(help.contains("\nUsage: agentfs "), "{help}");
}
