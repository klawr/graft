use crate::config::{Config, Injectable};
use crate::deps;
use crate::docker::{Docker, shell_quote};
use anyhow::{Context, Result, bail};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Command;

const GRAFT_LIB: &str = "/opt/graft/lib";
const GRAFT_BIN: &str = "/opt/graft/bin";
// The dynamic linker is also placed here so a binary whose PT_INTERP we patch
// to /graft/<ld> resolves its interpreter when run directly. The prefix is
// 7 bytes, matching /lib64/, so PT_INTERP can be patched in place.
const GRAFT_DIR: &str = "/graft";

pub fn graft(container: &str, remote: &Option<String>, remote_user: Option<&str>) -> Result<()> {
    let config = Config::load()?;
    let docker = Docker::new(remote);
    docker.mkdir_p(container, &[GRAFT_LIB, GRAFT_BIN, GRAFT_DIR])?;

    let home = container_home(&docker, container, remote_user);
    let uid_gid = container_user(&docker, container, remote_user);

    for item in &config.inject {
        if let Some(binary) = &item.binary {
            inject_binary(
                &docker,
                container,
                &home,
                item,
                binary.to_string_lossy().as_ref(),
            )?;
        }
        if let Some(cfg) = &item.config {
            inject_config(
                &docker,
                container,
                &home,
                item,
                cfg.to_string_lossy().as_ref(),
                uid_gid.as_deref(),
            )?;
        }
    }

    inject_docker_path(&docker, container)?;
    configure_git_safe(&docker, container, &config.git.safe_directories)?;
    inject_aliases(&docker, container, &config.aliases)?;

    Ok(())
}

// Registers git `safe.directory` entries inside the container. Injected plugin
// repos and the mounted workspace are owned by the host uid, but git runs as
// root in the container, so without this git aborts with "detected dubious
// ownership" and anything that shells out to git (lazy.nvim, gitsigns, …) fails.
// Idempotent: only adds an entry that isn't already present. No-op if the
// container has no git.
fn configure_git_safe(docker: &Docker, container: &str, dirs: &[String]) -> Result<()> {
    if dirs.is_empty() {
        return Ok(());
    }
    if !docker.has_command(container, "git") {
        eprintln!("[graft] warning: git not found in container; skipping safe.directory setup");
        return Ok(());
    }

    println!(
        "[graft] marking {} git safe.director{}",
        dirs.len(),
        if dirs.len() == 1 { "y" } else { "ies" }
    );
    for dir in dirs {
        let q = crate::docker::shell_quote(dir);
        let script = format!(
            "git config --global --get-all safe.directory 2>/dev/null | grep -qxF {q} \
             || git config --global --add safe.directory {q}"
        );
        docker.exec(container, &["sh", "-c", &script])?;
    }
    Ok(())
}

fn inject_binary(
    docker: &Docker,
    container: &str,
    home: &str,
    item: &Injectable,
    binary: &str,
) -> Result<()> {
    let graft_binary = format!("{GRAFT_BIN}/{}", item.name);
    let target = resolve_home(
        item.target_binary
            .as_deref()
            .unwrap_or(&format!("/usr/local/bin/{}", item.name)),
        home,
    );

    if item.skip_if_exists && docker.file_exists(container, &graft_binary) {
        println!("[graft] {} already exists, skipping", item.name);
        return Ok(());
    }

    let linker = if item.copy_deps {
        inject_deps(docker, container, binary, &item.name)?
    } else {
        None
    };

    println!("[graft] injecting {} binary", item.name);

    // With grafted deps, patch a copy so the binary runs *directly* (no ld.so
    // wrapper) against our glibc — see patch_binary. Running directly keeps
    // /proc/self/exe pointing at the binary, which tools like Neovim need when
    // they re-exec themselves (`--embed`); baking the search path into the binary
    // (rather than exporting LD_LIBRARY_PATH) keeps subprocesses it spawns — git,
    // shells, LSPs — on the container's own glibc.
    match linker.as_deref() {
        Some(ld) => {
            let patched = patch_binary(binary, ld)?;
            docker.cp(&patched.path().to_string_lossy(), container, &graft_binary)?;
            // Make our linker available at /graft/<ld> for the patched interpreter.
            docker.exec_root(
                container,
                &[
                    "cp",
                    &format!("{GRAFT_LIB}/{ld}"),
                    &format!("{GRAFT_DIR}/{ld}"),
                ],
            )?;
        }
        None => {
            let real = std::fs::canonicalize(binary).unwrap_or_else(|_| PathBuf::from(binary));
            docker.cp(&real.to_string_lossy(), container, &graft_binary)?;
        }
    }

    docker.exec_root(container, &["chmod", "+x", &graft_binary])?;

    install_wrapper(docker, container, &graft_binary, &target)?;

    Ok(())
}

