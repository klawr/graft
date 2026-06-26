use anyhow::{Context, Result, bail};
use json_comments::StripComments;
use serde::Deserialize;
use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::docker::Docker;
use crate::forward::PortForward;

// Where the hash of the .devcontainer inputs is stored *inside* the container,
// so we can tell on the next `graft up` whether the config drifted since this
// container instance was created. Living in the container means it survives
// reboots and is discarded automatically when the container is recreated.
const HASH_PATH: &str = "/opt/graft/devcontainer.hash";

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct DevcontainerConfig {
    // Human-readable label, used (when present) for the tmux session name.
    name: Option<String>,
    // dockerComposeFile backend.
    docker_compose_file: Option<serde_json::Value>,
    service: Option<String>,
    // image / dockerFile backend.
    image: Option<String>,
    build: Option<BuildConfig>,
    docker_file: Option<String>,
    context: Option<String>,
    run_args: Option<Vec<String>>,
    workspace_mount: Option<String>,
    mounts: Option<Vec<serde_json::Value>>,
    // common.
    workspace_folder: Option<String>,
    forward_ports: Option<Vec<serde_json::Value>>,
    features: Option<serde_json::Map<String, serde_json::Value>>,
    override_feature_install_order: Option<Vec<String>>,
    container_env: Option<BTreeMap<String, String>>,
    remote_env: Option<BTreeMap<String, String>>,
    // Lifecycle hooks. Each may be a string ("sh -c"), an array (argv), or an
    // object of named commands. Absent fields deserialize to None.
    initialize_command: Option<Cmd>,
    on_create_command: Option<Cmd>,
    update_content_command: Option<Cmd>,
    post_create_command: Option<Cmd>,
    post_start_command: Option<Cmd>,
    post_attach_command: Option<Cmd>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct BuildConfig {
    dockerfile: Option<String>,
    context: Option<String>,
    target: Option<String>,
    #[serde(default)]
    args: BTreeMap<String, String>,
}

// A devcontainer lifecycle command in any of its three spec shapes.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
enum Cmd {
    Shell(String),
    Exec(Vec<String>),
    Named(BTreeMap<String, OneCmd>),
}

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
enum OneCmd {
    Shell(String),
    Exec(Vec<String>),
}

#[derive(Default)]
struct Lifecycle {
    initialize: Option<Cmd>,
    on_create: Option<Cmd>,
    update_content: Option<Cmd>,
    post_create: Option<Cmd>,
    post_start: Option<Cmd>,
    post_attach: Option<Cmd>,
}

// Brings a devcontainer's container up. Each backend (compose vs a plain
// image/built container) only has to know how to find an existing container and
// create/start one; everything after that — hashing, env, features, lifecycle
// hooks — is shared and works off the container id.
trait Backend {
    /// Id of an existing container for this devcontainer (running or stopped),
    /// or None if it hasn't been created yet.
    fn existing(&self) -> Result<Option<String>>;
    /// Brings the container up, recreating it from scratch when `recreate` is
    /// set, applying any feature `flags` at creation, and returns its id.
    fn up(&self, recreate: bool, flags: &crate::features::CreateFlags) -> Result<String>;
}

// The backend-agnostic parts of a parsed devcontainer.
struct Spec {
    config_path: PathBuf,
    // Directory the devcontainer.json lives in: hashed for change detection,
    // used as cwd for initializeCommand, and as the base for local feature refs.
    base_dir: PathBuf,
    // Extra files (besides the .devcontainer dir) to fold into the hash.
    hash_extra: Vec<PathBuf>,
    workspace_folder: String,
    // Human-readable tmux session name (e.g. "graft-myproject-1a2b3c").
    session_name: String,
    forward_ports: Vec<PortForward>,
    lifecycle: Lifecycle,
    features: Vec<crate::features::FeatureRequest>,
    override_feature_order: Vec<String>,
    // Merged containerEnv + remoteEnv (remoteEnv wins on conflict).
    env: BTreeMap<String, String>,
    backend: Box<dyn Backend>,
}

/// The result of bringing a devcontainer up: the resolved container, the
/// workspace folder to drop the user into, and the (deferred) postAttach hook.
pub struct Started {
    pub container: String,
    pub session_name: String,
    pub workdir: String,
    pub forward_ports: Vec<PortForward>,
    env: Vec<(String, String)>,
    post_attach: Option<Cmd>,
}

impl Started {
    /// Runs `postAttachCommand`, if any. Called after graft has injected the
    /// environment and right before attaching the interactive session.
    pub fn run_post_attach(&self, remote: &Option<String>) -> Result<()> {
        if let Some(cmd) = &self.post_attach {
            let docker = Docker::new(remote);
            run_in_container(
                &docker,
                &self.container,
                &self.workdir,
                &self.env,
                "postAttachCommand",
                cmd,
            );
        }
        Ok(())
    }
}

fn load_spec(project_path: &Path, remote: &Option<String>) -> Result<Spec> {
    let (config_path, raw) = match remote {
        Some(host) => {
            let (path, bytes) = ssh_find_and_read_config(host, project_path)?;
            let text = String::from_utf8(bytes).context("devcontainer.json is not valid UTF-8")?;
            (path, text)
        }
        None => {
            let path = find_config(project_path)?;
            let text = std::fs::read_to_string(&path)
                .with_context(|| format!("reading {}", path.display()))?;
            (path, text)
        }
    };
    let config: DevcontainerConfig = serde_json::from_reader(StripComments::new(raw.as_bytes()))
        .context("parsing devcontainer.json")?;
    // Destructure up front so each field moves cleanly into the right place.
    let DevcontainerConfig {
        name,
        docker_compose_file,
        service,
        image,
        build,
        docker_file,
        context,
        run_args,
        workspace_mount,
        mounts,
        workspace_folder,
        forward_ports,
        features,
        override_feature_install_order,
        container_env,
        remote_env,
        initialize_command,
        on_create_command,
        update_content_command,
        post_create_command,
        post_start_command,
        post_attach_command,
    } = config;

    let forward_ports = parse_forward_ports(forward_ports);
    let base_dir = config_path.parent().unwrap_or(project_path).to_path_buf();
    let basename = project_path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "workspace".to_string());
    let workspace_folder = workspace_folder
        .map(|wf| substitute_vars(&wf, project_path))
        .unwrap_or_else(|| format!("/workspaces/{basename}"));
    let session_name = session_name(name.as_deref(), &basename, project_path);

    let lifecycle = Lifecycle {
        initialize: initialize_command,
        on_create: on_create_command,
        update_content: update_content_command,
        post_create: post_create_command,
        post_start: post_start_command,
        post_attach: post_attach_command,
    };

    let features = features
        .unwrap_or_default()
        .into_iter()
        .map(|(reference, opts)| crate::features::FeatureRequest {
            reference,
            options: match opts {
                serde_json::Value::Object(m) => m,
                _ => Default::default(),
            },
        })
        .collect();
    let override_feature_order = override_feature_install_order.unwrap_or_default();

    // containerEnv + remoteEnv, with remoteEnv overriding on conflict. Values
    // are normalized: host-side vars resolved now, ${containerEnv:X} rewritten to
    // ${X} so the container's shell expands it when the profile is sourced.
    let mut env: BTreeMap<String, String> = BTreeMap::new();
    for (k, v) in container_env.unwrap_or_default() {
        env.insert(k, normalize_env_value(&v, project_path));
    }
    for (k, v) in remote_env.unwrap_or_default() {
        env.insert(k, normalize_env_value(&v, project_path));
    }

    // Pick a backend based on which form the devcontainer uses.
    let (backend, hash_extra): (Box<dyn Backend>, Vec<PathBuf>) = if let Some(compose_file) =
        docker_compose_file
    {
        let files = match compose_file {
            serde_json::Value::String(s) => vec![s],
            serde_json::Value::Array(a) => a
                .into_iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect(),
            _ => bail!("unexpected dockerComposeFile format"),
        };
        let service =
            service.context("devcontainer.json with dockerComposeFile is missing 'service'")?;
        let hash_extra = files.iter().map(|f| base_dir.join(f)).collect();
        let backend = ComposeBackend {
            remote: remote.clone(),
            dir: base_dir.clone(),
            files,
            service,
            // Compose project names must be lowercase.
            project: container_name(project_path).to_lowercase(),
        };
        (Box::new(backend), hash_extra)
    } else if image.is_some() || build.is_some() || docker_file.is_some() {
        let name = container_name(project_path);
        let source = if let Some(img) = image {
            ImageSource::Image(img)
        } else {
            let build = build.unwrap_or_default();
            let context_rel = build.context.or(context).unwrap_or_else(|| ".".to_string());
            ImageSource::Build {
                context: base_dir.join(context_rel),
                dockerfile: build.dockerfile.or(docker_file),
                target: build.target,
                args: build.args,
                tag: format!("{name}-img"),
            }
        };
        let run_args = build_run_args(
            run_args,
            workspace_mount,
            mounts,
            project_path,
            &workspace_folder,
        );
        (
            Box::new(ContainerBackend {
                remote: remote.clone(),
                name,
                source,
                run_args,
            }),
            vec![],
        )
    } else {
        bail!("devcontainer.json must set one of: dockerComposeFile, image, build, or dockerFile");
    };

    Ok(Spec {
        config_path,
        base_dir,
        hash_extra,
        workspace_folder,
        session_name,
        forward_ports,
        lifecycle,
        features,
        override_feature_order,
        env,
        backend,
    })
}

