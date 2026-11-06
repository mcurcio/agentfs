//! Target-model composition root (git-model §18).
use agentfs_core::tree_actor::Volume;
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
    /// Create an isolated sandbox branch at main's saved head; print its name.
    Fork,
    /// Recover and mount main, optionally also a sandbox. Unmount all views to exit.
    Mount {
        /// An existing empty directory, outside the state area.
        main: PathBuf,
        /// Mount a sandbox in this existing empty directory too.
        #[arg(long)]
        sandbox: Option<PathBuf>,
        /// Reuse this sandbox (fork/N); otherwise create one when --sandbox is set.
        #[arg(long, requires = "sandbox")]
        fork: Option<String>,
        /// Write process-local measurement counters to this private path.
        #[arg(long)]
        metrics_output: Option<PathBuf>,
        /// Main's trusted writer label (not an authenticated identity).
        #[arg(long, default_value = "main-client")]
        writer: String,
        /// The sandbox's trusted writer label.
        #[arg(long, default_value = "sandbox-client")]
        sandbox_writer: String,
    },
}

pub fn run(state: &Path, args: Args) -> Result<()> {
    let mut volume = Volume::open(state).context("opening tree state")?;
    match args.command {
        Command::Init => println!("main {}", volume.head("main")?),
        Command::Fork => println!("{}", volume.fork()?),
        Command::Mount {
            main,
            sandbox,
            fork,
            writer,
            sandbox_writer,
            metrics_output,
        } => {
            let mut bindings = vec![Binding {
                tree: "main".into(),
                writer,
                mountpoint: main,
            }];
            if let Some(mountpoint) = sandbox {
                let tree = if let Some(fork) = fork {
                    if !fork.starts_with("fork/") {
                        bail!("--fork must name an existing fork/N");
                    }
                    volume.head(&fork)?;
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
