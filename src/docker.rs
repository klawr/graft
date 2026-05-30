use anyhow::{Context, Result, bail};
use std::process::Command;

/// Runs `docker` commands against the local daemon, or — when `remote` is set —
/// the remote one via `DOCKER_HOST=ssh://<host>`. Using `DOCKER_HOST` (rather
/// than wrapping each call in `ssh host docker …`) keeps the docker CLI local,
/// so `docker cp` reads/writes *local* files and TTY allocation works the same
/// as locally.
pub struct Docker {
    remote: Option<String>,
}

impl Docker {
    pub fn new(remote: &Option<String>) -> Self {
        Self {
            remote: remote.clone(),
        }
    }

    /// Builds a `docker <args>` command, targeting the remote daemon over SSH
    /// when configured.
    fn command(&self, args: &[&str]) -> Command {
        let mut c = Command::new("docker");
        c.args(args);
        docker_host_env(&mut c, &self.remote);
        c
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
        let status = self
            .command(&args)
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
        let status = self
            .command(&refs)
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
        let out = self
            .command(&args)
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
    pub fn cp_out(&self, container: &str, src: &str, local_dst: &str) -> Result<()> {
        let from = format!("{container}:{src}");
        let status = self
            .command(&["cp", &from, local_dst])
            .status()
            .context("spawning docker cp")?;
        if !status.success() {
            bail!("docker cp failed: {} -> {}", from, local_dst);
        }
        Ok(())
    }

    /// `docker cp <src> <container>:<dest>`.
    pub fn cp(&self, src: &str, container: &str, dest: &str) -> Result<()> {
        let target = format!("{container}:{dest}");
        let status = self
            .command(&["cp", src, &target])
            .status()
            .context("spawning docker cp")?;
        if !status.success() {
            bail!("docker cp failed: {} -> {}", src, target);
        }
        Ok(())
    }

    /// True if `path` is a regular file inside the container.
    pub fn file_exists(&self, container: &str, path: &str) -> bool {
        self.command(&Self::exec_args(container, &["test", "-f", path]))
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }

    /// True if `path` exists (file or directory) inside the container.
    pub fn path_exists(&self, container: &str, path: &str) -> bool {
        self.command(&Self::exec_args(container, &["test", "-e", path]))
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }

    /// True if `cmd` is on PATH inside the container.
    pub fn has_command(&self, container: &str, cmd: &str) -> bool {
        self.command(&Self::exec_args(
            container,
            &[
                "sh",
                "-c",
                &format!("command -v {} >/dev/null 2>&1", shell_quote(cmd)),
            ],
        ))
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

/// Points a `docker`/`docker compose` command at the remote daemon over SSH,
/// when a remote is configured. No-op locally.
pub fn docker_host_env(cmd: &mut Command, remote: &Option<String>) {
    if let Some(host) = remote {
        cmd.env("DOCKER_HOST", format!("ssh://{host}"));
    }
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
    use super::shell_quote;

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
}
