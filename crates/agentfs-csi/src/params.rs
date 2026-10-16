//! StorageClass and PersistentVolume parameters, and what the CO tells a
//! node about the pod (design §14.2).
//!
//! Every setting of a volume is the operator's: a StorageClass's
//! `parameters` (or a static PV's `volumeAttributes`) name a **profile**,
//! a config fragment, rules and credentials the driver's own pods mount
//! from its namespace. Pod authors choose only the StorageClass.
//!
//! | Parameter | Meaning |
//! |---|---|
//! | `profile` | required: `<profile>.config`, `.exclude`, `.attributes` in the profiles directory |
//! | `volumeName` | the agentfs volume name, a template over `{pvcNamespace}`, `{pvcName}` and `{pvName}`; default [`DEFAULT_TEMPLATE`] (with the provisioner's `--extra-create-metadata`), else the PV name. In a static PV's attributes, a literal name |
//! | `branch` | the history branch (default `agentfs/<volume>`) |
//! | `onDelete` | `retain` (default: DeleteVolume keeps the directory) or `delete` |
//! | `fork` | `"true"`: a fork class ([`ForkClass`], design §11.5); `"false"` or absent: an ordinary one |
//!
//! Keys under `csi.storage.k8s.io/` (the provisioner's metadata) are read
//! for the template and otherwise ignored; any other key is refused.
//!
//! The default name includes the PV name, so it is unique per volume
//! (design §11.4): a claim deleted and created again gets a new branch
//! instead of a fresh root commit on the old claim's branch, which every
//! push would then find diverged (design §11.3).

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use agentfs_core::config::validate_name;

/// The provisioner's metadata keys (`--extra-create-metadata`).
pub const PVC_NAME: &str = "csi.storage.k8s.io/pvc/name";
pub const PVC_NAMESPACE: &str = "csi.storage.k8s.io/pvc/namespace";
pub const PV_NAME: &str = "csi.storage.k8s.io/pv/name";

/// `podInfoOnMount` keys in NodePublish's volume context.
pub const POD_NAME: &str = "csi.storage.k8s.io/pod.name";
pub const POD_NAMESPACE: &str = "csi.storage.k8s.io/pod.namespace";
pub const POD_UID: &str = "csi.storage.k8s.io/pod.uid";
pub const POD_SERVICE_ACCOUNT: &str = "csi.storage.k8s.io/serviceAccount.name";
/// Set by the kubelet for inline (ephemeral) volumes, which are refused.
pub const EPHEMERAL: &str = "csi.storage.k8s.io/ephemeral";

/// The prefix of keys the CO adds on its own.
const CO_PREFIX: &str = "csi.storage.k8s.io/";

/// Prefixes of the keys the CO adds to a volume context on its own.
const CO_CONTEXT_PREFIXES: [&str; 2] = [CO_PREFIX, "storage.kubernetes.io/"];

/// The default `volumeName` template when the provisioner passes the
/// claim's metadata: unique per PV, readable per claim.
pub const DEFAULT_TEMPLATE: &str = "{pvcNamespace}.{pvcName}.{pvName}";

