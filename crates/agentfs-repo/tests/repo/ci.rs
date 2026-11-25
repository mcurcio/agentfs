//! The workflows, the CI scripts and the Dockerfile.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use agentfs_repo::workspace::Workspace;

use crate::{files_under, markdown_files, root};

/// The runner every CI job uses. Pinned, not `-latest`, so a
/// new image cannot change the kernel, FUSE, kind or nginx under the
/// tests.
const RUNNER: &str = "ubuntu-24.04";

#[test]
fn workflows_pin_runners() {
    let dir = root().join(".github/workflows");
    let mut problems = Vec::new();
    let mut jobs = 0;
    for entry in std::fs::read_dir(&dir).unwrap() {
        let path = entry.unwrap().path();
        if !path.extension().is_some_and(|e| e == "yml" || e == "yaml") {
            continue;
        }
        let text = std::fs::read_to_string(&path).unwrap();
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        for (n, line) in text.lines().enumerate() {
            let Some(value) = line.trim().strip_prefix("runs-on:") else {
                continue;
            };
            jobs += 1;
            let value = value.split('#').next().unwrap_or("").trim();
            if value != RUNNER {
                problems.push(format!("{name}:{}: runs-on: {value} (use {RUNNER})", n + 1));
            }
        }
        if text.contains("ubuntu-latest") {
            problems.push(format!("{name} mentions ubuntu-latest"));
        }
    }
    assert!(jobs > 0, "no runs-on lines found in {}", dir.display());
    // The docs name the runner the numbers were measured on. A line may
    // name ubuntu-latest only to explain the pin.
    for path in markdown_files() {
        if path.components().any(|c| c.as_os_str() == "reviews") {
            continue;
        }
        let text = std::fs::read_to_string(&path).unwrap();
        for (n, line) in text.lines().enumerate() {
            if line.contains("ubuntu-latest") && !line.contains(RUNNER) {
                problems.push(format!(
                    "{}:{}: names ubuntu-latest; CI runs on {RUNNER}",
                    path.display(),
                    n + 1
                ));
            }
        }
    }
    assert!(problems.is_empty(), "{}", problems.join("\n"));
}

/// The comment that gives a `cargo test` in CI its reason.
const NOT_NEXTEST: &str = "cargo test, not nextest:";

/// CI runs the unit and integration tests with cargo-nextest (AGENTS.md,
/// "Local gates"). A workflow or `ci/` script runs `cargo test` only for
/// doctests (`--doc`), which nextest does not run, or under a comment
/// with `cargo test, not nextest:` and the reason, in the same block of
/// lines.
#[test]
fn ci_runs_tests_with_nextest() {
    let root = root();
    let mut files = Vec::new();
    files_under(&root.join(".github/workflows"), &mut files);
    files_under(&root.join("ci"), &mut files);
    files.retain(|p| {
        p.extension()
            .is_some_and(|e| e == "yml" || e == "yaml" || e == "sh")
    });
    files.sort();
    let mut problems = Vec::new();
    let mut nextest = 0;
    for path in &files {
        let text = std::fs::read_to_string(path).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        for (n, line) in lines.iter().enumerate() {
            let code = line.trim_start();
            if code.starts_with('#') {
                continue;
            }
            if code.contains("cargo nextest run") {
                nextest += 1;
            }
            let words: Vec<&str> = code.split_whitespace().collect();
            if !words.windows(2).any(|w| w == ["cargo", "test"]) || words.contains(&"--doc") {
                continue;
            }
            let reasoned = lines[..n]
                .iter()
                .rev()
                .take_while(|l| !l.trim().is_empty())
                .any(|l| l.trim_start().starts_with('#') && l.contains(NOT_NEXTEST));
            if !reasoned {
                problems.push(format!(
                    "{}:{}: cargo test; run cargo nextest run, or say why above (`# {NOT_NEXTEST} ...`)",
                    path.strip_prefix(&root).unwrap_or(path).display(),
                    n + 1
                ));
            }
        }
    }
    assert!(
        nextest > 3,
        "only {nextest} cargo nextest runs found: is the scan broken?"
    );
    assert!(problems.is_empty(), "{}", problems.join("\n"));
}

