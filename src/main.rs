mod cli;
mod config;
mod container;
mod deps;
mod devcontainer;
mod docker;
mod features;
mod forward;
mod inject;
mod ssh;
mod verbose;

use anyhow::Result;
use clap::Parser;
use cli::{Cli, Command};

fn main() -> Result<()> {
    let cli = Cli::parse();
    verbose::set(cli.verbose);

    match cli.command {
        Command::Up { path, build } => {
            let project_path = path.unwrap_or_else(|| std::env::current_dir().unwrap());
            let started = devcontainer::start(&project_path, build, &cli.remote)?;
            forward::spawn_daemon(&started.container, &cli.remote, &started.forward_ports);
            inject::graft(&started.container, &cli.remote)?;
            started.run_post_attach(&cli.remote)?;
            container::enter(
                &started.container,
                &started.session_name,
                &started.workdir,
                &cli.remote,
            )?;
        }
        Command::Down { path } => {
            let project_path = path.unwrap_or_else(|| std::env::current_dir().unwrap());
            devcontainer::stop(&project_path, &cli.remote)?;
        }
        Command::Exec { container } => {
            inject::graft(&container, &cli.remote)?;
            let session_name = format!("graft-{container}");
            container::enter(&container, &session_name, "/", &cli.remote)?;
        }
        Command::Forward { container, port } => {
            let ports: Vec<_> = port
                .iter()
                .filter_map(|s| forward::PortForward::decode(s))
                .collect();
            forward::run_daemon(&container, &cli.remote, &ports);
        }
    }

    Ok(())
}
