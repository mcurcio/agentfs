//! Committer and author identities from templates (design §9).

use std::collections::BTreeMap;

use crate::record::Caller;

pub const DEFAULT_COMMITTER_NAME: &str = "{replica}";
pub const DEFAULT_COMMITTER_EMAIL: &str = "{podUid}@{volume}.agentfs.invalid";
pub const DEFAULT_AUTHOR_NAME: &str = "uid-{uid}";
pub const DEFAULT_AUTHOR_EMAIL: &str = "uid-{uid}@{volume}.agentfs.invalid";

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum IdentityError {
    #[error("unknown template variable {{{0}}}")]
    UnknownVariable(String),
    #[error("unclosed `{{` in template {0:?}")]
    Unclosed(String),
    #[error("identity {0:?} is empty or contains `<`, `>` or a newline")]
    Invalid(String),
}

/// Where the mount runs. Phase 1 fills this from the downward API; the CSI
/// driver from `podInfoOnMount`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Context {
    pub volume: String,
    pub namespace: String,
    pub pod: String,
    pub pod_uid: String,
    pub node: String,
    /// The StatefulSet pod name when there is one, else the pod name.
    pub replica: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Templates {
    pub committer_name: String,
    pub committer_email: String,
    pub author_name: String,
    pub author_email: String,
}

impl Default for Templates {
    fn default() -> Self {
        Self {
            committer_name: DEFAULT_COMMITTER_NAME.into(),
            committer_email: DEFAULT_COMMITTER_EMAIL.into(),
            author_name: DEFAULT_AUTHOR_NAME.into(),
            author_email: DEFAULT_AUTHOR_EMAIL.into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Identity {
    pub name: String,
    pub email: String,
}

/// Renders identities for one mount. The committer is fixed per mount; the
/// author depends on the writer.
#[derive(Debug, Clone)]
pub struct IdentityRenderer {
    templates: Templates,
    vars: BTreeMap<&'static str, String>,
    committer: Identity,
}

impl IdentityRenderer {
    /// Validates every template up front, so a bad configuration fails at
    /// mount rather than at the first commit.
    pub fn new(templates: Templates, context: &Context) -> Result<Self, IdentityError> {
        let vars = BTreeMap::from([
            ("volume", context.volume.clone()),
            ("namespace", context.namespace.clone()),
            ("pod", context.pod.clone()),
            ("podUid", context.pod_uid.clone()),
            ("node", context.node.clone()),
            ("replica", context.replica.clone()),
        ]);
        let committer = Identity {
            name: checked(render(&templates.committer_name, &vars, None)?)?,
            email: checked(render(&templates.committer_email, &vars, None)?)?,
        };
        let renderer = Self {
            templates,
            vars,
            committer,
        };
        renderer.author(Caller {
            uid: 0,
            gid: 0,
            pid: 0,
        })?;
        Ok(renderer)
    }

    pub fn committer(&self) -> &Identity {
        &self.committer
    }

    pub fn author(&self, caller: Caller) -> Result<Identity, IdentityError> {
        let ids = Some(caller);
        Ok(Identity {
            name: checked(render(&self.templates.author_name, &self.vars, ids)?)?,
            email: checked(render(&self.templates.author_email, &self.vars, ids)?)?,
        })
    }
}

fn render(
    template: &str,
    vars: &BTreeMap<&'static str, String>,
    caller: Option<Caller>,
) -> Result<String, IdentityError> {
    let mut out = String::new();
    let mut rest = template;
    while let Some(start) = rest.find('{') {
        out.push_str(&rest[..start]);
        let after = &rest[start + 1..];
        let end = after
            .find('}')
            .ok_or_else(|| IdentityError::Unclosed(template.to_owned()))?;
        let name = &after[..end];
        let value = match (name, caller) {
            ("uid", Some(c)) => c.uid.to_string(),
            ("gid", Some(c)) => c.gid.to_string(),
            _ => vars
                .get(name)
                .cloned()
                .ok_or_else(|| IdentityError::UnknownVariable(name.to_owned()))?,
        };
        out.push_str(&value);
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    Ok(out)
}

/// git's fsck rejects names and emails that are empty of meaning or would
/// break the `Name <email>` header.
fn checked(value: String) -> Result<String, IdentityError> {
    let bad = value.trim().is_empty() || value.contains(['<', '>', '\n', '\r', '\0']);
    if bad {
        Err(IdentityError::Invalid(value))
    } else {
        Ok(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn context() -> Context {
        Context {
            volume: "hermes".into(),
            namespace: "agent".into(),
            pod: "hermes-7c9d8-x2k4q".into(),
            pod_uid: "0b6f".into(),
            node: "hawking".into(),
            replica: "hermes-7c9d8-x2k4q".into(),
        }
    }

    #[test]
    fn defaults_render() {
        let r = IdentityRenderer::new(Templates::default(), &context()).unwrap();
        assert_eq!(
            r.committer(),
            &Identity {
                name: "hermes-7c9d8-x2k4q".into(),
                email: "0b6f@hermes.agentfs.invalid".into()
            }
        );
        let author = r
            .author(Caller {
                uid: 10000,
                gid: 10000,
                pid: 7,
            })
            .unwrap();
        assert_eq!(author.name, "uid-10000");
        assert_eq!(author.email, "uid-10000@hermes.agentfs.invalid");
    }

    #[test]
    fn custom_templates() {
        let templates = Templates {
            committer_name: "agentfs {namespace}/{replica}".into(),
            committer_email: "bot@example.com".into(),
            ..Templates::default()
        };
        let r = IdentityRenderer::new(templates, &context()).unwrap();
        assert_eq!(r.committer().name, "agentfs agent/hermes-7c9d8-x2k4q");
    }

    #[test]
    fn bad_templates_fail_at_construction() {
        let unknown = Templates {
            committer_name: "{nope}".into(),
            ..Templates::default()
        };
        assert_eq!(
            IdentityRenderer::new(unknown, &context()).unwrap_err(),
            IdentityError::UnknownVariable("nope".into())
        );
        let uid_in_committer = Templates {
            committer_name: "{uid}".into(),
            ..Templates::default()
        };
        assert!(IdentityRenderer::new(uid_in_committer, &context()).is_err());
        let unclosed = Templates {
            author_name: "uid-{uid".into(),
            ..Templates::default()
        };
        assert!(matches!(
            IdentityRenderer::new(unclosed, &context()),
            Err(IdentityError::Unclosed(_))
        ));
    }

    #[test]
    fn identities_that_break_headers_are_rejected() {
        let mut ctx = context();
        ctx.replica = "evil <x>".into();
        assert!(matches!(
            IdentityRenderer::new(Templates::default(), &ctx),
            Err(IdentityError::Invalid(_))
        ));
        ctx.replica = String::new();
        assert!(IdentityRenderer::new(Templates::default(), &ctx).is_err());
    }
}