/// Longest profile name: a DNS label, so `<profile>.config` is always a
/// valid ConfigMap key.
pub const MAX_PROFILE_LEN: usize = 63;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ParamError {
    #[error("the StorageClass parameter `profile` is required (design §14.2)")]
    MissingProfile,
    #[error(
        "profile {0:?} is not a valid name: lower-case letters, digits and '-', starting and \
         ending with a letter or digit, at most 63 characters"
    )]
    BadProfile(String),
    #[error(
        "unknown StorageClass parameter {0:?} (known: profile, volumeName, branch, onDelete, \
         workloadMounts, fork)"
    )]
    UnknownKey(String),
    #[error(
        "unknown volume attribute {0:?} (known: profile, volumeName (or volume), branch, onDelete, \
         workloadMounts)"
    )]
    UnknownAttribute(String),
    #[error("the volume attributes name two volumes, {a:?} and {b:?} (volume and volumeName)")]
    ConflictingNames { a: String, b: String },
    #[error(
        "volumeName {template:?}: unknown placeholder {{{name}}} (known: pvcNamespace, pvcName, pvName)"
    )]
    UnknownPlaceholder { template: String, name: String },
    #[error(
        "volumeName {template:?} uses {{{name}}}, which the CO did not provide (run csi-provisioner with --extra-create-metadata)"
    )]
    MissingMetadata { template: String, name: String },
    #[error(
        "volumeName {template:?} gives {value:?}, which is not a valid agentfs volume name ([A-Za-z0-9][A-Za-z0-9._-]*, no '..', no .lock suffix)"
    )]
    BadVolumeName { template: String, value: String },
    #[error("branch {0:?} is not a valid branch name")]
    BadBranch(String),
    #[error("onDelete {0:?} must be retain or delete")]
    BadOnDelete(String),
    #[error("workloadMounts: {0}")]
    BadWorkloadMount(String),
    #[error(
        "a fork class (fork: \"true\") must not set {0}: a fork's profile is its source's and \
         its branch is fixed"
    )]
    ForkClassSets(&'static str),
    #[error("fork {0:?} must be \"true\" or \"false\"")]
    BadFork(String),
    #[error(
        "volumeName {template:?} of a fork class must contain {{pvName}}, so each fork is unique \
         on the remote"
    )]
    ForkNameNotUnique { template: String },
}

/// What DeleteVolume does with the volume's directory.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OnDelete {
    /// Keep the backing tree and history (re-usable through a static PV).
    #[default]
    Retain,
    /// Remove the directory.
    Delete,
}

impl std::str::FromStr for OnDelete {
    type Err = ParamError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "retain" => Ok(Self::Retain),
            "delete" => Ok(Self::Delete),
            other => Err(ParamError::BadOnDelete(other.to_owned())),
        }
    }
}

impl std::fmt::Display for OnDelete {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Retain => "retain",
            Self::Delete => "delete",
        })
    }
}

/// A volume's settings, from CreateVolume's parameters or a static PV's
/// volume context.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VolumeParams {
    pub profile: String,
    /// The agentfs volume name (`agentfs.volume`).
    pub volume: String,
    /// The history branch; `None` is `agentfs/<volume>`.
    pub branch: Option<String>,
    pub on_delete: OnDelete,
    /// Where pods mount the volume (`workloadMounts`: comma-separated
    /// `PATH` or `PATH=DIR`), passed to every front as `--workload-mount`
    /// so absolute worktree `gitdir:` targets map exactly (design §5).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub workload_mounts: Vec<String>,
}

impl VolumeParams {
    /// From CreateVolume: `name` is the PV name the CO chose, `parameters`
    /// the StorageClass's (plus the provisioner's metadata).
    pub fn from_create(
        name: &str,
        parameters: &HashMap<String, String>,
    ) -> Result<Self, ParamError> {
        let mut profile = None;
        let mut template = None;
        let mut branch = None;
        let mut on_delete = OnDelete::default();
        let mut workload_mounts = Vec::new();
        for (key, value) in parameters {
            match key.as_str() {
                "profile" => profile = Some(value.clone()),
                "volumeName" => template = Some(value.clone()),
                "branch" => branch = Some(value.clone()),
                "onDelete" => on_delete = value.parse()?,
                "workloadMounts" => workload_mounts = parse_workload_mounts(value)?,
                // An ordinary class may say so; a fork class is a ForkClass.
                "fork" if value == "false" => {}
                "fork" => return Err(ParamError::BadFork(value.clone())),
                k if k.starts_with(CO_PREFIX) => {}
                other => return Err(ParamError::UnknownKey(other.to_owned())),
            }
        }
        let profile = profile.ok_or(ParamError::MissingProfile)?;
        validate_profile(&profile)?;
        let template = template.unwrap_or_else(|| default_template(parameters));
        let volume = volume_name(name, &template, parameters)?;
        if let Some(branch) = &branch {
            validate_branch(branch)?;
        }
        Ok(Self {
            profile,
            volume,
            branch,
            on_delete,
            workload_mounts,
        })
    }

