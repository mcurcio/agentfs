//! The engine's history as a replay reads it, from its bare repository (VERIFYING.md,
//! "The model as the conformance suite"): one long-lived `git cat-file`, this
//! crate's own reader of each commit's layout (docs/trees.md, "What a commit
//! holds": `files/`, `lines/` and `names/` under two hashed folders), and
//! [`EngineHistory`], the collisions and heads a replay compares. Never the
//! library's bookkeeping (`tree_actor`): the folders are hashed here with `git
//! hash-object`, not the library's code.

use crate::engine_names::EngineNames;
use agentfs_model::calls::{CallError, History, ShownCollision, TreeId, gather_collisions};
use agentfs_model::content::{Content, Line, State};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

/// A line version in a commit: the file's identity, the line's, the bytes.
#[derive(Clone, Debug)]
pub(crate) struct FileMap {
    pub(crate) path: String,
    pub(crate) identity: u64,
    pub(crate) lines: Vec<(String, String, Option<Vec<u8>>)>,
    pub(crate) ancestors: BTreeMap<String, Option<String>>,
}

/// One commit, parsed.
#[derive(Clone, Debug, Default)]
pub(crate) struct Commit {
    pub(crate) parents: Vec<String>,
    pub(crate) message: String,
    pub(crate) files: BTreeMap<String, Vec<u8>>,
    pub(crate) maps: BTreeMap<u64, FileMap>,
    /// Each file identity's name history: head, versions.
    pub(crate) names: BTreeMap<u64, (u64, BTreeMap<u64, NameVersion>)>,
}

#[derive(Clone, Debug)]
pub(crate) struct NameVersion {
    pub(crate) path: Option<String>,
    pub(crate) over: Option<u64>,
}

impl Commit {
    pub(crate) fn trailers(&self, key: &str) -> Vec<&str> {
        let prefix = format!("{key}: ");
        self.message
            .lines()
            .filter_map(|l| l.strip_prefix(prefix.as_str()))
            .collect()
    }

    pub(crate) fn holds_line(&self, file: u64, version: &str) -> bool {
        self.maps
            .get(&file)
            .is_some_and(|m| m.lines.iter().any(|l| l.0 == version))
    }

    pub(crate) fn name_path(&self, file: u64) -> Option<&str> {
        let (head, versions) = self.names.get(&file)?;
        versions.get(head)?.path.as_deref()
    }

    pub(crate) fn holds_name(&self, file: u64, version: u64) -> bool {
        self.names.get(&file).is_some_and(|(h, _)| *h == version)
    }