/// The workflows, the development image (`ci/dev-image.sh`) and AGENTS.md's
/// install line name one release of cargo-nextest.
#[test]
fn nextest_is_pinned_to_one_release() {
    let root = root();
    // Each `marker<version>` in `file`, the version ending at the first character
    // that is not a digit or a dot.
    let versions = |file: &Path, marker: &str| -> Vec<(String, String)> {
        let text = std::fs::read_to_string(file).unwrap();
        let name = file
            .strip_prefix(&root)
            .unwrap_or(file)
            .display()
            .to_string();
        text.match_indices(marker)
            .map(|(at, _)| {
                let rest = &text[at + marker.len()..];
                let end = rest
                    .find(|c: char| !(c.is_ascii_digit() || c == '.'))
                    .unwrap_or(rest.len());
                (name.clone(), rest[..end].trim_end_matches('.').to_owned())
            })
            .collect()
    };
    let mut found = Vec::new();
    let mut workflows = Vec::new();
    files_under(&root.join(".github/workflows"), &mut workflows);
    workflows.sort();
    for path in &workflows {
        found.extend(versions(path, "tool: cargo-nextest@"));
    }
    let in_workflows = found.len();
    let image = versions(&root.join("ci/dev-image.sh"), "version=");
    let agents = versions(&root.join("AGENTS.md"), "get.nexte.st/");
    assert!(
        in_workflows > 0 && image.len() == 1 && agents.len() == 1,
        "expected cargo-nextest pins in the workflows, one in ci/dev-image.sh and one in AGENTS.md: \
         {found:?} {image:?} {agents:?}"
    );
    found.extend(image);
    found.extend(agents);
    let releases: BTreeSet<&str> = found.iter().map(|(_, v)| v.as_str()).collect();
    assert!(
        releases.len() == 1 && releases.iter().all(|v| v.split('.').count() == 3),
        "cargo-nextest is pinned to more than one release, or not to a release:\n{found:?}"
    );
}

/// CI pulls Docker Hub through mirror.gcr.io (docs/development.md,
/// "Runners, images and runbooks"). A workflow sets up buildx only through
/// `.github/actions/docker-hub-mirror`, whose BuildKit pulls through the
/// mirror, and creates kind clusters only with `ci/kind-cluster.sh`, whose
/// nodes do; a job that pulls with the runner's daemon uses the action.
/// Scripts that pull check the daemon themselves (`require_docker_hub_mirror`
/// in `ci/lib.sh`).
#[test]
fn workflows_pull_docker_hub_through_the_mirror() {
    const ACTION: &str = "uses: ./.github/actions/docker-hub-mirror";
    let pulls = |line: &str| {
        [
            "docker run",
            "docker build",
            "docker pull",
            "docker compose",
        ]
        .iter()
        .any(|c| line.contains(c))
            || line.contains("ci/kind-cluster.sh")
            || line.contains("docker/build-push-action")
    };
    let dir = root().join(".github/workflows");
    let mut problems = Vec::new();
    let mut pulling_jobs = 0;
    for entry in std::fs::read_dir(&dir).unwrap() {
        let path = entry.unwrap().path();
        if !path.extension().is_some_and(|e| e == "yml" || e == "yaml") {
            continue;
        }
        let text = std::fs::read_to_string(&path).unwrap();
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        // (job, first pulling line, uses the action)
        let mut jobs: Vec<(String, Option<usize>, bool)> = Vec::new();
        let mut in_jobs = false;
        for (n, line) in text.lines().enumerate() {
            let trimmed = line.trim();
            if trimmed.starts_with('#') {
                continue;
            }
            if !line.starts_with(' ') && !trimmed.is_empty() {
                in_jobs = line == "jobs:";
                continue;
            }
            if !in_jobs {
                continue;
            }
            if line.starts_with("  ") && !line.starts_with("   ") && trimmed.ends_with(':') {
                jobs.push((trimmed.trim_end_matches(':').to_owned(), None, false));
                continue;
            }
            let at = format!("{name}:{}", n + 1);
            if line.contains("docker/setup-buildx-action") {
                problems.push(format!(
                    "{at}: set up buildx with the docker-hub-mirror action's `buildx: true`"
                ));
            }
            if line.contains("kind create cluster") {
                problems.push(format!(
                    "{at}: create kind clusters with ci/kind-cluster.sh"
                ));
            }
            let Some(job) = jobs.last_mut() else {
                continue;
            };
            if trimmed.trim_start_matches("- ") == ACTION {
                job.2 = true;
            }
            if job.1.is_none() && pulls(line) {
                job.1 = Some(n + 1);
            }
        }
        for (job, pulling, mirrored) in jobs {
            let Some(line) = pulling else { continue };
            pulling_jobs += 1;
            if !mirrored {
                problems.push(format!(
                    "{name}:{line}: job {job} pulls images without {ACTION}"
                ));
            }
        }
    }
    assert!(
        pulling_jobs > 5,
        "only {pulling_jobs} pulling jobs found: is the parser broken?"
    );
    assert!(problems.is_empty(), "{}", problems.join("\n"));
}

