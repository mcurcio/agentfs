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
    /// A key tree volumes build, set to a value they refuse.
    #[error("operator settings: {key} = {value:?}: {expected}")]
    BadValue {
        key: String,
        value: String,
        expected: &'static str,
    },
}

/// The settings `<state>/config` holds, its includes followed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OperatorSettings {
    /// Every entry, key and value, in file order with includes resolved in
    /// place.
    entries: Vec<(String, String)>,
    /// Every `include.path` at any depth, each resolved against the directory
    /// of the file that includes it, as git resolves it; in the order found.
    includes: Vec<PathBuf>,
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
        let mut settings = Self::default();
        if !path.exists() {
            return Ok(settings);
        }
        let config = git2::Config::open(&path).map_err(read)?;
        let mut entries = config.entries(None).map_err(read)?;
        while let Some(entry) = entries.next() {
            let entry = entry.map_err(read)?;
            let key = String::from_utf8_lossy(entry.name_bytes()).into_owned();
            let value = String::from_utf8_lossy(entry.value_bytes()).into_owned();
            settings.entries.push((key, value));
        }
        settings.includes = includes_of(&path)?;
        Ok(settings)
    }

    /// Every `include.path` at any depth, in the order found, each resolved
    /// against the directory of the file that includes it.
    pub fn includes(&self) -> &[PathBuf] {
        &self.includes
    }

    /// The includes, at any depth, that do not exist (git skips them
    /// silently; the launcher warns).
    pub fn missing_includes(&self) -> Vec<PathBuf> {
        self.includes
            .iter()
            .filter(|include| !include.exists())
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

    /// `agentfs.overlayLimit`, the last value set (git's rule): a positive
    /// size (`k`, `m`, `g`: powers of 1024, `config::parse_size`), or the
    /// lag gate's default when unset. Tree volumes build it: [`classify`]
    /// answers `None` for it.
    ///
    /// # Errors
    ///
    /// [`SettingsError::BadValue`] for a value that is not a positive size.
    pub fn overlay_limit(&self) -> Result<u64, SettingsError> {
        todo!("agentfs-ln6.13")
    }
}

/// The `include.path` entries of `file` and, depth first, of each file it
/// includes, each resolved against the directory of the file naming it (git's
/// rule for a relative include). A missing include is listed and not followed;
/// a file already visited is not read again (an include cycle).
fn includes_of(file: &Path) -> Result<Vec<PathBuf>, SettingsError> {
    let mut found = Vec::new();
    let mut visited = std::collections::BTreeSet::new();
    let mut pending = vec![file.to_path_buf()];
    while let Some(file) = pending.pop() {
        if !visited.insert(file.clone()) || !file.exists() {
            continue;
        }
        let read = |source| SettingsError::Read {
            path: file.clone(),
            source,
        };
        let config = git2::Config::open(&file).map_err(read)?;
        let dir = file.parent().unwrap_or(Path::new("/")).to_path_buf();
        let mut direct = Vec::new();
        let mut entries = config.entries(None).map_err(read)?;
        while let Some(entry) = entries.next() {
            let entry = entry.map_err(read)?;
            // Depth 0: this file's own lines; deeper ones are read from the
            // file that holds them, resolved against its directory.
            if entry.include_depth() == 0
                && String::from_utf8_lossy(entry.name_bytes()).eq_ignore_ascii_case("include.path")
            {
                let value = String::from_utf8_lossy(entry.value_bytes()).into_owned();
                direct.push(dir.join(value));
            }
        }
        found.extend(direct.iter().cloned());
        // Depth first, in file order.
        pending.extend(direct.into_iter().rev());
    }
    Ok(found)
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

/// The operator's rule files for a tree volume, as a launcher installs them.
pub struct RuleFiles<'a> {
    pub exclude: Option<&'a Path>,
    pub attributes: Option<&'a Path>,
}

/// Makes `info/exclude` and `info/attributes` under
/// [`tree_actor::rules_dir`](crate::tree_actor::rules_dir) copies of the
/// given files, each written whole (a temporary file in the same directory,
/// fsynced and renamed over it, then the directory fsynced). A `None` source
/// removes the installed file. Call only while no process serves the volume:
/// rules are read at open.
///
/// # Errors
///
/// `NotFound` when `<state>/history.git` does not exist; the I/O error of a
/// read, write or rename.
pub fn install_rules(state: &Path, rules: RuleFiles<'_>) -> std::io::Result<()> {
    let git = state.join("history.git");
    if !git.is_dir() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!(
                "no tree volume at {}: {} is missing",
                state.display(),
                git.display()
            ),
        ));
    }
    let info = crate::tree_actor::rules_dir(state);
    fs::create_dir_all(&info)?;
    for (name, source) in [("exclude", rules.exclude), ("attributes", rules.attributes)] {
        let installed = info.join(name);
        match source {
            Some(source) => {
                let bytes = fs::read(source)?;
                let temporary = info.join(format!("{name}.tmp"));
                {
                    let mut file = File::create(&temporary)?;
                    file.write_all(&bytes)?;
                    file.sync_all()?;
                }
                fs::rename(&temporary, &installed)?;
            }
            None => match fs::remove_file(&installed) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e),
            },
        }
    }
    sync_dir(&info)
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

