//! `agentfs-csi`: the agentfs CSI driver (design §14.2, docs/csi.md).
//!
//! `--mode controller` runs beside csi-provisioner and csi-attacher;
//! `--mode node` in the node DaemonSet, privileged; `--mode all` both, for
//! csi-sanity and single-node clusters.

use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use clap::{Parser, ValueEnum};

use agentfs_core::config::parse_duration;
use agentfs_csi::controller::ControllerService;
use agentfs_csi::identity::IdentityService;
use agentfs_csi::launcher::{AgentfsLauncher, FrontOptions, PluginIdentity};
use agentfs_csi::layout::{Layout, validate_node};
use agentfs_csi::mounter::KernelMounter;
use agentfs_csi::node::NodeService;
use agentfs_csi::server;
use agentfs_csi::supervisor::{Backoff, NodeConfig, Supervisor};

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum Mode {
    Controller,
    Node,
    All,
}

fn duration(value: &str) -> Result<Duration, String> {
    parse_duration(value).ok_or_else(|| format!("{value:?} is not a duration (use e.g. 500ms, 5s)"))
}

/// The agentfs CSI driver.
#[derive(Debug, Parser)]
#[command(version, about)]
struct Cli {
    /// Which CSI services to serve.
    #[arg(long, env = "AGENTFS_CSI_MODE", value_enum)]
    mode: Mode,
    /// The CSI socket.
    #[arg(long, env = "CSI_ENDPOINT", default_value = "unix:///csi/csi.sock")]
    endpoint: String,
    /// This node's name (node and all modes): `spec.nodeName`.
    #[arg(long, env = "NODE_NAME")]
    node_id: Option<String>,
    /// Where volumes live: one filesystem, at the same path on the
    /// controller and every node.
    #[arg(long, default_value = "/var/lib/agentfs-csi/volumes")]
    base_path: PathBuf,
    /// The node's records of staged and published volumes, and the history
    /// services' sockets; must survive plugin restarts.
    #[arg(long, default_value = "/var/lib/agentfs-csi/node")]
    node_state: PathBuf,
    /// Where each front mounts before the bind onto the kubelet's target.
    #[arg(long, default_value = "/run/agentfs-csi/mounts")]
    mount_dir: PathBuf,
    /// The profiles: `<profile>.config`, `.exclude`, `.attributes`.
    #[arg(long, default_value = "/etc/agentfs-csi/profiles")]
    profiles_dir: PathBuf,
    /// The `agentfs` binary the node plugin runs.
    #[arg(long, default_value = "/usr/local/bin/agentfs")]
    agentfs_bin: PathBuf,
    /// Fronts' kernel passthrough: auto, on or off.
    #[arg(long, default_value = "auto")]
    passthrough: String,
    /// FUSE threads per front.
    #[arg(long, default_value_t = 4)]
    front_threads: usize,
    /// How long NodeStage waits for the history service before answering
    /// UNAVAILABLE (the kubelet retries).
    #[arg(long, default_value = "60s", value_parser = duration)]
    stage_wait: Duration,
    /// How long NodePublish waits for the front's mount.
    #[arg(long, default_value = "60s", value_parser = duration)]
    publish_wait: Duration,
    /// How long NodeUnpublish waits for a front to stop before SIGKILL.
    #[arg(long, default_value = "20s", value_parser = duration)]
    unpublish_grace: Duration,
    /// How long NodeUnstage waits for a service to stop before SIGKILL.
    #[arg(long, default_value = "30s", value_parser = duration)]
    unstage_grace: Duration,
    /// How long a front waits for its history service.
    #[arg(long, default_value = "30s", value_parser = duration)]
    history_wait: Duration,
    /// The longest delay between restarts of a crashed child.
    #[arg(long, default_value = "30s", value_parser = duration)]
    backoff_max: Duration,
    /// The plugin pod's name, for the history services' identity.
    #[arg(long, env = "AGENTFS_CSI_POD", default_value = "")]
    pod: String,
    /// The plugin pod's namespace.
    #[arg(long, env = "AGENTFS_CSI_NAMESPACE", default_value = "")]
    namespace: String,
}

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(cli).await {
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
    if !matches!(cli.passthrough.as_str(), "auto" | "on" | "off") {
        return Err(format!(
            "--passthrough {:?} must be auto, on or off",
            cli.passthrough
        ));
    }
    // Every volume's state lock lives under the base path: a filesystem
    // whose locks would not keep another node out is refused before
    // anything is served (design §14.2).
    std::fs::create_dir_all(&cli.base_path)
        .map_err(|e| agentfs_csi::volume::chain(&e))
        .and_then(|()| {
            agentfs_core::shared_fs::require_safe_locks(&cli.base_path)
                .map_err(|e| agentfs_csi::volume::chain(&e))
        })
        .map_err(|e| format!("base path {}: {e}", cli.base_path.display()))?;
    let layout = Layout {
        base: cli.base_path.clone(),
        node_state: cli.node_state.clone(),
        mount_dir: cli.mount_dir.clone(),
        profiles: cli.profiles_dir.clone(),
    };
    let controller = matches!(cli.mode, Mode::Controller | Mode::All)
        .then(|| ControllerService::new(layout.clone()));
    let supervisor = if matches!(cli.mode, Mode::Node | Mode::All) {
        let node_id = cli
            .node_id
            .clone()
            .filter(|n| !n.is_empty())
            .ok_or("--node-id (or NODE_NAME) is required in node mode")?;
        validate_node(&node_id).map_err(|e| e.to_string())?;
        let mut cfg = NodeConfig::new(node_id.clone(), layout.clone());
        cfg.plugin = PluginIdentity {
            namespace: cli.namespace.clone(),
            pod: cli.pod.clone(),
            node: node_id,
        };
        cfg.front = FrontOptions {
            passthrough: cli.passthrough.clone(),
            threads: cli.front_threads,
            history_wait: cli.history_wait,
        };
        cfg.stage_wait = cli.stage_wait;
        cfg.publish_wait = cli.publish_wait;
        cfg.unpublish_grace = cli.unpublish_grace;
        cfg.unstage_grace = cli.unstage_grace;
        cfg.backoff = Backoff {
            max: cli.backoff_max,
            ..Backoff::default()
        };
        let supervisor = Supervisor::new(
            cfg,
            Arc::new(AgentfsLauncher {
                bin: cli.agentfs_bin.clone(),
            }),
            Arc::new(KernelMounter),
        );
        let report = supervisor.reconcile().await.map_err(|e| e.to_string())?;
        for bad in &report.unreadable {
            eprintln!("warning: an unreadable node record was skipped: {bad}");
        }
        eprintln!(
            "reconciled: {} history service(s) started, {} front(s) recovering, {} record(s) \
             dropped",
            report.services, report.recovering, report.dropped
        );
        Some(supervisor)
    } else {
        None
    };
    eprintln!(
        "agentfs-csi {} serving {:?} on {} (base path {})",
        env!("CARGO_PKG_VERSION"),
        cli.mode,
        socket.display(),
        layout.base.display()
    );
    let result = server::serve(
        &socket,
        IdentityService::new(controller.is_some()),
        controller,
        supervisor.clone().map(NodeService::new),
        shutdown_signal(),
    )
    .await;
    if let Some(supervisor) = supervisor {
        eprintln!("stopping the fronts and history services");
        supervisor.shutdown().await;
    }
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