// Copies the binary to a self-cleaning temp file and uses `patchelf` to make it
// run *directly* against our grafted glibc:
//   --set-interpreter /graft/<ld> : our dynamic linker is the program interpreter
//   --force-rpath --set-rpath …   : DT_RPATH (not RUNPATH) so the search path
//                                   also resolves *transitive* deps (e.g. libnsl
//                                   pulling libtirpc); RUNPATH would not.
// patchelf handles the ELF rewriting robustly even when there's no slack in the
// original .dynstr/.dynamic — which is the common case for real binaries.
fn patch_binary(binary: &str, linker_name: &str) -> Result<TempFile> {
    let data = std::fs::read(binary).with_context(|| format!("reading {binary} to patch"))?;
    let tmp = TempFile::write(&format!("graft-patched-{}", file_stem(binary)), &data)?;

    let mut cmd = Command::new("patchelf");
    cmd.arg("--set-interpreter")
        .arg(format!("{GRAFT_DIR}/{linker_name}"))
        .arg("--force-rpath")
        .arg("--set-rpath")
        .arg(GRAFT_LIB)
        .arg(tmp.path());
    crate::verbose::trace(&cmd);
    let status = cmd.status().context(
        "running patchelf — it is required to graft dynamically linked binaries \
             (install it, e.g. `pacman -S patchelf` / `apt install patchelf`)",
    )?;
    if !status.success() {
        bail!("patchelf failed to patch {binary}");
    }

    Ok(tmp)
}

fn inject_config(
    docker: &Docker,
    container: &str,
    home: &str,
    item: &Injectable,
    cfg: &str,
    uid_gid: Option<&str>,
) -> Result<()> {
    let target = resolve_home(
        item.target_config
            .as_deref()
            .unwrap_or(&format!("~/.config/{}", item.name)),
        home,
    );

    if item.skip_if_exists && docker.path_exists(container, &target) {
        println!("[graft] {} config already present, skipping", item.name);
        // Fix ownership even when skipping the copy — the files may be
        // root-owned from a previous run.
        if let Some(owner) = uid_gid {
            chown_home_chain(docker, container, home, &target, owner)?;
        }
        return Ok(());
    }

    println!("[graft] injecting {} config → {target}", item.name);

    // Create parent dirs as the container user when the target is under their
    // home directory — `mkdir -p` as root would leave the whole chain
    // (e.g. /home/dev/.local) root-owned, blocking the user from creating
    // siblings later. For system paths (e.g. /usr/local/share) root is correct.
    let parent = Path::new(&target)
        .parent()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|| "/".to_string());
    if uid_gid.is_some() && target.starts_with(home) {
        docker.exec(container, &["mkdir", "-p", &parent])?;
    } else {
        docker.mkdir_p(container, &[&parent])?;
    }

    // Replace any existing target before copying: `docker cp` of a directory
    // onto an existing directory nests it (dest/src) instead of overwriting, so
    // removing first makes the copy mirror the host exactly (and drops files
    // deleted on the host).
    docker.exec_root(container, &["rm", "-rf", &target])?;
    docker.cp(cfg, container, &target)?;

    // Fix ownership of the entire path chain from home to target. The parent
    // dirs were created as the container user above, but docker cp (especially
    // in remote mode) may recreate intermediate dirs as root. Chowning the
    // chain ensures the user can create siblings (e.g. .local/state) at runtime.
    if let Some(owner) = uid_gid {
        chown_home_chain(docker, container, home, &target, owner)?;
    }

    Ok(())
}

