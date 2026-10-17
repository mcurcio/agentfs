//! Checks the documentation against the code (design §4.2, §4.3):
//!
//! - every git config key agentfs reads ([`config::KEYS`]) has a row in
//!   its section of docs/configuration.md, and every key row there is a
//!   key agentfs reads;
//! - the same for the operator attributes ([`rules::OPERATOR_ATTRIBUTES`]);
//! - every relative link and `#anchor` in README.md and docs/ resolves;
//! - every CI job runs on the pinned runner, `ubuntu-24.04` (design §18);
//! - every script under ci/ is executable;
//! - every page under docs/ is in the book's docs/SUMMARY.md;
//! - every `git push` in a runbook writes only refs literally under
//!   `refs/agentfs/` and never forces (F8 S2).
//!
//! The flags and environment variables are checked by the binaries' own
//! unit tests (`crates/agentfs/src/reference_tests.rs`, and the
//! `reference_tests` module of `crates/agentfs-csi/src/main.rs`), which
//! can walk their clap definitions.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use agentfs_core::{config, rules};

#[path = "../../agentfs/src/doc_tables.rs"]
mod doc_tables;

use doc_tables::Section;

fn workspace() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn configuration() -> Vec<Section> {
    doc_tables::sections(&doc_tables::read_doc(
        env!("CARGO_MANIFEST_DIR"),
        "configuration.md",
    ))
}

/// The section of docs/configuration.md a key's row belongs in, and the
/// key as written in that table's first column.
fn home(key: &str) -> (&'static str, String) {
    if let Some(k) = key.strip_prefix("remote.<name>.") {
        ("`[remote \"<name>\"]`", k.to_owned())
    } else if key.starts_with("agentfs.identity.") {
        ("`[agentfs \"identity\"]`", key.to_owned())
    } else if key.starts_with("agentfs.fork.") {
        ("`[agentfs \"fork\"]`", key.to_owned())
    } else if key.starts_with("agentfs.start.") {
        ("`[agentfs \"start\"]`", key.to_owned())
    } else if key.starts_with("agentfs.") {
        ("`[agentfs]`", key.to_owned())
    } else if key.starts_with("lfs.") {
        ("`[lfs]`", key.to_owned())
    } else if key.starts_with("http.") {
        ("`[http]`", key.to_owned())
    } else if key.starts_with("include") {
        ("Includes", key.to_owned())
    } else {
        panic!("config::KEYS has {key}, which has no section in this test: add one")
    }
}

/// The key cells of a section's tables: first cells that are exactly one
/// code span.
fn key_cells(section: &Section) -> BTreeSet<String> {
    section
        .rows()
        .filter_map(|row| doc_tables::sole_code_span(&row[0]).map(str::to_owned))
        .collect()
}

#[test]
fn every_config_key_is_documented_in_its_section() {
    let sections = configuration();
    let mut expected: BTreeMap<&str, BTreeSet<String>> = BTreeMap::new();
    for key in config::KEYS {
        let (heading, cell) = home(key);
        expected.entry(heading).or_default().insert(cell);
    }
    let mut problems = Vec::new();
    for (heading, keys) in &expected {
        let Some(section) = doc_tables::section(&sections, heading) else {
            problems.push(format!("add a section headed {heading} with a key table"));
            continue;
        };
        let documented = key_cells(section);
        for key in keys.difference(&documented) {
            problems.push(format!(
                "add a row `| `{key}` | ... |` to the table under {heading}"
            ));
        }
        for key in documented.difference(keys) {
            problems.push(format!(
                "remove the row for `{key}` under {heading}, or add it to config::KEYS if \
                 agentfs reads it"
            ));
        }
    }
    assert!(
        problems.is_empty(),
        "docs/configuration.md and config::KEYS disagree:\n  {}",
        problems.join("\n  ")
    );
}

#[test]
fn every_operator_attribute_is_documented() {
    let sections = configuration();
    let section = doc_tables::section(&sections, "Operator attributes")
        .expect("docs/configuration.md has no `## Operator attributes` section");
    let documented = key_cells(section);
    let expected: BTreeSet<String> = rules::OPERATOR_ATTRIBUTES
        .iter()
        .map(|a| match *a {
            rules::RECORD_ATTR => format!("-{a}"),
            a => a.to_owned(),
        })
        .collect();
    let missing: Vec<_> = expected.difference(&documented).collect();
    let stale: Vec<_> = documented.difference(&expected).collect();
    assert!(
        missing.is_empty() && stale.is_empty(),
        "docs/configuration.md, Operator attributes: add rows for {missing:?}; remove (or \
         add to rules::OPERATOR_ATTRIBUTES) {stale:?}"
    );
}