/// Pure. `None` for `include.path` (structure) and for what tree volumes build (`agentfs.nested` = `exclude`, case-insensitive).
/// The key is compared lower-cased (libgit2 lower-cases section and variable
/// names). First match wins: `agentfs.lfsthreshold`, `lfs.*`,
/// `remote.<n>.lfsurl`, `remote.<n>.lfspushurl`,
/// `remote.<n>.agentfslfscredentials`, `remote.<n>.agentfsmaxlfsobjectsize`
/// → Lfs; `agentfs.patch*`, `agentfspusher.*` → Patches; `remote.*`,
/// `agentfs.pushinterval`, `agentfs.forkrefs`, `http.proxy` → Push;
/// everything else → Other, `includeif.<c>.path` included (its target is
/// never read).
pub fn classify(key: &str, value: &str) -> Option<Capability> {
    let key = key.to_ascii_lowercase();
    let remote_variable = |variable: &str| {
        key.strip_prefix("remote.")
            .and_then(|rest| rest.rsplit_once('.'))
            .is_some_and(|(_, name)| name == variable)
    };
    if key == "include.path" {
        return None;
    }
    // libgit2 reading a standalone file evaluates no condition: an
    // includeIf's target is never read, so the line is logged.
    if key.starts_with("includeif.") && key.ends_with(".path") {
        return Some(Capability::Other);
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

    /// A volume's git directory, as `init` leaves it, and two rule files.
    fn rules_fixture() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("state/history.git")).unwrap();
        let exclude = dir.path().join("exclude.src");
        let attributes = dir.path().join("attributes.src");
        fs::write(&exclude, "*.tmp\n").unwrap();
        fs::write(&attributes, "*.bin binary\n").unwrap();
        (dir, exclude, attributes)
    }

    #[test]
    fn install_rules_installs_replaces_and_removes() {
        let (dir, exclude, attributes) = rules_fixture();
        let state = dir.path().join("state");
        let info = crate::tree_actor::rules_dir(&state);
        install_rules(
            &state,
            RuleFiles {
                exclude: Some(&exclude),
                attributes: Some(&attributes),
            },
        )
        .unwrap();
        assert_eq!(fs::read_to_string(info.join("exclude")).unwrap(), "*.tmp\n");
        assert_eq!(
            fs::read_to_string(info.join("attributes")).unwrap(),
            "*.bin binary\n"
        );
        // Replaced whole, no temporary left.
        fs::write(&exclude, "cache/\n").unwrap();
        install_rules(
            &state,
            RuleFiles {
                exclude: Some(&exclude),
                attributes: Some(&attributes),
            },
        )
        .unwrap();
        assert_eq!(
            fs::read_to_string(info.join("exclude")).unwrap(),
            "cache/\n"
        );
        assert!(!info.join("exclude.tmp").exists());
        // `None` removes, and removing again is no error.
        for _ in 0..2 {
            install_rules(
                &state,
                RuleFiles {
                    exclude: None,
                    attributes: Some(&attributes),
                },
            )
            .unwrap();
            assert!(!info.join("exclude").exists());
            assert!(info.join("attributes").exists());
        }
    }

    #[test]
    fn install_rules_needs_a_volume() {
        let (dir, exclude, _) = rules_fixture();
        let none = dir.path().join("none");
        let refused = install_rules(
            &none,
            RuleFiles {
                exclude: Some(&exclude),
                attributes: None,
            },
        )
        .unwrap_err();
        assert_eq!(refused.kind(), std::io::ErrorKind::NotFound);
        assert!(!none.exists(), "nothing written");
    }

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
            ("includeif.gitdir:/x.path", "/x", Some(Capability::Other)),
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
    fn a_relative_include_resolves_against_the_state_area() {
        let state = tempfile::tempdir().unwrap();
        fs::write(state.path().join(FILE), "[include]\n\tpath = here\n").unwrap();
        let settings = OperatorSettings::read(state.path()).unwrap();
        assert_eq!(settings.includes(), [state.path().join("here")]);
        assert_eq!(settings.missing_includes(), [state.path().join("here")]);
    }

    /// An include inside an included file is listed too, resolved against
    /// the directory of the file naming it; a missing one is listed as
    /// missing.
    #[test]
    fn nested_includes_are_listed_at_every_depth() {
        let state = tempfile::tempdir().unwrap();
        let fragments = tempfile::tempdir().unwrap();
        let top = fragments.path().join("top");
        let sub = fragments.path().join("sub");
        fs::create_dir(&sub).unwrap();
        fs::write(
            &top,
            "[include]\n\tpath = sub/inner\n\tpath = nested-missing\n",
        )
        .unwrap();
        fs::write(
            sub.join("inner"),
            "[include]\n\tpath = deep\n[core]\n\tx = 1\n",
        )
        .unwrap();
        write_includes(state.path(), std::slice::from_ref(&top)).unwrap();
        let settings = OperatorSettings::read(state.path()).unwrap();
        let missing = fragments.path().join("nested-missing");
        assert_eq!(
            settings.includes(),
            [top, sub.join("inner"), missing.clone(), sub.join("deep")]
        );
        assert_eq!(settings.missing_includes(), [missing, sub.join("deep")]);
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