/// Every shell script under `ci/` registers its EXIT cleanup with `on_exit`
/// (`ci/lib.sh`), which runs it only in the shell that set it: under bash
/// 5.2 a bare `trap ... EXIT` also runs in a forked child that gets a fatal
/// signal before it resets its traps (docs/development.md, "Runners,
/// images and runbooks"). The one bare trap is `on_exit`'s own.
#[test]
fn ci_exit_traps_run_in_their_own_shell() {
    fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                walk(&path, out);
            } else if path.extension().is_some_and(|e| e == "sh") {
                out.push(path);
            }
        }
    }
    let ci = root().join("ci");
    let mut scripts = Vec::new();
    walk(&ci, &mut scripts);
    scripts.sort();
    let mut problems = Vec::new();
    let mut registered = 0;
    for path in &scripts {
        let text = std::fs::read_to_string(path).unwrap();
        let own = path.file_name().is_some_and(|n| n == "lib.sh");
        for (n, line) in text.lines().enumerate() {
            let code = line.trim_start();
            if code.starts_with('#') {
                continue;
            }
            let words: Vec<&str> = code
                .split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
                .collect();
            if words.first() == Some(&"on_exit") {
                registered += 1;
            }
            if words.contains(&"trap")
                && words.contains(&"EXIT")
                && !(own && code.contains("BASHPID"))
            {
                problems.push(format!(
                    "{}:{}: a bare EXIT trap; register it with on_exit (ci/lib.sh)",
                    path.strip_prefix(&ci).unwrap_or(path).display(),
                    n + 1
                ));
            }
        }
    }
    assert!(
        registered > 0,
        "no on_exit registrations under {}",
        ci.display()
    );
    assert!(problems.is_empty(), "{}", problems.join("\n"));
}

/// `rust-toolchain.toml` is the one pin of the Rust release: no workflow names
/// its own toolchain, and the Dockerfile's Rust image, which the development
/// image builds from too, is that release (docs/development.md, "The Rust
/// toolchain").
#[test]
fn rust_toolchain_is_pinned_once() {
    let root = root();
    let pin = std::fs::read_to_string(root.join("rust-toolchain.toml")).unwrap();
    let channel = pin
        .lines()
        .find_map(|l| l.trim().strip_prefix("channel = "))
        .expect("rust-toolchain.toml has a channel")
        .trim_matches('"')
        .to_string();
    assert!(
        channel.split('.').count() == 3 && channel.split('.').all(|p| p.parse::<u32>().is_ok()),
        "rust-toolchain.toml pins a release, not {channel:?}"
    );
    let mut problems = Vec::new();
    for entry in std::fs::read_dir(root.join(".github/workflows")).unwrap() {
        let path = entry.unwrap().path();
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        let text = std::fs::read_to_string(&path).unwrap();
        for (n, line) in text.lines().enumerate() {
            let line = line.trim();
            if line.contains("rust-toolchain@")
                || line.starts_with("toolchain:")
                || line.contains("RUSTUP_TOOLCHAIN")
                || (line.contains("rustup") && line.contains("default"))
            {
                problems.push(format!("{name}:{}: {line}", n + 1));
            }
        }
    }
    let dockerfile = std::fs::read_to_string(root.join("Dockerfile")).unwrap();
    let image = dockerfile
        .lines()
        .find_map(|l| l.strip_prefix("ARG RUST_IMAGE="))
        .expect("the Dockerfile has ARG RUST_IMAGE");
    // The official image's tag names the release without its patch when it is .0.
    let release = channel.strip_suffix(".0").unwrap_or(&channel);
    if !image.starts_with(&format!("rust:{release}-")) || !image.contains("@sha256:") {
        problems.push(format!(
            "Dockerfile: RUST_IMAGE={image} is not rust:{release}-... pinned by digest"
        ));
    }
    assert!(
        problems.is_empty(),
        "rust-toolchain.toml pins {channel}; name it nowhere else:\n{}",
        problems.join("\n")
    );
}

