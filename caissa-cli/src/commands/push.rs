use caissa_core::config::load_config;

pub async fn run(generation: &str, registry: Option<&str>) -> anyhow::Result<()> {
    let config = load_config()?;
    let local_tag = format!("caissa-sandbox:{}", generation);

    let reg = registry
        .map(|s| s.to_string())
        .or(config.registry)
        .unwrap_or_default();

    if reg.is_empty() {
        eprintln!("[caissa] pushing: {}", local_tag);
        let status = tokio::process::Command::new("docker")
            .args(["push", &local_tag])
            .status()
            .await?;
        if !status.success() {
            anyhow::bail!("docker push failed for {}", local_tag);
        }
    } else {
        let remote_tag = format!("{}/caissa-sandbox:{}", reg, generation);

        eprintln!("[caissa] tagging {} -> {}", local_tag, remote_tag);
        let status = tokio::process::Command::new("docker")
            .args(["tag", &local_tag, &remote_tag])
            .status()
            .await?;
        if !status.success() {
            anyhow::bail!("docker tag failed");
        }

        eprintln!("[caissa] pushing: {}", remote_tag);
        let status = tokio::process::Command::new("docker")
            .args(["push", &remote_tag])
            .status()
            .await?;
        if !status.success() {
            anyhow::bail!("docker push failed for {}", remote_tag);
        }
        eprintln!("[caissa] pushed: {}", remote_tag);
    }

    Ok(())
}
