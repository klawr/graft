use crate::docker::Docker;
use anyhow::{Context, Result, bail};
use serde::Deserialize;
use serde_json::{Map, Value};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

const FEATURES_DIR: &str = "/tmp/graft-features";
const PROFILE_PATH: &str = "/etc/profile.d/graft-features.sh";

/// A feature as requested in devcontainer.json: an OCI reference plus the
/// user-supplied options object.
#[derive(Clone)]
pub struct FeatureRequest {
    pub reference: String,
    pub options: Map<String, Value>,
}

/// Relevant subset of a feature's `devcontainer-feature.json` metadata.
#[derive(Debug, Default, Deserialize)]
struct FeatureMeta {
    id: String,
    #[serde(default)]
    options: BTreeMap<String, OptionSpec>,
    #[serde(default, rename = "installsAfter")]
    installs_after: Vec<String>,
    #[serde(default, rename = "containerEnv")]
    container_env: BTreeMap<String, String>,
    // Container *creation* settings — applied to the backend at up time
    // (see CreateFlags).
    #[serde(default)]
    mounts: Vec<Value>,
    #[serde(default)]
    privileged: bool,
    #[serde(default, rename = "capAdd")]
    cap_add: Vec<String>,
    #[serde(default, rename = "securityOpt")]
    security_opt: Vec<String>,
    #[serde(default)]
    init: bool,
    // entrypoint would replace the container's command; graft runs `sleep
    // infinity`, so this is warned-and-skipped rather than applied.
    #[serde(default)]
    entrypoint: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct OptionSpec {
    #[serde(default)]
    default: Value,
    #[serde(default, rename = "enum")]
    enum_values: Option<Vec<Value>>,
}

struct Resolved {
    reference: String,
    options: Map<String, Value>,
    meta: FeatureMeta,
    dir: PathBuf, // host dir with install.sh + metadata
}

/// Container-creation settings a feature contributes, aggregated across all
/// features. Applied to the backend at `up` time (compose override / `docker
/// run` flags). `entrypoint` is intentionally not represented — graft runs the
/// container as `sleep infinity`.
#[derive(Default)]
pub struct CreateFlags {
    mounts: Vec<FeatureMount>,
    privileged: bool,
    cap_add: Vec<String>,
    security_opt: Vec<String>,
    init: bool,
}

struct FeatureMount {
    source: String,
    target: String,
    kind: Option<String>,
}

impl CreateFlags {
    pub fn is_empty(&self) -> bool {
        self.mounts.is_empty()
            && !self.privileged
            && self.cap_add.is_empty()
            && self.security_opt.is_empty()
            && !self.init
    }

    /// `docker run` flags (for the image/build backend).
    pub fn run_args(&self) -> Vec<String> {
        let mut a = Vec::new();
        if self.privileged {
            a.push("--privileged".into());
        }
        if self.init {
            a.push("--init".into());
        }
        for c in &self.cap_add {
            a.push("--cap-add".into());
            a.push(c.clone());
        }
        for s in &self.security_opt {
            a.push("--security-opt".into());
            a.push(s.clone());
        }
        for m in &self.mounts {
            a.push("--mount".into());
            a.push(m.to_mount_arg());
        }
        a
    }