// ── backends ───────────────────────────────────────────────────────────────────

struct ComposeBackend {
    remote: Option<String>,
    dir: PathBuf,
    files: Vec<String>,
    service: String,
    // Explicit compose project name (-p). Without it, compose derives the
    // project from the compose file's directory — which is ".devcontainer"
    // for most devcontainers, so *every* such project on a daemon would share
    // one project name and graft could adopt a different project's container.
    project: String,
}

impl Backend for ComposeBackend {
    fn existing(&self) -> Result<Option<String>> {
        let id = compose_ps(
            &self.remote,
            &self.dir,
            &self.project,
            &self.files,
            &self.service,
            true,
        )?;
        Ok((!id.is_empty()).then_some(id))
    }

    fn up(&self, recreate: bool, flags: &crate::features::CreateFlags) -> Result<String> {
        // On recreate, rebuild and force a fresh container; otherwise keep the
        // existing one (and stop compose recreating it on its own config-hash).
        let extra: &[&str] = if recreate {
            &["--build", "--force-recreate"]
        } else {
            &["--no-recreate"]
        };

        // Feature create-flags go in a temporary compose override, appended as
        // the last `-f` so it layers over the project's files without changing
        // the project identity (the first file still sets the project dir).
        // Compose reads it client-side, so with --remote it is written on the
        // remote host (where compose runs).
        let mut files = self.files.clone();
        let override_file = if flags.is_empty() {
            None
        } else {
            let content = flags.compose_override(&self.service);
            let path = match &self.remote {
                None => {
                    let p = std::env::temp_dir()
                        .join(format!("graft-compose-override-{}.yml", std::process::id()));
                    std::fs::write(&p, &content).context("writing compose override")?;
                    p.to_string_lossy().into_owned()
                }
                Some(host) => {
                    let p = format!("/tmp/graft-compose-override-{}.yml", std::process::id());
                    ssh_write_file(host, &p, &content).context("writing compose override")?;
                    p
                }
            };
            files.push(path.clone());
            Some(path)
        };

        let result = compose_up(&self.remote, &self.dir, &self.project, &files, extra);
        if let Some(p) = &override_file {
            match &self.remote {
                None => {
                    let _ = std::fs::remove_file(p);
                }
                Some(host) => {
                    let _ = ssh_output(host, &format!("rm -f {}", crate::docker::shell_quote(p)));
                }
            }
        }
        result?;

        let id = compose_ps(
            &self.remote,
            &self.dir,
            &self.project,
            &self.files,
            &self.service,
            false,
        )?;
        if id.is_empty() {
            bail!(
                "could not find running container for service '{}'",
                self.service
            );
        }
        Ok(id)
    }
}

enum ImageSource {
    Image(String),
    Build {
        context: PathBuf,
        dockerfile: Option<String>,
        target: Option<String>,
        args: BTreeMap<String, String>,
        tag: String,
    },
}

// A plain (non-compose) devcontainer: an `image` to run or a `build` to build,
// run detached as `sleep infinity` with the workspace mounted, under a stable
// per-project name so graft can find/recreate it.
struct ContainerBackend {
    remote: Option<String>,
    name: String,
    source: ImageSource,
    run_args: Vec<String>,
}

impl Backend for ContainerBackend {
    fn existing(&self) -> Result<Option<String>> {
        let out = docker_capture(
            &self.remote,
            &["ps", "-aq", "--filter", &format!("name=^{}$", self.name)],
        )?;
        Ok(out
            .lines()
            .map(str::trim)
            .find(|s| !s.is_empty())
            .map(String::from))
    }