/// The Dockerfile's dependency-cache stage copies every workspace member's
/// manifest and stubs its `src/lib.rs`: cargo refuses to load a workspace
/// with a member it cannot read, so a crate added to `Cargo.toml` and not
/// here breaks every image build.
#[test]
fn dockerfile_stubs_every_workspace_member() {
    let root = root();
    let workspace = Workspace::load();
    let members: Vec<String> = workspace
        .members
        .iter()
        .map(|m| m.dir.to_string_lossy().into_owned())
        .collect();
    assert!(
        !members.is_empty(),
        "no workspace members in cargo metadata"
    );
    let dockerfile = std::fs::read_to_string(root.join("Dockerfile")).unwrap();
    let words: BTreeSet<&str> = dockerfile.split_whitespace().collect();
    let mut problems = Vec::new();
    for m in &members {
        let copied = dockerfile
            .lines()
            .any(|l| l.starts_with("COPY ") && l.contains(&format!("{m}/Cargo.toml")));
        if !copied {
            problems.push(format!("{m}: no COPY of {m}/Cargo.toml"));
        }
        if !words.contains(format!("{m}/src").as_str()) {
            problems.push(format!("{m}: no stub directory {m}/src"));
        }
        if !words.contains(format!("{m}/src/lib.rs").as_str())
            && !dockerfile.contains(&format!("> {m}/src/main.rs"))
        {
            problems.push(format!("{m}: no stub {m}/src/lib.rs or main.rs"));
        }
    }
    assert!(
        problems.is_empty(),
        "the Dockerfile's dependency-cache stage misses workspace members:\n{}",
        problems.join("\n")
    );
}

/// Workflows and other scripts run the CI scripts directly, so each needs
/// its execute bit (a sourced library too, so none is missed).
#[test]
fn ci_scripts_are_executable() {
    use std::os::unix::fs::PermissionsExt;
    let mut files = Vec::new();
    files_under(&root().join("ci"), &mut files);
    let scripts: Vec<_> = files
        .into_iter()
        .filter(|p| p.extension().is_some_and(|e| e == "sh" || e == "py"))
        .collect();
    assert!(scripts.len() > 10, "no ci scripts found: {scripts:?}");
    let offenders: Vec<_> = scripts
        .iter()
        .filter(|p| std::fs::metadata(p).unwrap().permissions().mode() & 0o111 == 0)
        .map(|p| p.display().to_string())
        .collect();
    assert!(
        offenders.is_empty(),
        "not executable (git update-index --chmod=+x):\n{}",
        offenders.join("\n")
    );
}

/// The pipe stages in one line of shell that stop reading their input
/// early: `head`, and `grep` with `-q`, `-m`, `-l` or `-L` (or a long
/// form). Under `set -o pipefail` the producer then dies of SIGPIPE (141)
/// whenever it is still writing, and the pipeline fails though the reader
/// matched (ci run 43420737106).
fn early_exit_readers(line: &str) -> Vec<String> {
    let bytes = line.as_bytes();
    let mut found = Vec::new();
    for (i, &b) in bytes.iter().enumerate() {
        let lone_pipe =
            b == b'|' && bytes.get(i + 1) != Some(&b'|') && (i == 0 || bytes[i - 1] != b'|');
        if !lone_pipe {
            continue;
        }
        let stage: String = line[i + 1..]
            .chars()
            .take_while(|c| !matches!(c, '|' | ';' | '&' | ')'))
            .collect();
        let mut words = stage.split_whitespace();
        let early = match words.next() {
            Some("head") => true,
            Some("grep") => words.any(|w| {
                matches!(
                    w,
                    "--quiet" | "--silent" | "--files-with-matches" | "--files-without-match"
                ) || w.starts_with("--max-count")
                    || (w.starts_with('-')
                        && !w.starts_with("--")
                        && w[1..].chars().all(|c| c.is_ascii_alphanumeric())
                        && w[1..].contains(['q', 'm', 'l', 'L']))
            }),
            _ => false,
        };
        if early {
            found.push(format!("|{}", stage.trim_end()));
        }
    }
    found
}

