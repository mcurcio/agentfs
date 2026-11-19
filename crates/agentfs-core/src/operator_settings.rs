//! The operator's settings of a tree volume (docs/trees.md, "Operator settings"):
//! `<state>/config`, git-config syntax, holding the `include.path` lines
//! `agentfs init --include` writes; read at every mount. Keys tree volumes do
//! not build are classified, never silently ignored.
//!
//! The file is the launcher's, kept apart from `history.git/config`, which the
//! engine rewrites and its git children read: no operator key reaches them.
//! libgit2 reads it and follows its includes, as git does.

use std::fs::{self, File, OpenOptions};
use std::io::Write as _;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use crate::fsutil::sync_dir;

/// The operator settings file's name in the state area.
pub const FILE: &str = "config";

/// What a configured key would need that tree volumes do not build yet.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Capability {
    Push,
    Patches,
    Lfs,
    Other,
}

impl Capability {
    /// "push", "patches", "lfs", "other": the gauge's attribute value.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Push => "push",
            Self::Patches => "patches",
            Self::Lfs => "lfs",
            Self::Other => "other",
        }
    }

    /// One plain sentence for the boot log.
    pub fn meaning(self) -> &'static str {
        match self {
            Self::Push => {
                "this volume pushes to no remote yet; its history stays in the state area"
            }
            Self::Patches => "queued patches are not applied",
            Self::Lfs => "large files are recorded as ordinary git blobs",
            Self::Other => "tree volumes do not read this setting",
        }
    }
}

/// A configured key whose capability tree volumes do not build yet.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct NotBuilt {
    /// As libgit2 reports it: section and variable lower-cased.
    pub key: String,
    pub capability: Capability,
}

#[derive(Debug, thiserror::Error)]
pub enum SettingsError {
    #[error("reading the operator settings {}: {source}", path.display())]
    Read {
        path: PathBuf,
        #[source]
        source: git2::Error,
    },
    #[error("writing the operator settings {}: {source}", path.display())]
    Write {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("include {} is not an absolute path", .0.display())]
    RelativeInclude(PathBuf),
}

/// The settings `<state>/config` holds, its includes followed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OperatorSettings {
    /// Every entry, key and value, in file order with includes resolved in
    /// place.
    entries: Vec<(String, String)>,
    /// The top-level file's `include.path` values, as written.
    includes: Vec<PathBuf>,
    /// The state area: where a relative include resolves, as git resolves it.
    base: PathBuf,
}

impl OperatorSettings {
    /// `<state>/config` with its includes followed (libgit2 follows
    /// `include.path`); a missing file is empty settings.
    ///
    /// # Errors
    ///
    /// [`SettingsError::Read`] for a file or include that cannot be read or
    /// parsed.
    pub fn read(state: &Path) -> Result<Self, SettingsError> {
        let path = state.join(FILE);
        let read = |source| SettingsError::Read {
            path: path.clone(),
            source,
        };
        let mut settings = Self {
            base: state.to_path_buf(),
            ..Self::default()
        };
        if !path.exists() {
            return Ok(settings);
        }
        let config = git2::Config::open(&path).map_err(read)?;
        let mut entries = config.entries(None).map_err(read)?;
        while let Some(entry) = entries.next() {
            let entry = entry.map_err(read)?;
            let key = String::from_utf8_lossy(entry.name_bytes()).into_owned();
            let value = String::from_utf8_lossy(entry.value_bytes()).into_owned();
            if entry.include_depth() == 0 && key.eq_ignore_ascii_case("include.path") {
                settings.includes.push(PathBuf::from(&value));
            }
            settings.entries.push((key, value));
        }
        Ok(settings)
    }

    /// The top-level file's `include.path` values, in order, as written.
    pub fn includes(&self) -> &[PathBuf] {
        &self.includes
    }

    /// The includes that do not exist (git skips them silently; the launcher
    /// warns).
    pub fn missing_includes(&self) -> Vec<PathBuf> {
        self.includes
            .iter()
            .filter(|include| !self.base.join(include).exists())
            .cloned()
            .collect()
    }

    /// Every configured key [`classify`] does not call built, sorted by key,
    /// one per key.
    pub fn not_built(&self) -> Vec<NotBuilt> {
        let mut found = std::collections::BTreeMap::new();
        for (key, value) in &self.entries {
            if let Some(capability) = classify(key, value) {
                found.entry(key.clone()).or_insert(capability);
            }
        }
        found
            .into_iter()
            .map(|(key, capability)| NotBuilt { key, capability })
            .collect()
    }
}

