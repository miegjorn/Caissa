use std::collections::HashMap;
use std::path::{Path, PathBuf};
use caissa_core::config::{load_config, SandboxConfig};
use caissa_core::pii::{PiiProxy, RegexPiiProxy};

/// Build the `docker run` argument list from a SandboxConfig + optional workspace mount + user args.
/// Extracted so it can be tested without spawning a real process.
pub fn build_docker_args(
    config: &SandboxConfig,
    workspace_path: Option<&Path>,
    user_args: &[String],
) -> Vec<String> {
    let mut args = vec![
        "run".into(),
        "--rm".into(),
        "--network".into(),
        config.network.clone(),
        "-m".into(),
        config.memory_limit.clone(),
    ];
    if let Some(ref cpus) = config.cpu_limit {
        args.push("--cpus".into());
        args.push(cpus.clone());
    }
    if let Some(ws) = workspace_path {
        // Mount the per-session workspace directory at /workspace inside the container.
        // This gives each session a persistent, scoped filesystem without cross-contamination.
        let mount = format!("{}:/workspace", ws.display());
        args.push("-v".into());
        args.push(mount);
    }
    args.push(config.image.clone());
    args.extend_from_slice(user_args);
    args
}

/// Redact PII from each arg using the proxy. Returns (redacted_args, merged_vault).
pub fn redact_args(proxy: &dyn PiiProxy, user_args: &[String]) -> (Vec<String>, HashMap<String, String>) {
    let mut redacted = Vec::with_capacity(user_args.len());
    let mut vault = HashMap::new();
    for arg in user_args {
        let (r, v) = proxy.redact(arg);
        redacted.push(r);
        vault.extend(v);
    }
    (redacted, vault)
}

/// Resolve the workspace path for a given session id, creating the directory if needed.
/// Returns the absolute path so Docker's -v flag can reference it.
pub fn resolve_workspace(workspaces_dir: &str, session_id: &str) -> anyhow::Result<PathBuf> {
    // Sanitize session_id: replace characters that are invalid in directory names
    let safe_id: String = session_id
        .chars()
        .map(|c| if c.is_alphanumeric() || c == '-' || c == '_' || c == '.' { c } else { '_' })
        .collect();

    let dir = PathBuf::from(workspaces_dir).join(&safe_id);
    let abs = dir.canonicalize().unwrap_or_else(|_| {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(&dir)
    });

    std::fs::create_dir_all(&abs)
        .map_err(|e| anyhow::anyhow!("failed to create workspace {}: {}", abs.display(), e))?;

    // canonicalize again after creation
    let abs = abs.canonicalize()
        .unwrap_or(abs);

    Ok(abs)
}

