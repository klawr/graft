use anyhow::{Context, Result, bail};
use std::process::{Command, Stdio};

/// Runs `docker` commands against the local daemon, or — when `remote` is set —
/// the remote one by running the docker CLI *on the remote host* over SSH
/// (`ssh <host> docker …`). This means no local docker CLI is required for
/// `--remote`: only `ssh` (and `tar`, for copies) need to exist locally.
///
/// The trade-off versus the old `DOCKER_HOST=ssh://…` approach is that the
/// docker client no longer runs locally, so file copies can't be passed by
/// path — the source files live on *this* machine, not the remote. `cp`/
/// `cp_out` therefore stream the bytes through the SSH pipe instead (see their
/// implementations).
pub struct Docker {
    remote: Option<String>,
}

impl Docker {
    pub fn new(remote: &Option<String>) -> Self {
        Self {
            remote: remote.clone(),
        }
    }

    fn exec_args<'a>(container: &'a str, argv: &[&'a str]) -> Vec<&'a str> {
        let mut a = Vec::with_capacity(argv.len() + 2);
        a.push("exec");
        a.push(container);
        a.extend_from_slice(argv);
        a
    }

    /// Runs `docker exec <container> <argv>`, inheriting stdio.
    pub fn exec(&self, container: &str, argv: &[&str]) -> Result<()> {
        let args = Self::exec_args(container, argv);
        let status = command(&self.remote, &args)
            .status()
            .context("spawning docker exec")?;
        if !status.success() {
            bail!("docker exec failed: {:?}", argv);
        }
        Ok(())
    }

    /// Runs `docker exec [--workdir W] [-e K=V ...] <container> <argv>`,
    /// inheriting stdio. Used for lifecycle hooks and feature install scripts.
    pub fn exec_in(
        &self,
        container: &str,
        workdir: Option<&str>,
        env: &[(String, String)],
        argv: &[&str],
    ) -> Result<()> {
        let mut args: Vec<String> = vec!["exec".into()];
        if let Some(wd) = workdir {
            args.push("--workdir".into());
            args.push(wd.into());
        }
        for (k, v) in env {
            args.push("-e".into());
            args.push(format!("{k}={v}"));
        }
        args.push(container.into());
        args.extend(argv.iter().map(|s| s.to_string()));

        let refs: Vec<&str> = args.iter().map(String::as_str).collect();
        let status = command(&self.remote, &refs)
            .status()
            .context("spawning docker exec")?;
        if !status.success() {
            bail!("docker exec failed: {:?}", argv);
        }
        Ok(())
    }

    /// Runs `docker exec <container> <argv>` and returns captured stdout.
    pub fn exec_capture(&self, container: &str, argv: &[&str]) -> Result<String> {
        let args = Self::exec_args(container, argv);
        let out = command(&self.remote, &args)
            .output()
            .context("spawning docker exec")?;
        if !out.status.success() {
            bail!(
                "docker exec failed: {:?}\n{}",
                argv,
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Ok(String::from_utf8(out.stdout)?)
    }

    /// `docker cp <container>:<src> <local_dst>` (works on stopped containers).
    /// With `--remote`, the remote docker writes a tar to its stdout, which is
    /// piped back over SSH and unpacked into `local_dst` here.
    pub fn cp_out(&self, container: &str, src: &str, local_dst: &str) -> Result<()> {
        match &self.remote {
            None => {
                let from = format!("{container}:{src}");
                let status = command(&self.remote, &["cp", &from, local_dst])
                    .status()
                    .context("spawning docker cp")?;
                if !status.success() {
                    bail!("docker cp failed: {} -> {}", from, local_dst);
                }
                Ok(())
            }
            Some(host) => self.cp_out_remote(host, container, src, local_dst),
        }
    }

    // Streams `docker cp <container>:<src> -` from the remote and unpacks the
    // resulting tar locally, then moves the single top-level entry (named after
    // `src`'s basename) to `local_dst`.
    fn cp_out_remote(&self, host: &str, container: &str, src: &str, local_dst: &str) -> Result<()> {
        use std::path::Path;

        let from = format!("{container}:{src}");
        let (mut ssh, dest) = crate::ssh::base_command(host);
        ssh.arg(dest)
            .arg(remote_script(&["cp", &from, "-"]))
            .stdin(Stdio::null())
            .stdout(Stdio::piped());
        crate::verbose::trace(&ssh);
        let mut child = ssh.spawn().context("spawning ssh docker cp")?;
        let stdout = child.stdout.take().expect("piped");

        let staging = std::env::temp_dir().join(format!("graft-cpout-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&staging);
        std::fs::create_dir_all(&staging).context("creating cp staging dir")?;

        let mut tar = Command::new("tar");
        tar.args(["-xf", "-"])
            .arg("-C")
            .arg(&staging)
            .stdin(Stdio::from(stdout));
        crate::verbose::trace(&tar);
        let tar_status = tar.status().context("spawning tar to unpack cp")?;
        let ssh_status = child.wait().context("waiting on ssh docker cp")?;

        if !ssh_status.success() || !tar_status.success() {
            let _ = std::fs::remove_dir_all(&staging);
            bail!("docker cp failed: {} -> {}", from, local_dst);
        }

        let name = Path::new(src).file_name().unwrap_or_default();
        let extracted = staging.join(name);
        if let Some(parent) = Path::new(local_dst).parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let _ = std::fs::remove_file(local_dst);
        let moved = std::fs::rename(&extracted, local_dst)
            .or_else(|_| std::fs::copy(&extracted, local_dst).map(|_| ()));
        let _ = std::fs::remove_dir_all(&staging);
        moved.with_context(|| format!("placing extracted file at {local_dst}"))?;
        Ok(())
    }

    /// `docker cp <src> <container>:<dest>`. With `--remote`, `src` is a *local*
    /// path that the remote docker can't see, so the bytes are streamed over
    /// SSH instead.
    pub fn cp(&self, src: &str, container: &str, dest: &str) -> Result<()> {
        match &self.remote {
            None => {
                let target = format!("{container}:{dest}");
                let status = command(&self.remote, &["cp", src, &target])
                    .status()
                    .context("spawning docker cp")?;
                if !status.success() {
                    bail!("docker cp failed: {} -> {}", src, target);
                }
                Ok(())
            }
            Some(host) => self.cp_remote(host, src, container, dest),
        }
    }

    // Copies a local `src` into the container without a local docker CLI:
    //   - a directory is tar'd locally and `docker cp -`'d into `dest` (the
    //     daemon unpacks the tar, so the container needs no tar);
    //   - a file is streamed straight into `dest` via `docker exec -i … cat`.
    fn cp_remote(&self, host: &str, src: &str, container: &str, dest: &str) -> Result<()> {
        let meta = std::fs::metadata(src).with_context(|| format!("stat {src}"))?;
        if meta.is_dir() {
            // `docker cp -` extracts into an existing directory, so create it
            // first, then stream the *contents* of `src` (so `dest` mirrors
            // `src` regardless of their basenames).
            self.mkdir_p(container, &[dest])?;
            let mut tar = Command::new("tar");
            tar.args(["-C", src, "-cf", "-", "."])
                .stdin(Stdio::null())
                .stdout(Stdio::piped());
            crate::verbose::trace(&tar);
            let mut tar_child = tar.spawn().context("spawning tar for docker cp")?;
            let tar_out = tar_child.stdout.take().expect("piped");

            let target = format!("{container}:{dest}");
            let (mut ssh, dest_host) = crate::ssh::base_command(host);
            ssh.arg(dest_host)
                .arg(remote_script(&["cp", "-", &target]))
                .stdin(Stdio::from(tar_out));
            crate::verbose::trace(&ssh);
            let ssh_status = ssh.status().context("spawning ssh docker cp")?;
            let tar_status = tar_child.wait().context("waiting on tar")?;
            if !tar_status.success() {
                bail!("tar failed packing {src}");
            }
            if !ssh_status.success() {
                bail!("docker cp failed: {} -> {}", src, target);
            }
        } else {
            let file = std::fs::File::open(src).with_context(|| format!("open {src}"))?;
            let inner = format!("cat > {}", shell_quote(dest));
            let script = remote_script(&["exec", "-i", container, "sh", "-c", &inner]);
            let (mut ssh, dest_host) = crate::ssh::base_command(host);
            ssh.arg(dest_host).arg(script).stdin(Stdio::from(file));
            crate::verbose::trace(&ssh);
            let status = ssh.status().context("spawning ssh docker exec for cp")?;
            if !status.success() {
                bail!("docker cp (stream) failed: {} -> {}:{}", src, container, dest);
            }
        }
        Ok(())
    }

    /// True if `path` is a regular file inside the container.
    pub fn file_exists(&self, container: &str, path: &str) -> bool {
        command(&self.remote, &Self::exec_args(container, &["test", "-f", path]))
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }

    /// True if `path` exists (file or directory) inside the container.
    pub fn path_exists(&self, container: &str, path: &str) -> bool {
        command(&self.remote, &Self::exec_args(container, &["test", "-e", path]))
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }

    /// True if `cmd` is on PATH inside the container.
    pub fn has_command(&self, container: &str, cmd: &str) -> bool {
        command(
            &self.remote,
            &Self::exec_args(
                container,
                &[
                    "sh",
                    "-c",
                    &format!("command -v {} >/dev/null 2>&1", shell_quote(cmd)),
                ],
            ),
        )
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
    }

    /// `mkdir -p` for one or more directories inside the container.
    pub fn mkdir_p(&self, container: &str, dirs: &[&str]) -> Result<()> {
        let mut argv = vec!["mkdir", "-p"];
        argv.extend_from_slice(dirs);
        self.exec(container, &argv)
    }

    /// True if the one-shot task `key` has already completed in this container.
    /// Lets create-time hooks / feature installs retry on a later `graft up`
    /// (until they succeed) rather than only on a recreate.
    pub fn task_done(&self, container: &str, key: &str) -> bool {
        self.path_exists(container, &format!("{GRAFT_STATE}/{}", sanitize_key(key)))
    }

    /// Records that the one-shot task `key` completed.
    pub fn mark_task_done(&self, container: &str, key: &str) -> Result<()> {
        let path = format!("{GRAFT_STATE}/{}", sanitize_key(key));
        self.exec(
            container,
            &[
                "sh",
                "-c",
                &format!("mkdir -p {GRAFT_STATE} && : > {}", shell_quote(&path)),
            ],
        )
    }
}

const GRAFT_STATE: &str = "/opt/graft/state";

fn sanitize_key(key: &str) -> String {
    key.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// Builds a command that runs `docker <args>` against the configured daemon.
/// Locally that's a plain `docker` invocation; with `remote` set it becomes
/// `ssh <host> docker <args>`, so no local docker CLI is needed. For remote
/// commands stdin is closed (`/dev/null`) so ssh doesn't swallow the terminal;
/// callers that need to stream data build their own ssh command.
pub fn command(remote: &Option<String>, args: &[&str]) -> Command {
    let c = match remote {
        None => {
            let mut c = Command::new("docker");
            c.args(args);
            c
        }
        Some(host) => {
            let (mut c, dest) = crate::ssh::base_command(host);
            c.arg(dest).arg(remote_script(args)).stdin(Stdio::null());
            c
        }
    };
    crate::verbose::trace(&c);
    c
}

/// Joins `docker <args>` into a single shell-quoted command line for the remote
/// login shell that ssh hands the command to.
fn remote_script(args: &[&str]) -> String {
    let mut s = String::from("docker");
    for a in args {
        s.push(' ');
        s.push_str(&shell_quote(a));
    }
    s
}

/// POSIX single-quote escaping: wrap in single quotes, and turn any embedded
/// single quote into `'\''`. Safe for interpolation into an `sh -c`/SSH string.
pub fn shell_quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for c in s.chars() {
        if c == '\'' {
            out.push_str("'\\''");
        } else {
            out.push(c);
        }
    }
    out.push('\'');
    out
}

#[cfg(test)]
mod tests {
    use super::{remote_script, shell_quote};

    #[test]
    fn quotes_plain() {
        assert_eq!(shell_quote("abc"), "'abc'");
    }

    #[test]
    fn quotes_specials_literally() {
        assert_eq!(shell_quote("a b; rm -rf /"), "'a b; rm -rf /'");
        assert_eq!(shell_quote("$(whoami)"), "'$(whoami)'");
    }

    #[test]
    fn escapes_embedded_single_quote() {
        assert_eq!(shell_quote("a'b"), "'a'\\''b'");
    }

    #[test]
    fn remote_script_quotes_each_arg() {
        assert_eq!(
            remote_script(&["exec", "my container", "sh"]),
            "docker 'exec' 'my container' 'sh'"
        );
    }
}
