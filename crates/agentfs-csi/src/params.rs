//! StorageClass and PersistentVolume parameters, and what the CO tells a
//! node about the pod (docs/csi.md).
//!
//! Every setting of a volume is the operator's: a StorageClass's
//! `parameters` (or a static PV's `volumeAttributes`) name a **profile**,
//! a config fragment and rule files the driver's own pods mount from its
//! namespace. Pod authors choose only the StorageClass.
//!
//! | Parameter | Meaning |
//! |---|---|
//! | `profile` | required: `<profile>.config`, `.exclude`, `.attributes` in the profiles directory |
//! | `volumeName` | the agentfs volume name, a template over `{pvcNamespace}`, `{pvcName}` and `{pvName}` that must contain `{pvName}`; default [`DEFAULT_TEMPLATE`] (with the provisioner's `--extra-create-metadata`), else the PV name. In a static PV's attributes, a literal name |
//! | `onDelete` | `retain` only: DeleteVolume keeps the volume |
//! | `fork` | `"true"`: a fork class ([`is_fork_class`]); `"false"` or absent: an ordinary one |
//!
//! `startFrom`, `workloadMounts` and `onDelete: delete` are refused: they
//! are not built on tree volumes. Keys under `csi.storage.k8s.io/` (the
//! provisioner's metadata) are read for the template and otherwise
//! ignored; any other key is refused.

use std::collections::HashMap;

use agentfs_contracts::name::{Label, VolumeName};
use agentfs_contracts::pod::PodContext;

/// The provisioner's metadata keys (`--extra-create-metadata`).
pub const PVC_NAME: &str = "csi.storage.k8s.io/pvc/name";
pub const PVC_NAMESPACE: &str = "csi.storage.k8s.io/pvc/namespace";
pub const PV_NAME: &str = "csi.storage.k8s.io/pv/name";

/// `podInfoOnMount` keys in NodePublish's volume context.
pub const POD_NAME: &str = "csi.storage.k8s.io/pod.name";
pub const POD_NAMESPACE: &str = "csi.storage.k8s.io/pod.namespace";
pub const POD_UID: &str = "csi.storage.k8s.io/pod.uid";
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

/// What `onDelete: delete` is told.
pub const ON_DELETE_NOT_BUILT: &str = "onDelete: delete is not built on tree volumes: a deleted \
    claim's volume is kept, because its history is pushed nowhere yet";
/// What `startFrom` is told.
pub const START_FROM_NOT_BUILT: &str = "startFrom is not built on tree volumes: a volume starts \
    empty; moving a volume between nodes is not built (docs/runbooks/moving.md)";
/// What `workloadMounts` is told.
pub const WORKLOAD_MOUNTS_NOT_BUILT: &str = "workloadMounts is not built on tree volumes: tree \
    volumes keep nested repositories untracked";

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ParamError {
    #[error("the StorageClass parameter `profile` is required")]
    MissingProfile,
    #[error(
        "profile {0:?} is not a valid name: lower-case letters, digits and '-', starting and \
         ending with a letter or digit, at most 63 characters"
    )]
    BadProfile(String),
    #[error("unknown StorageClass parameter {0:?} (known: profile, volumeName, onDelete, fork)")]
    UnknownKey(String),
    #[error("unknown volume attribute {0:?} (known: profile, volumeName (or volume), onDelete)")]
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
    /// Any class: volumeName must contain {pvName}.
    #[error(
        "volumeName {template:?} must contain {{pvName}}, so each claim is a volume of its own"
    )]
    NameNotUnique { template: String },
    #[error("{}", ON_DELETE_NOT_BUILT)]
    OnDeleteNotBuilt,
    #[error("onDelete {0:?} must be \"retain\"")]
    BadOnDelete(String),
    #[error("{}", START_FROM_NOT_BUILT)]
    StartFromNotBuilt,
    #[error("{}", WORKLOAD_MOUNTS_NOT_BUILT)]
    WorkloadMountsNotBuilt,
    #[error("fork {0:?} must be \"true\" or \"false\"")]
    BadFork(String),
    #[error("fork claims are not built yet")]
    ForkClass,
    #[error("{0} is a fork class's parameter (fork: \"true\"); an ordinary class has no source")]
    ForkOnly(&'static str),
    #[error("the pod's {what} {value:?} is not a valid label")]
    BadPod { what: &'static str, value: String },
}

/// A tree volume's settings, from CreateVolume's parameters or a static
/// PV's volume attributes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VolumeParams {
    pub profile: String,
    pub name: VolumeName,
}