// Chowns every path component between home (exclusive) and target (inclusive,
// recursive) so the container user owns the full chain and can create siblings
// (e.g. nvim writing .local/state next to .local/share) at runtime.
fn chown_home_chain(
    docker: &Docker,
    container: &str,
    home: &str,
    target: &str,
    owner: &str,
) -> Result<()> {
    let home_path = Path::new(home);
    let target_path = Path::new(target);

    if !target_path.starts_with(home_path) {
        return Ok(());
    }

    // Collect the dirs between home and target (home excluded, target included).
    let mut chain: Vec<&Path> = target_path
        .ancestors()
        .filter(|p| p.starts_with(home_path) && *p != home_path)
        .collect();
    chain.reverse();

    for dir in &chain {
        let s = dir.to_string_lossy();
        if *dir == target_path {
            docker.exec_root(container, &["chown", "-R", owner, &s])?;
        } else {
            docker.exec_root(container, &["chown", owner, &s])?;
        }
    }

    Ok(())
}

// Copies all shared library deps (including the dynamic linker) into
// /opt/graft/lib/, skipping ones already there. Returns the linker filename
// (e.g. "ld-linux-x86-64.so.2") so the wrapper can launch the binary through it.
fn inject_deps(
    docker: &Docker,
    container: &str,
    binary: &str,
    name: &str,
) -> Result<Option<String>> {
    let deps = deps::resolve(binary)?;
    if deps.is_empty() {
        return Ok(None);
    }

    let linker_name = deps
        .linker
        .as_ref()
        .and_then(|p| p.file_name())
        .map(|n| n.to_string_lossy().into_owned());

    let missing = missing_graft_deps(docker, container, &deps.libs)?;
    if !missing.is_empty() {
        println!(
            "[graft] copying {} missing deps for {}",
            missing.len(),
            name
        );
        for dep in &missing {
            // Resolve symlinks so we copy the real .so, then place it under its
            // requested name so the linker's --library-path search finds it.
            let real = std::fs::canonicalize(dep).unwrap_or_else(|_| dep.to_path_buf());
            let filename = dep.file_name().unwrap().to_string_lossy();
            docker.cp(
                &real.to_string_lossy(),
                container,
                &format!("{GRAFT_LIB}/{filename}"),
            )?;
        }
    }

    Ok(linker_name)
}

// Returns deps whose filename is not yet present in /opt/graft/lib/.
fn missing_graft_deps<'a>(
    docker: &Docker,
    container: &str,
    deps: &'a [PathBuf],
) -> Result<Vec<&'a PathBuf>> {
    let check = deps
        .iter()
        .filter_map(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
        .map(|name| format!("test -f '{GRAFT_LIB}/{name}' || printf '%s\\n' '{name}'"))
        .collect::<Vec<_>>()
        .join("; ");

    let stdout = docker.exec_capture(container, &["sh", "-c", &check])?;
    let missing_names: HashSet<&str> = stdout.lines().filter(|l| !l.is_empty()).collect();

    Ok(deps
        .iter()
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .map(|n| missing_names.contains(n))
                .unwrap_or(false)
        })
        .collect())
}

// Installs a thin wrapper at `target` that execs the grafted binary directly.
//
// The binary itself was patched (PT_INTERP + DT_RUNPATH) to run against our
// grafted glibc, so the wrapper needs no LD_LIBRARY_PATH and no ld.so
// indirection — it just forwards args. Execing directly is what keeps
// /proc/self/exe pointing at the binary for tools that re-exec themselves.
fn install_wrapper(
    docker: &Docker,
    container: &str,
    graft_binary: &str,
    target: &str,
) -> Result<()> {
    let script = format!("#!/bin/sh\nexec {graft_binary} \"$@\"\n");

    let tmp = TempFile::write(
        &format!("graft-wrapper-{}", file_stem(target)),
        script.as_bytes(),
    )?;

    ensure_parent(docker, container, target)?;
    docker.cp(&tmp.path().to_string_lossy(), container, target)?;
    docker.exec_root(container, &["chmod", "+x", target])?;

    Ok(())
}

fn ensure_parent(docker: &Docker, container: &str, path: &str) -> Result<()> {
    let parent = Path::new(path)
        .parent()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|| "/".to_string());
    docker.mkdir_p(container, &[&parent])
}

// Login shells source /etc/profile, which on many distros resets PATH to a
// minimal system default — dropping anything added via Docker ENV. Read the
// container's current PATH and write it into /etc/profile.d/ so it survives.
fn inject_docker_path(docker: &Docker, container: &str) -> Result<()> {
    let path = match docker.exec_capture(container, &["printenv", "PATH"]) {
        Ok(p) => p.trim().to_string(),
        Err(_) => return Ok(()),
    };
    if path.is_empty() {
        return Ok(());
    }
    let script = format!("export PATH={}\n", crate::docker::shell_quote(&path));
    let tmp = TempFile::write("graft-path.sh", script.as_bytes())?;
    docker.mkdir_p(container, &["/etc/profile.d"])?;
    docker.cp(
        &tmp.path().to_string_lossy(),
        container,
        "/etc/profile.d/graft-path.sh",
    )
}

