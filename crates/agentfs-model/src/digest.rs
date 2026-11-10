//! A run's outcome as one digest, read only from git, so a refactor can show it
//! changed no outcome (VERIFYING.md, "Refactoring without changing outcomes").
//!
//! The text holds every commit reachable from `refs/heads` and
//! `refs/collisions`, numbered by a depth-first walk from those refs in label
//! order: its git tree, its parents by number and its whole message. A commit id
//! depends on the commit's date, and under [`crate::git::with_reversed_dates`]
//! on how many git processes ran before it, so no commit id is in the text. Two
//! fields of a message name something that a refactor may rename or that is an
//! id: `Agentfs-Tree` and the tree in `Agentfs-Collision` become a label (`T0`
//! the root, `T<fork id>` a fork), and the loser in `Agentfs-Collision` its
//! number. New trailers are in by default. Collision and transaction refs stay
//! verbatim.

use std::collections::BTreeMap;
use std::path::Path;

use crate::git::{git, git_in};
use crate::world::{COLLISION, Tree, World};

/// Where `World::drop` writes each run's digest.
pub const DIGEST_DIR: &str = "AGENTFS_MODEL_DIGEST_DIR";

/// Set, it writes each run's canonical text beside its digest, to diff.
const DIGEST_TEXT: &str = "AGENTFS_MODEL_DIGEST_TEXT";

const TREE: &str = "Agentfs-Tree";

/// The label of every tree's name and ref. Tree names reach the digest only
/// through here.
fn labels(w: &World) -> BTreeMap<String, String> {
    let trees = std::iter::once((Tree::Main, "T0".to_string()))
        .chain(w.forks.keys().map(|f| (Tree::Fork(*f), format!("T{f}"))));
    trees
        .flat_map(|(t, label)| [(World::name(t), label.clone()), (World::branch(t), label)])
        .collect()
}

/// A commit as git holds it.
struct Commit {
    parents: Vec<String>,
    tree: String,
    message: String,
}

fn commits(w: &World) -> BTreeMap<String, Commit> {
    let log = git(
        w.repo(),
        &[
            "log",
            "--glob=refs/heads",
            "--glob=refs/collisions",
            "--format=%x01%H%x1f%P%x1f%T%x1f%B%x02",
        ],
    );
    log.split('\u{1}')
        .filter(|rec| !rec.trim().is_empty())
        .map(|rec| {
            let rec = rec.trim_end_matches(['\n', '\u{2}']);
            let mut fields = rec.splitn(4, '\u{1f}');
            let mut field = || fields.next().expect("a commit's four fields").to_string();
            let (id, parents, tree, message) = (field(), field(), field(), field());
            let parents = parents.split_whitespace().map(str::to_string).collect();
            (
                id,
                Commit {
                    parents,
                    tree,
                    message,
                },
            )
        })
        .collect()
}

/// Each ref of `refs/heads` and `refs/collisions` by its label, with its tip.
fn tips(w: &World, labels: &BTreeMap<String, String>) -> Vec<(String, String)> {
    let refs = git(
        w.repo(),
        &[
            "for-each-ref",
            "--format=%(refname) %(objectname)",
            "refs/heads",
            "refs/collisions",
        ],
    );
    let mut tips: Vec<(String, String)> = refs
        .lines()
        .map(|l| {
            let (r, tip) = l.split_once(' ').expect("a ref and its commit");
            let label = labels.get(r).map_or(r, String::as_str);
            (label.to_string(), tip.to_string())
        })
        .collect();
    tips.sort();
    tips
}

/// Numbers every commit reachable from `tips`, parents before children, in the
/// order of a depth-first walk from the tips as given.
fn walk(commits: &BTreeMap<String, Commit>, tips: &[(String, String)]) -> Vec<String> {
    let mut seen: BTreeMap<&str, ()> = BTreeMap::new();
    let mut order = Vec::new();
    for (_, tip) in tips {
        let mut stack = vec![(tip.as_str(), false)];
        while let Some((c, expanded)) = stack.pop() {
            if seen.contains_key(c) {
                continue;
            }
            if expanded {
                seen.insert(c, ());
                order.push(c.to_string());
            } else {
                stack.push((c, true));
                stack.extend(commits[c].parents.iter().rev().map(|p| (p.as_str(), false)));
            }
        }
    }
    order
}

/// `line` with the two fields a refactor may rename, or that hold a commit id,
/// normalised. `Agentfs-Collision`'s layout is `Decision::render`'s.
fn canonical_line(
    line: &str,
    labels: &BTreeMap<String, String>,
    number: &BTreeMap<&str, usize>,
) -> String {
    let label = |v: &str| labels.get(v).map_or(v, String::as_str).to_string();
    if let Some(v) = line.strip_prefix(&format!("{TREE}: ")) {
        return format!("{TREE}: {}", label(v));
    }
    if let Some(v) = line.strip_prefix(&format!("{COLLISION}: ")) {
        let mut f: Vec<String> = v.split(' ').map(str::to_string).collect();
        assert!(f.len() == 8, "a collision record: {v}");
        f[0] = label(&f[0]);
        // A loser no ref keeps is a broken run: its id shows it as a difference.
        if let Some(n) = number.get(f[3].as_str()) {
            f[3] = n.to_string();
        }
        return format!("{COLLISION}: {}", f.join(" "));
    }
    line.to_string()
}

/// The run as one canonical text (this module's doc comment).
pub fn canonical_text(w: &World) -> String {
    let labels = labels(w);
    let commits = commits(w);
    let tips = tips(w, &labels);
    let order = walk(&commits, &tips);
    let number: BTreeMap<&str, usize> = order
        .iter()
        .enumerate()
        .map(|(n, c)| (c.as_str(), n))
        .collect();
    let mut out = String::new();
    for c in &order {
        let commit = &commits[c];
        let parents: Vec<String> = commit
            .parents
            .iter()
            .map(|p| number[p.as_str()].to_string())
            .collect();
        out.push_str(&format!(
            "C {} tree={} parents={}\n",
            number[c.as_str()],
            commit.tree,
            parents.join(",")
        ));
        for line in commit.message.lines() {
            out.push_str(&canonical_line(line, &labels, &number));
            out.push('\n');
        }
    }
    for (label, tip) in &tips {
        out.push_str(&format!("R {label} {}\n", number[tip.as_str()]));
    }
    out
}

/// The sha of [`canonical_text`], as `git hash-object` gives it.
pub fn digest(w: &World) -> String {
    git_in(w.repo(), &["hash-object", "--stdin"], &canonical_text(w))
}

/// Writes `<name>.digest` to `dir`, and `<name>.text` when [`DIGEST_TEXT`] is
/// set. A digest that cannot be written must fail the job.
pub(crate) fn write(w: &World, dir: &Path, name: &str) {
    let text = canonical_text(w);
    let sha = git_in(w.repo(), &["hash-object", "--stdin"], &text);
    let write = |ext: &str, body: &str| {
        let file = dir.join(format!("{name}.{ext}"));
        std::fs::create_dir_all(dir)
            .and_then(|()| std::fs::write(&file, body))
            .unwrap_or_else(|e| panic!("write {}: {e}", file.display()));
    };
    write("digest", &format!("{sha}\n"));
    if std::env::var_os(DIGEST_TEXT).is_some() {
        write("text", &text);
    }
}
