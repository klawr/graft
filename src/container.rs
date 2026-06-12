use crate::config::Config;
use crate::docker::shell_quote;
use anyhow::{Result, bail};
use std::process::Command;

pub fn enter(container: &str, workdir: &str, remote: &Option<String>) -> Result<()> {
    let config = Config::load()?;
    let shell = &config.session.shell;

    // The command that opens an interactive shell inside the container. Values
    // are shell-quoted because it's run through a shell (tmux pane / sh -c). For
    // a remote daemon we prefix DOCKER_HOST=ssh://… so the local docker CLI
    // attaches over SSH (same mechanism the rest of graft uses).
    let prefix = match remote {
        None => String::new(),
        Some(host) => format!(
            "DOCKER_HOST={} ",
            shell_quote(&crate::docker::docker_host(host))
        ),
    };
    let run_cmd = format!(
        "{prefix}docker exec -it --workdir {} {} {} -l",
        shell_quote(workdir),
        shell_quote(container),
        shell_quote(shell)
    );

    println!("[graft] entering {container}");
    if crate::verbose::enabled() {
        eprintln!("[graft] $ {run_cmd}");
    }

    match config.session.multiplexer.as_str() {
        "tmux" => enter_tmux(container, &run_cmd),
        "none" => enter_direct(&run_cmd),
        other => bail!("unknown session.multiplexer {other:?} (supported: \"tmux\", \"none\")"),
    }
}

// Opens (or reuses) a per-container tmux session. If graft itself is being run
// from inside tmux, we must not nest — `attach-session` refuses that — so we
// `switch-client` the current client to the graft session instead. Outside
// tmux we attach normally.
fn enter_tmux(container: &str, run_cmd: &str) -> Result<()> {
    let session = format!("graft-{}", sanitize(container));

    // Create the session detached if it doesn't already exist (idempotent).
    Command::new("tmux")
        .args(["new-session", "-d", "-s", &session, run_cmd])
        .status()
        .ok(); // ignore error — session may already exist

    // New windows in this session also exec into the container.
    Command::new("tmux")
        .args(["set-option", "-t", &session, "default-command", run_cmd])
        .status()?;

    let already_in_tmux = std::env::var_os("TMUX").is_some();
    let action = if already_in_tmux {
        "switch-client"
    } else {
        "attach-session"
    };
    let status = Command::new("tmux")
        .args([action, "-t", &session])
        .status()?;
    if !status.success() {
        bail!("tmux {action} -t {session} failed");
    }
    Ok(())
}

// Execs straight into the container with no multiplexer.
fn enter_direct(run_cmd: &str) -> Result<()> {
    let status = Command::new("sh").arg("-c").arg(run_cmd).status()?;
    if !status.success() {
        bail!("session exited with failure");
    }
    Ok(())
}

/// Replaces characters tmux dislikes in session names with '-'.
fn sanitize(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::sanitize;

    #[test]
    fn keeps_alphanumeric() {
        assert_eq!(sanitize("abc123DEF"), "abc123DEF");
    }

    #[test]
    fn replaces_punctuation() {
        // container ids/names with '.'/':' would be rejected by tmux as-is.
        assert_eq!(sanitize("svc.dev_1:tag/x"), "svc-dev-1-tag-x");
    }
}
