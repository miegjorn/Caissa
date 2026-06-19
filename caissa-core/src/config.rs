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
}

impl Default for CaissaConfig {
    fn default() -> Self {
        Self {
            farga_url: "http://localhost:7500".into(),
            project: "default".into(),
            pii_patterns: vec!["email".into(), "phone".into()],
            workspaces_dir: "workspaces".into(),
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
            let cfg: CaissaConfig = toml::from_str(&text)
                .map_err(|e| anyhow::anyhow!("parsing {}: {}", path.display(), e))?;
            return Ok(cfg);
        }
    }

    Ok(CaissaConfig::default())
}

fn dirs_config_path() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    PathBuf::from(home).join(".config").join("caissa").join("caissa.toml")
}
