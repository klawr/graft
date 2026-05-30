use clap::{Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser)]
#[command(
    name = "graft",
    about = "Inject your environment into any container",
    long_about = "Inject your environment into any container.\n\n\
        graft copies your personal tools (editor, shell config, …) together with \
        their shared-library dependencies into a running Docker container and drops \
        you into a shell. It also drives devcontainers: `graft up` reads \
        devcontainer.json, starts the container, installs features, runs lifecycle \
        hooks, grafts your environment on top, and forwards container ports to the host.",
)]
pub struct Cli {
    /// SSH remote to operate on (user@host); all Docker operations run against
    /// the remote daemon via DOCKER_HOST=ssh://user@host
    #[arg(long, short, global = true, value_name = "USER@HOST")]
    pub remote: Option<String>,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand)]
pub enum Command {
    /// Start a devcontainer and graft in
    #[command(
        long_about = "Find devcontainer.json in PATH (default: current directory), bring the \
            container up, run lifecycle hooks and features, inject your environment, \
            then open an interactive shell.\n\n\
            Supports dockerComposeFile, image, build, and dockerFile backends. \
            Forwards ports declared in forwardPorts and any port that starts \
            listening at runtime. Config: ~/.config/graft/config.toml."
    )]
    Up {
        /// Project path containing .devcontainer/ or .devcontainer.json
        /// (defaults to current directory)
        path: Option<PathBuf>,
        /// Force-rebuild images and recreate the container from scratch
        #[arg(long)]
        build: bool,
    },
    /// Graft into an already-running container
    #[command(
        long_about = "Inject your environment into a container that is already running, \
            then open an interactive shell. The container is identified by name or ID \
            as shown by `docker ps`."
    )]
    Exec {
        /// Container name or ID
        container: String,
    },
    /// Internal: port-forwarding daemon spawned by `graft up`
    #[command(name = "_forward", hide = true)]
    Forward {
        container: String,
        #[arg(long)]
        port: Vec<u16>,
    },
}
