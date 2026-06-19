use std::collections::HashMap;
use caissa_core::config::{load_config, SandboxConfig};
use caissa_core::pii::{PiiProxy, RegexPiiProxy};

/// Build the `docker run` argument list from a SandboxConfig + user args.
/// Extracted so it can be tested without spawning a real process.
pub fn build_docker_args(config: &SandboxConfig, user_args: &[String]) -> Vec<String> {
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

pub async fn run(user_args: &[String]) -> anyhow::Result<()> {
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

    let docker_args = build_docker_args(&sandbox, &redacted_args);

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
        let args = build_docker_args(&config, &[]);
        assert!(args.contains(&"--network".to_string()));
        assert!(args.contains(&"none".to_string()), "default network should be none");
        assert!(args.contains(&"-m".to_string()));
        assert!(args.contains(&"2g".to_string()), "default memory should be 2g");
    }

    #[test]
    fn build_docker_args_includes_image() {
        let config = SandboxConfig::default();
        let args = build_docker_args(&config, &[]);
        assert!(
            args.contains(&"caissa-sandbox:latest".to_string()),
            "image must appear in args"
        );
    }

    #[test]
    fn build_docker_args_appends_user_args() {
        let config = SandboxConfig::default();
        let user_args = vec!["echo".to_string(), "hello".to_string()];
        let args = build_docker_args(&config, &user_args);
        let last_two: Vec<_> = args.iter().rev().take(2).rev().cloned().collect();
        assert_eq!(last_two, vec!["echo", "hello"]);
    }

    #[test]
    fn build_docker_args_includes_cpu_limit_when_set() {
        let config = SandboxConfig {
            cpu_limit: Some("1.5".into()),
            ..SandboxConfig::default()
        };
        let args = build_docker_args(&config, &[]);
        assert!(args.contains(&"--cpus".to_string()));
        assert!(args.contains(&"1.5".to_string()));
    }

    #[test]
    fn build_docker_args_omits_cpu_flag_when_none() {
        let config = SandboxConfig::default(); // cpu_limit = None
        let args = build_docker_args(&config, &[]);
        assert!(!args.contains(&"--cpus".to_string()));
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
}