/// The `.rs` files under a crate's `src/`, relative to it, without the
/// `tests.rs` files of test-only submodules.
fn modules(krate: &str) -> BTreeSet<String> {
    fn walk(root: &Path, dir: &Path, out: &mut BTreeSet<String>) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                walk(root, &path, out);
            } else if path.extension().is_some_and(|e| e == "rs")
                && path.file_name().is_some_and(|n| n != "tests.rs")
            {
                let rel = path.strip_prefix(root).unwrap();
                out.insert(rel.to_string_lossy().replace('\\', "/"));
            }
        }
    }
    let root = workspace().join("crates").join(krate).join("src");
    let mut out = BTreeSet::new();
    walk(&root, &root, &mut out);
    out
}

#[test]
fn every_module_is_in_the_architecture_map() {
    let sections = doc_tables::sections(&doc_tables::read_doc(
        env!("CARGO_MANIFEST_DIR"),
        "architecture.md",
    ));
    let mut problems = Vec::new();
    for krate in ["agentfs-core", "agentfs-fuse", "agentfs", "agentfs-csi"] {
        let heading = format!("`{krate}`");
        let Some(section) = doc_tables::section(&sections, &heading) else {
            problems.push(format!(
                "add a section headed ### {heading} with a module table"
            ));
            continue;
        };
        let documented = key_cells(section);
        let actual = modules(krate);
        assert!(actual.len() > 3, "no modules found for {krate}");
        for module in actual.difference(&documented) {
            problems.push(format!("{krate}: add a row for `{module}`"));
        }
        for module in documented.difference(&actual) {
            problems.push(format!(
                "{krate}: remove the row for `{module}`: no such file"
            ));
        }
    }
    assert!(
        problems.is_empty(),
        "docs/architecture.md's module map is out of date:\n  {}",
        problems.join("\n  ")
    );
}

/// Every markdown file the link check covers: README.md and docs/**.
fn markdown_files() -> Vec<PathBuf> {
    fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
        let mut entries: Vec<_> = std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().path())
            .collect();
        entries.sort();
        for path in entries {
            if path.is_dir() {
                walk(&path, out);
            } else if path.extension().is_some_and(|e| e == "md") {
                out.push(path);
            }
        }
    }
    let root = workspace();
    let mut files = vec![root.join("README.md")];
    walk(&root.join("docs"), &mut files);
    files
}

/// GitHub's anchor for a heading: lower case, everything but letters,
/// digits, `-`, `_` and spaces dropped, spaces turned into `-`; repeats
/// get `-1`, `-2`, ...
fn slug(heading: &str) -> String {
    heading
        .to_lowercase()
        .chars()
        .filter(|c| c.is_alphanumeric() || matches!(c, '-' | '_' | ' '))
        .map(|c| if c == ' ' { '-' } else { c })
        .collect()
}

fn anchors(markdown: &str) -> BTreeSet<String> {
    let mut seen: BTreeMap<String, usize> = BTreeMap::new();
    let mut out = BTreeSet::new();
    for section in doc_tables::sections(markdown) {
        if section.level == 0 {
            continue;
        }
        let base = slug(&section.heading);
        let n = seen.entry(base.clone()).or_default();
        out.insert(if *n == 0 {
            base.clone()
        } else {
            format!("{base}-{n}")
        });
        *n += 1;
    }
    out
}

/// The targets of inline links (`[text](target)`) outside code, and of
/// reference definitions (`[label]: target`).
fn links(markdown: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut fence = false;
    for line in markdown.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            fence = !fence;
            continue;
        }
        if fence {
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix('[')
            && let Some((_, target)) = rest.split_once("]: ")
        {
            out.push(target.trim().to_owned());
            continue;
        }
        // Drop code spans, whose brackets are not links.
        let mut text = String::new();
        for (i, part) in line.split('`').enumerate() {
            if i % 2 == 0 {
                text.push_str(part);
            }
        }
        let mut rest = text.as_str();
        while let Some(at) = rest.find("](") {
            let after = &rest[at + 2..];
            let Some(end) = after.find(')') else { break };
            let target = after[..end].split_whitespace().next().unwrap_or("");
            out.push(target.to_owned());
            rest = &after[end + 1..];
        }
    }
    out
}

