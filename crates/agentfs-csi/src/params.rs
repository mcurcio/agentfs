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
//! | `volumeName` | the agentfs volume name, a template over `{pvcNamespace}`, `{pvcName}` and `{pvName}`; default `{pvcNamespace}.{pvcName}` (with the provisioner's `--extra-create-metadata`), else the PV name |
//! | `branch` | the history branch (default `agentfs/<volume>`) |
//! | `onDelete` | `retain` (default: DeleteVolume keeps the directory) or `delete` |
//!
//! Keys under `csi.storage.k8s.io/` (the provisioner's metadata) are read
//! for the template and otherwise ignored; any other key is refused.

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
    #[error("unknown StorageClass parameter {0:?} (known: profile, volumeName, branch, onDelete)")]
    UnknownKey(String),
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
        for (key, value) in parameters {
            match key.as_str() {
                "profile" => profile = Some(value.clone()),
                "volumeName" => template = Some(value.clone()),
                "branch" => branch = Some(value.clone()),
                "onDelete" => on_delete = value.parse()?,
                k if k.starts_with(CO_PREFIX) => {}
                other => return Err(ParamError::UnknownKey(other.to_owned())),
            }
        }
        let profile = profile.ok_or(ParamError::MissingProfile)?;
        validate_profile(&profile)?;
        let pv_name = parameters.get(PV_NAME).map_or(name, String::as_str);
        let template = template.unwrap_or_else(|| {
            if parameters.contains_key(PVC_NAME) && parameters.contains_key(PVC_NAMESPACE) {
                "{pvcNamespace}.{pvcName}".to_owned()
            } else {
                "{pvName}".to_owned()
            }
        });
        let volume = render(&template, parameters, pv_name)?;
        if validate_name("agentfs.volume", &volume).is_err() {
            return Err(ParamError::BadVolumeName {
                template,
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
        })
    }

    /// From a volume context: what CreateVolume returned, or a static PV's
    /// `volumeAttributes` (`profile` required; the volume name defaults to
    /// the volume handle). Keys this driver does not know are ignored: the
    /// CO adds its own (`storage.kubernetes.io/csiProvisionerIdentity`).
    pub fn from_context(
        handle: &str,
        context: &HashMap<String, String>,
    ) -> Result<Self, ParamError> {
        let profile = context
            .get("profile")
            .cloned()
            .ok_or(ParamError::MissingProfile)?;
        validate_profile(&profile)?;
        let volume = context
            .get("volume")
            .cloned()
            .unwrap_or_else(|| handle.to_owned());
        if validate_name("agentfs.volume", &volume).is_err() {
            return Err(ParamError::BadVolumeName {
                template: "volume".into(),
                value: volume,
            });
        }
        let branch = context.get("branch").cloned().filter(|b| !b.is_empty());
        if let Some(branch) = &branch {
            validate_branch(branch)?;
        }
        let on_delete = match context.get("onDelete") {
            Some(v) => v.parse()?,
            None => OnDelete::default(),
        };
        Ok(Self {
            profile,
            volume,
            branch,
            on_delete,
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
        HashMap::from([
            ("profile".to_owned(), self.profile.clone()),
            ("volume".to_owned(), self.volume.clone()),
            ("branch".to_owned(), self.branch_name()),
            ("onDelete".to_owned(), self.on_delete.to_string()),
        ])
    }
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

    fn map(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    }

    #[test]
    fn the_volume_name_defaults_to_namespace_and_claim() {
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
        assert_eq!(p.volume, "agents.data");
        assert_eq!(p.branch, None);
        assert_eq!(p.branch_name(), "agentfs/agents.data");
        assert_eq!(p.on_delete, OnDelete::Retain);
        let ctx = p.to_context();
        assert_eq!(ctx["profile"], "example");
        assert_eq!(ctx["volume"], "agents.data");
        assert_eq!(ctx["branch"], "agentfs/agents.data");
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
