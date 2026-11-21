//! `agentfs-csi`: the agentfs CSI driver (docs/csi.md).
//!
//! Runs in the node DaemonSet on every node, privileged, beside
//! csi-provisioner (`--node-deployment`) and node-driver-registrar, and
//! serves the Identity, Controller and Node services. Its base path must
//! be on the node's own disk: a network filesystem is refused before
//! anything is served.

use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use clap::Parser;

use agentfs_core::config::parse_duration;
use agentfs_csi::controller::ControllerService;
use agentfs_csi::identity::IdentityService;
use agentfs_csi::launcher::{AgentfsLauncher, PluginIdentity};
use agentfs_csi::layout::{Layout, validate_node};
use agentfs_csi::mounter::KernelMounter;
use agentfs_csi::node::NodeService;
use agentfs_csi::server;
use agentfs_csi::supervisor::{Backoff, NodeConfig, Supervisor};

fn duration(value: &str) -> Result<Duration, String> {
    parse_duration(value).ok_or_else(|| format!("{value:?} is not a duration (use e.g. 500ms, 5s)"))
}

/// The agentfs CSI driver.
#[derive(Debug, Parser)]
#[command(version, about)]
struct Cli {
    /// The CSI socket.
    #[arg(long, env = "CSI_ENDPOINT", default_value = "unix:///csi/csi.sock")]
    endpoint: String,
    /// This node's name: `spec.nodeName`.
    #[arg(long, env = "NODE_NAME")]
    node_id: String,
    /// Where this node's volumes live: a directory on the node's own disk
    /// (a network filesystem is refused).
    #[arg(long, default_value = "/var/lib/agentfs-csi/volumes")]
    base_path: PathBuf,
    /// The node's records of published volumes; must survive plugin
    /// restarts.
    #[arg(long, default_value = "/var/lib/agentfs-csi/node")]
    node_state: PathBuf,
    /// The profiles: `<profile>.config`, `.exclude`, `.attributes`.
    #[arg(long, default_value = "/etc/agentfs-csi/profiles")]
    profiles_dir: PathBuf,
    /// The `agentfs` binary the node plugin runs.
    #[arg(long, default_value = "/usr/local/bin/agentfs")]
    agentfs_bin: PathBuf,
    /// The longest delay between restarts of a crashed host.
    #[arg(long, default_value = "30s", value_parser = duration)]
    backoff_max: Duration,
    /// The plugin pod's name, for its children's identity.
    #[arg(long, env = "AGENTFS_CSI_POD", default_value = "")]
    pod: String,
    /// The plugin pod's namespace.
    #[arg(long, env = "AGENTFS_CSI_NAMESPACE", default_value = "")]
    namespace: String,
}

fn main() -> ExitCode {
    // Before tokio's threads start: arenas made before it stay.
    agentfs_core::allocator::limit_arenas();
    let cli = Cli::parse();
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(e) => {
            eprintln!("agentfs-csi: the async runtime did not start: {e}");
            return ExitCode::FAILURE;
        }
    };
    match runtime.block_on(run(cli)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("agentfs-csi: {e}");
            ExitCode::FAILURE
        }
    }
}

async fn run(cli: Cli) -> Result<(), String> {
    let socket = server::parse_endpoint(&cli.endpoint).map_err(|e| e.to_string())?;
    std::fs::read_dir(&cli.profiles_dir).map_err(|e| {
        format!(
            "the profiles directory {} is unreadable: {e} (mount the agentfs-csi-profiles \
             ConfigMap there)",
            cli.profiles_dir.display()
        )
    })?;
    let node_id = cli.node_id.clone();
    if node_id.is_empty() {
        return Err("--node-id (or NODE_NAME) is required".into());
    }
    validate_node(&node_id).map_err(|e| e.to_string())?;
    // Volumes live on this node's own disk: a network filesystem is
    // refused before anything is served, naming its type.
    std::fs::create_dir_all(&cli.base_path)
        .map_err(|e| agentfs_csi::volume::chain(&e))
        .and_then(|()| {
            agentfs_core::shared_fs::network_filesystem(&cli.base_path)
                .map_err(|e| agentfs_csi::volume::chain(&e))
        })
        .and_then(|network| match network {
            None => Ok(()),
            Some(mount) => Err(format!(
                "it is on {} ({} at {}), a network filesystem: each node keeps its volumes \
                 on its own disk; use a local directory, ideally on a disk of its \
                 own",
                mount.fstype,
                mount.source,
                mount.mountpoint.display()
            )),
        })
        .map_err(|e| format!("base path {}: {e}", cli.base_path.display()))?;
    let layout = Layout {
        base: cli.base_path.clone(),
        node_state: cli.node_state.clone(),
        profiles: cli.profiles_dir.clone(),
    };
    let mut cfg = NodeConfig::new(node_id.clone(), layout.clone());
    cfg.plugin = PluginIdentity {
        namespace: cli.namespace.clone(),
        pod: cli.pod.clone(),
        node: node_id.clone(),
    };
    cfg.backoff = Backoff {
        max: cli.backoff_max,
        ..Backoff::default()
    };
    let launcher = Arc::new(AgentfsLauncher {
        bin: cli.agentfs_bin.clone(),
    });
    let supervisor = Supervisor::new(cfg, launcher, Arc::new(KernelMounter));
    let report = supervisor.reconcile().await;
    eprintln!(
        "reconciled: {} host(s) started, {} view(s) opened again, {} record(s) dropped, {} \
         unreadable",
        report.hosts,
        report.views,
        report.dropped,
        report.unreadable.len()
    );
    eprintln!(
        "agentfs-csi {} serving on {} for node {node_id} (base path {})",
        env!("CARGO_PKG_VERSION"),
        socket.display(),
        layout.base.display()
    );
    let result = match server::bind(&socket) {
        Ok(bound) => {
            bound
                .serve(
                    IdentityService::new(),
                    ControllerService::new(layout, node_id, supervisor.clone()),
                    NodeService::new(supervisor.clone()),
                    shutdown_signal(),
                )
                .await
        }
        Err(e) => Err(e),
    };
    eprintln!("stopping the hosts");
    supervisor.shutdown().await;
    result.map_err(|e| e.to_string())
}