pub async fn run(session_id: Option<&str>, user_args: &[String]) -> anyhow::Result<()> {
    let config = load_config()?;
    let sandbox = SandboxConfig::default();

    let proxy = RegexPiiProxy::new(&config.pii_patterns)
        .map_err(|e| anyhow::anyhow!("PII proxy init failed: {}", e))?;

    let (redacted_args, vault) = redact_args(&proxy, user_args);

    if !vault.is_empty() {
        eprintln!("[caissa] PII redacted before sandbox invocation:");
        for (placeholder, original) in &vault {
            eprintln!("  {} → {}", placeholder, original);
        }
    }

    let workspace_path = session_id
        .map(|id| resolve_workspace(&config.workspaces_dir, id))
        .transpose()?;

    if let Some(ref ws) = workspace_path {
        eprintln!("[caissa] workspace: {}", ws.display());
    }

    let docker_args = build_docker_args(&sandbox, workspace_path.as_deref(), &redacted_args);

    let status = tokio::process::Command::new("docker")
        .args(&docker_args)
        .status()
        .await?;

    if !status.success() {
        anyhow::bail!("sandbox exited with status: {}", status);
    }
    Ok(())
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_docker_args_includes_network_and_memory() {
        let config = SandboxConfig::default();
        let args = build_docker_args(&config, None, &[]);
        assert!(args.contains(&"--network".to_string()));
        assert!(args.contains(&"none".to_string()), "default network should be none");
        assert!(args.contains(&"-m".to_string()));
        assert!(args.contains(&"2g".to_string()), "default memory should be 2g");
    }

    #[test]
    fn build_docker_args_includes_image() {
        let config = SandboxConfig::default();
        let args = build_docker_args(&config, None, &[]);
        assert!(
            args.contains(&"caissa-sandbox:latest".to_string()),
            "image must appear in args"
        );
    }

    #[test]
    fn build_docker_args_appends_user_args() {
        let config = SandboxConfig::default();
        let user_args = vec!["echo".to_string(), "hello".to_string()];
        let args = build_docker_args(&config, None, &user_args);
        let last_two: Vec<_> = args.iter().rev().take(2).rev().cloned().collect();
        assert_eq!(last_two, vec!["echo", "hello"]);
    }

    #[test]
    fn build_docker_args_includes_cpu_limit_when_set() {
        let config = SandboxConfig {
            cpu_limit: Some("1.5".into()),
            ..SandboxConfig::default()
        };
        let args = build_docker_args(&config, None, &[]);
        assert!(args.contains(&"--cpus".to_string()));
        assert!(args.contains(&"1.5".to_string()));
    }

    #[test]
    fn build_docker_args_omits_cpu_flag_when_none() {
        let config = SandboxConfig::default(); // cpu_limit = None
        let args = build_docker_args(&config, None, &[]);
        assert!(!args.contains(&"--cpus".to_string()));
    }

    #[test]
    fn build_docker_args_mounts_workspace_when_provided() {
        let config = SandboxConfig::default();
        let ws = Path::new("/tmp/workspaces/my-session");
        let args = build_docker_args(&config, Some(ws), &[]);
        assert!(args.contains(&"-v".to_string()), "-v flag must appear");
        let v_idx = args.iter().position(|a| a == "-v").unwrap();
        assert_eq!(
            args[v_idx + 1],
            "/tmp/workspaces/my-session:/workspace",
            "workspace must be mounted at /workspace"
        );
    }

    #[test]
    fn build_docker_args_no_volume_without_session() {
        let config = SandboxConfig::default();
        let args = build_docker_args(&config, None, &[]);
        assert!(!args.contains(&"-v".to_string()), "no -v without a session");
    }

    #[test]
    fn redact_args_removes_email_from_args() {
        let proxy = RegexPiiProxy::new(&["email".to_string()]).unwrap();
        let args = vec![
            "run".to_string(),
            "contact user@example.com".to_string(),
        ];
        let (redacted, vault) = redact_args(&proxy, &args);
        assert!(!redacted[1].contains("user@example.com"), "email must be redacted");
        assert!(vault.values().any(|v| v == "user@example.com"), "vault must hold original");
    }

    #[test]
    fn redact_args_clean_input_unchanged() {
        let proxy = RegexPiiProxy::new(&["email".to_string(), "phone".to_string()]).unwrap();
        let args = vec!["echo".to_string(), "hello world".to_string()];
        let (redacted, vault) = redact_args(&proxy, &args);
        assert_eq!(redacted, args);
        assert!(vault.is_empty());
    }

    #[test]
    fn resolve_workspace_sanitizes_session_id() {
        let tmp = std::env::temp_dir().join("caissa_test_ws");
        let _ = std::fs::remove_dir_all(&tmp);
        let path = resolve_workspace(tmp.to_str().unwrap(), "!room:example.com#99")
            .expect("resolve must succeed");
        let name = path.file_name().unwrap().to_str().unwrap();
        assert!(!name.contains('!'), "! must be sanitized");
        assert!(!name.contains(':'), ": must be sanitized");
        assert!(!name.contains('#'), "# must be sanitized");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn resolve_workspace_creates_directory() {
        let tmp = std::env::temp_dir().join("caissa_test_ws2");
        let _ = std::fs::remove_dir_all(&tmp);
        let path = resolve_workspace(tmp.to_str().unwrap(), "my-session")
            .expect("resolve must succeed");
        assert!(path.exists(), "workspace dir must be created");
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
