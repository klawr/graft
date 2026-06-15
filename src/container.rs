use crate::config::Config;
use crate::docker::shell_quote;
use anyhow::{Result, bail};
use std::process::Command;

pub fn enter(
    container: &str,
    session_name: &str,
    workdir: &str,
    remote: &Option<String>,
) -> Result<()> {
    let config = Config::load()?;
    let shell = &config.session.shell;

    // The command that opens an interactive shell inside the container. Values
    // are shell-quoted because it's run through a shell (tmux pane / sh -c). The
    // local form runs the docker CLI directly; for a remote daemon we run the
    // docker CLI *on the remote* over `ssh -t` (so it gets a PTY for `exec -it`),
    // matching the rest of graft — no local docker CLI required.
    let exec = format!(
        "docker exec -it --workdir {} {} {} -l",
        shell_quote(workdir),
        shell_quote(container),
        shell_quote(shell)
    );
    let run_cmd = match remote {
        None => exec,
        Some(host) => {
            let (dest, port) = crate::ssh::split(host);
            let mut s = String::from("ssh -t");
            if let Some(p) = port {
                s.push_str(&format!(" -p {p}"));
            }
            let v = crate::verbose::level().min(3);
            if v > 0 {
                s.push_str(&format!(" -{}", "v".repeat(v as usize)));
            }
            s.push(' ');
            s.push_str(&shell_quote(&dest));
            s.push(' ');
            s.push_str(&shell_quote(&exec));
            s
        }
    };

    println!("[graft] entering {container}");
    if crate::verbose::enabled() {
        eprintln!("[graft] $ {run_cmd}");
    }

    match config.session.multiplexer.as_str() {
        "tmux" => enter_tmux(session_name, &run_cmd),
        "none" => enter_direct(&run_cmd),
        other => bail!("unknown session.multiplexer {other:?} (supported: \"tmux\", \"none\")"),
    }
}

// Opens (or reuses) the named tmux session for this workspace. If graft itself is being run
// from inside tmux, we must not nest — `attach-session` refuses that — so we
// `switch-client` the current client to the graft session instead. Outside
// tmux we attach normally.
fn enter_tmux(session_name: &str, run_cmd: &str) -> Result<()> {
    let session = sanitize(session_name);

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
