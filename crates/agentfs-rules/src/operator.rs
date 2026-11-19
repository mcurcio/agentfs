//! The operator's rule files, `info/exclude` and `info/attributes`, parsed
//! from buffers with the pure-Rust `gix-ignore` and `gix-attributes`: no
//! system, global or tree file is ever read, and matching is always
//! case-sensitive (docs/trees.md, "Rules and the untracked area").

use std::collections::BTreeMap;
use std::fmt;

use bstr::ByteSlice;
use gix_attributes::StateRef;
use gix_attributes::search::{MetadataCollection, Outcome};
use gix_ignore::glob::pattern::Case;
use gix_ignore::search::Ignore;

/// An attribute's state for a path, as `info/attributes` sets it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AttrState {
    /// `name`.
    Set,
    /// `-name`.
    Unset,
    /// `name=value`.
    Value(String),
}

/// `info/exclude` and `info/attributes` parsed from buffers only (no system,
/// global or tree file). `Send + Sync`.
pub struct OperatorRules {
    ignore: gix_ignore::Search,
    attributes: gix_attributes::Search,
    macros: MetadataCollection,
}

impl fmt::Debug for OperatorRules {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OperatorRules").finish_non_exhaustive()
    }
}

impl OperatorRules {
    /// Parses both files. Never fails: lines git would skip are skipped.
    pub fn parse(exclude: &[u8], attributes: &[u8]) -> Self {
        let mut ignore = gix_ignore::Search::default();
        // With no root a list has no base to convert, so neither call can
        // fail: patterns are relative to the tree's root, as info/exclude's
        // and info/attributes' are in git.
        ignore
            .add_patterns_buffer(exclude, "info/exclude", None, Ignore::default())
            .expect("a list without a root has no base to convert");
        let mut macros = MetadataCollection::default();
        let mut search = gix_attributes::Search::default();
        search
            .add_patterns_buffer(
                attributes,
                "info/attributes".into(),
                None,
                &mut macros,
                true,
            )
            .expect("a list without a root has no base to convert");
        Self {
            ignore,
            attributes: search,
            macros,
        }
    }

    /// The `info/exclude` pattern (as written) that excludes `path` when it
    /// is new: the last match wins, case-sensitive; once a directory above
    /// `path` is excluded nothing beneath it is re-included. `path` is a
    /// tree path (`a/b`, no leading or trailing `/`); `is_dir` says whether
    /// `path` itself is a directory.
    pub fn ignored(&self, path: &str, is_dir: bool) -> Option<String> {
        let components: Vec<&str> = path.split('/').collect();
        let mut end = 0;
        for (i, component) in components.iter().enumerate() {
            end += component.len() + usize::from(i > 0);
            let prefix = &path[..end];
            let dir = i + 1 < components.len() || is_dir;
            if let Some(m) = self.ignore.pattern_matching_relative_path(
                prefix.as_bytes().as_bstr(),
                Some(dir),
                Case::Sensitive,
            ) && !m.pattern.is_negative()
            {
                return Some(m.pattern.to_string());
            }
        }
        None
    }

    /// Every attribute `info/attributes` sets for `path`; `None` is an
    /// explicit unspecify (`!name`).
    pub fn attributes(&self, path: &str) -> BTreeMap<String, Option<AttrState>> {
        let mut out = Outcome::default();
        out.initialize(&self.macros);
        self.attributes.pattern_matching_relative_path(
            path.as_bytes().as_bstr(),
            Case::Sensitive,
            Some(false),
            &mut out,
        );
        out.iter()
            .map(|m| {
                let state = match m.assignment.state {
                    StateRef::Set => Some(AttrState::Set),
                    StateRef::Unset => Some(AttrState::Unset),
                    StateRef::Value(v) => Some(AttrState::Value(v.as_bstr().to_string())),
                    StateRef::Unspecified => None,
                };
                (m.assignment.name.as_str().to_owned(), state)
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn last_match_wins_and_an_excluded_directory_keeps_its_children() {
        let rules = OperatorRules::parse(b"*.log\n!keep.log\nbuild/\n!build/x\n", b"");
        assert_eq!(rules.ignored("a.log", false).as_deref(), Some("*.log"));
        assert_eq!(rules.ignored("keep.log", false), None);
        assert_eq!(rules.ignored("build", true).as_deref(), Some("build/"));
        assert_eq!(rules.ignored("build", false), None, "dir-only pattern");
        assert_eq!(rules.ignored("build/x", false).as_deref(), Some("build/"));
        assert_eq!(rules.ignored("A.LOG", false), None, "case-sensitive");
    }

    #[test]
    fn attributes_report_set_unset_value_and_unspecified() {
        let rules = OperatorRules::parse(b"", b"*.db -agentfs diff=x\n*.db !diff\n*.md text\n");
        let db = rules.attributes("a/s.db");
        assert_eq!(db.get("agentfs"), Some(&Some(AttrState::Unset)));
        assert_eq!(db.get("diff"), Some(&None));
        assert_eq!(
            rules.attributes("x.md").get("text"),
            Some(&Some(AttrState::Set))
        );
        assert!(rules.attributes("x.rs").is_empty());
    }
}