    pub(crate) fn holds(&self, v: &Version) -> bool {
        match v {
            Version::Line { file, version } => self.holds_line(*file, version),
            Version::Name { file, version } => self.holds_name(*file, *version),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Version {
    Line { file: u64, version: String },
    Name { file: u64, version: u64 },
}

impl Version {
    pub(crate) fn parse(v: &Value) -> Result<Self, String> {
        let number = |x: &Value| x.as_u64().ok_or_else(|| format!("bad version {v}"));
        if let Some(line) = v.get("Line") {
            Ok(Self::Line {
                file: number(&line["file"])?,
                version: line["version"]
                    .as_str()
                    .ok_or_else(|| format!("bad version {v}"))?
                    .to_owned(),
            })
        } else if let Some(name) = v.get("Name") {
            Ok(Self::Name {
                file: number(&name["file"])?,
                version: number(&name["version"])?,
            })
        } else {
            Err(format!("bad version {v}"))
        }
    }

    pub(crate) fn file(&self) -> u64 {
        match self {
            Self::Line { file, .. } | Self::Name { file, .. } => *file,
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct Decision {
    pub(crate) lost: Version,
    pub(crate) winner: Version,
    pub(crate) whole: bool,
}

pub(crate) fn decisions(c: &Commit) -> Result<Vec<Decision>, String> {
    let mut out = Vec::new();
    for json in c.trailers("Agentfs-Collisions") {
        let parsed: Value = serde_json::from_str(json).map_err(|e| e.to_string())?;
        for d in parsed.as_array().ok_or("collisions are not a list")? {
            out.push(Decision {
                lost: Version::parse(&d["lost"])?,
                winner: Version::parse(&d["winner"])?,
                whole: d["kind"] == "Whole",
            });
        }
    }
    Ok(out)
}

/// One long-lived `git cat-file --batch`, and short `git` commands.
pub(crate) struct Git {
    dir: PathBuf,
    child: Child,
    input: ChildStdin,
    output: BufReader<ChildStdout>,
    /// Each name's two hashed folders, once asked of `git hash-object`.
    folders: BTreeMap<String, String>,
}

impl Git {
    pub(crate) fn new(dir: &Path) -> Result<Self, String> {
        let mut child = Command::new("git")
            .arg("--git-dir")
            .arg(dir)
            .args(["cat-file", "--batch"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .map_err(|e| e.to_string())?;
        let input = child.stdin.take().ok_or("no stdin")?;
        let output = BufReader::new(child.stdout.take().ok_or("no stdout")?);
        Ok(Self {
            dir: dir.to_owned(),
            child,
            input,
            output,
            folders: BTreeMap::new(),
        })
    }

    pub(crate) fn run(&self, args: &[&str]) -> Result<String, String> {
        let out = Command::new("git")
            .arg("--git-dir")
            .arg(&self.dir)
            .args(args)
            .output()
            .map_err(|e| e.to_string())?;
        if !out.status.success() {
            return Err(format!(
                "git {args:?}: {}",
                String::from_utf8_lossy(&out.stderr)
            ));
        }
        String::from_utf8(out.stdout).map_err(|e| e.to_string())
    }

    pub(crate) fn object(&mut self, name: &str) -> Result<Vec<u8>, String> {
        writeln!(self.input, "{name}").map_err(|e| e.to_string())?;
        self.input.flush().map_err(|e| e.to_string())?;
        let mut header = String::new();
        self.output
            .read_line(&mut header)
            .map_err(|e| e.to_string())?;
        let size: usize = header
            .trim_end()
            .rsplit(' ')
            .next()
            .and_then(|s| s.parse().ok())
            .ok_or_else(|| format!("{name}: {header:?}"))?;
        let mut bytes = vec![0; size + 1];
        self.output
            .read_exact(&mut bytes)
            .map_err(|e| e.to_string())?;
        bytes.pop();
        Ok(bytes)
    }

    /// Commit `oid`'s parents and message only.
    pub(crate) fn header(&mut self, oid: &str) -> Result<Commit, String> {
        let raw = self.object(oid)?;
        let raw = String::from_utf8_lossy(&raw);
        let (head, message) = raw.split_once("\n\n").unwrap_or((&raw, ""));
        let parents = head
            .lines()
            .filter_map(|l| l.strip_prefix("parent "))
            .map(str::to_owned)
            .collect();
        Ok(Commit {
            parents,
            message: message.to_owned(),
            ..Commit::default()
        })
    }

    /// `<aa>/<bb>`: hex digits 1-2 and 3-4 of `git hash-object --stdin` of
    /// `name`, the folders the layout keeps it in.
    fn folders(&mut self, name: &str) -> Result<String, String> {
        if let Some(folders) = self.folders.get(name) {
            return Ok(folders.clone());
        }
        let mut child = Command::new("git")
            .args(["hash-object", "--stdin"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .map_err(|e| e.to_string())?;
        child
            .stdin
            .take()
            .ok_or("no stdin")?
            .write_all(name.as_bytes())
            .map_err(|e| e.to_string())?;
        let out = child.wait_with_output().map_err(|e| e.to_string())?;
        let hex = String::from_utf8(out.stdout).map_err(|e| e.to_string())?;
        if !out.status.success() || hex.len() < 4 {
            return Err(format!("git hash-object of {name:?}: {hex:?}"));
        }
        let folders = format!("{}/{}", &hex[..2], &hex[2..4]);
        self.folders.insert(name.to_owned(), folders.clone());
        Ok(folders)
    }

    /// Every blob under `top/` of commit `oid`: its name and object. A blob
    /// outside the folders its name hashes to is a layout the engine never
    /// writes.
    fn leaves(&mut self, oid: &str, top: &str) -> Result<Vec<(String, String)>, String> {
        let listing = self.run(&[
            "ls-tree",
            "-r",
            "-z",
            "--full-tree",
            oid,
            &format!("{top}/"),
        ])?;
        let mut leaves = Vec::new();
        for record in listing.split_terminator('\0') {
            let (meta, path) = record.split_once('\t').ok_or_else(|| record.to_owned())?;
            let blob = meta.rsplit(' ').next().ok_or_else(|| record.to_owned())?;
            let mut parts = path.splitn(4, '/');
            let (Some(_), Some(aa), Some(bb), Some(name)) =
                (parts.next(), parts.next(), parts.next(), parts.next())
            else {
                return Err(format!("{oid:.7}: {path} is not {top}/<aa>/<bb>/<name>"));
            };
            if self.folders(name)? != format!("{aa}/{bb}") {
                return Err(format!("{oid:.7}: {path} is not in its name's folders"));
            }
            leaves.push((name.to_owned(), blob.to_owned()));
        }
        Ok(leaves)
    }

    pub(crate) fn commit(&mut self, oid: &str) -> Result<Commit, String> {
        let header = self.header(oid)?;
        // The root commit has no envelope and its files are at the top.
        if header.trailers("Agentfs-Format").is_empty() {
            let mut files = BTreeMap::new();
            for line in self.run(&["ls-tree", "-r", "--full-tree", oid])?.lines() {
                let (meta, path) = line.split_once('\t').ok_or_else(|| line.to_owned())?;
                let blob = meta.rsplit(' ').next().ok_or_else(|| line.to_owned())?;
                files.insert(path.to_owned(), self.object(blob)?);
            }
            return Ok(Commit { files, ..header });
        }
        let mut commit = header;
        for (name, blob) in self.leaves(oid, "files")? {
            let bytes = self.object(&blob)?;
            commit.files.insert(name, bytes);
        }
        for (file, blob) in self.leaves(oid, "names")? {
            let history: Value =
                serde_json::from_slice(&self.object(&blob)?).map_err(|e| e.to_string())?;
            let file: u64 = file.parse().map_err(|_| "bad file identity")?;
            let head = history["head"].as_u64().ok_or("bad name head")?;
            let mut versions = BTreeMap::new();
            for (n, v) in history["versions"].as_object().ok_or("bad name versions")? {
                versions.insert(
                    n.parse::<u64>().map_err(|_| "bad name version")?,
                    NameVersion {
                        path: v["path"].as_str().map(str::to_owned),
                        over: v["over"].as_u64(),
                    },
                );
            }
            commit.names.insert(file, (head, versions));
        }
        for (path, blob) in self.leaves(oid, "lines")? {
            let entry: Value =
                serde_json::from_slice(&self.object(&blob)?).map_err(|e| e.to_string())?;
            let map = parse_map(&path, &entry).map_err(|e| format!("{oid} {path}: {e}"))?;
            if commit.name_path(map.identity) != Some(path.as_str()) {
                return Err(format!(
                    "{oid:.7}: lines of {path} name file {}, whose name is elsewhere",
                    map.identity
                ));
            }
            commit.maps.insert(map.identity, map);
        }
        Ok(commit)
    }
}

impl Drop for Git {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// A `lines/` entry: `[identity, line map]`.
pub(crate) fn parse_map(path: &str, entry: &Value) -> Result<FileMap, String> {
    let identity = entry[0].as_u64().ok_or("bad identity")?;
    let map = &entry[1];
    let mut lines = Vec::new();
    for l in map["lines"].as_array().ok_or("bad lines")? {
        let bytes = match &l["bytes"] {
            Value::Null => None,
            b => Some(
                b.as_array()
                    .ok_or("bad bytes")?
                    .iter()
                    .map(|x| x.as_u64().map(|x| x as u8).ok_or("bad byte"))
                    .collect::<Result<Vec<u8>, _>>()?,
            ),
        };
        lines.push((
            l["version"].as_str().ok_or("bad version")?.to_owned(),
            l["identity"]
                .as_str()
                .ok_or("bad line identity")?
                .to_owned(),
            bytes,
        ));
    }
    let mut ancestors = BTreeMap::new();
    for (v, over) in map["ancestors"].as_object().ok_or("bad ancestors")? {
        ancestors.insert(v.clone(), over.as_str().map(str::to_owned));
    }
    Ok(FileMap {
        path: path.to_owned(),
        identity,
        lines,
        ancestors,
    })
}

/// A file in the model's terms, so its pure checks apply. Its name is one more
/// line, its ids this file's own.
pub(crate) fn content(c: &Commit, file: u64) -> Option<Content> {
    let map = c.maps.get(&file)?;
    let (head, versions) = c.names.get(&file)?;
    let named = versions.get(head)?;
    let name = Line {
        id: format!("name{head}"),
        line: format!("name-of-{file}"),
        over: named.over.map(|o| format!("name{o}")),
        state: named.path.clone().map_or(State::Gone, State::Live),
    };
    let lines = map
        .lines
        .iter()
        .map(|(version, identity, bytes)| Line {
            id: version.clone(),
            line: identity.clone(),
            over: map.ancestors.get(version).cloned().flatten(),
            state: bytes.as_ref().map_or(State::Gone, |b| {
                State::Live(String::from_utf8_lossy(b).into_owned())
            }),
        })
        .collect();
    Some(Content { name, lines })
}

pub(crate) fn descends<'a>(map: &'a FileMap, mut version: &'a str, from: &str) -> bool {
    loop {
        if version == from {
            return true;
        }
        match map.ancestors.get(version) {
            Some(Some(over)) => version = over,
            _ => return false,
        }
    }
}

/// The engine's collisions and heads, read from its bare repository after each
/// call. A landing's lost lines come from the `Agentfs-Collisions` it records (R3b,
/// R3e), each live line's bytes from the incoming commit, its tree from
/// `Agentfs-Tree`.
pub struct EngineHistory {
    git: Git,
    names: EngineNames,
    refs: String,
    seen: BTreeSet<String>,
    lost: Vec<ShownCollision>,
}

impl EngineHistory {
    /// Over the bare repository at `repo`, its trees named by `names`.
    pub fn new(repo: &Path, names: EngineNames) -> Result<EngineHistory, String> {
        Ok(EngineHistory {
            git: Git::new(repo)?,
            names,
            refs: String::new(),
            seen: BTreeSet::new(),
            lost: Vec::new(),
        })
    }

    /// After a ref moved: the forks, and each new landing's lost lines.
    fn refresh(&mut self) -> Result<(), String> {
        let refs = self.git.run(&["for-each-ref"])?;
        if refs == self.refs {
            return Ok(());
        }
        let forks = self.git.run(&EngineNames::LIST_FORKS)?;
        self.names.read_forks(&forks).map_err(|e| e.to_string())?;
        for oid in self.git.run(&["rev-list", "--all"])?.lines() {
            if self.seen.insert(oid.to_owned())
                && self.git.header(oid)?.trailers("Agentfs-Cause") == ["Landing"]
            {
                self.landing(oid)?;
            }
        }
        self.refs = refs;
        Ok(())
    }

    fn landing(&mut self, oid: &str) -> Result<(), String> {
        let c = self.git.commit(oid)?;
        let [_, incoming] = c.parents.as_slice() else {
            return Err(format!("{oid:.7}: a landing without two parents"));
        };
        let incoming = self.git.commit(incoming)?;
        let name = c.trailers("Agentfs-Tree").join("");
        let tree = self
            .names
            .tree(&name)
            .ok_or_else(|| format!("{oid:.7}: a landing in {name:?}, no tree of the trace"))?;
        for d in decisions(&c)? {
            let path = c
                .name_path(d.lost.file())
                .or(incoming.name_path(d.lost.file()))
                .unwrap_or_default()
                .to_owned();
            let lost_bytes = match &d.lost {
                Version::Line { file, version } => incoming
                    .maps
                    .get(file)
                    .and_then(|m| m.lines.iter().find(|l| l.0 == *version))
                    .and_then(|l| l.2.clone())
                    .into_iter()
                    .collect(),
                Version::Name { .. } => Vec::new(),
            };
            self.lost.push(ShownCollision {
                tree,
                path,
                lost_bytes,
                whole: d.whole,
            });
        }
        Ok(())
    }
}

impl<T: ?Sized> History<T> for EngineHistory {
    fn collisions(&mut self, _: &T) -> Result<Vec<ShownCollision>, CallError> {
        self.refresh().map_err(CallError::new)?;
        Ok(gather_collisions(self.lost.iter().cloned()))
    }

    fn head(&mut self, _: &T, t: TreeId) -> Result<(String, usize), CallError> {
        self.refresh().map_err(CallError::new)?;
        let oid = self
            .git
            .run(&["rev-parse", &self.names.branch(t)?])
            .map_err(CallError::new)?;
        let head = self.git.header(oid.trim()).map_err(CallError::new)?;
        Ok((head.trailers("Agentfs-Cause").concat(), head.parents.len()))
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A bare repository whose commits the real `one_writer` reads.
    pub(crate) struct Repo {
        dir: tempfile::TempDir,
    }

    impl Repo {
        /// The repository's directory.
        pub(crate) fn path(&self) -> &Path {
            self.dir.path()
        }

        pub(crate) fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let status = Command::new("git")
                .args(["init", "--bare", "-q"])
                .arg(dir.path())
                .status()
                .unwrap();
            assert!(status.success());
            Self { dir }
        }

        pub(crate) fn git(&self, args: &[&str], input: &[u8]) -> String {
            let mut child = Command::new("git")
                .arg("--git-dir")
                .arg(self.dir.path())
                .args(args)
                .env("GIT_AUTHOR_NAME", "agentfs")
                .env("GIT_AUTHOR_EMAIL", "agentfs@localhost")
                .env("GIT_COMMITTER_NAME", "agentfs")
                .env("GIT_COMMITTER_EMAIL", "agentfs@localhost")
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .spawn()
                .unwrap();
            child.stdin.take().unwrap().write_all(input).unwrap();
            let out = child.wait_with_output().unwrap();
            assert!(out.status.success(), "git {args:?}");
            String::from_utf8(out.stdout).unwrap().trim().to_owned()
        }

        /// A commit with `parents`, `cause` and, for file 1, the line versions it
        /// holds; `None` for a tree that ended the file.
        pub(crate) fn commit(
            &self,
            parents: &[&str],
            cause: &str,
            lines: Option<&[&str]>,
        ) -> String {
            self.commit_with(parents, cause, lines, "")
        }

        /// [`Repo::commit`] with `extra` trailers after its own.
        pub(crate) fn commit_with(
            &self,
            parents: &[&str],
            cause: &str,
            lines: Option<&[&str]>,
            extra: &str,
        ) -> String {
            // File 1 at `a` (`git hash-object` gives 2e65...), its name history
            // at `1` (56a6...).
            let (map, history) = match lines {
                Some(lines) => {
                    let lines: Vec<String> = lines
                        .iter()
                        .map(|v| {
                            format!(r#"{{"version":"{v}","identity":"{v}","bytes":[120,10]}}"#)
                        })
                        .collect();
                    (
                        Some(format!(
                            r#"[1,{{"lines":[{}],"ancestors":{{}}}}]"#,
                            lines.join(",")
                        )),
                        r#"{"head":1,"versions":{"1":{"path":"a","over":null}}}"#.to_owned(),
                    )
                }
                None => (
                    None,
                    r#"{"head":2,"versions":{"2":{"path":null,"over":1}}}"#.to_owned(),
                ),
            };
            let blob = |text: &str| self.git(&["hash-object", "-w", "--stdin"], text.as_bytes());
            let folder = |name: &str, entry: &str| {
                self.git(&["mktree"], format!("{entry}\t{name}\n").as_bytes())
            };
            let hashed = |top: &str, aa: &str, bb: &str, leaf: &str, text: &str| {
                let leaf = folder(leaf, &format!("100644 blob {}", blob(text)));
                let bb = folder(bb, &format!("040000 tree {leaf}"));
                format!(
                    "040000 tree {}\t{top}\n",
                    folder(aa, &format!("040000 tree {bb}"))
                )
            };
            let mut listing = hashed("names", "56", "a6", "1", &history);
            if let Some(map) = &map {
                listing.push_str(&hashed("lines", "2e", "65", "a", map));
            }
            let tree = self.git(&["mktree"], listing.as_bytes());
            let mut args = vec!["commit-tree", tree.as_str()];
            for parent in parents {
                args.extend(["-p", parent]);
            }
            args.extend(["-F", "-"]);
            self.git(
            &args,
            format!(
                "{cause}\n\nAgentfs-Writer: agentfs\nAgentfs-Cause: {cause}\nAgentfs-Format: envelope-1\n{extra}"
            )
            .as_bytes(),
        )
        }
    }

    /// The engine's history reads a landing's lost line, its bytes from the
    /// incoming commit and its tree by name, and a head's cause and parents.
    #[test]
    fn a_landings_lost_line_and_a_head_are_read_from_git() {
        let repo = Repo::new();
        let base = repo.commit(&[], "Root", Some(&["b"]));
        let before = repo.commit(&[&base], "Direct", Some(&["u"]));
        let incoming = repo.commit(&[&base], "Direct", Some(&["f"]));
        let trailers = format!(
            "Agentfs-Tree: fork/3\n{}{}\n",
            r#"Agentfs-Collisions: [{"lost":{"Line":{"file":1,"version":"f"}},"#,
            r#""winner":{"Line":{"file":1,"version":"u"}},"kind":"Line"}]"#,
        );
        let landing = repo.commit_with(&[&before, &incoming], "Landing", Some(&["u"]), &trailers);
        repo.git(&["update-ref", "refs/heads/root", &landing], b"");
        repo.git(&["update-ref", "refs/heads/fork/3", &incoming], b"");
        let mut history = EngineHistory::new(repo.path(), EngineNames::new()).unwrap();
        let fork = TreeId::new(1);
        assert_eq!(
            History::<()>::collisions(&mut history, &()).unwrap(),
            [ShownCollision {
                tree: fork,
                path: "a".into(),
                lost_bytes: vec![b"x\n".to_vec()],
                whole: false,
            }]
        );
        assert_eq!(
            History::<()>::head(&mut history, &(), TreeId::ROOT).unwrap(),
            ("Landing".into(), 2)
        );
        assert_eq!(
            History::<()>::head(&mut history, &(), fork).unwrap(),
            ("Direct".into(), 1)
        );
    }
}
