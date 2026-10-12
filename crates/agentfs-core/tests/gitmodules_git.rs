//! `agentfs_core::gitmodules` against the real `git` binary (design §3.4).
//!
//! - The config parser must read every corpus file as
//!   `git config --file F --list -z --no-includes` does, and fail where it
//!   fails.
//! - The fsck checks must refuse exactly the `.gitmodules` blobs `git fsck`
//!   reports a `gitmodules*` message for, with one of git's ids.
//!
//! Skipped, with a message, when `git` is not installed. The CI `test` job
//! runs it against the runner's git.

use std::collections::BTreeSet;
use std::path::Path;
use std::process::{Command, Output, Stdio};

use agentfs_core::gitmodules::{self, ConfigEntry, CurlUrlCheck};

fn git(dir: &Path, args: &[&str], stdin: Option<&[u8]>) -> Output {
    let mut cmd = Command::new("git");
    cmd.args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("HOME", dir)
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@example.com")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@example.com")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn().expect("running git");
    {
        use std::io::Write as _;
        let mut input = child.stdin.take().unwrap();
        if let Some(bytes) = stdin {
            input.write_all(bytes).unwrap();
        }
    }
    child.wait_with_output().unwrap()
}

fn have_git() -> bool {
    match Command::new("git").arg("--version").output() {
        Ok(out) if out.status.success() => {
            eprintln!("using {}", String::from_utf8_lossy(&out.stdout).trim());
            true
        }
        _ => {
            eprintln!("skipped: git is not installed");
            false
        }
    }
}

/// Config files for the parser differential.
const PARSER_CORPUS: &[&[u8]] = &[
    b"",
    b"\n\n",
    b"# only a comment\n; and another\n",
    b"[core]\n\tbare = false\n",
    b"[submodule \"lib\"]\n\tpath = lib\n\turl = https://example.com/lib.git\n",
    b"[Submodule \"MiXeD\"]\n\tPATH = A\n\tUrl=B\n",
    b"[Submodule.MiXeD]\nPath=A\n",
    b"[a.b.c]\nk=v\n",
    b"[a \"x.y\"]\nk=v\n",
    b"[a \"q\\\"uote\\\\back\\x\"]\nk=v\n",
    b"[a   \"spaces before\"]\nk=v\n",
    b"[a\t\"tab\"]\nk=v\n",
    b"[s]\nflag\nother\n",
    b"[s]\nk = v # comment\n",
    b"[s]\nk = v ; comment\n",
    b"[s]\nk = \"v # not a comment\"\n",
    b"[s]\nk =   padded   \n",
    b"[s]\nk = a\\tb\\nc\\\\d\\\"e\\bf\n",
    b"[s]\nk = one \\\n  two\n",
    b"[s]\nk = x\"y\"z\n",
    b"[s]\nk = \"\" x\n",
    b"[s]\nk = a \"b\" \n",
    b"[s]\nk =\n",
    b"[s]\nk=\"\"\n",
    b"[s]\r\nk = v\r\nj = w\r\n",
    b"[s]\rk=v\n",
    b"[s]\nk = v\r",
    b"\xef\xbb\xbf[s]\nk=v\n",
    b"[s]\nk=v",
    b"[s]\n  k  =  v  \n\t\tj\t=\tw\t\n",
    b"[s] k = v\n",
    b"[s][t]k=v\n",
    b"a=b\n",
    b"[s]\nkey-with-dash=1\nKEY2=2\n",
    b"[s]\nk=\xc3\xa9t\xc3\xa9\n",
    b"[s \"\xc3\xa9\"]\nk=v\n",
    b"[submodule.]\nurl\n",
    b"[s]\nk = v\n[t \"u\"]\n[v]\nw\n",
    // Errors.
    b"[s]\nk=\"unterminated\n",
    b"[s]\nk=bad\\escape\n",
    b"[s]\nk # comment\n",
    b"[s]\n1key=v\n",
    b"[s\n",
    b"[]\n",
    b"[s \"x]\n",
    b"[s \"x\" ]\n",
    b"[s x]\n",
    b"[s_x]\n",
    b"[s]\nk_ey=v\n",
    b"\xef\xbbx",
    b"[s]\nk=v\n=x\n",
    b"[s]\n-k=v\n",
    b"[s \"x\"\n]\n",
    b"[s]\nk=a\\",
    b"[s]\nk=\"a\\\"\n",
];