async fn shutdown_signal() {
    use tokio::signal::unix::{SignalKind, signal};
    let (Ok(mut term), Ok(mut int)) = (
        signal(SignalKind::terminate()),
        signal(SignalKind::interrupt()),
    ) else {
        eprintln!("warning: cannot handle SIGTERM/SIGINT");
        return std::future::pending().await;
    };
    let name = tokio::select! {
        _ = term.recv() => "SIGTERM",
        _ = int.recv() => "SIGINT",
    };
    eprintln!("{name}: shutting down");
}

/// Shared with the `agentfs` binary's and `agentfs-core`'s doc tests.
#[cfg(test)]
#[path = "../../agentfs/src/doc_tables.rs"]
mod doc_tables;

/// Checks the `agentfs-csi` flags table of docs/configuration.md against
/// [`Cli`]: every flag and its environment variable, and no stale rows.
#[cfg(test)]
mod reference_tests {
    use clap::CommandFactory;

    use super::Cli;
    use super::doc_tables;

    /// The `agentfs-csi` flags table: the table headed `Flag` in the CSI
    /// section, and the section's text.
    fn table() -> (doc_tables::Table, String) {
        let sections = doc_tables::sections(&doc_tables::read_doc(
            env!("CARGO_MANIFEST_DIR"),
            "configuration.md",
        ));
        let section = doc_tables::section(&sections, "The CSI driver (`agentfs-csi`)")
            .expect("docs/configuration.md has no `## The CSI driver (`agentfs-csi`)` section");
        let table = section
            .tables
            .iter()
            .find(|t| t.header.first().map(String::as_str) == Some("Flag"))
            .expect("the CSI section has no table headed `Flag`")
            .clone();
        let env = doc_tables::section(&sections, "Environment variables")
            .expect("docs/configuration.md has no `## Environment variables` section")
            .text
            .clone();
        (table, env)
    }

    fn long_flags(cell: &str) -> Vec<String> {
        doc_tables::code_spans(cell)
            .iter()
            .filter_map(|s| {
                s.split_whitespace()
                    .next()?
                    .strip_prefix("--")
                    .map(str::to_owned)
            })
            .collect()
    }

    #[test]
    fn every_flag_is_documented() {
        let (table, env_section) = table();
        let env_spans = doc_tables::code_spans(&env_section);
        let cli = Cli::command();
        let mut missing = Vec::new();
        let mut longs = Vec::new();
        for arg in cli.get_arguments() {
            let Some(long) = arg.get_long() else { continue };
            if matches!(long, "help" | "version") {
                continue;
            }
            longs.push(long.to_owned());
            let env = arg.get_env().map(|e| e.to_string_lossy().into_owned());
            let row = table
                .rows
                .iter()
                .find(|row| long_flags(&row[0]).iter().any(|l| l == long));
            match (row, &env) {
                (None, _) => missing.push(format!("a row for --{long}")),
                (Some(row), Some(env))
                    if !row.iter().any(|c| doc_tables::code_spans(c).contains(env)) =>
                {
                    missing.push(format!("`{env}` in the row of --{long}"))
                }
                _ => {}
            }
            if let Some(env) = env
                && !env_spans.contains(&env)
            {
                missing.push(format!("`{env}` under \"Environment variables\""));
            }
        }
        assert!(
            missing.is_empty(),
            "docs/configuration.md, `agentfs-csi` flags: add {missing:?}"
        );
        let stale: Vec<String> = table
            .rows
            .iter()
            .flat_map(|row| long_flags(&row[0]))
            .filter(|l| !longs.contains(l))
            .collect();
        assert!(
            stale.is_empty(),
            "docs/configuration.md documents agentfs-csi flags it does not have: {stale:?}"
        );
    }
}