    /// A docker-compose override snippet adding these settings to `service`.
    pub fn compose_override(&self, service: &str) -> String {
        let mut s = format!("services:\n  {service}:\n");
        if self.privileged {
            s.push_str("    privileged: true\n");
        }
        if self.init {
            s.push_str("    init: true\n");
        }
        if !self.cap_add.is_empty() {
            s.push_str("    cap_add:\n");
            for c in &self.cap_add {
                s.push_str(&format!("      - {c}\n"));
            }
        }
        if !self.security_opt.is_empty() {
            s.push_str("    security_opt:\n");
            for o in &self.security_opt {
                s.push_str(&format!("      - {o}\n"));
            }
        }
        if !self.mounts.is_empty() {
            s.push_str("    volumes:\n");
            for m in &self.mounts {
                s.push_str(&format!("      - \"{}:{}\"\n", m.source, m.target));
            }
        }
        s
    }
}

impl FeatureMount {
    fn to_mount_arg(&self) -> String {
        let mut parts = Vec::new();
        if let Some(k) = &self.kind {
            parts.push(format!("type={k}"));
        }
        parts.push(format!("source={}", self.source));
        parts.push(format!("target={}", self.target));
        parts.join(",")
    }
}

fn parse_mount(v: &Value) -> Option<FeatureMount> {
    match v {
        // "type=bind,source=/a,target=/b"
        Value::String(s) => {
            let (mut source, mut target, mut kind) = (None, None, None);
            for kv in s.split(',') {
                if let Some((k, val)) = kv.split_once('=') {
                    match k.trim() {
                        "source" | "src" => source = Some(val.to_string()),
                        "target" | "dst" | "destination" => target = Some(val.to_string()),
                        "type" => kind = Some(val.to_string()),
                        _ => {}
                    }
                }
            }
            Some(FeatureMount {
                source: source?,
                target: target?,
                kind,
            })
        }
        Value::Object(o) => Some(FeatureMount {
            source: o.get("source").and_then(|v| v.as_str())?.to_string(),
            target: o.get("target").and_then(|v| v.as_str())?.to_string(),
            kind: o.get("type").and_then(|v| v.as_str()).map(String::from),
        }),
        _ => None,
    }
}

/// A fetched, ordered set of features ready to install. Produced by `resolve`
/// (which does the network/disk work up front, so create-flags are known before
/// the container is brought up) and consumed by `install`.
#[derive(Default)]
pub struct Plan {
    resolved: Vec<Resolved>,
    work: Option<PathBuf>,
}

impl Drop for Plan {
    fn drop(&mut self) {
        if let Some(w) = &self.work {
            let _ = std::fs::remove_dir_all(w);
        }
    }
}

impl Plan {
    /// The container-creation settings these features require.
    pub fn create_flags(&self) -> CreateFlags {
        let mut f = CreateFlags::default();
        for r in &self.resolved {
            let m = &r.meta;
            f.privileged |= m.privileged;
            f.init |= m.init;
            f.cap_add.extend(m.cap_add.iter().cloned());
            f.security_opt.extend(m.security_opt.iter().cloned());
            f.mounts.extend(m.mounts.iter().filter_map(parse_mount));
        }
        f
    }

