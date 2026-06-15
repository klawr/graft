use clap::{ArgAction, Parser, Subcommand};
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
        hooks, grafts your environment on top, and forwards container ports to the host."
)]
pub struct Cli {
    /// SSH remote to operate on; the docker CLI is run on the remote host over
    /// SSH (`ssh <dest> docker …`), so no local docker CLI is needed — only an
    /// ssh client. Accepts any form the SSH client understands: a bare Host
    /// alias from ~/.ssh/config, user@host, or user@host:port (for a
    /// non-standard SSH port).
    #[arg(long, short, global = true, value_name = "SSH_DEST")]
    pub remote: Option<String>,

    /// Show what graft is doing: print every docker/ssh command it runs and
    /// pass -v to graft's own ssh connections (config probe, port tunnels).
    /// Repeat for more ssh verbosity (-vv, -vvv).
    #[arg(long, short, global = true, action = ArgAction::Count)]
    pub verbose: u8,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand)]
pub enum Command {
    /// Start a devcontainer and graft in
    #[command(
        long_about = "Read the project's devcontainer config — looked up under PATH (default: \
            current directory) as .devcontainer/devcontainer.json, .devcontainer.json, or \
            .devcontainer/<folder>/devcontainer.json — bring the container up, run lifecycle \
            hooks and features, inject your environment, then open an interactive shell.\n\n\
            Supports dockerComposeFile, image, build, and dockerFile backends. \
            Forwards ports declared in forwardPorts and any port that starts \
            listening at runtime. Config: ~/.config/graft/config.toml."
    )]
    Up {
        /// Project path containing the devcontainer config
        /// (defaults to current directory)
        path: Option<PathBuf>,
        /// Force-rebuild images and recreate the container from scratch
        #[arg(long)]
        build: bool,
    },
    /// Stop a project's devcontainer
    #[command(
        long_about = "Stop the container that `graft up` started for the project. The \
            port-forwarding daemon notices the container stopping and exits on its own. \
            The container is kept (with everything installed in it) and can be resumed \
            with `graft up`."
    )]
    Down {
        /// Project path containing the devcontainer config
        /// (defaults to current directory)
        path: Option<PathBuf>,
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
        /// Forward spec as serialized by PortForward::encode
        /// (local:port or local:host:port)
        #[arg(long)]
        port: Vec<String>,
    },
}