// Detects the container user's home directory. Queries as `remote_user` when
// set so $HOME reflects their account. Falls back to /root on failure.
fn container_home(docker: &Docker, container: &str, remote_user: Option<&str>) -> String {
    docker
        .exec_capture_as(container, remote_user, &["sh", "-c", "echo $HOME"])
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "/root".to_string())
}

// Returns the owner to use for chown, or None when the container runs as root
// and no `remoteUser` override is set (chowning is then unnecessary).
// When `remote_user` is specified, returns it directly as the owner name
// (chown accepts usernames). Otherwise detects uid:gid via `id`.
fn container_user(
    docker: &Docker,
    container: &str,
    remote_user: Option<&str>,
) -> Option<String> {
    if let Some(u) = remote_user {
        return if u == "root" { None } else { Some(u.to_string()) };
    }
    let out = docker
        .exec_capture_as(
            container,
            None,
            &["sh", "-c", "printf '%s:%s' \"$(id -u)\" \"$(id -g)\""],
        )
        .ok()?;
    let s = out.trim().to_string();
    if s.starts_with("0:") { None } else { Some(s) }
}

// Expands a leading `~` to the container user's home directory.
fn resolve_home(path: &str, home: &str) -> String {
    let home = home.trim_end_matches('/');
    if path == "~" {
        home.to_string()
    } else if let Some(rest) = path.strip_prefix("~/") {
        format!("{home}/{rest}")
    } else {
        path.to_string()
    }
}

fn file_stem(path: &str) -> String {
    Path::new(path)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "graft".to_string())
}

fn inject_aliases(
    docker: &Docker,
    container: &str,
    aliases: &HashMap<String, String>,
) -> Result<()> {
    if aliases.is_empty() {
        return Ok(());
    }

    let mut content = String::from("# Generated by graft — do not edit\n");
    let mut pairs: Vec<_> = aliases.iter().collect();
    pairs.sort_by_key(|(k, _)| k.as_str());
    for (name, cmd) in pairs {
        content.push_str(&format!("alias {}={}\n", name, shell_quote(cmd)));
    }

    let tmp = TempFile::write("graft-aliases.sh", content.as_bytes())?;
    docker.mkdir_p(container, &["/etc/profile.d"])?;
    docker.cp(
        &tmp.path().to_string_lossy(),
        container,
        "/etc/profile.d/graft.sh",
    )?;
    println!(
        "[graft] installed {} alias{}",
        aliases.len(),
        if aliases.len() == 1 { "" } else { "es" }
    );

    Ok(())
}

/// A temp file that deletes itself on drop, so we don't leak `graft-*` files
/// in the system temp dir when an error short-circuits the injection.
struct TempFile {
    path: PathBuf,
}

impl TempFile {
    fn write(name: &str, contents: &[u8]) -> Result<Self> {
        let path = std::env::temp_dir().join(name);
        std::fs::write(&path, contents)?;
        Ok(Self { path })
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

#[cfg(test)]
mod tests {
    use super::{file_stem, resolve_home};

    #[test]
    fn takes_basename() {
        assert_eq!(file_stem("/usr/local/bin/nvim"), "nvim");
        assert_eq!(file_stem("nvim"), "nvim");
    }

    #[test]
    fn falls_back_when_no_basename() {
        assert_eq!(file_stem("/"), "graft");
    }

    #[test]
    fn resolve_home_expands_tilde() {
        assert_eq!(
            resolve_home("~/.config/nvim", "/home/vscode"),
            "/home/vscode/.config/nvim"
        );
        assert_eq!(resolve_home("~", "/home/vscode"), "/home/vscode");
        assert_eq!(
            resolve_home("/usr/local/bin/nvim", "/home/vscode"),
            "/usr/local/bin/nvim"
        );
    }

    #[test]
    fn resolve_home_strips_trailing_slash_from_home() {
        assert_eq!(resolve_home("~/.config", "/root/"), "/root/.config");
    }
}