#[test]
fn relative_links_and_anchors_resolve() {
    let mut broken = Vec::new();
    let mut checked = 0;
    for file in markdown_files() {
        let markdown = std::fs::read_to_string(&file).unwrap();
        let shown = file
            .strip_prefix(workspace())
            .unwrap_or(&file)
            .display()
            .to_string();
        for target in links(&markdown) {
            if target.is_empty()
                || target.contains("://")
                || target.starts_with("mailto:")
                || target.starts_with('<')
            {
                continue;
            }
            checked += 1;
            let (path, anchor) = match target.split_once('#') {
                Some((path, anchor)) => (path, Some(anchor)),
                None => (target.as_str(), None),
            };
            let resolved = if path.is_empty() {
                file.clone()
            } else {
                file.parent().unwrap().join(path)
            };
            if !resolved.exists() {
                broken.push(format!("{shown}: {target}: no such file"));
                continue;
            }
            let Some(anchor) = anchor else { continue };
            if resolved.extension().is_none_or(|e| e != "md") {
                continue;
            }
            let anchors = anchors(&std::fs::read_to_string(&resolved).unwrap());
            if !anchors.contains(anchor) {
                broken.push(format!("{shown}: {target}: no heading with that anchor"));
            }
        }
    }
    assert!(
        checked > 50,
        "only {checked} links found: is the parser broken?"
    );
    assert!(
        broken.is_empty(),
        "broken links:\n  {}",
        broken.join("\n  ")
    );
}

#[test]
fn slugs_follow_github() {
    assert_eq!(slug("`[remote \"<name>\"]`"), "remote-name");
    assert_eq!(slug("4. Flags and environment"), "4-flags-and-environment");
    assert_eq!(slug("SSH remotes"), "ssh-remotes");
    assert_eq!(
        slug("Restores with fronts (§14)"),
        "restores-with-fronts-14"
    );
    let a = anchors("# A\n## Same\n## Same\n");
    assert!(a.contains("same") && a.contains("same-1"), "{a:?}");
    assert_eq!(
        links("see [x](a.md#b) and `[y](not)` [z]: c.md\n[ref]: d.md\n"),
        ["a.md#b", "d.md"]
    );
}

/// The runner every CI job uses (design §18). Pinned, not `-latest`, so a
/// new image cannot change the kernel, FUSE, kind or nginx under the
/// tests.
const RUNNER: &str = "ubuntu-24.04";

#[test]
fn workflows_pin_runners() {
    let dir = workspace().join(".github/workflows");
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

/// Every file under `dir`, recursively.
fn files_under(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            files_under(&path, out);
        } else {
            out.push(path);
        }
    }
}