/// Makes `includes` exactly the `include.path` lines of `<state>/config`: the
/// file written whole (temporary, fsync, rename, directory fsync), mode 0600.
/// Each must be absolute.
///
/// # Errors
///
/// [`SettingsError::RelativeInclude`] with nothing written;
/// [`SettingsError::Write`] for a failed write.
pub fn write_includes(state: &Path, includes: &[PathBuf]) -> Result<(), SettingsError> {
    if let Some(relative) = includes.iter().find(|include| !include.is_absolute()) {
        return Err(SettingsError::RelativeInclude(relative.clone()));
    }
    let mut text = String::new();
    if !includes.is_empty() {
        text.push_str("[include]\n");
        for include in includes {
            text.push_str("\tpath = ");
            text.push_str(&quoted(&include.to_string_lossy()));
            text.push('\n');
        }
    }
    let path = state.join(FILE);
    let write = |source| SettingsError::Write {
        path: path.clone(),
        source,
    };
    let temporary = state.join(format!("{FILE}.tmp"));
    {
        let mut file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&temporary)
            .map_err(write)?;
        file.write_all(text.as_bytes()).map_err(write)?;
        file.sync_all().map_err(write)?;
    }
    fs::rename(&temporary, &path).map_err(write)?;
    // A file replaced keeps its own mode through the rename: make it 0600.
    fs::set_permissions(&path, std::os::unix::fs::PermissionsExt::from_mode(0o600))
        .map_err(write)?;
    File::open(&path)
        .and_then(|file| file.sync_all())
        .map_err(write)?;
    sync_dir(state).map_err(write)
}