    /// From a volume context: what CreateVolume returned, or a static PV's
    /// `volumeAttributes`: `profile` (required), `volumeName` (a literal
    /// name, not a template; `volume`, as CreateVolume returns it, is the
    /// same; default: the volume handle), `branch`, `onDelete` and
    /// `workloadMounts`. Keys
    /// under the CO's prefixes (`csi.storage.k8s.io/`,
    /// `storage.kubernetes.io/csiProvisionerIdentity`) are ignored; any
    /// other key is refused, so a typo in a static PV fails loudly rather
    /// than naming the volume after its handle.
    pub fn from_context(
        handle: &str,
        context: &HashMap<String, String>,
    ) -> Result<Self, ParamError> {
        let mut profile = None;
        let mut names: Vec<&str> = Vec::new();
        let mut branch = None;
        let mut on_delete = OnDelete::default();
        let mut workload_mounts = Vec::new();
        for (key, value) in context {
            match key.as_str() {
                "profile" => profile = Some(value.clone()),
                "volume" | "volumeName" => names.push(value),
                "branch" => branch = Some(value.clone()).filter(|b| !b.is_empty()),
                "onDelete" => on_delete = value.parse()?,
                "workloadMounts" => workload_mounts = parse_workload_mounts(value)?,
                k if CO_CONTEXT_PREFIXES.iter().any(|p| k.starts_with(p)) => {}
                other => return Err(ParamError::UnknownAttribute(other.to_owned())),
            }
        }
        let profile = profile.ok_or(ParamError::MissingProfile)?;
        validate_profile(&profile)?;
        let volume = match names.as_slice() {
            [] => handle.to_owned(),
            [one] => (*one).to_owned(),
            [a, b] if a == b => (*a).to_owned(),
            [a, b, ..] => {
                return Err(ParamError::ConflictingNames {
                    a: (*a).to_owned(),
                    b: (*b).to_owned(),
                });
            }
        };
        if validate_name("agentfs.volume", &volume).is_err() {
            return Err(ParamError::BadVolumeName {
                template: "volumeName".into(),
                value: volume,
            });
        }
        if let Some(branch) = &branch {
            validate_branch(branch)?;
        }
        Ok(Self {
            profile,
            volume,
            branch,
            on_delete,
            workload_mounts,
        })
    }

    /// The branch history is written to.
    pub fn branch_name(&self) -> String {
        self.branch
            .clone()
            .unwrap_or_else(|| format!("agentfs/{}", self.volume))
    }

    /// The volume context CreateVolume returns, which every node call
    /// carries back.
    pub fn to_context(&self) -> HashMap<String, String> {
        let mut context = HashMap::from([
            ("profile".to_owned(), self.profile.clone()),
            ("volume".to_owned(), self.volume.clone()),
            ("branch".to_owned(), self.branch_name()),
            ("onDelete".to_owned(), self.on_delete.to_string()),
        ]);
        if !self.workload_mounts.is_empty() {
            context.insert(WORKLOAD_MOUNTS.to_owned(), self.workload_mounts.join(","));
        }
        context
    }
}

/// The default `volumeName` template: [`DEFAULT_TEMPLATE`] when the
/// provisioner passes the claim's metadata, else `{pvName}`.
fn default_template(parameters: &HashMap<String, String>) -> String {
    if parameters.contains_key(PVC_NAME) && parameters.contains_key(PVC_NAMESPACE) {
        DEFAULT_TEMPLATE.to_owned()
    } else {
        "{pvName}".to_owned()
    }
}

