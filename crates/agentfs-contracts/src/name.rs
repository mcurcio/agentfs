//! Validated names and labels: each is built only by its `parse`, so a value of
//! the type is valid by construction (docs/target-architecture/system.md, the
//! shared kernel). Their text is what history and rows record.

use serde::{Deserialize, Serialize};

/// Text that is not a valid name of its kind.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{value:?} is not a valid {what}: {rule}")]
pub struct NameError {
    /// The kind of name, as a flag or key names it (`volume`, `fork key`).
    pub what: &'static str,
    pub value: String,
    /// The rule it breaks, in words.
    pub rule: &'static str,
}

/// The safe-name rule, its one home: an ASCII letter or digit first, then
/// letters, digits, `.`, `_` and `-`; no `..`, no trailing `.` or `.lock`.
/// A volume name, a client label and a launcher id follow it, and so does
/// `agentfs_core::config::validate_name` (it calls this).
pub fn is_safe_name(value: &str) -> bool {
    let _ = value;
    todo!("S1: move the rule from agentfs_core::config::validate_name")
}

/// A volume's name: a label and a guard, never its identity (the
/// [`crate::volume::VolumeId`] is). [`is_safe_name`], at most 253 bytes.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct VolumeName(String);

impl VolumeName {
    /// # Errors
    ///
    /// [`NameError`] (`what`: `volume`) for text that breaks the rule.
    pub fn parse(text: &str) -> Result<Self, NameError> {
        let _ = text;
        todo!("S1")
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// One field of a pod context: Kubernetes object names and UIDs. 1 to 253
/// bytes of printable ASCII with no whitespace.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Label(String);

impl Label {
    /// # Errors
    ///
    /// [`NameError`] (`what`: `pod context field`).
    pub fn parse(text: &str) -> Result<Self, NameError> {
        let _ = text;
        todo!("S1")
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// The text of a commit's `Agentfs-Writer` trailer: 1 to 256 bytes, no control
/// character (the tree actor's rule today). The trial CLI's `--writer` until
/// slice S4, then a [`crate::clients::ClientId`]'s text.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct WriterLabel(String);

impl WriterLabel {
    /// The writer of a sync and of the Root commit (git-model §3).
    pub const AGENTFS: &'static str = "agentfs";

    /// # Errors
    ///
    /// [`NameError`] (`what`: `writer`).
    pub fn parse(text: &str) -> Result<Self, NameError> {
        let _ = text;
        todo!("S1")
    }

    /// `agentfs`.
    pub fn agentfs() -> Self {
        todo!("S1")
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// A client's label, the launcher's word for it (clients.md): [`is_safe_name`],
/// at most 64 bytes. Today's `--writer` label, from slice S4.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct ClientLabel(String);

impl ClientLabel {
    /// # Errors
    ///
    /// [`NameError`] (`what`: `client label`).
    pub fn parse(text: &str) -> Result<Self, NameError> {
        let _ = text;
        todo!("S4")
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Who launches clients (system.md §2.2): [`is_safe_name`], at most 64 bytes.
/// The trial CLI is `cli`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct LauncherId(String);

impl LauncherId {
    /// # Errors
    ///
    /// [`NameError`] (`what`: `launcher`).
    pub fn parse(text: &str) -> Result<Self, NameError> {
        let _ = text;
        todo!("S1")
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// A fork's idempotency key (trees.md §2): 1 to 128 bytes of printable ASCII
/// with no whitespace, so a CSI volume handle fits.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct ForkKey(String);

impl ForkKey {
    /// # Errors
    ///
    /// [`NameError`] (`what`: `fork key`).
    pub fn parse(text: &str) -> Result<Self, NameError> {
        let _ = text;
        todo!("S5")
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

macro_rules! text_conversions {
    ($($name:ident),*) => {$(
        impl TryFrom<String> for $name {
            type Error = NameError;

            fn try_from(text: String) -> Result<Self, NameError> {
                Self::parse(&text)
            }
        }

        impl From<$name> for String {
            fn from(name: $name) -> Self {
                name.0
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(&self.0)
            }
        }
    )*};
}

text_conversions!(
    VolumeName,
    Label,
    WriterLabel,
    ClientLabel,
    LauncherId,
    ForkKey
);
