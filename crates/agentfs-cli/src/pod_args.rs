//! The pod flags one clap group shares: the recording engine's commands
//! flatten it beside `--replica`, and `trees` flattens it alone
//! (docs/configuration.md). In Kubernetes they come from the downward API.

use agentfs_contracts::name::{Label, NameError};
use agentfs_contracts::pod::PodContext;

/// Where the volume is served or mounted. An empty flag is absent.
#[derive(clap::Args, Clone, Default)]
pub struct PodArgs {
    #[arg(long, env = "AGENTFS_NAMESPACE", default_value = "")]
    pub namespace: String,
    #[arg(long, env = "AGENTFS_POD", default_value = "")]
    pub pod: String,
    #[arg(long, env = "AGENTFS_POD_UID", default_value = "")]
    pub pod_uid: String,
    #[arg(long, env = "AGENTFS_NODE", default_value = "")]
    pub node: String,
}

impl PodArgs {
    /// The pod context a tree volume's commits carry: each nonempty flag.
    ///
    /// # Errors
    ///
    /// A flag that is no [`Label`].
    pub fn context(&self) -> Result<PodContext, NameError> {
        let field = |text: &str| (!text.is_empty()).then(|| Label::parse(text)).transpose();
        Ok(PodContext {
            namespace: field(&self.namespace)?,
            pod: field(&self.pod)?,
            pod_uid: field(&self.pod_uid)?,
            node: field(&self.node)?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_empty_flag_is_absent() {
        let args = PodArgs {
            pod: "p-1".into(),
            ..PodArgs::default()
        };
        let context = args.context().unwrap();
        assert_eq!(context.pod.unwrap().as_str(), "p-1");
        assert_eq!(context.namespace, None);
        assert!(
            PodArgs {
                node: "a b".into(),
                ..PodArgs::default()
            }
            .context()
            .is_err()
        );
    }
}
