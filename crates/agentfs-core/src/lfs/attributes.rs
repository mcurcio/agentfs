//! The `.gitattributes` lines agentfs maintains for LFS-stored paths
//! (design §7), so ordinary clones turn pointers back into content.
//!
//! Every commit rebuilds a block at the end of the root `.gitattributes`
//! **in history only** (the backing tree is never written):
//!
//! ```text
//! <the workload's own content, byte for byte>
//! # agentfs-lfs-begin: maintained by agentfs (Git LFS paths); edits are replaced
//! /big.bin filter=lfs diff=lfs merge=lfs -text
//! /dir[[:space:]]with[[:space:]]space/x.psd filter=lfs diff=lfs merge=lfs -text
//! # agentfs-lfs-end
//! ```
//!
//! - One line per path, anchored with a leading `/` and escaped so it
//!   matches that path alone: `\`, `*`, `?` and `[` get a backslash, a space
//!   becomes `[[:space:]]` (as `git lfs track` writes it; attribute
//!   patterns end at whitespace).
//! - The attributes are exactly those `git lfs track` writes.
//! - At the end of the file, so they win over the workload's own lines
//!   (the last match wins); a deeper `.gitattributes` can still override
//!   them, which agentfs does not detect.
//! - A path whose line could not be written exactly (it holds a tab, CR or
//!   LF, or the line would reach git's 2048-byte limit) is never stored as
//!   LFS (see [`pattern_for`]).
//! - A block the workload wrote (a line exactly [`BEGIN`] through the next
//!   line exactly [`END`]) is removed before the new one is added, so a
//!   forged block never decides anything. Every other line is kept, even
//!   one that only starts like a marker, or an unterminated [`BEGIN`].

use std::collections::BTreeSet;
use std::path::Path;

use crate::checks::ATTR_MAX_LINE_LENGTH;
use crate::paths::RepoPath;

/// The block's first line, exactly.
pub const BEGIN: &str =
    "# agentfs-lfs-begin: maintained by agentfs (Git LFS paths); edits are replaced";
/// The block's last line.
pub const END: &str = "# agentfs-lfs-end";
/// The attributes of each managed path, as `git lfs track` writes them.
pub const LFS_ATTRS: &str = "filter=lfs diff=lfs merge=lfs -text";

/// The root `.gitattributes` path.
pub const ROOT_GITATTRIBUTES: &str = ".gitattributes";

/// The escaped `/path` pattern matching exactly `path`, or `None` if no
/// line can: the path holds a tab, CR or LF, or its line would be 2048
/// bytes or longer (git's fsck limit). Such a path is never stored as LFS.
pub fn pattern_for(path: &RepoPath) -> Option<String> {
    let mut out = String::with_capacity(path.as_str().len() + 1);
    out.push('/');
    for c in path.as_str().chars() {
        match c {
            '\t' | '\r' | '\n' => return None,
            ' ' => out.push_str("[[:space:]]"),
            '\\' | '*' | '?' | '[' => {
                out.push('\\');
                out.push(c);
            }
            c => out.push(c),
        }
    }
    let line_len = out.len() + 1 + LFS_ATTRS.len();
    (line_len < ATTR_MAX_LINE_LENGTH).then_some(out)
}

/// The path a pattern from [`pattern_for`] matches, if it is one.
fn path_of(pattern: &str) -> Option<RepoPath> {
    let rest = pattern.strip_prefix('/')?;
    let mut out = String::with_capacity(rest.len());
    let mut chars = rest.char_indices();
    while let Some((i, c)) = chars.next() {
        match c {
            '\\' => out.push(chars.next()?.1),
            '[' => {
                if !rest[i..].starts_with("[[:space:]]") {
                    return None;
                }
                out.push(' ');
                for _ in 0.."[[:space:]]".len() - 1 {
                    chars.next();
                }
            }
            c => out.push(c),
        }
    }
    let path = RepoPath::new(out).ok()?;
    // Only the canonical spelling counts.
    (pattern_for(&path).as_deref() == Some(pattern)).then_some(path)
}

/// One managed line.
fn line_for(pattern: &str) -> String {
    format!("{pattern} {LFS_ATTRS}")
}