/// `template` rendered for the PV `name`, as a valid volume name.
fn volume_name(
    name: &str,
    template: &str,
    parameters: &HashMap<String, String>,
) -> Result<String, ParamError> {
    let pv_name = parameters.get(PV_NAME).map_or(name, String::as_str);
    let volume = render(template, parameters, pv_name)?;
    if validate_name("agentfs.volume", &volume).is_err() {
        return Err(ParamError::BadVolumeName {
            template: template.to_owned(),
            value: volume,
        });
    }
    Ok(volume)
}

/// A fork class's settings (design §11.5): a StorageClass with `fork:
/// "true"`, whose claims clone a `dataSource` claim.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForkClass {
    /// The fork's volume name, unique on the remote (it names
    /// `{pvName}`).
    pub volume: String,
    pub on_delete: OnDelete,
    pub workload_mounts: Vec<String>,
}

impl ForkClass {
    /// `Ok(None)` unless `parameters` has `fork: "true"`. A fork class must
    /// not set `profile` or `branch`, and its `volumeName` template must
    /// contain `{pvName}`; `onDelete` defaults to `delete` (publishing
    /// already gates deletion, design §19.1 "Fork onDelete").
    pub fn from_create(
        name: &str,
        parameters: &HashMap<String, String>,
    ) -> Result<Option<Self>, ParamError> {
        match parameters.get("fork").map(String::as_str) {
            None | Some("false") => return Ok(None),
            Some("true") => {}
            Some(other) => return Err(ParamError::BadFork(other.to_owned())),
        }
        let mut template = None;
        let mut on_delete = OnDelete::Delete;
        let mut workload_mounts = Vec::new();
        for (key, value) in parameters {
            match key.as_str() {
                "fork" => {}
                "profile" => return Err(ParamError::ForkClassSets("profile")),
                "branch" => return Err(ParamError::ForkClassSets("branch")),
                "volumeName" => template = Some(value.clone()),
                "onDelete" => on_delete = value.parse()?,
                "workloadMounts" => workload_mounts = parse_workload_mounts(value)?,
                k if k.starts_with(CO_PREFIX) => {}
                other => return Err(ParamError::UnknownKey(other.to_owned())),
            }
        }
        let template = template.unwrap_or_else(|| default_template(parameters));
        if !template.contains("{pvName}") {
            return Err(ParamError::ForkNameNotUnique { template });
        }
        Ok(Some(Self {
            volume: volume_name(name, &template, parameters)?,
            on_delete,
            workload_mounts,
        }))
    }

    /// The fork's parameters: the source's profile, no branch.
    pub fn params(&self, source: &VolumeParams) -> VolumeParams {
        VolumeParams {
            profile: source.profile.clone(),
            volume: self.volume.clone(),
            branch: None,
            on_delete: self.on_delete,
            workload_mounts: self.workload_mounts.clone(),
        }
    }
}

/// The StorageClass parameter and volume attribute naming where pods mount
/// the volume.
pub const WORKLOAD_MOUNTS: &str = "workloadMounts";

/// `workloadMounts`: comma-separated `PATH` or `PATH=DIR` (design §5),
/// each as `agentfs mount --workload-mount` takes it.
pub fn parse_workload_mounts(value: &str) -> Result<Vec<String>, ParamError> {
    value
        .split(',')
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(|v| {
            agentfs_fuse::views::parse_workload_mount(v)
                .map(|_| v.to_owned())
                .map_err(ParamError::BadWorkloadMount)
        })
        .collect()
}

/// The `workloadMounts` of a NodePublish volume context; none if absent.
pub fn workload_mounts(context: &HashMap<String, String>) -> Result<Vec<String>, ParamError> {
    context
        .get(WORKLOAD_MOUNTS)
        .map_or(Ok(Vec::new()), |v| parse_workload_mounts(v))
}

