//! A run's outcome as one digest, read only from git, so a refactor can show it
//! changed no outcome (VERIFYING.md, "Refactoring without changing outcomes").
//!
//! The text holds every commit reachable from `refs/heads` and
//! `refs/collisions`, numbered by a depth-first walk from those refs in label
//! order: its git tree, its parents by number and its whole message. Then each
//! of those refs, and each the shared remote holds, with the number of its tip.
//! A commit id depends on the commit's date, and under
//! [`crate::git::with_reversed_dates`] on how many git processes ran before it,
//! so no commit id is in the text. Every tree name, as a ref or as a message's
//! `Agentfs-Tree` or `Agentfs-Collision` field, becomes a label: `T0` the root,
//! `T<fork id>` a fork, `X<id>` a transaction. The names come from the one place
//! that names trees ([`World::name`], [`World::branch`], [`World::txn_name`],
//! [`World::txn_branch`]), so renaming a tree changes no digest. The loser in
//! `Agentfs-Collision` becomes its number. New trailers are in by default.
//! Collision refs stay verbatim.

use std::collections::BTreeMap;
use std::path::Path;

use crate::calls::TreeId;
use crate::calls::TxnId;
use crate::git::{git, git_in, try_git};
use crate::world::{COLLISION, World};

/// Where `World::drop` writes each run's digest.
pub const DIGEST_DIR: &str = "AGENTFS_MODEL_DIGEST_DIR";

/// Set, it writes each run's canonical text beside its digest, to diff.
const DIGEST_TEXT: &str = "AGENTFS_MODEL_DIGEST_TEXT";

const TREE: &str = "Agentfs-Tree";

/// The label of each tree's name and ref: the root, forks `forks` and
/// transactions 1 to `txns`.
fn labels_of(trees: impl IntoIterator<Item = TreeId>, txns: TxnId) -> BTreeMap<String, String> {
    let trees = trees.into_iter().map(|t| (t, format!("T{}", t.number())));
    let mut labels: BTreeMap<String, String> = trees
        .flat_map(|(t, label)| [(World::name(t), label.clone()), (World::branch(t), label)])
        .collect();
    for id in 1..=txns {
        let label = format!("X{id}");
        labels.insert(World::txn_name(id), label.clone());
        labels.insert(World::txn_branch(id), label);
    }
    labels
}

/// A commit as git holds it.
struct Commit {
    parents: Vec<String>,
    tree: String,
    message: String,
}

fn commits(repo: &Path) -> BTreeMap<String, Commit> {
    let log = git(
        repo,
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

/// Each ref of `refs/heads` and `refs/collisions` in `repo` by its label, with
/// its tip, sorted. None for a repository that does not exist yet.
fn tips(repo: &Path, labels: &BTreeMap<String, String>) -> Vec<(String, String)> {
    let refs = try_git(
        repo,
        &[
            "for-each-ref",
            "--format=%(refname) %(objectname)",
            "refs/heads",
            "refs/collisions",
        ],
        None,
    );
    assert!(
        refs.code == 0,
        "for-each-ref in {}: {}",
        repo.display(),
        refs.stderr
    );
    let mut tips: Vec<(String, String)> = refs
        .stdout
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

/// A commit's number; a commit no local ref reaches is a broken run, and its id
/// shows it as a difference.
fn number_of(number: &BTreeMap<&str, usize>, c: &str) -> String {
    number
        .get(c)
        .map_or_else(|| c.to_string(), usize::to_string)
}

/// `line` with the two fields that name a tree or hold a commit id normalised.
/// `Agentfs-Collision`'s layout is `Decision::render`'s.
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
        f[3] = number_of(number, &f[3]);
        return format!("{COLLISION}: {}", f.join(" "));
    }
    line.to_string()
}

/// The canonical text of `repo` and its shared `remote`, tree names replaced by
/// `labels`.
fn canonical(repo: &Path, remote: &Path, labels: &BTreeMap<String, String>) -> String {
    let commits = commits(repo);
    let tips = tips(repo, labels);
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
            .map(|p| number_of(&number, p))
            .collect();
        out.push_str(&format!(
            "C {} tree={} parents={}\n",
            number[c.as_str()],
            commit.tree,
            parents.join(",")
        ));
        for line in commit.message.lines() {
            out.push_str(&canonical_line(line, labels, &number));
            out.push('\n');
        }
    }
    for (label, tip) in &tips {
        out.push_str(&format!("R {label} {}\n", number_of(&number, tip)));
    }
    for (label, tip) in &self::tips(remote, labels) {
        out.push_str(&format!("P {label} {}\n", number_of(&number, tip)));
    }
    out
}

