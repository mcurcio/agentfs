//! The operator settings at every mount (docs/trees.md, "Operator
//! settings"): `<state>/config` read with its includes followed; an include
//! a view would show the workload is refused; a missing include is warned
//! of; each key whose capability tree volumes do not build yet is logged,
//! one line per key, and counted by the `agentfs.not_built` gauge; the keys
//! tree volumes build are read and checked (`agentfs.overlayLimit`).

use std::path::{Path, PathBuf};

use agentfs_engine::operator_settings::{NotBuilt, OperatorSettings};
use anyhow::{Context as _, Result, bail};

use super::mountpoint::resolved;

/// What the operator settings give a mount or host at boot.
pub(super) struct BootSettings {
    /// Each configured key tree volumes do not build yet, logged.
    pub not_built: Vec<NotBuilt>,
    /// The lag gate's overlay limit (`agentfs.overlayLimit`).
    pub overlay_limit: u64,
}

/// Reads the operator settings of the volume at `state`, served at
/// `views`, and logs what is not built.
///
/// # Errors
///
/// Settings that cannot be read, an include under a view's mount point
/// (the workload could edit the operator's settings), or a built key with a
/// value it refuses.
pub(super) fn at_boot(state: &Path, views: &[&Path]) -> Result<BootSettings> {
    let settings = OperatorSettings::read(state).context("reading the operator settings")?;
    let views: Vec<PathBuf> = views.iter().map(|at| resolved(at)).collect();
    for include in settings.includes() {
        let path = resolved(include);
        if let Some(view) = views.iter().find(|view| path.starts_with(view)) {
            bail!(
                "operator settings: include {} is under the mount point {}, where the workload \
                 could change it: keep the operator's settings outside every view",
                path.display(),
                view.display()
            );
        }
    }
    for missing in settings.missing_includes() {
        eprintln!(
            "warning: operator settings: include {} is missing",
            missing.display()
        );
    }
    let overlay_limit = settings.overlay_limit()?;
    let not_built = settings.not_built();
    for gap in &not_built {
        eprintln!(
            "not built: {} ({}): {}",
            gap.key,
            gap.capability.as_str(),
            gap.capability.meaning()
        );
    }
    Ok(BootSettings {
        not_built,
        overlay_limit,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_bad_overlay_limit_fails_at_boot() {
        let state = tempfile::tempdir().unwrap();
        std::fs::write(
            state.path().join(agentfs_engine::operator_settings::FILE),
            "[agentfs]\n\toverlayLimit = 0\n",
        )
        .unwrap();
        let refused = at_boot(state.path(), &[]).err().expect("refused");
        assert!(
            refused.to_string().contains("agentfs.overlaylimit"),
            "{refused:#}"
        );
    }

    #[test]
    fn the_overlay_limit_is_read_at_boot() {
        let state = tempfile::tempdir().unwrap();
        assert_eq!(
            at_boot(state.path(), &[]).unwrap().overlay_limit,
            agentfs_engine::operator_settings::DEFAULT_OVERLAY_LIMIT
        );
        std::fs::write(
            state.path().join(agentfs_engine::operator_settings::FILE),
            "[agentfs]\n\toverlayLimit = 1m\n",
        )
        .unwrap();
        let boot = at_boot(state.path(), &[]).unwrap();
        assert_eq!(boot.overlay_limit, 1 << 20);
        assert!(boot.not_built.is_empty());
    }
}
