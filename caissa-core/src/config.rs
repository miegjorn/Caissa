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
    /// Container registry prefix for `caissa push` (e.g. "ghcr.io/miegjorn").
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
    /// Charradissa Matrix MCP endpoint — exposes matrix_send, matrix_invite, matrix_kick,
    /// matrix_get_dm so agents can act in Matrix without holding their own Matrix token.
    #[serde(default = "default_charradissa_mcp_url")]
    pub charradissa_mcp_url: String,
    /// Nervi NATS JetStream MCP endpoint — exposes nervi_publish and nervi_subscribe.
    #[serde(default = "default_nervi_mcp_url")]
    pub nervi_mcp_url: String,
    /// Model used for non-interactive chronicle runs (`caissa listen`).
    /// Defaults to Haiku — fast, cheap, right for routine observation.
    /// Override per-deployment: chronicle_model = "claude-sonnet-4-6"
    #[serde(default = "default_chronicle_model")]
    pub chronicle_model: String,
    /// Model used for interactive Matrix reply runs (`caissa listen /matrix/reply`).
    /// Defaults to Sonnet — interactive sessions need quality over speed.
    #[serde(default = "default_matrix_model")]
    pub matrix_model: String,
    /// Model used for the nightly dream consolidation run.
    /// Dream requires sophisticated synthesis — defaults to Sonnet.
    #[serde(default = "default_dream_model")]
    pub dream_model: String,
    /// Amassada event bus URL. When set, matrix reply events are published there
    /// so WebSocket subscribers get cross-session visibility.
    #[serde(default = "default_amassada_url")]
    pub amassada_url: String,
    /// SRE watchdog: how long to wait for a /health response before counting it as a failure.
    #[serde(default = "default_sre_health_timeout_secs")]
    pub sre_health_timeout_secs: u64,
    /// SRE watchdog: how often to run a full probe sweep (seconds).
    #[serde(default = "default_sre_watchdog_interval_secs")]
    pub sre_watchdog_interval_secs: u64,
    /// SRE watchdog: maximum expected age of the last Farga chronicle signal (hours).
    /// Used to detect a stalled chronicle cron. Requires Farga to return signal timestamps
    /// (not yet implemented — field is reserved, check is currently skipped).
    #[serde(default = "default_sre_chronicle_max_age_hours")]
    pub sre_chronicle_max_age_hours: u64,
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

fn default_charradissa_mcp_url() -> String {
    "http://charradissa.occitan-system.svc.cluster.local:8448/mcp".into()
}

fn default_nervi_mcp_url() -> String {
    "http://nervi-mcp.occitan-system.svc.cluster.local:8080/mcp".into()
}

fn default_chronicle_model() -> String {
    "claude-haiku-4-5-20251001".into()
}

fn default_matrix_model() -> String {
    "claude-sonnet-4-6".into()
}

fn default_dream_model() -> String {
    "claude-sonnet-4-6".to_string()
}

fn default_amassada_url() -> String {
    "http://amassada.occitan-system.svc.cluster.local:7700".into()
}

fn default_sre_health_timeout_secs() -> u64 { 10 }
fn default_sre_watchdog_interval_secs() -> u64 { 300 }
fn default_sre_chronicle_max_age_hours() -> u64 { 7 }

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
            charradissa_mcp_url: default_charradissa_mcp_url(),
            nervi_mcp_url: default_nervi_mcp_url(),
            chronicle_model: default_chronicle_model(),
            matrix_model: default_matrix_model(),
            dream_model: default_dream_model(),
            amassada_url: default_amassada_url(),
            sre_health_timeout_secs: default_sre_health_timeout_secs(),
            sre_watchdog_interval_secs: default_sre_watchdog_interval_secs(),
            sre_chronicle_max_age_hours: default_sre_chronicle_max_age_hours(),
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

    let mut config = CaissaConfig::default();
    for path in &candidates {
        if path.exists() {
            let text = std::fs::read_to_string(path)
                .map_err(|e| anyhow::anyhow!("reading {}: {}", path.display(), e))?;
            let file: ConfigFile = toml::from_str(&text)
                .map_err(|e| anyhow::anyhow!("parsing {}: {}", path.display(), e))?;
            config = file.caissa;
            break;
        }
    }
    // Env overrides — useful for k8s deployments where the toml is a ConfigMap
    // but per-pod values (farga project, model) come from env.
    if let Ok(v) = std::env::var("FARGA_URL") {
        // When FARGA_URL is overridden, derive farga_mcp_url from it unless
        // FARGA_MCP_URL is explicitly set or the config file already specified one.
        let mcp_derived = format!("{}/mcp", v.trim_end_matches('/'));
        if config.farga_mcp_url == default_farga_mcp_url() {
            config.farga_mcp_url = mcp_derived;
        }
        config.farga_url = v;
    }
    if let Ok(v) = std::env::var("FARGA_MCP_URL")      { config.farga_mcp_url = v; }
    if let Ok(v) = std::env::var("FARGA_PROJECT")      { config.project = v; }
    if let Ok(v) = std::env::var("CHRONICLE_MODEL")    { config.chronicle_model = v; }
    if let Ok(v) = std::env::var("MATRIX_MODEL")       { config.matrix_model = v; }
    if let Ok(v) = std::env::var("AMASSADA_URL")       { config.amassada_url = v; }
    if let Ok(v) = std::env::var("DREAM_MODEL")        { config.dream_model = v; }
    Ok(config)
}

fn dirs_config_path() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    PathBuf::from(home).join(".config").join("caissa").join("caissa.toml")
}
