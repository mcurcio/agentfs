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
    let bytes = value.as_bytes();
    bytes.first().is_some_and(u8::is_ascii_alphanumeric)
        && bytes
            .iter()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
        && !value.contains("..")
        && !value.ends_with(".lock")
        && !value.ends_with('.')
}

/// `Ok` when `holds`; else a [`NameError`] naming `what` and `rule`.
fn check(
    holds: bool,
    what: &'static str,
    value: &str,
    rule: &'static str,
) -> Result<(), NameError> {
    if holds {
        Ok(())
    } else {
        Err(NameError {
            what,
            value: value.to_owned(),
            rule,
        })
    }
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
        check(
            is_safe_name(text) && text.len() <= 253,
            "volume",
            text,
            "a safe name of at most 253 bytes: an ASCII letter or digit, then letters, digits, \
             '.', '_' or '-'; no '..', no trailing '.' or '.lock'",
        )?;
        Ok(Self(text.to_owned()))
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
        check(
            (1..=253).contains(&text.len()) && text.bytes().all(|b| b.is_ascii_graphic()),
            "pod context field",
            text,
            "1 to 253 bytes of printable ASCII with no whitespace",
        )?;
        Ok(Self(text.to_owned()))
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
        check(
            (1..=256).contains(&text.len()) && !text.chars().any(char::is_control),
            "writer",
            text,
            "1 to 256 bytes with no control character",
        )?;
        Ok(Self(text.to_owned()))
    }

    /// `agentfs`.
    pub fn agentfs() -> Self {
        Self(Self::AGENTFS.to_owned())
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
        check(
            is_safe_name(text) && text.len() <= 64,
            "launcher",
            text,
            "a safe name of at most 64 bytes: an ASCII letter or digit, then letters, digits, \
             '.', '_' or '-'; no '..', no trailing '.' or '.lock'",
        )?;
        Ok(Self(text.to_owned()))
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn safe_names_follow_the_rule() {
        for good in ["a", "t", "vol-1", "a.b_c", "9x"] {
            assert!(is_safe_name(good), "{good}");
        }
        for bad in ["", "-a", ".a", "a..b", "a.", "a.lock", "a/b", "a b", "é"] {
            assert!(!is_safe_name(bad), "{bad}");
        }
    }

    #[test]
    fn each_parse_keeps_its_bounds() {
        assert!(VolumeName::parse(&"a".repeat(253)).is_ok());
        assert_eq!(
            VolumeName::parse(&"a".repeat(254)).unwrap_err().what,
            "volume"
        );
        assert!(VolumeName::parse("a..b").is_err());
        assert!(LauncherId::parse(&"c".repeat(64)).is_ok());
        assert!(LauncherId::parse(&"c".repeat(65)).is_err());
        assert!(Label::parse("ns-1").is_ok());
        assert!(Label::parse("").is_err());
        assert!(Label::parse("a b").is_err());
        assert!(Label::parse(&"x".repeat(254)).is_err());
        assert!(WriterLabel::parse("w x").is_ok());
        assert!(WriterLabel::parse("").is_err());
        assert!(WriterLabel::parse("a\nb").is_err());
        assert!(WriterLabel::parse(&"w".repeat(257)).is_err());
        assert_eq!(WriterLabel::agentfs().as_str(), WriterLabel::AGENTFS);
    }

    #[test]
    fn text_round_trips_through_serde() {
        let name = VolumeName::parse("t").unwrap();
        let json = serde_json::to_string(&name).unwrap();
        assert_eq!(json, "\"t\"");
        assert_eq!(serde_json::from_str::<VolumeName>(&json).unwrap(), name);
        assert!(serde_json::from_str::<VolumeName>("\"a..b\"").is_err());
    }
}