    fn up(&self, recreate: bool, flags: &crate::features::CreateFlags) -> Result<String> {
        let exists = self.existing()?.is_some();
        if exists && !recreate {
            println!("[graft] starting {}", self.name);
            let _ = self.docker(&["start", &self.name]).status();
            return Ok(self.name.clone());
        }
        if exists {
            let _ = self.docker(&["rm", "-f", &self.name]).status();
        }

        let image = self.resolve_image()?;
        println!("[graft] creating container {}", self.name);
        let mut args: Vec<String> = vec![
            "run".into(),
            "-d".into(),
            "--name".into(),
            self.name.clone(),
        ];
        args.extend(self.run_args.iter().cloned());
        args.extend(flags.run_args());
        args.push(image);
        args.extend(["sleep".into(), "infinity".into()]);
        docker_run(&self.remote, &args).context("docker run")?;
        Ok(self.name.clone())
    }
}

impl ContainerBackend {
    // A `docker` command targeting this backend's daemon (local or remote).
    fn docker(&self, args: &[&str]) -> Command {
        crate::docker::command(&self.remote, args)
    }

    fn resolve_image(&self) -> Result<String> {
        match &self.source {
            ImageSource::Image(img) => Ok(img.clone()),
            ImageSource::Build {
                context,
                dockerfile,
                target,
                args,
                tag,
            } => {
                println!("[graft] building image {tag}");
                let mut a: Vec<String> = vec!["build".into(), "-t".into(), tag.clone()];
                if let Some(df) = dockerfile {
                    a.push("-f".into());
                    a.push(context.join(df).to_string_lossy().into_owned());
                }
                if let Some(t) = target {
                    a.push("--target".into());
                    a.push(t.clone());
                }
                for (k, v) in args {
                    a.push("--build-arg".into());
                    a.push(format!("{k}={v}"));
                }
                a.push(context.to_string_lossy().into_owned());
                // The build context (and -f dockerfile) are read client-side,
                // so with --remote the build runs on the remote host via ssh.
                let mut cmd = docker_files_command(&self.remote, None, &a);
                crate::verbose::trace(&cmd);
                let status = cmd.status().context("docker build")?;
                if !status.success() {
                    bail!("docker build failed");
                }
                Ok(tag.clone())
            }
        }
    }
}

// A stable container name for a non-compose devcontainer: project basename plus
// a short hash of its path, so distinct projects with the same basename don't
// collide.
fn container_name(project_path: &Path) -> String {
    let base: String = project_path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "workspace".to_string())
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    let h = fnv1a(project_path.to_string_lossy().as_bytes(), FNV_OFFSET);
    format!("graft-{base}-{:06x}", h & 0xff_ffff)
}

// A readable tmux session name: the devcontainer's `name` (or the project
// basename when it has none), sanitized for tmux, plus a short path hash so two
// projects that share a label don't land on the same session. We derive this
// from the human label rather than the container id, whose compose form is an
// opaque hash that makes "graft-<hash>" sessions impossible to tell apart.
fn session_name(name: Option<&str>, basename: &str, project_path: &Path) -> String {
    let label: String = name
        .unwrap_or(basename)
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    let label = label.trim_matches('-');
    let label = if label.is_empty() { "workspace" } else { label };
    let h = fnv1a(project_path.to_string_lossy().as_bytes(), FNV_OFFSET);
    format!("graft-{label}-{:06x}", h & 0xff_ffff)
}

// Builds the `docker run` arguments for a ContainerBackend: the workspace mount
// (or a default bind), the working dir, any extra `mounts`, and the user's
// `runArgs`.
fn build_run_args(
    user_args: Option<Vec<String>>,
    workspace_mount: Option<String>,
    mounts: Option<Vec<serde_json::Value>>,
    project_path: &Path,
    workspace_folder: &str,
) -> Vec<String> {
    let mut args = Vec::new();
    match workspace_mount {
        Some(m) => {
            args.push("--mount".into());
            args.push(substitute_vars(&m, project_path));
        }
        None => {
            args.push("-v".into());
            args.push(format!("{}:{}", project_path.display(), workspace_folder));
        }
    }
    args.push("-w".into());
    args.push(workspace_folder.to_string());

    for m in mounts.unwrap_or_default() {
        if let Some(spec) = mount_to_arg(&m, project_path) {
            args.push("--mount".into());
            args.push(spec);
        }
    }
    args.extend(
        user_args
            .unwrap_or_default()
            .into_iter()
            .map(|a| substitute_vars(&a, project_path)),
    );
    args
}

// A devcontainer `mounts` entry → a `--mount` value (string or {source,target,type}).
fn mount_to_arg(m: &serde_json::Value, project_path: &Path) -> Option<String> {
    match m {
        serde_json::Value::String(s) => Some(substitute_vars(s, project_path)),
        serde_json::Value::Object(o) => {
            let mut parts = Vec::new();
            if let Some(t) = o.get("type").and_then(|v| v.as_str()) {
                parts.push(format!("type={t}"));
            }
            if let Some(s) = o.get("source").and_then(|v| v.as_str()) {
                parts.push(format!("source={}", substitute_vars(s, project_path)));
            }
            if let Some(t) = o.get("target").and_then(|v| v.as_str()) {
                parts.push(format!("target={t}"));
            }
            (!parts.is_empty()).then(|| parts.join(","))
        }
        _ => None,
    }
}

// Parses forwardPorts entries. The spec allows an integer (a port of the
// primary container) or a "host:port" string, where host is another host on
// the container network — typically a compose service ("db:5432"). On top of
// that, a numeric prefix ("3000:8080") is read docker -p style as a
// local-port:container-port mapping, since a purely numeric hostname is never
// meaningful. A "/proto" suffix is tolerated and ignored (TCP only).
fn parse_forward_ports(values: Option<Vec<serde_json::Value>>) -> Vec<PortForward> {
    values
        .unwrap_or_default()
        .into_iter()
        .filter_map(|v| match v {
            serde_json::Value::Number(n) => n
                .as_u64()
                .and_then(|n| u16::try_from(n).ok())
                .map(PortForward::same),
            serde_json::Value::String(s) => parse_forward_entry(&s),
            _ => None,
        })
        .collect()
}

fn parse_forward_entry(s: &str) -> Option<PortForward> {
    let s = s.split('/').next().unwrap_or(s);
    match s.split_once(':') {
        None => s.parse().ok().map(PortForward::same),
        Some((prefix, port)) => {
            let port: u16 = port.parse().ok()?;
            match prefix.parse::<u16>() {
                Ok(local) => Some(PortForward {
                    local,
                    host: None,
                    port,
                }),
                Err(_) => Some(PortForward {
                    local: port,
                    host: Some(prefix.to_string()),
                    port,
                }),
            }
        }
    }
}