/// Parses `git config --list -z`: `name\nvalue\0` or `name\0`.
fn git_entries(out: &[u8]) -> Vec<ConfigEntry> {
    out.split(|&b| b == 0)
        .filter(|item| !item.is_empty())
        .map(|item| match item.iter().position(|&b| b == b'\n') {
            Some(i) => ConfigEntry {
                name: item[..i].to_vec(),
                value: Some(item[i + 1..].to_vec()),
            },
            None => ConfigEntry {
                name: item.to_vec(),
                value: None,
            },
        })
        .collect()
}

#[test]
fn parser_matches_git_config() {
    if !have_git() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("config");
    let mut failures = Vec::new();
    for content in PARSER_CORPUS {
        std::fs::write(&file, content).unwrap();
        let out = git(
            dir.path(),
            &[
                "config",
                "--file",
                "config",
                "--list",
                "-z",
                "--no-includes",
            ],
            None,
        );
        let ours = gitmodules::parse(content);
        let shown = String::from_utf8_lossy(content);
        if out.status.success() {
            if let Some(e) = ours.error {
                failures.push(format!("{shown:?}: git parses it, we fail at {e:?}"));
            } else if ours.entries != git_entries(&out.stdout) {
                failures.push(format!(
                    "{shown:?}: git {:?}, ours {:?}",
                    git_entries(&out.stdout),
                    ours.entries
                ));
            }
        } else if ours.error.is_none() {
            failures.push(format!(
                "{shown:?}: git fails ({}), we parse it",
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

/// URLs for the fsck differential, each put in a `url` value.
const URLS: &[&str] = &[
    "./bar/baz/foo.git",
    "https://example.com/foo.git",
    "http://example.com:80/deeper/foo.git",
    "-sneaky-option",
    "./-sneaky-option",
    "../-sneaky-option",
    "../relative/path",
    "../../relative/path",
    "./%0ahost=example.com/foo.git",
    "./%0Ahost=example.com/foo.git",
    "../a\nb",
    "git://example.com/%0a",
    "git://example.com/x.git",
    "https://one.example.com/evil?%0ahost=two.example.com",
    "https:///example.com/foo.git",
    "http://example.com:test/foo.git",
    "https::example.com/foo.git",
    "http:::example.com/foo.git",
    "http::https:///x",
    "http::https://example.com/x",
    "../:x",
    ".././/x",
    "..\\..\\:x",
    "https://example.com:0/x",
    "https://example.com:65535/x",
    "https://example.com:65536/x",
    "https://example.com:123456/x",
    "https://example.com:000000080/x",
    "https://exa mple.com/x",
    "https://example.com/../x",
    "https://example.com/a/../../x",
    "https://example.com/a%0a/../b",
    "https://example.com/%zz",
    "https://example.com/%4",
    "https://example.com/a%0/b",
    "https://example.com/%00",
    "https://u%0a@example.com/x",
    "https://user:pass@example.com:8443/a/./b/../c?q=1#f",
    "https://[::1]:8080/x",
    "ftp://example.com/a\nb",
    "ftps://example.com/x",
    "https://example.com#%0a",
    "git@github.com:a/b.git",
    "ssh://example.com/x",
    "/abs/path",
    "./%00",
    "",
];

/// `.gitmodules` files for the fsck differential, beyond the URLs.
const GITMODULES: &[&[u8]] = &[
    b"",
    b"[submodule \"lib\"]\n\tpath = lib\n\turl = https://example.com/lib.git\n",
    b"[submodule \"../../modules/evil\"]\n\tpath = x\n\turl = ./x\n",
    b"[submodule \"..\"]\nurl=./x\n",
    b"[submodule \"a\\\\..\"]\nurl=./x\n",
    b"[submodule \"x\"]\npath = -p\n",
    b"[submodule \"x\"]\npath = p\nupdate = !rm -rf /\n",
    b"[submodule \"x\"]\nupdate = rebase\n",
    b"[submodule \"x\"]\nurl\n",
    b"[submodule.]\nurl\n",
    b"[submodule]\nurl=-x\n",
    b"[other \"..\"]\nurl=-x\n",
    b"[submodule \"x\"]\nbranch = -x\n",
    b"[submodule \"x\"]\nurl = ./x\n[\n",
    b"[submodule \"x\"]\npath = -p\n[\n",
    b"[submodule \"x\"\n",
    b"\xef\xbbx",
];

fn quote(value: &str) -> String {
    let mut out = String::from("\"");
    for c in value.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// The `gitmodules*` message ids `git fsck` reports for a tree holding
/// `content` at `name` (a top-level name, or `dir/name`).
fn fsck_ids(dir: &Path, name: &str, content: &[u8]) -> BTreeSet<String> {
    let repo = tempfile::tempdir_in(dir).unwrap();
    let r = repo.path();
    assert!(git(r, &["init", "-q"], None).status.success());
    let out = git(r, &["hash-object", "-w", "--stdin"], Some(content));
    let blob = String::from_utf8(out.stdout).unwrap().trim().to_owned();
    let mut tree_input = String::new();
    let (parent, leaf) = match name.split_once('/') {
        Some((d, n)) => (Some(d), n),
        None => (None, name),
    };
    tree_input.push_str(&format!("100644 blob {blob}\t{leaf}\n"));
    let out = git(r, &["mktree"], Some(tree_input.as_bytes()));
    let mut tree = String::from_utf8(out.stdout).unwrap().trim().to_owned();
    if let Some(d) = parent {
        let input = format!("040000 tree {tree}\t{d}\n");
        let out = git(r, &["mktree"], Some(input.as_bytes()));
        tree = String::from_utf8(out.stdout).unwrap().trim().to_owned();
    }
    let out = git(r, &["commit-tree", &tree, "-m", "m"], None);
    let commit = String::from_utf8(out.stdout).unwrap().trim().to_owned();
    assert!(
        git(r, &["update-ref", "refs/heads/main", &commit], None)
            .status
            .success()
    );
    let out = git(r, &["fsck", "--no-dangling", "--no-progress"], None);
    let report = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let mut ids = BTreeSet::new();
    for (i, _) in report.match_indices("gitmodules") {
        let id: String = report[i..]
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric())
            .collect();
        if report[i + id.len()..].starts_with(':') {
            ids.insert(id);
        }
    }
    ids
}

#[test]
fn checks_match_git_fsck() {
    if !have_git() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let mut cases: Vec<(String, Vec<u8>)> = GITMODULES
        .iter()
        .map(|c| (".gitmodules".to_owned(), c.to_vec()))
        .collect();
    // fsck checks every spelling, in any directory.
    let bad = b"[submodule \"x\"]\nurl = -x\n".to_vec();
    for name in [".GITMODULES", "gitmod~1", ".gitmodules.", "sub/.gitmodules"] {
        cases.push((name.to_owned(), bad.clone()));
    }

    let checks = agentfs_core::checks::Checks::default();
    let mut failures = Vec::new();
    for (name, content) in &cases {
        let theirs = fsck_ids(dir.path(), name, content);
        let path = agentfs_core::paths::RepoPath::new(name.as_str()).unwrap();
        let ours = checks.check_blob(&path, content);
        let shown = String::from_utf8_lossy(content);
        let ok = match &ours {
            None => theirs.is_empty(),
            Some(refusal) => refusal.fsck_id().is_some_and(|id| theirs.contains(id)),
        };
        if !ok {
            failures.push(format!(
                "{name} {shown:?}: git fsck {theirs:?}, ours {:?}",
                ours.map(|r| r.fsck_id())
            ));
        }
    }

    // URLs: git changed its check for http(s) and ftp(s) URLs in 2.40
    // (`CurlUrlCheck`). The installed git must agree exactly with one of
    // the two, and agentfs, which refuses what either refuses, must refuse
    // whatever it refuses.
    let path = agentfs_core::paths::RepoPath::new(".gitmodules").unwrap();
    let mut verdicts = Vec::new();
    for url in URLS {
        let content = format!("[submodule \"x\"]\n\tpath = x\n\turl = {}\n", quote(url));
        let theirs = fsck_ids(dir.path(), ".gitmodules", content.as_bytes());
        let git_ok = theirs.is_empty();
        if !git_ok && !theirs.contains("gitmodulesUrl") {
            failures.push(format!("{url:?}: git fsck {theirs:?}"));
        }
        let ours = checks.check_blob(&path, content.as_bytes());
        if !git_ok && ours.as_ref().and_then(|r| r.fsck_id()) != Some("gitmodulesUrl") {
            failures.push(format!("{url:?}: git fsck {theirs:?}, ours {ours:?}"));
        }
        verdicts.push((url, git_ok));
    }
    let variant = [CurlUrlCheck::Normalize, CurlUrlCheck::Credential]
        .into_iter()
        .find(|v| {
            verdicts.iter().all(|(url, git_ok)| {
                gitmodules::check_submodule_url_as(url.as_bytes(), *v) == *git_ok
            })
        });
    match variant {
        Some(v) => eprintln!("git's submodule url check matches {v:?}"),
        None => {
            for v in [CurlUrlCheck::Normalize, CurlUrlCheck::Credential] {
                for (url, git_ok) in &verdicts {
                    if gitmodules::check_submodule_url_as(url.as_bytes(), v) != *git_ok {
                        failures.push(format!("{v:?} {url:?}: git accepts: {git_ok}"));
                    }
                }
            }
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}
