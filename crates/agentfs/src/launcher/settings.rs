//! The operator settings at every mount (docs/trees.md, "Operator
//! settings"): `<state>/config` read with its includes followed; an include
//! a view would show the workload is refused; a missing include is warned
//! of; each key whose capability tree volumes do not build yet is logged,
//! one line per key, and counted by the `agentfs.not_built` gauge.

use std::path::{Path, PathBuf};

use agentfs_core::operator_settings::{NotBuilt, OperatorSettings};
use anyhow::{Context as _, Result, bail};

use super::mountpoint::resolved;

/// Reads the operator settings of the volume at `state`, served at
/// `views`, and logs what is not built.
///
/// # Errors
///
/// Settings that cannot be read, or an include under a view's mount point
/// (the workload could edit the operator's settings).
pub(super) fn at_boot(state: &Path, views: &[&Path]) -> Result<Vec<NotBuilt>> {
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
    let not_built = settings.not_built();
    for gap in &not_built {
        eprintln!(
            "not built: {} ({}): {}",
            gap.key,
            gap.capability.as_str(),
            gap.capability.meaning()
        );
    }
    Ok(not_built)
}
