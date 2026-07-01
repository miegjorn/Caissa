use std::path::{Path, PathBuf};
use caissa_core::{agent, config::load_config};

const AGENT_DOCKERFILE: &str = include_str!("../../../sandbox/Dockerfile.agent");
const AGENT_ENTRYPOINT: &str = include_str!("../../../sandbox/entrypoint.sh");
const AGENT_SIDECAR: &str = include_str!("../../../sandbox/agent-sidecar.js");
const AGENT_MINT_TOKEN_SCRIPT: &str = include_str!("../../../sandbox/mint-github-token.sh");

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
    std::fs::write(build_dir.join("agent-sidecar.js"), AGENT_SIDECAR)?;
    std::fs::write(build_dir.join("mint-github-token.sh"), AGENT_MINT_TOKEN_SCRIPT)?;

    // Copy Fondament domain + role definitions into the build context so they
    // are baked into the image at /fondament/domains/ and /fondament/roles/.
    copy_dir_into(
        &std::path::Path::new(&fondament).join("definitions").join("domains"),
        &build_dir.join("fondament_domains"),
    )?;
    copy_dir_into(
        &std::path::Path::new(&fondament).join("definitions").join("fondament"),
        &build_dir.join("fondament_roles"),
    )?;

    // Skills/plugins (e.g. Superpowers) are deliberately not baked into the
    // image yet — see Dockerfile.agent's comment for why. The sidecar runs
    // with skills: [] until a real decision is made on how a CI runner
    // (no local ~/.claude/plugins/) provisions third-party plugin code.

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

/// Copy all files from `src` into `dst`, creating `dst` if needed.
/// Non-recursive: only copies top-level files (definitions are flat directories).
fn copy_dir_into(src: &Path, dst: &Path) -> anyhow::Result<()> {
    if !src.exists() {
        eprintln!("[caissa] warning: fondament dir not found, skipping: {}", src.display());
        return Ok(());
    }
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        if entry.file_type()?.is_file() {
            std::fs::copy(entry.path(), dst.join(entry.file_name()))?;
        }
    }
    Ok(())
}