/// `onDelete`: only `retain` is built.
fn on_delete(value: &str) -> Result<(), ParamError> {
    match value {
        "retain" => Ok(()),
        "delete" => Err(ParamError::OnDeleteNotBuilt),
        other => Err(ParamError::BadOnDelete(other.to_owned())),
    }
}

impl VolumeParams {
    /// From CreateVolume: `pv_name` is the PV name the CO chose,
    /// `parameters` the StorageClass's (plus the provisioner's metadata).
    pub fn from_create(
        pv_name: &str,
        parameters: &HashMap<String, String>,
    ) -> Result<Self, ParamError> {
        let mut profile = None;
        let mut template = None;
        // Refusals in a fixed order, so a class with several gets the same
        // answer every time.
        let mut keys: Vec<&String> = parameters.keys().collect();
        keys.sort();
        for key in keys {
            let value = &parameters[key];
            match key.as_str() {
                "profile" => profile = Some(value.clone()),
                "volumeName" => template = Some(value.clone()),
                "onDelete" => on_delete(value)?,
                "startFrom" => return Err(ParamError::StartFromNotBuilt),
                "workloadMounts" => return Err(ParamError::WorkloadMountsNotBuilt),
                "fork" if value == "false" => {}
                "fork" if value == "true" => return Err(ParamError::ForkClass),
                "fork" => return Err(ParamError::BadFork(value.clone())),
                "link" => return Err(ParamError::ForkOnly("link")),
                "remotes" => return Err(ParamError::ForkOnly("remotes")),
                k if k.starts_with(CO_PREFIX) => {}
                other => return Err(ParamError::UnknownKey(other.to_owned())),
            }
        }
        let profile = profile.ok_or(ParamError::MissingProfile)?;
        validate_profile(&profile)?;
        let name = unique_volume_name(pv_name, template, parameters)?;
        Ok(Self { profile, name })
    }

    /// From a static PV's `volumeAttributes`: `profile` (required),
    /// `volumeName` or `volume` (a literal name, not a template; default:
    /// the volume handle) and `onDelete` (`retain` only). Keys under the
    /// CO's prefixes are ignored; any other key is refused, so a typo fails
    /// loudly rather than naming the volume after its handle.
    pub fn from_static(
        handle: &str,
        attributes: &HashMap<String, String>,
    ) -> Result<Self, ParamError> {
        let mut profile = None;
        let mut names: Vec<&str> = Vec::new();
        let mut keys: Vec<&String> = attributes.keys().collect();
        keys.sort();
        for key in keys {
            let value = &attributes[key];
            match key.as_str() {
                "profile" => profile = Some(value.clone()),
                "volume" | "volumeName" => names.push(value),
                "onDelete" => on_delete(value)?,
                k if CO_CONTEXT_PREFIXES.iter().any(|p| k.starts_with(p)) => {}
                other => return Err(ParamError::UnknownAttribute(other.to_owned())),
            }
        }
        let profile = profile.ok_or(ParamError::MissingProfile)?;
        validate_profile(&profile)?;
        let name = match names.as_slice() {
            [] => handle,
            [one] => *one,
            [a, b] if a == b => *a,
            [a, b, ..] => {
                return Err(ParamError::ConflictingNames {
                    a: (*a).to_owned(),
                    b: (*b).to_owned(),
                });
            }
        };
        let name = VolumeName::parse(name).map_err(|_| ParamError::BadVolumeName {
            template: "volumeName".into(),
            value: name.to_owned(),
        })?;
        Ok(Self { profile, name })
    }

    /// The volume context CreateVolume returns, which every node call
    /// carries back: `profile` and `volume`. It reads back with
    /// [`VolumeParams::from_static`].
    pub fn to_context(&self) -> HashMap<String, String> {
        HashMap::from([
            ("profile".to_owned(), self.profile.clone()),
            ("volume".to_owned(), self.name.as_str().to_owned()),
        ])
    }
}