/// Workflows and other scripts run the CI scripts directly, so each needs
/// its execute bit (a sourced library too, so none is missed).
#[test]
fn ci_scripts_are_executable() {
    use std::os::unix::fs::PermissionsExt;
    let mut files = Vec::new();
    files_under(&workspace().join("ci"), &mut files);
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
    let status = std::process::Command::new(workspace().join("ci/publish-image.sh"))
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

/// The shell words of `line`: split on blanks outside quotes, with the
/// quotes removed (`$VAR` is left as written, so a destination is checked
/// as the reader sees it).
fn shell_words(line: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut word = String::new();
    let mut quote = None;
    let mut in_word = false;
    for c in line.chars() {
        match (quote, c) {
            (None, '#') if !in_word => break,
            (None, ' ' | '\t') => {
                if in_word {
                    words.push(std::mem::take(&mut word));
                    in_word = false;
                }
            }
            (None, '\'' | '"') => {
                quote = Some(c);
                in_word = true;
            }
            (Some(q), c) if c == q => quote = None,
            (None, ';' | '|' | '&' | '(' | ')') => {
                if in_word {
                    words.push(std::mem::take(&mut word));
                    in_word = false;
                }
                words.push(c.to_string());
            }
            (_, c) => {
                word.push(c);
                in_word = true;
            }
        }
    }
    if in_word {
        words.push(word);
    }
    words
}

/// Why the `git push` whose arguments (after `push`) are `args` breaks the
/// runbooks' push rule, if it does (F8 S2): a runbook writes only refs
/// literally under `refs/agentfs/`, and never forces. A volume's own
/// branch is replaced only by `agentfs push --force-with-lease` (§15).
/// Two leases are allowed, each on a destination of the same push
/// literally under `refs/agentfs/`: the create-only one,
/// `--force-with-lease=<ref>:` with an empty expected value (patch
/// submission, §12 and §19.1), which can create a ref, never replace one;
/// and `--force-with-lease=<ref>:<oid>` on a ref the push deletes
/// (`--delete`, or a `:<ref>` refspec), which deletes it only while it
/// holds the oid listed (history rewrite, step 9).
fn push_rule_breach(args: &[String]) -> Option<String> {
    const TAKES_VALUE: &[&str] = &["--repo", "--receive-pack", "--exec", "-o", "--push-option"];
    const WHOLESALE: &[&str] = &["--all", "--branches", "--mirror", "--tags", "--prune"];
    let mut leases = Vec::new();
    let mut positional = Vec::new();
    let mut delete_all = false;
    let mut words = args.iter();
    while let Some(word) = words.next() {
        let word = word.as_str();
        if matches!(word, ";" | "|" | "&" | "(" | ")") {
            break;
        }
        if word == "--force" || word == "-f" {
            return Some(format!("forces ({word})"));
        }
        if let Some(lease) = word.strip_prefix("--force-with-lease") {
            leases.push(lease.strip_prefix('=').unwrap_or("").to_owned());
            continue;
        }
        if WHOLESALE.contains(&word) {
            return Some(format!("pushes refs wholesale ({word})"));
        }
        if TAKES_VALUE.contains(&word) {
            words.next();
            continue;
        }
        if let Some(short) = word.strip_prefix('-')
            && !short.starts_with('-')
            && short.contains('f')
        {
            return Some(format!("forces ({word})"));
        }
        if word == "--delete" || word == "-d" {
            delete_all = true;
            continue;
        }
        if word.starts_with('-') {
            continue;
        }
        positional.push(word.to_owned());
    }
    // The first is the repository; the rest are refspecs.
    let refspecs = positional.get(1..).unwrap_or_default();
    if refspecs.is_empty() {
        return Some("names no refspec, so git picks what to push".into());
    }
    let mut destinations = Vec::new();
    let mut deleted = Vec::new();
    for spec in refspecs {
        if spec.starts_with('+') {
            return Some(format!("forces ({spec})"));
        }
        let destination = spec.rsplit_once(':').map_or(spec.as_str(), |(_, d)| d);
        if !destination.starts_with("refs/agentfs/") {
            return Some(format!(
                "writes {destination}, not literally under refs/agentfs/"
            ));
        }
        destinations.push(destination);
        if delete_all || spec.starts_with(':') {
            deleted.push(destination);
        }
    }
    for lease in leases {
        match lease.split_once(':') {
            Some((name, "")) if destinations.contains(&name) => {}
            Some((name, _)) if deleted.contains(&name) => {}
            _ => {
                return Some(format!(
                    "uses --force-with-lease{}{lease}: only `agentfs push \
                     --force-with-lease` replaces a branch",
                    if lease.is_empty() { "" } else { "=" }
                ));
            }
        }
    }
    None
}

/// F8 S2: no runbook writes a branch, or forces anything. In every fenced
/// block of docs/runbooks/*.md, a `git push` must write only refs
/// literally under `refs/agentfs/`, with no `--force`, `-f`, `+` refspec
/// or lease (but a create-only one, or one on a ref the push deletes,
/// [`push_rule_breach`]): a volume's own
/// branch is replaced only through `agentfs push --force-with-lease`, which
/// checks it shares history with the head.
#[test]
fn runbooks_push_only_the_namespace() {
    let dir = workspace().join("docs/runbooks");
    let mut pages: Vec<_> = std::fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|e| e == "md"))
        .collect();
    pages.sort();
    assert!(pages.len() > 5, "no runbooks found in {}", dir.display());
    let mut pushes = 0;
    let mut problems = Vec::new();
    for page in &pages {
        let text = std::fs::read_to_string(page).unwrap();
        let name = page.file_name().unwrap().to_string_lossy().into_owned();
        let mut fence = false;
        let mut pending = String::new();
        let mut start = 0;
        for (n, line) in text.lines().enumerate() {
            let trimmed = line.trim_start();
            if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
                fence = !fence;
                pending.clear();
                continue;
            }
            if !fence {
                continue;
            }
            if pending.is_empty() {
                start = n + 1;
            }
            // A backslash continues the command on the next line.
            if let Some(head) = line.strip_suffix('\\') {
                pending.push_str(head);
                pending.push(' ');
                continue;
            }
            pending.push_str(line);
            let words = shell_words(&std::mem::take(&mut pending));
            for (i, word) in words.iter().enumerate() {
                if word != "git" {
                    continue;
                }
                // git's own options before the subcommand.
                let mut j = i + 1;
                while let Some(option) = words.get(j).filter(|w| w.starts_with('-')) {
                    j += if matches!(option.as_str(), "-C" | "-c" | "--git-dir" | "--work-tree") {
                        2
                    } else {
                        1
                    };
                }
                if words.get(j).map(String::as_str) != Some("push") {
                    continue;
                }
                pushes += 1;
                if let Some(why) = push_rule_breach(&words[j + 1..]) {
                    problems.push(format!("docs/runbooks/{name}:{start}: `git push` {why}"));
                }
            }
        }
    }
    assert!(
        pushes > 3,
        "found only {pushes} git push commands in the runbooks"
    );
    assert!(problems.is_empty(), "{}", problems.join("\n"));
}

