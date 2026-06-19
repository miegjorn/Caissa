use caissa_core::config::SandboxConfig;

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

pub async fn run(user_args: &[String]) -> anyhow::Result<()> {
    let config = SandboxConfig::default();
    let docker_args = build_docker_args(&config, user_args);

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
}