/// Whether the StorageClass is a fork class (`fork: "true"`).
pub fn is_fork_class(parameters: &HashMap<String, String>) -> bool {
    parameters.get("fork").is_some_and(|v| v == "true")
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

/// The volume name of a class's `volumeName` template (default
/// [`default_template`]) for the PV `name`: it must contain `{pvName}`.
fn unique_volume_name(
    name: &str,
    template: Option<String>,
    parameters: &HashMap<String, String>,
) -> Result<VolumeName, ParamError> {
    let template = template.unwrap_or_else(|| default_template(parameters));
    if !template.contains("{pvName}") {
        return Err(ParamError::NameNotUnique { template });
    }
    let pv_name = parameters.get(PV_NAME).map_or(name, String::as_str);
    let value = render(&template, parameters, pv_name)?;
    VolumeName::parse(&value).map_err(|_| ParamError::BadVolumeName { template, value })
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

/// The pod context of a view: the pod's namespace, name and uid from the
/// `podInfoOnMount` keys (each absent when the CO does not send it, as
/// csi-sanity does not), and this node.
pub fn pod_context(
    volume_context: &HashMap<String, String>,
    node: &str,
) -> Result<PodContext, ParamError> {
    let label = |what: &'static str, value: Option<&str>| {
        value
            .filter(|v| !v.is_empty())
            .map(|v| {
                Label::parse(v).map_err(|_| ParamError::BadPod {
                    what,
                    value: v.to_owned(),
                })
            })
            .transpose()
    };
    let get = |key: &str| volume_context.get(key).map(String::as_str);
    Ok(PodContext {
        namespace: label("namespace", get(POD_NAMESPACE))?,
        pod: label("name", get(POD_NAME))?,
        pod_uid: label("uid", get(POD_UID))?,
        node: label("node", Some(node))?,
    })
}

/// Whether the volume context is an inline ephemeral volume's.
pub fn is_ephemeral(volume_context: &HashMap<String, String>) -> bool {
    volume_context.get(EPHEMERAL).is_some_and(|v| v == "true")
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

    fn name(text: &str) -> VolumeName {
        VolumeName::parse(text).unwrap()
    }

    const META: [(&str, &str); 3] = [
        (PVC_NAME, "data"),
        (PVC_NAMESPACE, "apps"),
        (PV_NAME, "pvc-1"),
    ];

    fn with_meta(extra: &[(&str, &str)]) -> HashMap<String, String> {
        let mut m = map(&META);
        m.extend(map(extra));
        m
    }

    #[test]
    fn every_accepted_key() {
        let p = VolumeParams::from_create("pvc-1", &with_meta(&[("profile", "p")])).unwrap();
        assert_eq!(
            p,
            VolumeParams {
                profile: "p".into(),
                name: name("apps.data.pvc-1")
            }
        );
        // Without the metadata the PV name is the volume name.
        let p = VolumeParams::from_create("pvc-2", &map(&[("profile", "p")])).unwrap();
        assert_eq!(p.name, name("pvc-2"));
        let p = VolumeParams::from_create(
            "pvc-1",
            &with_meta(&[
                ("profile", "web-1"),
                ("volumeName", "x-{pvcName}-{pvName}"),
                ("onDelete", "retain"),
                ("fork", "false"),
            ]),
        )
        .unwrap();
        assert_eq!(p.profile, "web-1");
        assert_eq!(p.name, name("x-data-pvc-1"));
        assert_eq!(
            p.to_context(),
            map(&[("profile", "web-1"), ("volume", "x-data-pvc-1")])
        );
        // The context reads back as a static PV's attributes.
        assert_eq!(VolumeParams::from_static("h", &p.to_context()).unwrap(), p);
    }

    #[test]
    fn every_refusal_and_its_message() {
        let refused = |extra: &[(&str, &str)]| {
            let mut m = with_meta(&[("profile", "p")]);
            m.extend(map(extra));
            VolumeParams::from_create("pvc-1", &m)
                .unwrap_err()
                .to_string()
        };
        assert_eq!(
            refused(&[("onDelete", "delete")]),
            "onDelete: delete is not built on tree volumes: a deleted claim's volume is kept, \
             because its history is pushed nowhere yet"
        );
        assert_eq!(
            refused(&[("onDelete", "Retain")]),
            "onDelete \"Retain\" must be \"retain\""
        );
        assert_eq!(
            refused(&[("startFrom", "origin:agentfs/v@x")]),
            "startFrom is not built on tree volumes: a volume starts empty; moving a volume \
             between nodes is not built (docs/runbooks/moving.md)"
        );
        assert_eq!(
            refused(&[("workloadMounts", "/workspace")]),
            "workloadMounts is not built on tree volumes: tree volumes keep nested repositories \
             untracked"
        );
        assert_eq!(
            refused(&[("branch", "main")]),
            "unknown StorageClass parameter \"branch\" (known: profile, volumeName, onDelete, \
             fork)"
        );
        assert_eq!(
            refused(&[("link", "linked")]),
            "link is a fork class's parameter (fork: \"true\"); an ordinary class has no source"
        );
        assert_eq!(
            refused(&[("remotes", "r")]),
            "remotes is a fork class's parameter (fork: \"true\"); an ordinary class has no \
             source"
        );
        assert_eq!(
            refused(&[("fork", "yes")]),
            "fork \"yes\" must be \"true\" or \"false\""
        );
        assert_eq!(
            refused(&[("fork", "true")]),
            "fork claims are not built yet"
        );
        assert_eq!(
            refused(&[("volumeName", "{pvcName}")]),
            "volumeName \"{pvcName}\" must contain {pvName}, so each claim is a volume of its own"
        );
        assert_eq!(
            refused(&[("volumeName", "{pvName}-{zone}")]),
            "volumeName \"{pvName}-{zone}\": unknown placeholder {zone} (known: pvcNamespace, \
             pvcName, pvName)"
        );
        assert_eq!(
            refused(&[("volumeName", "-{pvName}")]),
            "volumeName \"-{pvName}\" gives \"-pvc-1\", which is not a valid agentfs volume \
             name ([A-Za-z0-9][A-Za-z0-9._-]*, no '..', no .lock suffix)"
        );
        assert_eq!(
            VolumeParams::from_create(
                "pvc-1",
                &map(&[("profile", "p"), ("volumeName", "{pvcName}.{pvName}")])
            )
            .unwrap_err()
            .to_string(),
            "volumeName \"{pvcName}.{pvName}\" uses {pvcName}, which the CO did not provide (run \
             csi-provisioner with --extra-create-metadata)"
        );
        assert_eq!(
            VolumeParams::from_create("pvc-1", &map(&[])).unwrap_err(),
            ParamError::MissingProfile
        );
        assert_eq!(
            VolumeParams::from_create("pvc-1", &map(&[("profile", "Bad")])).unwrap_err(),
            ParamError::BadProfile("Bad".into())
        );
    }

    #[test]
    fn fork_classes() {
        assert!(is_fork_class(&map(&[("fork", "true")])));
        assert!(!is_fork_class(&map(&[("fork", "false")])));
        assert!(!is_fork_class(&map(&[])));
    }

    #[test]
    fn static_attributes() {
        let p = VolumeParams::from_static("h", &map(&[("profile", "p")])).unwrap();
        assert_eq!(p.name, name("h"));
        let p = VolumeParams::from_static(
            "h",
            &map(&[
                ("profile", "p"),
                ("volumeName", "apps.data.pvc-1"),
                ("onDelete", "retain"),
                ("storage.kubernetes.io/csiProvisionerIdentity", "x"),
                (POD_NAME, "w"),
            ]),
        )
        .unwrap();
        assert_eq!(p.name, name("apps.data.pvc-1"));
        assert_eq!(
            VolumeParams::from_static(
                "h",
                &map(&[("profile", "p"), ("volume", "a"), ("volumeName", "b")])
            )
            .unwrap_err(),
            ParamError::ConflictingNames {
                a: "a".into(),
                b: "b".into()
            }
        );
        assert_eq!(
            VolumeParams::from_static("h", &map(&[("profile", "p"), ("volumename", "x")]))
                .unwrap_err(),
            ParamError::UnknownAttribute("volumename".into())
        );
        assert_eq!(
            VolumeParams::from_static("h", &map(&[("profile", "p"), ("onDelete", "delete")]))
                .unwrap_err(),
            ParamError::OnDeleteNotBuilt
        );
        assert!(matches!(
            VolumeParams::from_static("h", &map(&[("profile", "p"), ("volume", "a b")]))
                .unwrap_err(),
            ParamError::BadVolumeName { .. }
        ));
    }

    #[test]
    fn pod_contexts_and_ephemeral() {
        let ctx = map(&[
            (POD_NAME, "writer-a"),
            (POD_NAMESPACE, "apps"),
            (POD_UID, "1234"),
            ("csi.storage.k8s.io/serviceAccount.name", "default"),
        ]);
        let pod = pod_context(&ctx, "node-1").unwrap();
        assert_eq!(pod.pod.unwrap().as_str(), "writer-a");
        assert_eq!(pod.namespace.unwrap().as_str(), "apps");
        assert_eq!(pod.pod_uid.unwrap().as_str(), "1234");
        assert_eq!(pod.node.unwrap().as_str(), "node-1");
        let bare = pod_context(&map(&[]), "node-1").unwrap();
        assert_eq!(bare.pod, None);
        assert_eq!(bare.namespace, None);
        assert_eq!(bare.node.unwrap().as_str(), "node-1");
        assert!(pod_context(&map(&[(POD_NAME, "a b")]), "n").is_err());
        assert!(!is_ephemeral(&ctx));
        assert!(is_ephemeral(&map(&[(EPHEMERAL, "true")])));
        assert!(!is_ephemeral(&map(&[(EPHEMERAL, "false")])));
    }
}
