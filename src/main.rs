mod cli;
mod config;
mod container;
mod deps;
mod devcontainer;
mod docker;
mod features;
mod forward;
mod inject;

use anyhow::Result;
use clap::Parser;
use cli::{Cli, Command};

fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Command::Up { path, build } => {
            let project_path = path.unwrap_or_else(|| std::env::current_dir().unwrap());
            let started = devcontainer::start(&project_path, build, &cli.remote)?;
            forward::spawn_daemon(&started.container, &cli.remote, &started.forward_ports);
            inject::graft(&started.container, &cli.remote)?;
            started.run_post_attach(&cli.remote)?;
            container::enter(&started.container, &started.workdir, &cli.remote)?;
        }
        Command::Exec { container } => {
            inject::graft(&container, &cli.remote)?;
            container::enter(&container, "/", &cli.remote)?;
        }
        Command::Forward { container, port } => {
            forward::run_daemon(&container, &cli.remote, &port);
        }
    }

    Ok(())
}