/// Splits a root `.gitattributes` into the workload's own content (every
/// block removed, byte for byte otherwise) and the paths of the
/// well-formed managed lines inside blocks. Only blocks agentfs wrote are
/// trusted with those paths; see the module documentation.
///
/// A block is a line exactly [`BEGIN`] through the next line exactly
/// [`END`], with no other [`BEGIN`] line between them (a trailing CR is
/// ignored on both). Any other line, including one that merely starts
/// like a marker or an unterminated [`BEGIN`], is the workload's.
pub fn split(content: &[u8]) -> (Vec<u8>, BTreeSet<RepoPath>) {
    fn text(line: &[u8]) -> &[u8] {
        let text = line.strip_suffix(b"\n").unwrap_or(line);
        text.strip_suffix(b"\r").unwrap_or(text)
    }
    let lines: Vec<&[u8]> = content.split_inclusive(|&b| b == b'\n').collect();
    let mut user = Vec::with_capacity(content.len());
    let mut paths = BTreeSet::new();
    let mut i = 0;
    while i < lines.len() {
        if text(lines[i]) == BEGIN.as_bytes()
            && let Some(end) = (i + 1..lines.len())
                .take_while(|&j| text(lines[j]) != BEGIN.as_bytes())
                .find(|&j| text(lines[j]) == END.as_bytes())
        {
            for line in &lines[i + 1..end] {
                let Ok(text) = std::str::from_utf8(text(line)) else {
                    continue;
                };
                if let Some(pattern) = text
                    .strip_suffix(LFS_ATTRS)
                    .and_then(|p| p.strip_suffix(' '))
                    && let Some(path) = path_of(pattern)
                {
                    paths.insert(path);
                }
            }
            i = end + 1;
            continue;
        }
        user.extend_from_slice(lines[i]);
        i += 1;
    }
    (user, paths)
}

/// Whether `path` is the root `.gitattributes` or beneath a directory of
/// that name.
pub fn at_or_beneath_root(path: &RepoPath) -> bool {
    path.components().next() == Some(ROOT_GITATTRIBUTES)
}

/// The workload's own content of a root `.gitattributes` from history.
pub fn user_part(content: &[u8]) -> Vec<u8> {
    split(content).0
}

/// `user` followed by a block for `paths` (nothing added when `paths` is
/// empty). `user` gets a final newline before the block if it lacks one.
/// Paths without a pattern are skipped (they are never stored as LFS).
pub fn compose(user: &[u8], paths: &BTreeSet<RepoPath>) -> Vec<u8> {
    let mut out = user.to_vec();
    if paths.is_empty() {
        return out;
    }
    if !out.is_empty() && !out.ends_with(b"\n") {
        out.push(b'\n');
    }
    out.extend_from_slice(BEGIN.as_bytes());
    out.push(b'\n');
    for path in paths {
        if let Some(pattern) = pattern_for(path) {
            out.extend_from_slice(line_for(&pattern).as_bytes());
            out.push(b'\n');
        }
    }
    out.extend_from_slice(END.as_bytes());
    out.push(b'\n');
    out
}

