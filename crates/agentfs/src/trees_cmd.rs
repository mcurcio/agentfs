//! Target-model composition root (git-model.md).
use agentfs_core::tree_actor::{TreeId, Volume};
use agentfs_fuse::tree_view::{Binding, serve_with_metrics};
use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};

#[derive(clap::Args)]
pub struct Args {
    #[command(subcommand)]
    command: Command,
}

#[derive(clap::Subcommand)]
enum Command {
    /// Initialize or recover the tree actor's state area.
    Init,
    /// Create an isolated sandbox fork at the root's saved head; print its identity.
    Fork,
    /// Recover and mount the root, optionally also a sandbox. Unmount all views to exit.
    Mount {
        /// An existing empty directory, outside the state area.
        root: PathBuf,
        /// Mount a sandbox in this existing empty directory too.
        #[arg(long)]
        sandbox: Option<PathBuf>,
        /// Reuse this sandbox (fork/N); otherwise create one when --sandbox is set.
        #[arg(long, requires = "sandbox")]
        fork: Option<TreeId>,
        /// Write process-local measurement counters to this private path.
        #[arg(long)]
        metrics_output: Option<PathBuf>,
        /// The root's trusted writer label (not an authenticated identity).
        #[arg(long, default_value = "root-client")]
        writer: String,
        /// The sandbox's trusted writer label.
        #[arg(long, default_value = "sandbox-client")]
        sandbox_writer: String,
    },
}

pub fn run(state: &Path, args: Args) -> Result<()> {
    let mut volume = Volume::open(state).context("opening tree state")?;
    match args.command {
        Command::Init => println!("{} {}", TreeId::Root, volume.head(TreeId::Root)?),
        Command::Fork => println!("{}", volume.fork()?),
        Command::Mount {
            root,
            sandbox,
            fork,
            writer,
            sandbox_writer,
            metrics_output,
        } => {
            let mut bindings = vec![Binding {
                tree: TreeId::Root,
                writer,
                mountpoint: root,
            }];
            if let Some(mountpoint) = sandbox {
                let tree = if let Some(fork) = fork {
                    if fork.is_root() {
                        bail!("--fork must name an existing fork/N, not the root");
                    }
                    volume.head(fork)?;
                    fork
                } else {
                    volume.fork()?
                };
                eprintln!("sandbox tree: {tree}");
                bindings.push(Binding {
                    tree,
                    writer: sandbox_writer,
                    mountpoint,
                });
            }
            serve_with_metrics(state, volume, bindings, metrics_output.as_deref())
                .context("serving tree views")?;
        }
    }
    Ok(())
}