/// `[a-z0-9]([a-z0-9-]*[a-z0-9])?`, at most [`MAX_PROFILE_LEN`].
pub fn validate_profile(profile: &str) -> Result<(), ParamError> {
    let b = profile.as_bytes();
    let edge = |c: &u8| c.is_ascii_lowercase() || c.is_ascii_digit();
    let ok = !b.is_empty()
        && b.len() <= MAX_PROFILE_LEN
        && b.first().is_some_and(edge)
        && b.last().is_some_and(edge)
        && b.iter().all(|c| edge(c) || *c == b'-');
    if ok {
        Ok(())
    } else {
        Err(ParamError::BadProfile(profile.to_owned()))
    }
}

/// A conservative subset of git's ref-name rules (the branch becomes
/// `refs/heads/<branch>`).
fn validate_branch(branch: &str) -> Result<(), ParamError> {
    let ok = !branch.is_empty()
        && branch
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'.' | b'_' | b'-' | b'/'))
        && !branch.starts_with(['-', '/', '.'])
        && !branch.ends_with(['/', '.'])
        && !branch.ends_with(".lock")
        && !branch.contains("..")
        && !branch.contains("//")
        && !branch.contains("/.")
        && !branch.split('/').any(|c| c.ends_with(".lock"));
    if ok {
        Ok(())
    } else {
        Err(ParamError::BadBranch(branch.to_owned()))
    }
}

fn render(
    template: &str,
    parameters: &HashMap<String, String>,
    pv_name: &str,
) -> Result<String, ParamError> {
    let mut out = String::new();
    let mut rest = template;
    while let Some(start) = rest.find('{') {
        out.push_str(&rest[..start]);
        let after = &rest[start + 1..];
        let Some(end) = after.find('}') else {
            out.push_str(&rest[start..]);
            rest = "";
            break;
        };
        let name = &after[..end];
        let value = match name {
            "pvcNamespace" => parameters.get(PVC_NAMESPACE).map(String::as_str),
            "pvcName" => parameters.get(PVC_NAME).map(String::as_str),
            "pvName" => Some(pv_name),
            _ => {
                return Err(ParamError::UnknownPlaceholder {
                    template: template.to_owned(),
                    name: name.to_owned(),
                });
            }
        };
        let Some(value) = value else {
            return Err(ParamError::MissingMetadata {
                template: template.to_owned(),
                name: name.to_owned(),
            });
        };
        out.push_str(value);
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    Ok(out)
}

/// What the kubelet says about the pod a volume is published for
/// (`podInfoOnMount: true`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PodInfo {
    pub namespace: String,
    pub name: String,
    pub uid: String,
    pub service_account: String,
}

impl PodInfo {
    /// The pod from NodePublish's volume context, if the CO sent it (the
    /// kubelet does with `podInfoOnMount`; csi-sanity does not).
    pub fn from_context(context: &HashMap<String, String>) -> Option<Self> {
        let get = |k: &str| context.get(k).filter(|v| !v.is_empty()).cloned();
        Some(Self {
            namespace: get(POD_NAMESPACE)?,
            name: get(POD_NAME)?,
            uid: get(POD_UID)?,
            service_account: get(POD_SERVICE_ACCOUNT).unwrap_or_default(),
        })
    }
}