/// The run as one canonical text (this module's doc comment).
pub fn canonical_text(w: &World) -> String {
    let labels = labels_of(w.trees(), w.txns_begun());
    canonical(w.repo(), w.remote(), &labels)
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// A temporary directory, removed when dropped.
    struct Dir(PathBuf);

    impl Drop for Dir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn commit(repo: &Path, parents: &[&str], tree: &str, msg: &str) -> String {
        let mut args = vec!["commit-tree", tree];
        for p in parents {
            args.extend(["-p", p]);
        }
        args.extend(["-F", "-"]);
        git_in(repo, &args, msg)
    }

    fn message(cause: &str, on: &str, extra: &str) -> String {
        format!(
            "{cause} by agentfs\n\nAgentfs-Writer: agentfs\nAgentfs-Cause: {cause}\n{TREE}: {on}\n{extra}"
        )
    }

    /// The canonical text of a hand-built repository: a root, two forks (one
    /// with a file), a landing in the root whose collision record names fork
    /// 2's losing commit under a collision ref, a transaction branch on fork 1,
    /// and a remote holding the root and the collision ref. Pinned whole: any
    /// change to the text, its normalisation or the tree names it maps fails
    /// here, so a change to them is a deliberate re-record of the golden.
    #[test]
    fn canonical_text_is_pinned() {
        let dir =
            Dir(std::env::temp_dir().join(format!("agentfs-digest-pin-{}", std::process::id())));
        let (repo, remote) = (dir.0.join("repo"), dir.0.join("origin.git"));
        std::fs::create_dir_all(&repo).unwrap();
        std::fs::create_dir_all(&remote).unwrap();
        git(&repo, &["init", "-q", "-b", "main"]);
        git(&remote, &["init", "-q", "--bare", "-b", "main"]);
        let main = World::name(TreeId::ROOT);
        let (f1, f2) = (World::name(TreeId::new(1)), World::name(TreeId::new(2)));
        let empty = git_in(&repo, &["mktree"], "");
        let blob = git_in(&repo, &["hash-object", "-w", "--stdin"], "x\n");
        let with_a = git_in(&repo, &["mktree"], &format!("100644 blob {blob}\ta\n"));
        let root = commit(&repo, &[], &empty, &message("Root", &main, ""));
        let on_f1 = commit(&repo, &[&root], &empty, &message("Direct", &f1, ""));
        let on_f2 = commit(&repo, &[&root], &with_a, &message("Direct", &f2, ""));
        let lost = format!("{COLLISION}: {f2} a Line {on_f2} refs/collisions/1 a c2.1 m1.1\n");
        let landing = commit(
            &repo,
            &[&root, &on_f2],
            &empty,
            &message("Landing", &main, &lost),
        );
        let in_txn = commit(
            &repo,
            &[&on_f1],
            &with_a,
            &message("Direct", &World::txn_name(1), "Agentfs-Open: 7\n"),
        );
        for (r, c) in [
            (World::branch(TreeId::ROOT), &landing),
            (World::branch(TreeId::new(1)), &on_f1),
            (World::branch(TreeId::new(2)), &on_f2),
            (World::txn_branch(1), &in_txn),
            ("refs/collisions/1".to_string(), &on_f2),
        ] {
            git(&repo, &["update-ref", &r, c]);
        }
        let to = remote.to_string_lossy().into_owned();
        git(
            &repo,
            &[
                "push",
                "-q",
                &to,
                &format!("{root}:refs/heads/main"),
                "refs/collisions/*:refs/collisions/*",
            ],
        );
        let text = canonical(
            &repo,
            &remote,
            &labels_of([TreeId::ROOT, TreeId::new(1), TreeId::new(2)], 1),
        );
        assert_eq!(text, PINNED, "the canonical text changed:\n{text}");
    }

    const PINNED: &str = "\
C 0 tree=4b825dc642cb6eb9a060e54bf8d69288fbee4904 parents=
Root by agentfs

Agentfs-Writer: agentfs
Agentfs-Cause: Root
Agentfs-Tree: T0
C 1 tree=4d593e935186bcc35450336864a1aad148210a14 parents=0
Direct by agentfs

Agentfs-Writer: agentfs
Agentfs-Cause: Direct
Agentfs-Tree: T2
C 2 tree=4b825dc642cb6eb9a060e54bf8d69288fbee4904 parents=0,1
Landing by agentfs

Agentfs-Writer: agentfs
Agentfs-Cause: Landing
Agentfs-Tree: T0
Agentfs-Collision: T2 a Line 1 refs/collisions/1 a c2.1 m1.1
C 3 tree=4b825dc642cb6eb9a060e54bf8d69288fbee4904 parents=0
Direct by agentfs

Agentfs-Writer: agentfs
Agentfs-Cause: Direct
Agentfs-Tree: T1
C 4 tree=4d593e935186bcc35450336864a1aad148210a14 parents=3
Direct by agentfs

Agentfs-Writer: agentfs
Agentfs-Cause: Direct
Agentfs-Tree: X1
Agentfs-Open: 7
R T0 2
R T1 3
R T2 1
R X1 4
R refs/collisions/1 1
P T0 0
P refs/collisions/1 1
";
}