/// The managed paths of `tree`'s root `.gitattributes` (empty when it has
/// none, or it is not a regular file).
pub fn managed_paths(
    repo: &git2::Repository,
    tree: &git2::Tree<'_>,
) -> Result<BTreeSet<RepoPath>, git2::Error> {
    match tree.get_path(Path::new(ROOT_GITATTRIBUTES)) {
        Ok(entry) if matches!(entry.filemode(), 0o100644 | 0o100755) => {
            Ok(split(repo.find_blob(entry.id())?.content()).1)
        }
        Ok(_) => Ok(BTreeSet::new()),
        Err(e) if e.code() == git2::ErrorCode::NotFound => Ok(BTreeSet::new()),
        Err(e) => Err(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> RepoPath {
        RepoPath::new(s).unwrap()
    }

    #[test]
    fn patterns_escape_what_wildmatch_and_the_parser_treat_specially() {
        for (path, pattern) in [
            ("big.bin", "/big.bin"),
            (
                "dir with space/a b",
                "/dir[[:space:]]with[[:space:]]space/a[[:space:]]b",
            ),
            ("star*.bin", "/star\\*.bin"),
            ("q?.bin", "/q\\?.bin"),
            ("[x].bin", "/\\[x].bin"),
            ("back\\slash", "/back\\\\slash"),
            ("#hash", "/#hash"),
            ("!bang", "/!bang"),
            ("\"quoted\"", "/\"quoted\""),
            ("ünïcødé/日本.bin", "/ünïcødé/日本.bin"),
        ] {
            assert_eq!(pattern_for(&p(path)).as_deref(), Some(pattern), "{path}");
            assert_eq!(path_of(pattern), Some(p(path)), "{pattern}");
        }
        for never in ["tab\there", "cr\rhere", "lf\nhere"] {
            assert_eq!(pattern_for(&p(never)), None, "{never:?}");
        }
        // The line limit: the pattern plus " filter=lfs ..." must stay
        // under 2048 bytes.
        let fits = 2048 - 1 - (1 + LFS_ATTRS.len()) - 1;
        let path = p(&"a".repeat(fits));
        assert_eq!(line_for(&pattern_for(&path).unwrap()).len(), 2047);
        assert_eq!(pattern_for(&p(&"a".repeat(fits + 1))), None);
        let spaced = format!("{}  ", "a".repeat(fits - 20));
        assert_eq!(pattern_for(&p(&spaced)), None);
        // Not canonical: not a managed path.
        for bad in [
            "big.bin",
            "/st*r",
            "/a\\ b",
            "/a[[:blank:]]b",
            "/",
            "/a/../b",
            "/.git/x",
        ] {
            assert_eq!(path_of(bad), None, "{bad}");
        }
    }

    #[test]
    fn compose_and_split_round_trip_and_keep_the_users_bytes() {
        let paths: BTreeSet<RepoPath> = [p("z.bin"), p("a b.psd"), p("dir/x*")].into();
        for user in [
            &b""[..],
            b"*.txt text\n",
            b"*.txt text",
            b"*.txt text\r\n# comment\n\n",
            b"\xff\xfe not utf-8\n",
        ] {
            let composed = compose(user, &paths);
            let (got_user, got_paths) = split(&composed);
            let mut expected_user = user.to_vec();
            if !expected_user.is_empty() && !expected_user.ends_with(b"\n") {
                expected_user.push(b'\n');
            }
            assert_eq!(got_user, expected_user, "{user:?}");
            assert_eq!(got_paths, paths);
            assert_eq!(user_part(&composed), expected_user);
            // Recomposing is stable.
            assert_eq!(compose(&got_user, &got_paths), composed);
        }
        let composed = String::from_utf8(compose(b"*.txt text\n", &paths)).unwrap();
        assert_eq!(
            composed,
            format!(
                "*.txt text\n{BEGIN}\n\
                 /a[[:space:]]b.psd filter=lfs diff=lfs merge=lfs -text\n\
                 /dir/x\\* filter=lfs diff=lfs merge=lfs -text\n\
                 /z.bin filter=lfs diff=lfs merge=lfs -text\n{END}\n"
            )
        );
        assert_eq!(compose(b"keep", &BTreeSet::new()), b"keep");
    }

    #[test]
    fn every_exact_block_is_removed_and_only_well_formed_lines_count() {
        let content = format!(
            "user line\n\
             {BEGIN}\r\n\
             /evil.bin filter=lfs diff=lfs merge=lfs -text\n\
             *.bin filter=lfs diff=lfs merge=lfs -text\n\
             /x filter=lfs\n\
             {END}\r\n\
             second user line\n\
             {BEGIN}\n\
             /ok.bin filter=lfs diff=lfs merge=lfs -text\n\
             {END}\n"
        );
        let (user, paths) = split(content.as_bytes());
        assert_eq!(user, b"user line\nsecond user line\n");
        let names: Vec<&str> = paths.iter().map(RepoPath::as_str).collect();
        assert_eq!(names, ["evil.bin", "ok.bin"]);
    }

    #[test]
    fn lines_that_only_look_like_markers_are_the_workloads() {
        // A comment starting like a marker, an END alone, and an
        // unterminated BEGIN (its END comes only after another BEGIN):
        // all kept, and the lines after them too.
        let user = format!(
            "# agentfs-lfs-begin notes\n\
             *.sh text eol=lf\n\
             {END}\n\
             {BEGIN} \n\
             {BEGIN}\n\
             *.md text\n"
        );
        let (got, paths) = split(user.as_bytes());
        assert_eq!(got, user.as_bytes());
        assert!(paths.is_empty());
        // Composed with a block, the workload's part survives intact.
        let managed: BTreeSet<RepoPath> = [p("big.bin")].into();
        let composed = compose(user.as_bytes(), &managed);
        let (got, paths) = split(&composed);
        assert_eq!(got, user.as_bytes());
        assert_eq!(paths, managed);
        // An unterminated BEGIN at the end is kept too.
        let open = format!("*.txt text\n{BEGIN}\n/a.bin {LFS_ATTRS}\n");
        assert_eq!(
            split(open.as_bytes()),
            (open.clone().into_bytes(), BTreeSet::new())
        );
        let composed = compose(open.as_bytes(), &managed);
        assert_eq!(split(&composed), (open.into_bytes(), managed));
    }

    #[test]
    fn root_gitattributes_and_beneath() {
        assert!(at_or_beneath_root(&p(".gitattributes")));
        assert!(at_or_beneath_root(&p(".gitattributes/x")));
        assert!(!at_or_beneath_root(&p("d/.gitattributes")));
        assert!(!at_or_beneath_root(&p(".gitattributesx")));
    }
}