    /// Runs each feature's `install.sh` (as root, options as env), records a
    /// success marker, and persists merged `containerEnv` for login shells.
    pub fn install(&self, docker: &Docker, container: &str) -> Result<()> {
        if self.resolved.is_empty() {
            return Ok(());
        }
        if docker.mkdir_p(container, &[FEATURES_DIR]).is_err() {
            return Ok(());
        }
        let mut container_env: BTreeMap<String, String> = BTreeMap::new();
        for r in &self.resolved {
            warn_unsupported(r);
            validate_options(r);
            match install_one(docker, container, r) {
                Ok(()) => {
                    for (k, v) in &r.meta.container_env {
                        container_env.insert(k.clone(), v.clone());
                    }
                    let _ = docker.mark_task_done(container, &marker_key(&r.reference));
                }
                Err(e) => eprintln!(
                    "[graft] feature {} install failed (continuing): {e}",
                    r.reference
                ),
            }
        }
        if !container_env.is_empty() {
            write_profile(docker, container, &container_env)?;
        }
        Ok(())
    }
}

/// Fetches and orders the given features (pulling OCI artifacts with `oras`,
/// reading local-path features from disk). Returns an empty plan if there's
/// nothing to do or `oras` is needed but missing.
pub fn resolve(
    base_dir: &Path,
    features: &[FeatureRequest],
    override_order: &[String],
) -> Result<Plan> {
    if features.is_empty() {
        return Ok(Plan::default());
    }
    if features.iter().any(|f| !is_local_ref(&f.reference)) && !oras_available() {
        eprintln!(
            "[graft] warning: devcontainer features need `oras` (https://oras.land); \
             skipping {} feature(s)",
            features.len()
        );
        return Ok(Plan::default());
    }

    let work = std::env::temp_dir().join(format!("graft-features-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&work);

    let mut resolved = Vec::with_capacity(features.len());
    for (i, req) in features.iter().enumerate() {
        println!("[graft] fetching feature {}", req.reference);
        let fetched = if is_local_ref(&req.reference) {
            fetch_local(&req.reference, base_dir)
        } else {
            fetch_oci(&req.reference, &work.join(i.to_string()))
        };
        match fetched {
            Ok((meta, dir)) => resolved.push(Resolved {
                reference: req.reference.clone(),
                options: req.options.clone(),
                meta,
                dir,
            }),
            Err(e) => eprintln!(
                "[graft] feature {} could not be fetched (skipping): {e}",
                req.reference
            ),
        }
    }

    Ok(Plan {
        resolved: order(resolved, override_order),
        work: Some(work),
    })
}

/// Marker-gated install for an already-running container: resolves and installs
/// only the features that haven't succeeded yet (cheap no-op when all are done,
/// retries failures). Create-flags can't be applied here (the container already
/// exists) — use `resolve` + `Plan::create_flags` before `up` for those.
pub fn install(
    docker: &Docker,
    container: &str,
    base_dir: &Path,
    features: &[FeatureRequest],
    override_order: &[String],
) -> Result<()> {
    let todo: Vec<FeatureRequest> = features
        .iter()
        .filter(|f| !docker.task_done(container, &marker_key(&f.reference)))
        .cloned()
        .collect();
    if todo.is_empty() {
        return Ok(());
    }
    resolve(base_dir, &todo, override_order)?.install(docker, container)
}

fn marker_key(reference: &str) -> String {
    format!("feature.{reference}")
}

// Local feature: a relative or absolute path (rather than an OCI reference).
fn is_local_ref(reference: &str) -> bool {
    reference.starts_with("./") || reference.starts_with("../") || reference.starts_with('/')
}

// Reads a feature shipped as a directory on disk (resolved relative to the
// devcontainer dir for relative paths).
fn fetch_local(reference: &str, base_dir: &Path) -> Result<(FeatureMeta, PathBuf)> {
    let dir = base_dir.join(reference);
    read_meta(&dir).map(|meta| (meta, dir))
}

// Pulls + extracts an OCI feature artifact and parses its metadata.
fn fetch_oci(reference: &str, work: &Path) -> Result<(FeatureMeta, PathBuf)> {
    let dir = pull_and_extract(reference, work)?;
    read_meta(&dir).map(|meta| (meta, dir))
}

fn read_meta(dir: &Path) -> Result<FeatureMeta> {
    let path = dir.join("devcontainer-feature.json");
    let raw =
        std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    serde_json::from_str(&raw).with_context(|| format!("parsing {}", path.display()))
}

// Warns (but doesn't fail) when a provided option value isn't one of the
// feature's declared enum values.
fn validate_options(r: &Resolved) {
    for (id, value) in &r.options {
        if let Some(spec) = r.meta.options.get(id)
            && let Some(allowed) = &spec.enum_values
            && !allowed.contains(value)
        {
            eprintln!(
                "[graft] warning: feature {} option {id}={} is not one of its declared values",
                r.reference, value
            );
        }
    }
}

// Copies a feature into the container and runs its install.sh as root.
fn install_one(docker: &Docker, container: &str, r: &Resolved) -> Result<()> {
    let dest = format!("{FEATURES_DIR}/{}", r.meta.id);
    docker.exec_root(container, &["rm", "-rf", &dest])?;
    docker.cp(&r.dir.to_string_lossy(), container, &dest)?;

    println!("[graft] installing feature {}", r.reference);
    let env = build_env(r);
    docker.exec_in(
        container,
        Some(&dest),
        &env,
        &["sh", "-c", "chmod +x ./install.sh && ./install.sh"],
        true,
    )
}

fn oras_available() -> bool {
    Command::new("oras")
        .arg("version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

// `oras pull` drops the feature's tarball layer into `work`; extract it and
// return the directory holding install.sh + devcontainer-feature.json.
fn pull_and_extract(reference: &str, work: &Path) -> Result<PathBuf> {
    std::fs::create_dir_all(work)?;
    let mut cmd = Command::new("oras");
    cmd.args(["pull", reference]).current_dir(work);
    crate::verbose::trace(&cmd);
    let status = cmd.status().context("running oras pull")?;
    if !status.success() {
        bail!("oras pull failed for feature {reference}");
    }

    let layer = std::fs::read_dir(work)?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .find(|p| p.is_file())
        .with_context(|| format!("oras pull produced no artifact for {reference}"))?;

    let content = work.join("content");
    std::fs::create_dir_all(&content)?;
    let mut cmd = Command::new("tar");
    cmd.arg("xf").arg(&layer).arg("-C").arg(&content);
    crate::verbose::trace(&cmd);
    let status = cmd.status().context("extracting feature tarball")?;
    if !status.success() {
        bail!("failed to extract feature {reference}");
    }
    Ok(content)
}

fn warn_unsupported(r: &Resolved) {
    if r.meta.entrypoint.is_some() {
        eprintln!(
            "[graft] warning: feature {} declares an entrypoint, which graft can't apply \
             (the container runs `sleep infinity`); ignoring it",
            r.reference
        );
    }
}

// Builds the env for install.sh: each declared option as an uppercased var
// (user value or its default), plus the _REMOTE_USER/_CONTAINER_USER variables
// scripts commonly read. graft runs as root.
fn build_env(r: &Resolved) -> Vec<(String, String)> {
    let mut env = Vec::new();
    for (id, spec) in &r.meta.options {
        let value = r.options.get(id).unwrap_or(&spec.default);
        env.push((env_name(id), stringify(value)));
    }
    for (k, v) in [
        ("_REMOTE_USER", "root"),
        ("_REMOTE_USER_HOME", "/root"),
        ("_CONTAINER_USER", "root"),
        ("_CONTAINER_USER_HOME", "/root"),
    ] {
        env.push((k.to_string(), v.to_string()));
    }
    env
}

fn env_name(id: &str) -> String {
    id.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_uppercase()
            } else {
                '_'
            }
        })
        .collect()
}

fn stringify(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n.to_string(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

// Persists feature containerEnv so login shells (graft enters with `-l`) see it.
// Values may reference other vars (e.g. PATH=".../bin:${PATH}"), so they're left
// unquoted for the shell to expand.
fn write_profile(docker: &Docker, container: &str, env: &BTreeMap<String, String>) -> Result<()> {
    let mut script = String::from("# generated by graft from devcontainer feature containerEnv\n");
    for (k, v) in env {
        script.push_str(&format!("export {k}={v}\n"));
    }
    let tmp = std::env::temp_dir().join(format!("graft-profile-{}", std::process::id()));
    std::fs::write(&tmp, script)?;
    let res = docker.cp(&tmp.to_string_lossy(), container, PROFILE_PATH);
    let _ = std::fs::remove_file(&tmp);
    res
}

// Orders features so that any `installsAfter` dependency that is *also* in this
// set comes first. Ties break by overrideFeatureInstallOrder, then reference —
// a deterministic topological sort (n is tiny, so an O(n²) scan is fine).
fn order(resolved: Vec<Resolved>, override_order: &[String]) -> Vec<Resolved> {
    let n = resolved.len();
    let rank = |r: &Resolved| {
        override_order
            .iter()
            .position(|o| matches_ref(o, r))
            .unwrap_or(usize::MAX)
    };

    // Stable base order: override rank, then reference.
    let mut idx: Vec<usize> = (0..n).collect();
    idx.sort_by(|&a, &b| {
        rank(&resolved[a])
            .cmp(&rank(&resolved[b]))
            .then(resolved[a].reference.cmp(&resolved[b].reference))
    });

    // deps[i] = indices that must precede i (its installsAfter present in the set).
    let mut deps: Vec<Vec<usize>> = vec![Vec::new(); n];
    for (i, ri) in resolved.iter().enumerate() {
        for dep in &ri.meta.installs_after {
            for (j, rj) in resolved.iter().enumerate() {
                if j != i && matches_ref(dep, rj) {
                    deps[i].push(j);
                }
            }
        }
    }

    let mut done = vec![false; n];
    let mut out: Vec<usize> = Vec::with_capacity(n);
    for _ in 0..n {
        // pick the first (by base order) node whose deps are all satisfied;
        // on a cycle, fall back to the first remaining node.
        let pick = idx
            .iter()
            .copied()
            .find(|&i| !done[i] && deps[i].iter().all(|&d| done[d]))
            .or_else(|| idx.iter().copied().find(|&i| !done[i]));
        if let Some(i) = pick {
            done[i] = true;
            out.push(i);
        }
    }

    let mut slots: Vec<Option<Resolved>> = resolved.into_iter().map(Some).collect();
    out.into_iter().map(|i| slots[i].take().unwrap()).collect()
}

// Matches an installsAfter / override entry against a resolved feature, by full
// reference, tag-stripped reference, or the feature's id (last path segment).
fn matches_ref(target: &str, r: &Resolved) -> bool {
    let strip_tag = |s: &str| s.rsplit_once(':').map(|(a, _)| a).unwrap_or(s).to_string();
    let t = strip_tag(target);
    if t == strip_tag(&r.reference) {
        return true;
    }
    t.rsplit('/').next() == Some(r.meta.id.as_str())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn resolved(reference: &str, id: &str, installs_after: &[&str]) -> Resolved {
        Resolved {
            reference: reference.to_string(),
            options: Map::new(),
            meta: FeatureMeta {
                id: id.to_string(),
                installs_after: installs_after.iter().map(|s| s.to_string()).collect(),
                ..Default::default()
            },
            dir: PathBuf::new(),
        }
    }

    #[test]
    fn env_name_uppercases_and_sanitizes() {
        assert_eq!(env_name("nodeGypDependencies"), "NODEGYPDEPENDENCIES");
        assert_eq!(env_name("install-yarn"), "INSTALL_YARN");
        assert_eq!(env_name("a.b"), "A_B");
    }

    #[test]
    fn stringify_each_kind() {
        assert_eq!(stringify(&json!("lts")), "lts"); // no surrounding quotes
        assert_eq!(stringify(&json!(true)), "true");
        assert_eq!(stringify(&json!(20)), "20");
        assert_eq!(stringify(&json!(null)), "");
    }

    #[test]
    fn distinguishes_local_refs() {
        assert!(is_local_ref("./my-feature"));
        assert!(is_local_ref("../shared/feat"));
        assert!(is_local_ref("/abs/feat"));
        assert!(!is_local_ref("ghcr.io/devcontainers/features/node:1"));
    }

    #[test]
    fn matches_ref_by_ref_tag_and_id() {
        let r = resolved("ghcr.io/devcontainers/features/node:1", "node", &[]);
        assert!(matches_ref("ghcr.io/devcontainers/features/node:1", &r));
        assert!(matches_ref("ghcr.io/devcontainers/features/node", &r)); // tag stripped
        assert!(matches_ref("node", &r)); // by id
        assert!(!matches_ref("ghcr.io/devcontainers/features/go", &r));
    }

    #[test]
    fn order_puts_installs_after_dependency_first() {
        // node installsAfter common-utils; input deliberately in the wrong order.
        let v = vec![
            resolved("ghcr.io/x/node:1", "node", &["ghcr.io/x/common-utils"]),
            resolved("ghcr.io/x/common-utils:1", "common-utils", &[]),
        ];
        let ids: Vec<_> = order(v, &[]).into_iter().map(|r| r.meta.id).collect();
        assert_eq!(ids, vec!["common-utils", "node"]);
    }

    #[test]
    fn order_respects_override_then_reference() {
        let v = vec![
            resolved("ghcr.io/x/a:1", "a", &[]),
            resolved("ghcr.io/x/b:1", "b", &[]),
        ];
        let ids: Vec<_> = order(v, &["ghcr.io/x/b".to_string()])
            .into_iter()
            .map(|r| r.meta.id)
            .collect();
        assert_eq!(ids, vec!["b", "a"]); // b promoted by override order
    }

    #[test]
    fn parse_mount_string_and_object() {
        let m = parse_mount(&json!("type=bind,source=/a,target=/b")).unwrap();
        assert_eq!(
            (m.source.as_str(), m.target.as_str(), m.kind.as_deref()),
            ("/a", "/b", Some("bind"))
        );
        let m = parse_mount(&json!({"source": "vol", "target": "/d"})).unwrap();
        assert_eq!(
            (m.source.as_str(), m.target.as_str(), m.kind),
            ("vol", "/d", None)
        );
        assert!(parse_mount(&json!("source=/a")).is_none()); // missing target
    }

    #[test]
    fn create_flags_render_run_args_and_compose() {
        let flags = CreateFlags {
            mounts: vec![FeatureMount {
                source: "v".into(),
                target: "/d".into(),
                kind: Some("volume".into()),
            }],
            privileged: true,
            cap_add: vec!["SYS_PTRACE".into()],
            security_opt: vec![],
            init: true,
        };
        let args = flags.run_args();
        assert!(args.contains(&"--privileged".to_string()));
        assert!(args.contains(&"--init".to_string()));
        assert!(
            args.contains(&"--cap-add".to_string()) && args.contains(&"SYS_PTRACE".to_string())
        );
        assert!(args.iter().any(|a| a == "type=volume,source=v,target=/d"));

        let ov = flags.compose_override("dev");
        assert!(ov.starts_with("services:\n  dev:\n"));
        assert!(ov.contains("privileged: true"));
        assert!(ov.contains("init: true"));
        assert!(ov.contains("- SYS_PTRACE"));
        assert!(ov.contains("\"v:/d\""));
    }

    #[test]
    fn create_flags_empty_is_empty() {
        assert!(CreateFlags::default().is_empty());
    }

    #[test]
    fn order_does_not_hang_on_cycle() {
        let v = vec![
            resolved("ghcr.io/x/a:1", "a", &["ghcr.io/x/b"]),
            resolved("ghcr.io/x/b:1", "b", &["ghcr.io/x/a"]),
        ];
        assert_eq!(order(v, &[]).len(), 2); // both emitted, no panic/hang
    }
}