/// Whether the volume context is an inline ephemeral volume's.
pub fn is_ephemeral(context: &HashMap<String, String>) -> bool {
    context.get(EPHEMERAL).is_some_and(|v| v == "true")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A12: what a fork class may and may not set.
    #[test]
    fn fork_classes() {
        let meta = [
            (PVC_NAME, "sandbox"),
            (PVC_NAMESPACE, "agents"),
            (PV_NAME, "pvc-9"),
        ];
        let with = |extra: &[(&'static str, &'static str)]| {
            let mut pairs = meta.to_vec();
            pairs.extend_from_slice(extra);
            map(&pairs)
        };
        assert_eq!(ForkClass::from_create("pvc-9", &with(&[])).unwrap(), None);
        assert_eq!(
            ForkClass::from_create("pvc-9", &with(&[("fork", "false")])).unwrap(),
            None
        );
        let class = ForkClass::from_create("pvc-9", &with(&[("fork", "true")]))
            .unwrap()
            .unwrap();
        assert_eq!(class.volume, "agents.sandbox.pvc-9");
        assert_eq!(class.on_delete, OnDelete::Delete);
        let source = VolumeParams::from_create("pvc-1", &map(&[("profile", "example")])).unwrap();
        let params = class.params(&source);
        assert_eq!(params.profile, "example");
        assert_eq!(params.branch, None);
        assert_eq!(params.branch_name(), "agentfs/agents.sandbox.pvc-9");
        for (key, what) in [("profile", "profile"), ("branch", "branch")] {
            assert_eq!(
                ForkClass::from_create("pvc-9", &with(&[("fork", "true"), (key, "x")])),
                Err(ParamError::ForkClassSets(what))
            );
        }
        assert!(matches!(
            ForkClass::from_create(
                "pvc-9",
                &with(&[("fork", "true"), ("volumeName", "{pvcName}")])
            ),
            Err(ParamError::ForkNameNotUnique { .. })
        ));
        assert_eq!(
            ForkClass::from_create("pvc-9", &with(&[("fork", "yes")])),
            Err(ParamError::BadFork("yes".into()))
        );
        // An ordinary class accepts fork: "false" and refuses anything else.
        VolumeParams::from_create("pvc-1", &map(&[("profile", "p"), ("fork", "false")])).unwrap();
        assert_eq!(
            VolumeParams::from_create("pvc-1", &map(&[("profile", "p"), ("fork", "true")])),
            Err(ParamError::BadFork("true".into()))
        );
    }

    fn map(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    }

    #[test]
    fn the_volume_name_defaults_to_namespace_claim_and_pv() {
        let p = VolumeParams::from_create(
            "pvc-1234",
            &map(&[
                ("profile", "example"),
                (PVC_NAME, "data"),
                (PVC_NAMESPACE, "agents"),
                (PV_NAME, "pvc-1234"),
            ]),
        )
        .unwrap();
        assert_eq!(p.volume, "agents.data.pvc-1234");
        assert_eq!(p.branch, None);
        assert_eq!(p.branch_name(), "agentfs/agents.data.pvc-1234");
        assert_eq!(p.on_delete, OnDelete::Retain);
        let ctx = p.to_context();
        assert_eq!(ctx["profile"], "example");
        assert_eq!(ctx["volume"], "agents.data.pvc-1234");
        assert_eq!(ctx["branch"], "agentfs/agents.data.pvc-1234");
        assert_eq!(ctx["onDelete"], "retain");
        // The context round-trips (branch now explicit).
        let back = VolumeParams::from_context("pvc-1234", &ctx).unwrap();
        assert_eq!(back.volume, p.volume);
        assert_eq!(back.branch_name(), p.branch_name());
    }

    #[test]
    fn without_metadata_the_volume_is_the_pv_name() {
        let p = VolumeParams::from_create("sanity-x-1", &map(&[("profile", "s")])).unwrap();
        assert_eq!(p.volume, "sanity-x-1");
        let p = VolumeParams::from_create("ignored", &map(&[("profile", "s"), (PV_NAME, "pvc-9")]))
            .unwrap();
        assert_eq!(p.volume, "pvc-9");
    }

    #[test]
    fn templates_render_and_fail_clearly() {
        let md = [
            ("profile", "p"),
            (PVC_NAME, "data"),
            (PVC_NAMESPACE, "ns"),
            (PV_NAME, "pvc-1"),
        ];
        let with = |t: &str| {
            let mut m = map(&md);
            m.insert("volumeName".into(), t.into());
            VolumeParams::from_create("pvc-1", &m)
        };
        assert_eq!(with("team-{pvcName}").unwrap().volume, "team-data");
        assert_eq!(with("{pvName}").unwrap().volume, "pvc-1");
        assert_eq!(
            with("{pvcNamespace}_{pvcName}_{pvName}").unwrap().volume,
            "ns_data_pvc-1"
        );
        assert!(matches!(
            with("{pod}").unwrap_err(),
            ParamError::UnknownPlaceholder { .. }
        ));
        assert!(matches!(
            with("a/{pvcName}").unwrap_err(),
            ParamError::BadVolumeName { .. }
        ));
        assert!(matches!(
            with("{pvcName}..x").unwrap_err(),
            ParamError::BadVolumeName { .. }
        ));
        assert!(matches!(
            with("{pvcName}.lock").unwrap_err(),
            ParamError::BadVolumeName { .. }
        ));
        let err = VolumeParams::from_create(
            "pvc-1",
            &map(&[("profile", "p"), ("volumeName", "{pvcName}")]),
        )
        .unwrap_err();
        assert!(matches!(err, ParamError::MissingMetadata { .. }));
        assert!(err.to_string().contains("--extra-create-metadata"), "{err}");
    }

    #[test]
    fn profile_names_follow_dns_label_rules() {
        for good in ["a", "example", "ci-2", "0"] {
            validate_profile(good).unwrap();
        }
        let long = "a".repeat(64);
        for bad in [
            "",
            "Example",
            "-a",
            "a-",
            "a.b",
            "a_b",
            "a/b",
            long.as_str(),
        ] {
            assert!(validate_profile(bad).is_err(), "{bad}");
        }
        assert_eq!(
            VolumeParams::from_create("v", &map(&[])).unwrap_err(),
            ParamError::MissingProfile
        );
        assert!(matches!(
            VolumeParams::from_create("v", &map(&[("profile", "../etc")])).unwrap_err(),
            ParamError::BadProfile(_)
        ));
    }

    #[test]
    fn on_delete_branch_and_unknown_keys() {
        let p = VolumeParams::from_create(
            "v",
            &map(&[
                ("profile", "p"),
                ("onDelete", "delete"),
                ("branch", "team/x"),
            ]),
        )
        .unwrap();
        assert_eq!(p.on_delete, OnDelete::Delete);
        assert_eq!(p.branch.as_deref(), Some("team/x"));
        assert_eq!(
            VolumeParams::from_create("v", &map(&[("profile", "p"), ("onDelete", "keep")]))
                .unwrap_err(),
            ParamError::BadOnDelete("keep".into())
        );
        for bad in [
            "", "-x", "a..b", "a.lock", "a/.b", "a b", "a/", "/a", "x.lock/y",
        ] {
            assert!(
                VolumeParams::from_create("v", &map(&[("profile", "p"), ("branch", bad)])).is_err(),
                "{bad:?}"
            );
        }
        assert_eq!(
            VolumeParams::from_create("v", &map(&[("profile", "p"), ("remote", "x")])).unwrap_err(),
            ParamError::UnknownKey("remote".into())
        );
        // The CO's own keys are ignored.
        VolumeParams::from_create(
            "v",
            &map(&[("profile", "p"), ("csi.storage.k8s.io/fstype", "ext4")]),
        )
        .unwrap();
    }

    #[test]
    fn workload_mounts_reach_the_volume_context_and_back() {
        let p = VolumeParams::from_create(
            "pv-1",
            &map(&[
                ("profile", "p"),
                ("workloadMounts", "/workspace, /src=proj"),
            ]),
        )
        .unwrap();
        assert_eq!(p.workload_mounts, ["/workspace", "/src=proj"]);
        let context = p.to_context();
        assert_eq!(context["workloadMounts"], "/workspace,/src=proj");
        let back = VolumeParams::from_context("pv-1", &context).unwrap();
        assert_eq!(back.workload_mounts, p.workload_mounts);
        assert_eq!(
            workload_mounts(&context).unwrap(),
            ["/workspace", "/src=proj"]
        );
        assert!(workload_mounts(&map(&[])).unwrap().is_empty());
        for bad in ["workspace", "/w=../up"] {
            assert!(matches!(
                VolumeParams::from_create("v", &map(&[("profile", "p"), ("workloadMounts", bad)])),
                Err(ParamError::BadWorkloadMount(_))
            ));
        }
        // Without it, the context is as before.
        let p = VolumeParams::from_create("v", &map(&[("profile", "p")])).unwrap();
        assert!(!p.to_context().contains_key("workloadMounts"));
    }

    #[test]
    fn a_static_pv_context_defaults_the_volume_to_the_handle() {
        let p = VolumeParams::from_context(
            "retained-1",
            &map(&[
                ("profile", "p"),
                ("storage.kubernetes.io/csiProvisionerIdentity", "x"),
            ]),
        )
        .unwrap();
        assert_eq!(p.volume, "retained-1");
        assert_eq!(p.branch, None);
        assert_eq!(
            VolumeParams::from_context("h", &map(&[])).unwrap_err(),
            ParamError::MissingProfile
        );
        assert!(
            VolumeParams::from_context("h", &map(&[("profile", "p"), ("volume", "a/b")])).is_err()
        );
    }

    #[test]
    fn a_static_pv_names_its_volume_with_volume_name() {
        // As docs/csi.md §5 documents it.
        let p = VolumeParams::from_context(
            "pvc-0b6f",
            &map(&[("profile", "p"), ("volumeName", "research.notes")]),
        )
        .unwrap();
        assert_eq!(p.volume, "research.notes");
        assert_eq!(p.branch_name(), "agentfs/research.notes");
        // `volume` (CreateVolume's context) is the same key; agreeing is fine.
        let both = map(&[("profile", "p"), ("volume", "x"), ("volumeName", "x")]);
        assert_eq!(VolumeParams::from_context("h", &both).unwrap().volume, "x");
        let clash = map(&[("profile", "p"), ("volume", "x"), ("volumeName", "y")]);
        assert!(matches!(
            VolumeParams::from_context("h", &clash).unwrap_err(),
            ParamError::ConflictingNames { .. }
        ));
        // Not a template.
        assert!(matches!(
            VolumeParams::from_context("h", &map(&[("profile", "p"), ("volumeName", "{pvName}")]))
                .unwrap_err(),
            ParamError::BadVolumeName { .. }
        ));
        // A typo is refused, not ignored; the CO's own keys are ignored.
        assert_eq!(
            VolumeParams::from_context("h", &map(&[("profile", "p"), ("volumename", "x")]))
                .unwrap_err(),
            ParamError::UnknownAttribute("volumename".into())
        );
        VolumeParams::from_context(
            "h",
            &map(&[
                ("profile", "p"),
                ("storage.kubernetes.io/csiProvisionerIdentity", "x"),
                (POD_NAME, "w"),
            ]),
        )
        .unwrap();
    }

    #[test]
    fn pod_info_and_ephemeral() {
        let ctx = map(&[
            (POD_NAME, "writer-a"),
            (POD_NAMESPACE, "apps"),
            (POD_UID, "1234"),
            (POD_SERVICE_ACCOUNT, "default"),
        ]);
        let pod = PodInfo::from_context(&ctx).unwrap();
        assert_eq!(
            pod,
            PodInfo {
                namespace: "apps".into(),
                name: "writer-a".into(),
                uid: "1234".into(),
                service_account: "default".into(),
            }
        );
        assert_eq!(PodInfo::from_context(&map(&[(POD_NAME, "x")])), None);
        assert!(!is_ephemeral(&ctx));
        assert!(is_ephemeral(&map(&[(EPHEMERAL, "true")])));
        assert!(!is_ephemeral(&map(&[(EPHEMERAL, "false")])));
    }
}