#[test]
fn the_push_rule_reads_git_push_as_git_does() {
    let breach = |line: &str| {
        let words = shell_words(line);
        push_rule_breach(&words[words.iter().position(|w| w == "push").unwrap() + 1..])
    };
    for ok in [
        r#"git push -q "$URL" "refs/theirs/branch:refs/agentfs/$VOL/overwritten/$TS""#,
        r#"git --git-dir "$S" push -q "$URL" --delete "refs/agentfs/$VOL/diverged/$ref""#,
        r#"git push "$URL" --force-with-lease="refs/agentfs/$VOL/inbox/$N:" "$OID:refs/agentfs/$VOL/inbox/$N""#,
        "git push \"$URL\" ':refs/agentfs/v/finish' # a comment --force",
        r#"git push -q "$URL" --force-with-lease="refs/agentfs/$VOL/$REF:$OID" --delete "refs/agentfs/$VOL/$REF""#,
        r#"git push "$URL" --force-with-lease="refs/agentfs/v/x:$OID" ":refs/agentfs/v/x""#,
    ] {
        assert_eq!(breach(ok), None, "{ok}");
    }
    for bad in [
        r#"git push -q --force-with-lease="$BRANCH:$TIP" "$URL" "$BRANCH:$BRANCH""#,
        r#"git push "$URL" "$OID:$REF""#,
        r#"git push --force "$URL" "x:refs/agentfs/v/status""#,
        r#"git push -qf "$URL" "x:refs/agentfs/v/status""#,
        r#"git push "$URL" "+x:refs/agentfs/v/status""#,
        r#"git push "$URL" "refs/heads/agentfs/v""#,
        r#"git push "$URL" --force-with-lease="refs/agentfs/v/x:abc" "y:refs/agentfs/v/x""#,
        r#"git push "$URL" --force-with-lease="refs/agentfs/v/y:abc" --delete "refs/agentfs/v/x""#,
        r#"git push "$URL" --delete "$REF""#,
        r#"git push --mirror "$URL""#,
        r#"git push "$URL""#,
    ] {
        assert!(breach(bad).is_some(), "{bad}");
    }
}

/// The book (book.toml) renders only the pages docs/SUMMARY.md lists, so a
/// page left out of it is unreachable there and every link to it breaks.
#[test]
fn every_page_is_in_the_book() {
    let docs = workspace().join("docs");
    let summary = std::fs::read_to_string(docs.join("SUMMARY.md")).unwrap();
    let listed: BTreeSet<String> = links(&summary).into_iter().collect();
    let missing: Vec<_> = markdown_files()
        .iter()
        .filter_map(|p| p.strip_prefix(&docs).ok())
        .map(|p| p.display().to_string())
        .filter(|p| p != "SUMMARY.md" && !listed.contains(p))
        .collect();
    assert!(
        missing.is_empty(),
        "pages missing from docs/SUMMARY.md:\n  {}",
        missing.join("\n  ")
    );
}
