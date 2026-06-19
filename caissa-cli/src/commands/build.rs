use std::path::PathBuf;
use caissa_core::{agent, config::load_config};

const AGENT_DOCKERFILE: &str = include_str!("../../../sandbox/Dockerfile.agent");
const AGENT_ENTRYPOINT: &str = include_str!("../../../sandbox/entrypoint.sh");

pub async fn run(generation: &str, fondament_path: Option<&str>) -> anyhow::Result<()> {
    let config = load_config()?;
    let fondament = fondament_path
        .unwrap_or(&config.fondament_path)
        .to_string();

    eprintln!("[caissa] loading definition: fondament/{}", generation);
    let def = agent::load_definition(&fondament, generation)?;
    let claude_md = agent::assemble_claude_md(&def);

    // Build context under target/ so it is gitignored and reproducible.
    let build_dir = PathBuf::from("target")
        .join("build-contexts")
        .join(generation);
    std::fs::create_dir_all(&build_dir)?;

    std::fs::write(build_dir.join("claude.md"), &claude_md)?;
    std::fs::write(build_dir.join("Dockerfile"), AGENT_DOCKERFILE)?;
    std::fs::write(build_dir.join("entrypoint.sh"), AGENT_ENTRYPOINT)?;

    let tag = format!("caissa-sandbox:{}", generation);
    eprintln!("[caissa] building image: {}", tag);

    let status = tokio::process::Command::new("docker")
        .args(["build", "-t", &tag, build_dir.to_str().unwrap()])
        .status()
        .await?;

    if !status.success() {
        anyhow::bail!("docker build failed for {}", tag);
    }

    eprintln!("[caissa] built: {}", tag);
    Ok(())
}