/// A git-config value in double quotes, with `\`, `"` and line breaks escaped.
fn quoted(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
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

/// Pure. `None` for structure (`include.path`, `includeif.<c>.path`) and for
/// what tree volumes build (`agentfs.nested` = `exclude`, case-insensitive).
/// The key is compared lower-cased (libgit2 lower-cases section and variable
/// names). First match wins: `agentfs.lfsthreshold`, `lfs.*`,
/// `remote.<n>.lfsurl`, `remote.<n>.lfspushurl`,
/// `remote.<n>.agentfslfscredentials`, `remote.<n>.agentfsmaxlfsobjectsize`
/// → Lfs; `agentfs.patch*`, `agentfspusher.*` → Patches; `remote.*`,
/// `agentfs.pushinterval`, `agentfs.forkrefs`, `http.proxy` → Push;
/// everything else → Other.
pub fn classify(key: &str, value: &str) -> Option<Capability> {
    let key = key.to_ascii_lowercase();
    let remote_variable = |variable: &str| {
        key.strip_prefix("remote.")
            .and_then(|rest| rest.rsplit_once('.'))
            .is_some_and(|(_, name)| name == variable)
    };
    if key == "include.path" || (key.starts_with("includeif.") && key.ends_with(".path")) {
        return None;
    }
    if key == "agentfs.nested" && value.trim().eq_ignore_ascii_case("exclude") {
        return None;
    }
    if key == "agentfs.lfsthreshold"
        || key.starts_with("lfs.")
        || [
            "lfsurl",
            "lfspushurl",
            "agentfslfscredentials",
            "agentfsmaxlfsobjectsize",
        ]
        .into_iter()
        .any(remote_variable)
    {
        return Some(Capability::Lfs);
    }
    if key.starts_with("agentfs.patch") || key.starts_with("agentfspusher.") {
        return Some(Capability::Patches);
    }
    if key.starts_with("remote.")
        || key == "agentfs.pushinterval"
        || key == "agentfs.forkrefs"
        || key == "http.proxy"
    {
        return Some(Capability::Push);
    }
    Some(Capability::Other)
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt as _;
    use std::process::Command;

    use super::*;

    #[test]
    fn each_family_has_its_capability() {
        let cases = [
            (
                "remote.origin.url",
                "https://example.com/r.git",
                Some(Capability::Push),
            ),
            ("remote.Origin.pushurl", "x", Some(Capability::Push)),
            // The LFS rules come before `remote.*`.
            ("remote.origin.lfsurl", "x", Some(Capability::Lfs)),
            ("remote.origin.lfspushurl", "x", Some(Capability::Lfs)),
            (
                "remote.a.b.agentfslfscredentials",
                "x",
                Some(Capability::Lfs),
            ),
            (
                "remote.origin.agentfsmaxlfsobjectsize",
                "1m",
                Some(Capability::Lfs),
            ),
            ("agentfs.lfsthreshold", "1m", Some(Capability::Lfs)),
            ("lfs.url", "x", Some(Capability::Lfs)),
            ("agentfs.patchremote", "x", Some(Capability::Patches)),
            ("agentfs.patchAccept", "x", Some(Capability::Patches)),
            ("agentfspusher.x.right", "x", Some(Capability::Patches)),
            ("agentfs.pushinterval", "5m", Some(Capability::Push)),
            ("agentfs.forkrefs", "x", Some(Capability::Push)),
            ("http.proxy", "http://p", Some(Capability::Push)),
            ("agentfs.nested", "exclude", None),
            ("agentfs.nested", "Exclude", None),
            ("agentfs.nested", "working-tree", Some(Capability::Other)),
            ("include.path", "/x", None),
            ("includeif.gitdir:/x.path", "/x", None),
            ("core.autocrlf", "true", Some(Capability::Other)),
        ];
        for (key, value, expected) in cases {
            assert_eq!(classify(key, value), expected, "{key}={value}");
        }
    }

    #[test]
    fn a_missing_file_is_empty_settings() {
        let state = tempfile::tempdir().unwrap();
        let settings = OperatorSettings::read(state.path()).unwrap();
        assert!(settings.includes().is_empty());
        assert!(settings.not_built().is_empty());
    }

    #[test]
    fn includes_are_followed_and_unbuilt_keys_named() {
        let state = tempfile::tempdir().unwrap();
        let fragments = tempfile::tempdir().unwrap();
        let fragment = fragments.path().join("remotes");
        fs::write(
            &fragment,
            "[remote \"origin\"]\n\turl = https://example.com/r.git\n\
             \tlfsurl = https://example.com/lfs\n[agentfs]\n\tnested = exclude\n\
             \tpatchRemote = queue\n",
        )
        .unwrap();
        let absent = fragments.path().join("absent");
        write_includes(state.path(), &[fragment.clone(), absent.clone()]).unwrap();
        let settings = OperatorSettings::read(state.path()).unwrap();
        assert_eq!(settings.includes(), [fragment, absent.clone()]);
        assert_eq!(settings.missing_includes(), [absent]);
        let not_built = |key: &str, capability| NotBuilt {
            key: key.to_owned(),
            capability,
        };
        assert_eq!(
            settings.not_built(),
            [
                not_built("agentfs.patchremote", Capability::Patches),
                not_built("remote.origin.lfsurl", Capability::Lfs),
                not_built("remote.origin.url", Capability::Push),
            ]
        );
    }

    #[test]
    fn a_relative_include_is_reported_as_written() {
        let state = tempfile::tempdir().unwrap();
        fs::write(state.path().join(FILE), "[include]\n\tpath = here\n").unwrap();
        let settings = OperatorSettings::read(state.path()).unwrap();
        assert_eq!(settings.includes(), [PathBuf::from("here")]);
        assert_eq!(settings.missing_includes(), [PathBuf::from("here")]);
    }

    fn written(state: &Path) -> String {
        let output = Command::new("git")
            .arg("config")
            .arg("--file")
            .arg(state.join(FILE))
            .args(["--get-all", "include.path"])
            .output()
            .unwrap();
        String::from_utf8(output.stdout).unwrap()
    }

    #[test]
    fn write_includes_makes_them_exactly_the_includes() {
        let state = tempfile::tempdir().unwrap();
        let odd = PathBuf::from("/etc/agentfs/with \"quote\" and \\ ; # space ");
        write_includes(state.path(), &[PathBuf::from("/a"), odd.clone()]).unwrap();
        assert_eq!(written(state.path()), format!("/a\n{}\n", odd.display()));
        let mode = fs::metadata(state.path().join(FILE))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);

        write_includes(state.path(), &[PathBuf::from("/b")]).unwrap();
        assert_eq!(written(state.path()), "/b\n");

        let before = fs::read(state.path().join(FILE)).unwrap();
        assert!(matches!(
            write_includes(state.path(), &[PathBuf::from("/c"), PathBuf::from("rel")]),
            Err(SettingsError::RelativeInclude(path)) if path == Path::new("rel")
        ));
        assert_eq!(fs::read(state.path().join(FILE)).unwrap(), before);

        write_includes(state.path(), &[]).unwrap();
        assert_eq!(written(state.path()), "");
        assert!(
            OperatorSettings::read(state.path())
                .unwrap()
                .includes()
                .is_empty()
        );
    }
}
