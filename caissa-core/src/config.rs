use std::path::PathBuf;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct CaissaConfig {
    pub farga_url: String,
    pub project: String,
    /// Named patterns to activate: "email", "phone", "ssn", "credit_card"
    pub pii_patterns: Vec<String>,
    /// Base directory for per-session workspace volumes. Default: "./workspaces"
    pub workspaces_dir: String,
    /// Path to the Fondament repo root. Used by `caissa build` to resolve agent definitions.
    #[serde(default = "default_fondament_path")]
    pub fondament_path: String,
    /// Container registry prefix for `caissa push` (e.g. "ghcr.io/occitan").
    /// When absent, the local tag is pushed as-is.
    #[serde(default)]
    pub registry: Option<String>,
    /// Current generation name — the image tag used when spawning domain/facet agents.
    /// Defaults to "guilhem".
    #[serde(default = "default_generation")]
    pub generation: String,
    /// Farga MCP endpoint injected into the agent container at spawn time.
    /// Defaults to the cluster-internal DNS name (works in kind and EKS/AKS).
    #[serde(default = "default_farga_mcp_url")]
    pub farga_mcp_url: String,
    /// Dispatcher MCP endpoint injected into the agent container at spawn time.
    #[serde(default = "default_dispatcher_mcp_url")]
    pub dispatcher_mcp_url: String,
}

fn default_fondament_path() -> String {
    "../Fondament".into()
}

fn default_generation() -> String {
    "guilhem".into()
}

fn default_farga_mcp_url() -> String {
    "http://farga.occitan-system.svc.cluster.local:7500/mcp".into()
}

fn default_dispatcher_mcp_url() -> String {
    "http://dispatcher.agents.svc.cluster.local:9090/mcp".into()
}

impl Default for CaissaConfig {
    fn default() -> Self {
        Self {
            farga_url: "http://localhost:7500".into(),
            project: "default".into(),
            pii_patterns: vec!["email".into(), "phone".into()],
            workspaces_dir: "workspaces".into(),
            fondament_path: default_fondament_path(),
            registry: None,
            generation: default_generation(),
            farga_mcp_url: default_farga_mcp_url(),
            dispatcher_mcp_url: default_dispatcher_mcp_url(),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SandboxConfig {
    pub image: String,
    pub network: String,
    pub memory_limit: String,
    pub cpu_limit: Option<String>,
}

impl Default for SandboxConfig {
    fn default() -> Self {
        Self {
            image: "caissa-sandbox:latest".into(),
            network: "none".into(),
            memory_limit: "2g".into(),
            cpu_limit: None,
        }
    }
}

/// On-disk format: caissa.toml has a [caissa] table and an optional [sandbox] table.
#[derive(Debug, Deserialize)]
struct ConfigFile {
    caissa: CaissaConfig,
    #[allow(dead_code)]
    sandbox: Option<SandboxConfig>,
}

/// Load CaissaConfig from the first file found:
///   1. ./caissa.toml
///   2. ~/.config/caissa/caissa.toml
/// Falls back to CaissaConfig::default() if neither exists.
pub fn load_config() -> anyhow::Result<CaissaConfig> {
    let candidates: Vec<PathBuf> = vec![
        PathBuf::from("caissa.toml"),
        dirs_config_path(),
    ];

    for path in &candidates {
        if path.exists() {
            let text = std::fs::read_to_string(path)
                .map_err(|e| anyhow::anyhow!("reading {}: {}", path.display(), e))?;
            let file: ConfigFile = toml::from_str(&text)
                .map_err(|e| anyhow::anyhow!("parsing {}: {}", path.display(), e))?;
            return Ok(file.caissa);
        }
    }

    Ok(CaissaConfig::default())
}

fn dirs_config_path() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    PathBuf::from(home).join(".config").join("caissa").join("caissa.toml")
}