/// The CI scripts run under `set -o pipefail`, so no pipe in them feeds a
/// reader that exits early: `grep P >/dev/null` replaces `grep -q P`, and
/// `sed -n 1p` replaces `head -n1`, as both read all their input.
#[test]
fn ci_pipes_never_end_in_an_early_exit() {
    let mut files = Vec::new();
    files_under(&root().join("ci"), &mut files);
    let mut offenders = Vec::new();
    let mut scripts = 0;
    for path in files {
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let bash = path.extension().is_some_and(|e| e == "sh")
            || text.starts_with("#!/usr/bin/env bash")
            || text.starts_with("#!/bin/bash");
        if !bash {
            continue;
        }
        scripts += 1;
        // One logical line: a line ending in `|` or `\` goes on.
        let mut logical = String::new();
        let mut first = 0;
        for (n, line) in text.lines().enumerate() {
            if logical.is_empty() {
                first = n + 1;
            }
            let trimmed = line.trim_end();
            logical.push_str(trimmed.strip_suffix('\\').unwrap_or(trimmed));
            logical.push(' ');
            if trimmed.ends_with('|') || trimmed.ends_with('\\') {
                continue;
            }
            if !logical.trim_start().starts_with('#') {
                for stage in early_exit_readers(&logical) {
                    offenders.push(format!("{}:{first}: {stage}", path.display()));
                }
            }
            logical.clear();
        }
    }
    assert!(scripts > 10, "no ci scripts found");
    assert!(
        offenders.is_empty(),
        "a pipe into a reader that exits early fails under pipefail when the \
         producer gets SIGPIPE; use `grep P >/dev/null` for `grep -q P` and \
         `sed -n 1p` for `head -n1`:\n{}",
        offenders.join("\n")
    );
}

#[test]
fn early_exit_readers_are_found() {
    for bad in [
        "git log | grep -qx \"a b\" || fail",
        "x |\n grep -Fxq y",
        "a | grep -m1 y",
        "a | grep --quiet y",
        "a | head -n1",
        "sh -c \"a | grep -Eq '^b'\"",
    ] {
        assert!(!early_exit_readers(bad).is_empty(), "{bad}");
    }
    for good in [
        "git log | grep -x y >/dev/null || fail",
        "a || grep -q y file",
        "grep -q y file",
        "a | sed -n 1p",
        "a | grep -v y | grep -E y >/dev/null",
    ] {
        assert!(early_exit_readers(good).is_empty(), "{good}");
    }
}

/// `ci/publish-image.sh` against a stand-in `docker` whose first `fails`
/// pushes answer "unknown blob", as GHCR did in image run 39047451574:
/// its exit status and the `docker` calls it made.
fn publish_image(fails: usize) -> (bool, Vec<String>) {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let docker = dir.path().join("docker");
    std::fs::write(
        &docker,
        format!(
            "#!/bin/sh\necho \"$*\" >> \"$LOG\"\ncase \"$1\" in\n  push)\n    n=$(grep -c '^push' \"$LOG\")\n    [ \"$n\" -gt {fails} ] || {{ echo 'unknown blob' >&2; exit 1; }} ;;\n  inspect) echo 'ghcr.io/x@sha256:0' ;;\nesac\n"
        ),
    )
    .unwrap();
    std::fs::set_permissions(&docker, std::fs::Permissions::from_mode(0o755)).unwrap();
    let log = dir.path().join("log");
    let path = format!(
        "{}:{}",
        dir.path().display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let status = std::process::Command::new(root().join("ci/publish-image.sh"))
        .args(["img:ci", "ghcr.io/x", "abc"])
        .env("PATH", path)
        .env("LOG", &log)
        .env("PUBLISH_RETRY_DELAY", "0")
        .env("GITHUB_STEP_SUMMARY", dir.path().join("summary"))
        .status()
        .unwrap();
    let calls = std::fs::read_to_string(&log)
        .unwrap_or_default()
        .lines()
        .map(str::to_owned)
        .collect();
    (status.success(), calls)
}

/// image.yml's publish jobs retry a push GHCR fails, up to three attempts.
#[test]
fn publishing_an_image_retries_a_failed_push() {
    let (ok, calls) = publish_image(1);
    assert!(ok, "{calls:?}");
    assert_eq!(
        calls,
        [
            "tag img:ci ghcr.io/x:abc",
            "push ghcr.io/x:abc",
            "push ghcr.io/x:abc",
            "tag img:ci ghcr.io/x:latest",
            "push ghcr.io/x:latest",
            "inspect --format {{index .RepoDigests 0}} ghcr.io/x:abc",
        ]
    );
    let (ok, calls) = publish_image(3);
    assert!(!ok, "{calls:?}");
    assert_eq!(calls.iter().filter(|c| c.starts_with("push")).count(), 3);
}
