use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;

#[derive(Debug, Deserialize, Serialize)]
pub struct Config {
    #[serde(default)]
    pub inject: Vec<Injectable>,
    #[serde(default)]
    pub session: SessionConfig,
    #[serde(default)]
    pub git: GitConfig,
    #[serde(default)]
    pub aliases: HashMap<String, String>,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct Injectable {
    pub name: String,
    /// Local path to the binary to copy in
    pub binary: Option<PathBuf>,
    /// Local config directory/file to copy in
    pub config: Option<PathBuf>,
    /// Where to place the binary inside the container
    pub target_binary: Option<String>,
    /// Where to place the config inside the container
    pub target_config: Option<String>,
    /// If true, skip injecting the binary/config when it already exists in the
    /// container. Set false to refresh it on every graft (e.g. an evolving
    /// editor config); leave true for large stable trees (plugins, runtime).
    #[serde(default = "default_true")]
    pub skip_if_exists: bool,
    /// Copy shared library dependencies discovered via ldd (skips libs already present)
    #[serde(default = "default_true")]
    pub copy_deps: bool,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct SessionConfig {
    /// Shell to launch inside the container
    #[serde(default = "default_shell")]
    pub shell: String,
    /// Terminal multiplexer to wrap the session in: "tmux" or "none"
    /// ("none" execs straight into the container with no multiplexer).
    #[serde(default = "default_multiplexer")]
    pub multiplexer: String,
}

impl Default for SessionConfig {
    fn default() -> Self {
        Self {
            shell: default_shell(),
            multiplexer: default_multiplexer(),
        }
    }
}

fn default_shell() -> String {
    "/bin/bash".into()
}

fn default_multiplexer() -> String {
    "tmux".into()
}

#[derive(Debug, Deserialize, Serialize)]
pub struct GitConfig {
    /// Directories to register as git `safe.directory` inside the container.
    /// Injected plugins and the mounted workspace are owned by the host user but
    /// git runs as root in the container, so without this git refuses them with
    /// "detected dubious ownership" and plugins/repos fail to load. Defaults to
    /// `["*"]` (trust all); set to `[]` to disable.
    #[serde(default = "default_safe_directories")]
    pub safe_directories: Vec<String>,
}

impl Default for GitConfig {
    fn default() -> Self {
        Self {
            safe_directories: default_safe_directories(),
        }
    }
}

fn default_safe_directories() -> Vec<String> {
    vec!["*".to_string()]
}

fn default_true() -> bool {
    true
}

impl Config {
    pub fn load() -> Result<Self> {
        let path = config_path();
        if !path.exists() {
            return Ok(Self::default());
        }
        let raw = std::fs::read_to_string(&path)
            .with_context(|| format!("reading config at {}", path.display()))?;
        toml::from_str(&raw).context("parsing graft config")
    }

    fn default() -> Self {
        Self {
            inject: vec![],
            session: SessionConfig::default(),
            git: GitConfig::default(),
            aliases: HashMap::new(),
        }
    }
}

pub fn config_path() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("~/.config"))
        .join("graft")
        .join("config.toml")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_config_uses_defaults() {
        let c: Config = toml::from_str("").unwrap();
        assert!(c.inject.is_empty());
        assert_eq!(c.session.shell, "/bin/bash");
        assert_eq!(c.session.multiplexer, "tmux");
        assert_eq!(c.git.safe_directories, vec!["*".to_string()]);
        assert!(c.aliases.is_empty());
    }

    #[test]
    fn parses_full_config() {
        let toml = r#"
            [[inject]]
            name = "nvim"
            binary = "/usr/bin/nvim"
            skip_if_exists = false

            [session]
            shell = "/bin/zsh"
            multiplexer = "none"

            [git]
            safe_directories = ["/work"]

            [aliases]
            ll = "ls -la"
        "#;
        let c: Config = toml::from_str(toml).unwrap();
        assert_eq!(c.inject.len(), 1);
        assert_eq!(c.inject[0].name, "nvim");
        assert!(!c.inject[0].skip_if_exists);
        assert!(c.inject[0].copy_deps); // defaults to true
        assert_eq!(c.session.shell, "/bin/zsh");
        assert_eq!(c.session.multiplexer, "none");
        assert_eq!(c.git.safe_directories, vec!["/work".to_string()]);
        assert_eq!(c.aliases.get("ll").map(String::as_str), Some("ls -la"));
    }
}
