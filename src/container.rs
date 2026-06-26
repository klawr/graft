use crate::config::Config;
use crate::docker::shell_quote;
use anyhow::{Result, bail};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

pub fn enter(
    container: &str,
    session_name: &str,
    workdir: &str,
    remote: &Option<String>,
    remote_user: Option<&str>,
) -> Result<()> {
    let config = Config::load()?;
    let shell = &config.session.shell;

    // The command that opens an interactive shell inside the container. Values
    // are shell-quoted because it's run through a shell (tmux pane / sh -c). The
    // local form runs the docker CLI directly; for a remote daemon we run the
    // docker CLI *on the remote* over `ssh -tt`
    let mut exec = String::from("docker exec -it");
    if let Some(u) = remote_user {
        exec.push_str(&format!(" -u {}", shell_quote(u)));
    }
    exec.push_str(&format!(
        " --workdir {} {} {} -l",
        shell_quote(workdir),
        shell_quote(container),
        shell_quote(shell)
    ));
    let run_cmd = match remote {
        None => exec,
        Some(host) => {
            let (dest, port) = crate::ssh::split(host);
            let mut s = String::from("ssh -tt");
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

    let started = Instant::now();
    let result = match config.session.multiplexer.as_str() {
        "tmux" => enter_tmux(session_name, &run_cmd),
        "none" => enter_direct(&run_cmd),
        other => bail!("unknown session.multiplexer {other:?} (supported: \"tmux\", \"none\")"),
    };

    // A remote session that fails almost instantly is the signature of the SSH
    // server refusing a PTY: `docker exec -it` then has no tty on its stdin and
    // bails the moment the shell opens. Probe for it (only on a fast failure, so
    // a normal shell exiting non-zero after real use doesn't trigger an extra
    // connection) and point at the fix, which is server-side — no client flag,
    // not even `-tt`, can override it.
    if result.is_err()
        && started.elapsed() < Duration::from_secs(2)
        && let Some(host) = remote
        && pty_denied(host)
    {
        eprintln!(
            "[graft] the remote SSH server refused a PTY for this connection, so the\n\
             [graft] interactive shell (`docker exec -it`) was dropped immediately. Fix it\n\
             [graft] on the server: set `PermitTTY yes` in sshd_config, and remove any\n\
             [graft] `restrict`/`no-pty` option on this key in ~/.ssh/authorized_keys, then\n\
             [graft] reload sshd."
        );
    }
    result
}

// Probes whether the remote SSH server denies PTY allocation, which makes
// graft's interactive `docker exec -it` shell impossible. Forces a PTY (`-tt`)
// and runs `tty`: a server that refuses it makes ssh fail with "Pseudo-terminal
// allocation request failed" (or, if it proceeds without one, `tty` prints "not
// a tty"). Best-effort and quiet — any spawn error just yields false.
fn pty_denied(remote: &str) -> bool {
    let (mut cmd, dest) = crate::ssh::base_command(remote);
    cmd.args(["-tt", "-o", "BatchMode=yes"])
        .arg(dest)
        .arg("tty")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let Ok(out) = cmd.output() else {
        return false;
    };
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    combined.contains("allocation request failed") || combined.contains("not a tty")
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
