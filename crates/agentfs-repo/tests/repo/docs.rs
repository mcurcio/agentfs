//! The documentation against the workspace and itself: the module map and
//! the ranks of docs/architecture.md, links and anchors, the book's table of
//! contents, and the runbooks' pushes.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use agentfs_repo::doc_tables::{self, key_cells};
use agentfs_repo::workspace::{Rank, Workspace};

use crate::{Comments, is_separator, lex, markdown_files, root};

/// The `.rs` files under a member's `src/`, relative to it, without the
/// `tests.rs` files of test-only submodules.
fn modules(dir: &Path) -> BTreeSet<String> {
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
    let src = root().join(dir).join("src");
    let mut out = BTreeSet::new();
    walk(&src, &src, &mut out);
    out
}

/// Every member of the workspace has a section in docs/architecture.md's
/// module map, with one row per module.
#[test]
fn every_module_is_in_the_architecture_map() {
    let sections = doc_tables::sections(&doc_tables::read_doc("architecture.md"));
    let workspace = Workspace::load();
    assert!(
        workspace.members.len() > 10,
        "only {} members: is cargo metadata read right?",
        workspace.members.len()
    );
    let mut problems = Vec::new();
    for member in &workspace.members {
        let krate = &member.name;
        let heading = format!("`{krate}`");
        let Some(section) = doc_tables::section(&sections, &heading) else {
            problems.push(format!(
                "add a section headed ### {heading} with a module table"
            ));
            continue;
        };
        let documented = key_cells(section);
        let actual = modules(&member.dir);
        assert!(!actual.is_empty(), "no modules found for {krate}");
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

/// How docs/architecture.md's "Ranks" table differs from the workspace's
/// ranks: its first column's code spans are the ranks in order, its second
/// column's each rank's `uses`.
fn ranks_table_problems(markdown: &str, ranks: &[Rank]) -> Vec<String> {
    let sections = doc_tables::sections(markdown);
    let Some(section) = doc_tables::section(&sections, "Ranks") else {
        return vec!["add a section headed ## Ranks with a table of the ranks".into()];
    };
    let rows: Vec<&Vec<String>> = section.rows().collect();
    let documented: Vec<String> = rows
        .iter()
        .flat_map(|row| doc_tables::code_spans(&row[0]))
        .collect();
    let expected: Vec<&str> = ranks.iter().map(|r| r.name.as_str()).collect();
    let mut problems = Vec::new();
    if documented != expected {
        problems.push(format!(
            "the ranks, in order, are {expected:?}; the table lists {documented:?}"
        ));
    }
    for row in &rows {
        for name in doc_tables::code_spans(&row[0]) {
            let Some(rank) = ranks.iter().find(|r| r.name == name) else {
                continue;
            };
            let uses = row
                .get(1)
                .map(|c| doc_tables::code_spans(c))
                .unwrap_or_default();
            if uses != rank.uses {
                problems.push(format!(
                    "`{name}` uses {:?} in Cargo.toml; the table says {uses:?}",
                    rank.uses
                ));
            }
        }
    }
    problems
}

#[test]
fn the_ranks_are_documented() {
    let workspace = Workspace::load();
    assert!(
        workspace.ranks.len() > 3,
        "too few ranks: {:?}",
        workspace.ranks
    );
    let problems = ranks_table_problems(&doc_tables::read_doc("architecture.md"), &workspace.ranks);
    assert!(
        problems.is_empty(),
        "docs/architecture.md, \"Ranks\", and [workspace.metadata.agentfs] ranks disagree:\n  {}",
        problems.join("\n  ")
    );
}

#[test]
fn the_ranks_table_is_read_in_order() {
    let ranks = [
        Rank {
            name: "a".into(),
            uses: vec!["a".into()],
        },
        Rank {
            name: "b".into(),
            uses: vec![],
        },
    ];
    let page = |rows: &str| format!("## Ranks\n\n| Rank | Uses |\n|---|---|\n{rows}\n## Next\n");
    assert_eq!(
        ranks_table_problems(&page("| `a` | `a` |\n| `b` | none |\n"), &ranks),
        Vec::<String>::new()
    );
    assert_eq!(
        ranks_table_problems(&page("| `b` | none |\n| `a` | `a` |\n"), &ranks).len(),
        1,
        "two rows swapped"
    );
    assert_eq!(
        ranks_table_problems(&page("| `a` | none |\n| `b` | none |\n"), &ranks).len(),
        1,
        "a use missing"
    );
    assert_eq!(ranks_table_problems("# none\n", &ranks).len(), 1);
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
            .strip_prefix(root())
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

/// Why the `git push` whose arguments (after `push`) are `args` breaks the
/// runbooks' push rule, if it does (F8 S2): a runbook writes only refs
/// literally under `refs/agentfs/`, and never forces. A volume's own
/// branch is replaced only by `agentfs push --force-with-lease`.
/// Two leases are allowed, each on a destination of the same push
/// literally under `refs/agentfs/`: the create-only one,
/// `--force-with-lease=<ref>:` with an empty expected value (patch
/// submission), which can create a ref, never replace one;
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
        if is_separator(word) {
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
    let dir = root().join("docs/runbooks");
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
            let words = lex(&std::mem::take(&mut pending), Comments::End);
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
        let words = lex(line, Comments::End);
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
    let docs = root().join("docs");
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
