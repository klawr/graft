use clap::{Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "graft", about = "Inject your environment into any container")]
pub struct Cli {
    /// SSH remote to operate on (user@host)
    #[arg(long, short, global = true)]
    pub remote: Option<String>,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand)]
pub enum Command {
    /// Find devcontainer.json in a project, start it, then graft in
    Up {
        /// Project path (defaults to current directory)
        path: Option<PathBuf>,
        /// Rebuild images and force-recreate containers
        #[arg(long)]
        build: bool,
    },
    /// Graft into an already-running container by name
    Exec { container: String },
    /// Internal: port-forwarding daemon spawned by `graft up`
    #[command(name = "_forward", hide = true)]
    Forward {
        container: String,
        #[arg(long)]
        port: Vec<u16>,
    },
}
