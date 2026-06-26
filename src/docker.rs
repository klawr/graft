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

    /// Builds the `exec [-u 0] <container> <argv>` argument list. `root` forces
    /// the command to run as uid 0 regardless of the image's `USER`, which graft
    /// needs for setup that writes to root-owned locations (`/opt/graft`,
    /// `/graft`, `/etc/profile.d`); `docker exec -u 0` works even when the
    /// daemon's containers default to an unprivileged user.
    fn exec_args<'a>(container: &'a str, argv: &[&'a str], root: bool) -> Vec<&'a str> {
        let mut a = Vec::with_capacity(argv.len() + 4);
        a.push("exec");
        if root {
            a.push("-u");
            a.push("0");
        }
        a.push(container);
        a.extend_from_slice(argv);
        a
    }

    /// Runs `docker exec <container> <argv>` as the container's default user,
    /// inheriting stdio.
    pub fn exec(&self, container: &str, argv: &[&str]) -> Result<()> {
        self.exec_with(container, argv, false)
    }

    /// Like [`exec`](Self::exec) but runs as root (`-u 0`). Used for graft's own
    /// setup writes into root-owned paths, which would otherwise fail in images
    /// whose default `USER` is unprivileged.
    pub fn exec_root(&self, container: &str, argv: &[&str]) -> Result<()> {
        self.exec_with(container, argv, true)
    }

    fn exec_with(&self, container: &str, argv: &[&str], root: bool) -> Result<()> {
        let args = Self::exec_args(container, argv, root);
        let status = command(&self.remote, &args)
            .status()
            .context("spawning docker exec")?;
        if !status.success() {
            bail!("docker exec failed: {:?}", argv);
        }
        Ok(())
    }

    /// Runs `docker exec [-u 0] [--workdir W] [-e K=V ...] <container> <argv>`,
    /// inheriting stdio. Used for lifecycle hooks (which run as the container's
    /// default user, per the devcontainer spec) and feature install scripts
    /// (which the spec runs as root — pass `root = true`).
    pub fn exec_in(
        &self,
        container: &str,
        workdir: Option<&str>,
        env: &[(String, String)],
        argv: &[&str],
        root: bool,
    ) -> Result<()> {
        let mut args: Vec<String> = vec!["exec".into()];
        if root {
            args.push("-u".into());
            args.push("0".into());
        }
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
        let args = Self::exec_args(container, argv, false);
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

    /// Like [`exec_capture`](Self::exec_capture) but runs as `user` (`-u <user>`).
    /// Pass `None` to run as the container's default user.
    pub fn exec_capture_as(
        &self,
        container: &str,
        user: Option<&str>,
        argv: &[&str],
    ) -> Result<String> {
        let mut args: Vec<String> = vec!["exec".into()];
        if let Some(u) = user {
            args.push("-u".into());
            args.push(u.into());
        }
        args.push(container.into());
        args.extend(argv.iter().map(|s| s.to_string()));
        let refs: Vec<&str> = args.iter().map(String::as_str).collect();
        let out = command(&self.remote, &refs)
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
            // `-u 0`: local `docker cp` writes as root via the daemon, so this
            // streaming fallback must too, or destinations under root-owned trees
            // (e.g. /opt/graft) fail in images with an unprivileged default USER.
            let script = remote_script(&["exec", "-u", "0", "-i", container, "sh", "-c", &inner]);
            let (mut ssh, dest_host) = crate::ssh::base_command(host);
            ssh.arg(dest_host).arg(script).stdin(Stdio::from(file));
            crate::verbose::trace(&ssh);
            let status = ssh.status().context("spawning ssh docker exec for cp")?;
            if !status.success() {
                bail!(
                    "docker cp (stream) failed: {} -> {}:{}",
                    src,
                    container,
                    dest
                );
            }
        }
        Ok(())
    }

    /// Returns the mount destination paths inside the container (from `docker inspect`).
    pub fn inspect_mounts(&self, container: &str) -> Vec<String> {
        let Ok(out) = command(
            &self.remote,
            &[
                "inspect",
                "--format",
                "{{range .Mounts}}{{.Destination}}\n{{end}}",
                container,
            ],
        )
        .output() else {
            return vec![];
        };
        if !out.status.success() {
            return vec![];
        }
        let text = String::from_utf8_lossy(&out.stdout).into_owned();
        text.lines()
            .map(|l| l.trim())
            .filter(|s| !s.is_empty())
            .map(String::from)
            .collect()
    }

    /// True if `path` is a regular file inside the container.
    pub fn file_exists(&self, container: &str, path: &str) -> bool {
        command(
            &self.remote,
            &Self::exec_args(container, &["test", "-f", path], false),
        )
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
    }

    /// True if `path` exists (file or directory) inside the container.
    pub fn path_exists(&self, container: &str, path: &str) -> bool {
        command(
            &self.remote,
            &Self::exec_args(container, &["test", "-e", path], false),
        )
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
                false,
            ),
        )
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
    }

    /// `mkdir -p` for one or more directories inside the container. Runs as root
    /// because graft's directories live under root-owned trees (`/opt/graft`,
    /// `/graft`, and parents of system wrappers).
    pub fn mkdir_p(&self, container: &str, dirs: &[&str]) -> Result<()> {
        let mut argv = vec!["mkdir", "-p"];
        argv.extend_from_slice(dirs);
        self.exec_root(container, &argv)
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
        self.exec_root(
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