fn docker_run(remote: &Option<String>, args: &[String]) -> Result<()> {
    let refs: Vec<&str> = args.iter().map(String::as_str).collect();
    let status = crate::docker::command(remote, &refs)
        .status()
        .context("spawning docker")?;
    if !status.success() {
        bail!(
            "docker {} failed",
            args.first().map(String::as_str).unwrap_or("")
        );
    }
    Ok(())
}

fn docker_capture(remote: &Option<String>, args: &[&str]) -> Result<String> {
    let out = crate::docker::command(remote, args)
        .output()
        .context("spawning docker")?;
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

fn normalize_env_value(v: &str, project_path: &Path) -> String {
    substitute_vars(v, project_path).replace("${containerEnv:", "${")
}

// Resolves the devcontainer variable subset that can appear on the host side.
fn substitute_vars(s: &str, project_path: &Path) -> String {
    let workspace = project_path.to_string_lossy();
    let basename = project_path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();

    let mut out = s.replace("${localWorkspaceFolder}", &workspace);
    out = out.replace("${localWorkspaceFolderBasename}", &basename);

    // ${localEnv:VAR_NAME}
    while let Some(start) = out.find("${localEnv:") {
        let Some(rel_end) = out[start..].find('}') else {
            break;
        };
        let var_name = &out[start + 11..start + rel_end].to_string();
        let value = std::env::var(var_name).unwrap_or_default();
        out = format!("{}{}{}", &out[..start], value, &out[start + rel_end + 1..]);
    }

    out
}

pub fn start(project_path: &Path, force_build: bool, remote: &Option<String>) -> Result<Started> {
    let spec = load_spec(project_path, remote)?;
    let docker = Docker::new(remote);
    let current_hash = hash_inputs(&spec.config_path, &spec.hash_extra, remote)?;

    // Is there already a container for this devcontainer, and has its config
    // drifted since it was created?
    let existing = spec.backend.existing()?;
    let drifted = !force_build
        && match &existing {
            Some(id) => read_hash(&docker, id).as_deref() != Some(current_hash.as_str()),
            None => false,
        };
    let recreate = drifted
        && confirm(
            ".devcontainer changed since this container was created — recreate it? \
             (anything installed in it since will be lost)",
        )?;
    let created = force_build || existing.is_none() || recreate;

    // initializeCommand runs on the host before the container is created/started.
    if let Some(cmd) = &spec.lifecycle.initialize {
        run_on_host(&spec.base_dir, "initializeCommand", cmd, remote)?;
    }

    // When (re)creating, resolve features up front so their container-creation
    // flags (mounts/privileged/…) can be applied at `up`. On a reused container
    // those flags are already baked in, so we skip the fetch and just retry any
    // not-yet-installed features afterwards (marker-gated).
    let create_plan = if created {
        crate::features::resolve(&spec.base_dir, &spec.features, &spec.override_feature_order)?
    } else {
        crate::features::Plan::default()
    };

    let id = spec
        .backend
        .up(force_build || recreate, &create_plan.create_flags())?;

    if created {
        // Record the hash so future runs can detect drift.
        write_hash(&docker, &id, &current_hash)?;
    }

    // Persist containerEnv/remoteEnv so login shells inside the container see it.
    write_env_profile(&docker, &id, &spec.env)?;
    let hook_env = exec_env(&spec.env);

    // Features install before the create hooks (the spec bakes them into the
    // image first). On a fresh container we install the resolved plan; on a
    // reused one, install() is marker-gated — a cheap no-op once everything
    // succeeded, retrying anything that previously failed.
    if created {
        create_plan.install(&docker, &id)?;
    } else {
        crate::features::install(
            &docker,
            &id,
            &spec.base_dir,
            &spec.features,
            &spec.override_feature_order,
        )?;
    }

    // Create-time hooks run once *successfully*: each is skipped if its success
    // marker exists, and the marker is written only when it succeeds — so a
    // failed hook retries on the next `graft up` rather than only on a recreate.
    // A failure is reported but never aborts graft.
    for (label, key, cmd) in [
        ("onCreateCommand", "onCreate", &spec.lifecycle.on_create),
        (
            "updateContentCommand",
            "updateContent",
            &spec.lifecycle.update_content,
        ),
        (
            "postCreateCommand",
            "postCreate",
            &spec.lifecycle.post_create,
        ),
    ] {
        if let Some(c) = cmd {
            if docker.task_done(&id, key) {
                continue;
            }
            if run_in_container(&docker, &id, &spec.workspace_folder, &hook_env, label, c) {
                docker.mark_task_done(&id, key)?;
            }
        }
    }

    // postStartCommand runs on every start; a failure is a warning, not fatal.
    if let Some(c) = &spec.lifecycle.post_start {
        run_in_container(
            &docker,
            &id,
            &spec.workspace_folder,
            &hook_env,
            "postStartCommand",
            c,
        );
    }

    let workdir = if docker.path_exists(&id, &spec.workspace_folder) {
        spec.workspace_folder
    } else {
        select_workdir(&docker, &id, &spec.workspace_folder)
    };

    Ok(Started {
        container: id,
        session_name: spec.session_name,
        workdir,
        forward_ports: spec.forward_ports,
        env: hook_env,
        post_attach: spec.lifecycle.post_attach,
    })
}

/// Stops the container `graft up` manages for this project, if one exists.
/// The container is kept so `graft up` can resume it with everything intact.
pub fn stop(project_path: &Path, remote: &Option<String>) -> Result<()> {
    let spec = load_spec(project_path, remote)?;
    match spec.backend.existing()? {
        Some(id) => {
            println!("[graft] stopping {id}");
            docker_run(remote, &["stop".to_string(), id])
        }
        None => {
            println!("[graft] no container exists for this devcontainer");
            Ok(())
        }
    }
}

// Writes containerEnv/remoteEnv to /etc/profile.d so login shells inherit it.
// Lines are written verbatim (each shell-quoted for printf), so ${VAR}
// references inside values expand when the profile is sourced.
fn write_env_profile(
    docker: &Docker,
    container: &str,
    env: &BTreeMap<String, String>,
) -> Result<()> {
    if env.is_empty() {
        return Ok(());
    }
    let lines: Vec<String> = env
        .iter()
        .map(|(k, v)| crate::docker::shell_quote(&format!("export {k}={}", quote_env_value(v))))
        .collect();
    let script = format!(
        "printf '%s\\n' {} > /etc/profile.d/graft-env.sh",
        lines.join(" ")
    );
    docker.exec_root(container, &["sh", "-c", &script])
}

// Renders an env value for a POSIX `export K=<value>` line. Literal runs are
// shell-quoted so spaces and metacharacters (`code --wait`, `a;b`, `a'b`) are
// safe, while `${VAR}` references are left bare so the login shell still expands
// them — `normalize_env_value` deliberately rewrites `${containerEnv:X}` to
// `${X}` for exactly that, so quoting the whole value would break it.
fn quote_env_value(v: &str) -> String {
    let mut out = String::new();
    let mut rest = v;
    while let Some(start) = rest.find("${") {
        match rest[start..].find('}') {
            Some(end_rel) => {
                let end = start + end_rel + 1; // include the closing '}'
                if start > 0 {
                    out.push_str(&crate::docker::shell_quote(&rest[..start]));
                }
                out.push_str(&rest[start..end]); // ${VAR} kept bare
                rest = &rest[end..];
            }
            // No closing brace — treat the remainder as a literal.
            None => break,
        }
    }
    if !rest.is_empty() {
        out.push_str(&crate::docker::shell_quote(rest));
    }
    out
}

// Env to pass to hooks via `docker exec -e`. Values referencing other vars
// (${...}) are dropped here — they'd be set literally rather than expanded —
// and reach hooks via the sourced profile instead.
fn exec_env(env: &BTreeMap<String, String>) -> Vec<(String, String)> {
    env.iter()
        .filter(|(_, v)| !v.contains("${"))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect()
}

// Devcontainer config lookup, per spec precedence: .devcontainer/devcontainer.json,
// .devcontainer.json, then .devcontainer/<folder>/devcontainer.json (one level
// deep). With several subfolder configs the first (alphabetically) wins, with a
// warning — graft has no UI to pick one.
fn find_config(project_path: &Path) -> Result<PathBuf> {
    let candidates = [
        project_path.join(".devcontainer").join("devcontainer.json"),
        project_path.join(".devcontainer.json"),
    ];
    if let Some(p) = candidates.into_iter().find(|p| p.exists()) {
        return Ok(p);
    }

    let mut subs: Vec<PathBuf> = std::fs::read_dir(project_path.join(".devcontainer"))
        .into_iter()
        .flatten()
        .filter_map(|e| e.ok())
        .map(|e| e.path().join("devcontainer.json"))
        .filter(|p| p.is_file())
        .collect();
    subs.sort();
    if subs.len() > 1 {
        eprintln!(
            "[graft] multiple devcontainer configs found, using {}",
            subs[0].display()
        );
    }
    subs.into_iter().next().with_context(|| {
        format!(
            "no devcontainer config found in {} (looked for .devcontainer/devcontainer.json, \
             .devcontainer.json, and .devcontainer/*/devcontainer.json)",
            project_path.display()
        )
    })
}

// ── remote (SSH) file I/O helpers ─────────────────────────────────────────────

// Runs `sh -c <script>` on the remote and returns its output. Port and
// verbosity from the CLI are applied; the command line is traced at -v.
fn ssh_output(host: &str, script: &str) -> Result<std::process::Output> {
    let (mut cmd, dest) = crate::ssh::base_command(host);
    cmd.arg(&dest)
        .arg(format!("sh -c {}", crate::docker::shell_quote(script)));
    crate::verbose::trace(&cmd);
    let out = cmd.output().context("spawning ssh")?;
    // With ssh -v the debug chatter lands on stderr; surface it.
    if crate::verbose::enabled() && !out.stderr.is_empty() {
        eprint!("{}", String::from_utf8_lossy(&out.stderr));
    }
    Ok(out)
}

// Writes `content` to `path` on the remote host.
fn ssh_write_file(host: &str, path: &str, content: &str) -> Result<()> {
    use std::io::Write as _;
    let (mut cmd, dest) = crate::ssh::base_command(host);
    cmd.arg(&dest)
        .arg(format!("cat > {}", crate::docker::shell_quote(path)));
    cmd.stdin(Stdio::piped()).stdout(Stdio::null());
    crate::verbose::trace(&cmd);
    let mut child = cmd.spawn().context("spawning ssh")?;
    child
        .stdin
        .take()
        .expect("piped stdin")
        .write_all(content.as_bytes())
        .context("writing to ssh stdin")?;
    let status = child.wait().context("waiting for ssh")?;
    if !status.success() {
        bail!("writing {path} on {host} failed");
    }
    Ok(())
}

// One SSH call: probe the candidate config locations (same precedence as the
// local find_config) and cat the first one that exists. Output format:
// NUL-terminated path, then raw file bytes. Exit code 1 = no config found;
// anything else is an ssh/connection failure and is reported as such.
fn ssh_find_and_read_config(host: &str, project_path: &Path) -> Result<(PathBuf, Vec<u8>)> {
    let dc = crate::docker::shell_quote(
        &project_path
            .join(".devcontainer")
            .join("devcontainer.json")
            .to_string_lossy(),
    );
    let flat =
        crate::docker::shell_quote(&project_path.join(".devcontainer.json").to_string_lossy());
    // Quoted dir + unquoted glob, so the shell expands */devcontainer.json.
    let sub_glob = format!(
        "{}/*/devcontainer.json",
        crate::docker::shell_quote(&project_path.join(".devcontainer").to_string_lossy())
    );
    let script = format!(
        "for f in {dc} {flat} {sub_glob}; do [ -f \"$f\" ] && {{ printf '%s\\0' \"$f\"; cat \"$f\"; exit 0; }}; done; exit 1"
    );
    let out = ssh_output(host, &script)?;
    if !out.status.success() {
        // ssh exits with the remote command's status; our probe script exits 1
        // when nothing was found. Anything else (e.g. 255) means ssh itself
        // failed — auth, unknown host, … — and must not masquerade as a
        // missing config.
        if out.status.code() != Some(1) {
            bail!(
                "ssh to {host} failed ({}): {}",
                out.status,
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        bail!(
            "no devcontainer config found in {} on {host} (looked for \
             .devcontainer/devcontainer.json, .devcontainer.json, and \
             .devcontainer/*/devcontainer.json)",
            project_path.display()
        );
    }
    let nul = out
        .stdout
        .iter()
        .position(|&b| b == 0)
        .context("unexpected output from remote config probe")?;
    let path = PathBuf::from(String::from_utf8_lossy(&out.stdout[..nul]).as_ref());
    let content = out.stdout[nul + 1..].to_vec();
    Ok((path, content))
}

// One SSH call: hash every file under `dir` (plus any `extra` paths) using
// sha256sum, then hash the combined output for a stable fingerprint.
fn ssh_hash_inputs(host: &str, config_path: &Path, extra: &[PathBuf]) -> Result<String> {
    let dir = config_path.parent().unwrap_or(config_path);
    let mut targets = vec![crate::docker::shell_quote(&dir.to_string_lossy())];
    for e in extra {
        targets.push(crate::docker::shell_quote(&e.to_string_lossy()));
    }
    let script = format!(
        "find {} -type f 2>/dev/null | sort | xargs sha256sum 2>/dev/null | sha256sum",
        targets.join(" ")
    );
    let out = ssh_output(host, &script)?;
    if !out.status.success() {
        bail!(
            "hashing devcontainer inputs on {host} failed ({}): {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    // sha256sum output: "<hex>  -\n" — take just the hex token.
    let hash = String::from_utf8_lossy(&out.stdout)
        .split_whitespace()
        .next()
        .unwrap_or("0")
        .to_string();
    Ok(hash)
}

// ── lifecycle command execution ────────────────────────────────────────────────

// Runs a lifecycle command inside the container, in the workspace folder, with
// `env` exported. A failing command is reported but never aborts graft. Returns
// true only if every command succeeded (used to decide whether to mark the hook
// done).
fn run_in_container(
    docker: &Docker,
    container: &str,
    workdir: &str,
    env: &[(String, String)],
    label: &str,
    cmd: &Cmd,
) -> bool {
    println!("[graft] {label}");
    let run = |argv: &[&str]| -> bool {
        match docker.exec_in(container, Some(workdir), env, argv, false) {
            Ok(()) => true,
            Err(e) => {
                eprintln!("[graft] {label} failed (continuing): {e}");
                false
            }
        }
    };
    match cmd {
        Cmd::Shell(s) => run(&["sh", "-c", s]),
        Cmd::Exec(argv) => run(&argv.iter().map(String::as_str).collect::<Vec<_>>()),
        Cmd::Named(map) => {
            let mut ok = true;
            for (name, one) in map {
                println!("[graft]   {name}");
                ok &= match one {
                    OneCmd::Shell(s) => run(&["sh", "-c", s]),
                    OneCmd::Exec(argv) => run(&argv.iter().map(String::as_str).collect::<Vec<_>>()),
                };
            }
            ok
        }
    }
}

// Runs initializeCommand on the host, with the .devcontainer dir as cwd. The
// "host" is wherever the project files live: the local machine, or — with
// --remote — the remote host, over ssh (the dir doesn't exist locally there).
fn run_on_host(dir: &Path, label: &str, cmd: &Cmd, remote: &Option<String>) -> Result<()> {
    println!("[graft] {label} (host)");

    let run_one = |what: &str, one: &OneCmd| -> Result<()> {
        let status = match (remote, one) {
            (None, OneCmd::Shell(s)) => Command::new("sh")
                .arg("-c")
                .arg(s)
                .current_dir(dir)
                .status(),
            (None, OneCmd::Exec(argv)) => Command::new(&argv[0])
                .args(&argv[1..])
                .current_dir(dir)
                .status(),
            (Some(host), one) => {
                // Force `sh` semantics for the shell form rather than running
                // the string in the remote user's login shell.
                let body = match one {
                    OneCmd::Shell(s) => format!("sh -c {}", crate::docker::shell_quote(s)),
                    OneCmd::Exec(argv) => argv
                        .iter()
                        .map(|a| crate::docker::shell_quote(a))
                        .collect::<Vec<_>>()
                        .join(" "),
                };
                let script = format!(
                    "cd {} && {body}",
                    crate::docker::shell_quote(&dir.to_string_lossy())
                );
                let (mut c, dest) = crate::ssh::base_command(host);
                c.arg(dest).arg(script);
                crate::verbose::trace(&c);
                c.status()
            }
        };
        if !status.context("spawning host command")?.success() {
            bail!("{what} failed");
        }
        Ok(())
    };

    match cmd {
        Cmd::Shell(s) => run_one(label, &OneCmd::Shell(s.clone())),
        Cmd::Exec(argv) if !argv.is_empty() => run_one(label, &OneCmd::Exec(argv.clone())),
        Cmd::Exec(_) => Ok(()),
        Cmd::Named(map) => {
            for (name, one) in map {
                println!("[graft]   {name}");
                match one {
                    OneCmd::Shell(_) => run_one(name, one)?,
                    OneCmd::Exec(argv) if !argv.is_empty() => run_one(name, one)?,
                    OneCmd::Exec(_) => {}
                }
            }
            Ok(())
        }
    }
}

// ── docker compose helpers ─────────────────────────────────────────────────────

// Builds a command for docker operations that read *files* from disk — a
// compose project or a build context. Like `docker::command` it runs the CLI
// remotely over ssh when `--remote` is set, but it additionally `cd`s into
// `dir` first: compose files and build contexts are read client-side, i.e. on
// the remote host where the docker CLI now runs, so the working directory has
// to be the project dir *there*.
fn docker_files_command(remote: &Option<String>, dir: Option<&Path>, args: &[String]) -> Command {
    match remote {
        None => {
            let mut cmd = Command::new("docker");
            cmd.args(args);
            if let Some(d) = dir {
                cmd.current_dir(d);
            }
            cmd
        }
        Some(host) => {
            let mut script = String::new();
            if let Some(d) = dir {
                script.push_str(&format!(
                    "cd {} && ",
                    crate::docker::shell_quote(&d.to_string_lossy())
                ));
            }
            script.push_str("docker");
            for a in args {
                script.push(' ');
                script.push_str(&crate::docker::shell_quote(a));
            }
            let (mut cmd, dest) = crate::ssh::base_command(host);
            cmd.arg(dest).arg(script);
            cmd
        }
    }
}

fn compose_file_args(files: &[String]) -> Vec<String> {
    let mut args = vec![];
    for f in files {
        args.push("-f".to_string());
        args.push(f.clone());
    }
    args
}

fn compose_up(
    remote: &Option<String>,
    dir: &Path,
    project: &str,
    files: &[String],
    extra: &[&str],
) -> Result<()> {
    let mut args = vec!["compose".to_string(), "-p".to_string(), project.to_string()];
    args.extend(compose_file_args(files));
    args.extend(["up".to_string(), "-d".to_string()]);
    args.extend(extra.iter().map(|s| s.to_string()));

    let mut cmd = docker_files_command(remote, Some(dir), &args);
    crate::verbose::trace(&cmd);
    let status = cmd.status().context("running docker compose up")?;
    if !status.success() {
        bail!("docker compose up failed");
    }
    Ok(())
}

// Returns the (first) container id for the service, or "" if none. With
// `all`, includes stopped containers.
fn compose_ps(
    remote: &Option<String>,
    dir: &Path,
    project: &str,
    files: &[String],
    service: &str,
    all: bool,
) -> Result<String> {
    let mut args = vec!["compose".to_string(), "-p".to_string(), project.to_string()];
    args.extend(compose_file_args(files));
    args.push("ps".to_string());
    if all {
        args.push("-a".to_string());
    }
    args.extend(["-q".to_string(), service.to_string()]);

    let mut cmd = docker_files_command(remote, Some(dir), &args);
    crate::verbose::trace(&cmd);
    let output = cmd.output().context("docker compose ps")?;
    if !output.status.success() {
        bail!(
            "docker compose ps failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }

    Ok(String::from_utf8(output.stdout)?
        .lines()
        .next()
        .unwrap_or("")
        .trim()
        .to_string())
}

// ── hash storage in the container ──────────────────────────────────────────────

fn read_hash(docker: &Docker, container: &str) -> Option<String> {
    let tmp = std::env::temp_dir().join(format!("graft-dchash-{container}"));
    let hash = docker
        .cp_out(container, HASH_PATH, &tmp.to_string_lossy())
        .ok()
        .and_then(|_| std::fs::read_to_string(&tmp).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    let _ = std::fs::remove_file(&tmp);
    hash
}

fn write_hash(docker: &Docker, container: &str, hash: &str) -> Result<()> {
    let script = format!(
        "mkdir -p /opt/graft && printf %s {} > {HASH_PATH}",
        crate::docker::shell_quote(hash)
    );
    docker.exec_root(container, &["sh", "-c", &script])
}

// ── change detection ───────────────────────────────────────────────────────────

// Hashes the inputs that define the container. When the config lives in a
// `.devcontainer/` directory we hash the whole directory (Dockerfile, compose
// file, scripts, …); otherwise we hash the standalone config plus any `extra`
// files the backend declares (e.g. compose files referenced from elsewhere).
fn hash_inputs(config_path: &Path, extra: &[PathBuf], remote: &Option<String>) -> Result<String> {
    if let Some(host) = remote {
        return ssh_hash_inputs(host, config_path, extra);
    }

    let mut entries: Vec<(String, Vec<u8>)> = vec![];
    let parent = config_path.parent();

    if parent.and_then(Path::file_name) == Some(OsStr::new(".devcontainer")) {
        let root = parent.unwrap();
        collect_dir(root, root, &mut entries)?;
    } else {
        let mut files = vec![config_path.to_path_buf()];
        files.extend_from_slice(extra);
        for f in files {
            if let Ok(bytes) = std::fs::read(&f) {
                let name = f
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned();
                entries.push((name, normalize_for_hash(&f, bytes)));
            }
        }
    }

    entries.sort_by(|a, b| a.0.cmp(&b.0));

    let mut h = FNV_OFFSET;
    for (name, bytes) in &entries {
        h = fnv1a(name.as_bytes(), h);
        h = fnv1a(&[0x00], h);
        h = fnv1a(bytes, h);
        h = fnv1a(&[0xff], h);
    }
    Ok(format!("{h:016x}"))
}

// devcontainer.json is JSONC; canonicalize it before hashing so comment-only
// (and whitespace/key-order) edits don't trigger a spurious recreate prompt.
// Parsing to a serde_json::Value and re-serializing drops comments and
// formatting (serde_json's map is sorted, so key order is normalized too).
// Comment-stripping alone is insufficient: json_comments replaces comments with
// spaces, so a longer comment still changes the bytes. Other files hash as-is.
fn normalize_for_hash(path: &Path, bytes: Vec<u8>) -> Vec<u8> {
    let is_jsonc = matches!(
        path.file_name().and_then(|n| n.to_str()),
        Some("devcontainer.json") | Some(".devcontainer.json")
    );
    if is_jsonc
        && let Ok(value) =
            serde_json::from_reader::<_, serde_json::Value>(StripComments::new(bytes.as_slice()))
        && let Ok(canonical) = serde_json::to_vec(&value)
    {
        return canonical;
    }
    bytes
}

fn collect_dir(root: &Path, dir: &Path, out: &mut Vec<(String, Vec<u8>)>) -> Result<()> {
    let read = match std::fs::read_dir(dir) {
        Ok(rd) => rd,
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => return Ok(()),
        Err(e) => return Err(e).with_context(|| format!("reading {}", dir.display())),
    };
    let mut items: Vec<PathBuf> = read.filter_map(|e| e.ok().map(|e| e.path())).collect();
    items.sort();
    for path in items {
        if path.is_dir() {
            collect_dir(root, &path, out)?;
        } else if let Ok(bytes) = std::fs::read(&path) {
            let rel = path
                .strip_prefix(root)
                .unwrap_or(&path)
                .to_string_lossy()
                .into_owned();
            out.push((rel, normalize_for_hash(&path, bytes)));
        }
    }
    Ok(())
}

const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;

fn fnv1a(data: &[u8], mut h: u64) -> u64 {
    for &b in data {
        h ^= b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

// ── workspace selection ────────────────────────────────────────────────────────

// Called when the configured workspaceFolder doesn't exist in the container.
// Shows mount destinations and /workspaces/* entries as numbered candidates and
// lets the user pick one or type a custom path. Falls back to '/' without
// prompting when stdin is not a terminal.
fn select_workdir(docker: &Docker, container: &str, configured: &str) -> String {
    let candidates = discover_workdirs(docker, container);

    if candidates.is_empty() || !io::stdin().is_terminal() {
        eprintln!(
            "[graft] workspaceFolder '{configured}' not found in the container; falling back to '/'.\n\
             [graft]   Add \"workspaceFolder\": \"<path>\" to devcontainer.json to avoid this."
        );
        return "/".to_string();
    }

    eprintln!("[graft] workspaceFolder '{configured}' not found in the container.");
    eprintln!("[graft] Select a workspace directory (or press Enter for '/'):");
    for (i, dir) in candidates.iter().enumerate() {
        eprintln!("  {}) {dir}", i + 1);
    }

    loop {
        print!("[graft] > ");
        io::stdout().flush().ok();
        let mut line = String::new();
        if io::stdin().read_line(&mut line).is_err() || line.trim().is_empty() {
            return "/".to_string();
        }
        let input = line.trim();
        if let Ok(n) = input.parse::<usize>() {
            if n >= 1 && n <= candidates.len() {
                return candidates[n - 1].clone();
            }
            eprintln!(
                "[graft] Enter a number between 1 and {} or an absolute path.",
                candidates.len()
            );
        } else if input.starts_with('/') {
            return input.to_string();
        } else {
            eprintln!("[graft] Enter a number from the list or an absolute path starting with '/'.");
        }
    }
}

// Collects workspace candidates: actual mount destinations from `docker inspect`
// (the most relevant — the user's workspace is almost always a bind mount), plus
// any subdirectories found under /workspaces and /workspace (the standard
// devcontainer locations). Sorted and deduplicated.
fn discover_workdirs(docker: &Docker, container: &str) -> Vec<String> {
    let mut candidates = docker.inspect_mounts(container);

    if let Ok(out) = docker.exec_capture(
        container,
        &[
            "sh",
            "-c",
            "find /workspaces /workspace -maxdepth 1 -mindepth 1 -type d 2>/dev/null",
        ],
    ) {
        for line in out.lines().map(str::trim).filter(|s| !s.is_empty()) {
            let s = line.to_string();
            if !candidates.contains(&s) {
                candidates.push(s);
            }
        }
    }

    candidates.sort();
    candidates.dedup();
    candidates
}

// ── prompt ─────────────────────────────────────────────────────────────────────

fn confirm(question: &str) -> Result<bool> {
    if !io::stdin().is_terminal() {
        eprintln!(
            "[graft] {question}\n[graft] (no TTY — keeping the existing container; \
             re-run with `graft up --build` to recreate)"
        );
        return Ok(false);
    }
    print!("[graft] {question} [y/N] ");
    io::stdout().flush().ok();
    let mut line = String::new();
    io::stdin().read_line(&mut line)?;
    let answer = line.trim().to_ascii_lowercase();
    Ok(answer == "y" || answer == "yes")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cmd_parses_all_three_forms() {
        assert!(matches!(
            serde_json::from_str::<Cmd>(r#""echo hi""#).unwrap(),
            Cmd::Shell(_)
        ));
        assert!(matches!(
            serde_json::from_str::<Cmd>(r#"["echo", "hi"]"#).unwrap(),
            Cmd::Exec(_)
        ));
        assert!(matches!(
            serde_json::from_str::<Cmd>(r#"{"a": "echo hi", "b": ["echo", "yo"]}"#).unwrap(),
            Cmd::Named(_)
        ));
    }

    #[test]
    fn substitute_vars_resolves_workspace_and_env() {
        let p = Path::new("/home/me/proj");
        assert_eq!(
            substitute_vars("${localWorkspaceFolder}/x", p),
            "/home/me/proj/x"
        );
        assert_eq!(
            substitute_vars("${localWorkspaceFolderBasename}", p),
            "proj"
        );
        // PATH is reliably set; use it to exercise ${localEnv:...}.
        let path = std::env::var("PATH").unwrap();
        assert_eq!(substitute_vars("${localEnv:PATH}", p), path);
    }

    #[test]
    fn quote_env_value_quotes_literals_but_expands_vars() {
        // Plain literals with spaces/metacharacters get single-quoted whole.
        assert_eq!(quote_env_value("code --wait"), "'code --wait'");
        assert_eq!(quote_env_value("a;b&&c"), "'a;b&&c'");
        assert_eq!(quote_env_value("it's"), "'it'\\''s'");
        // ${VAR} references stay bare so the login shell still expands them
        // (normalize_env_value rewrites ${containerEnv:X} to ${X}).
        assert_eq!(quote_env_value("${PATH}"), "${PATH}");
        assert_eq!(quote_env_value("${PATH}:/foo"), "${PATH}':/foo'");
        assert_eq!(quote_env_value("${A} ${B}"), "${A}' '${B}");
        assert_eq!(quote_env_value("pre ${X} post"), "'pre '${X}' post'");
        // A dangling ${ with no closing brace is treated as a literal.
        assert_eq!(quote_env_value("a${b"), "'a${b'");
    }

    #[test]
    fn hash_normalization_ignores_comments_and_formatting() {
        let dc = Path::new("devcontainer.json");
        let pretty = normalize_for_hash(dc, br#"{ /* hi */ "a": 1, "b": 2 } // x"#.to_vec());
        let compact = normalize_for_hash(dc, br#"{"b":2,"a":1}"#.to_vec());
        assert_eq!(
            pretty, compact,
            "comments/whitespace/key-order must not affect the hash"
        );
    }

    #[test]
    fn hash_normalization_keeps_value_changes() {
        let dc = Path::new("devcontainer.json");
        let a = normalize_for_hash(dc, br#"{"a":1}"#.to_vec());
        let b = normalize_for_hash(dc, br#"{"a":2}"#.to_vec());
        assert_ne!(a, b);
    }

    #[test]
    fn hash_leaves_non_json_bytes_untouched() {
        let bytes = b"FROM debian\n// not a comment in a Dockerfile\n".to_vec();
        assert_eq!(
            normalize_for_hash(Path::new("Dockerfile"), bytes.clone()),
            bytes
        );
    }

    #[test]
    fn container_name_is_stable_and_path_scoped() {
        let a = container_name(Path::new("/home/me/proj"));
        assert_eq!(a, container_name(Path::new("/home/me/proj")));
        assert!(a.starts_with("graft-proj-"), "got {a}");
        // same basename, different path → different name (no collision)
        assert_ne!(a, container_name(Path::new("/elsewhere/proj")));
    }

    #[test]
    fn session_name_prefers_devcontainer_label() {
        let p = Path::new("/home/me/proj");
        // The devcontainer's `name` wins and is sanitized for tmux.
        let s = session_name(Some("My App (dev)"), "proj", p);
        assert!(s.starts_with("graft-My-App--dev-"), "got {s}");
        // Falls back to the basename when there's no name.
        assert!(session_name(None, "proj", p).starts_with("graft-proj-"));
        // Same label, different path → different session (no collision).
        assert_ne!(
            session_name(Some("app"), "proj", Path::new("/a/proj")),
            session_name(Some("app"), "proj", Path::new("/b/proj")),
        );
        // An all-punctuation label degrades to a usable placeholder.
        assert!(session_name(Some("***"), "proj", p).starts_with("graft-workspace-"));
    }

    #[test]
    fn forward_ports_parse_all_spec_forms() {
        let values = serde_json::json!([3000, "4000", "3000:8080", "db:5432", "9000/tcp", false]);
        let parsed = parse_forward_ports(Some(values.as_array().unwrap().clone()));
        assert_eq!(
            parsed,
            vec![
                PortForward::same(3000),
                PortForward::same(4000),
                PortForward {
                    local: 3000,
                    host: None,
                    port: 8080
                },
                PortForward {
                    local: 5432,
                    host: Some("db".into()),
                    port: 5432
                },
                PortForward::same(9000),
            ]
        );
    }

    #[test]
    fn mount_to_arg_handles_string_and_object() {
        let p = Path::new("/home/me/proj");
        assert_eq!(
            mount_to_arg(&serde_json::json!("type=bind,source=/a,target=/b"), p).unwrap(),
            "type=bind,source=/a,target=/b"
        );
        let obj = serde_json::json!({"type": "volume", "source": "vol", "target": "/data"});
        assert_eq!(
            mount_to_arg(&obj, p).unwrap(),
            "type=volume,source=vol,target=/data"
        );
    }
}
