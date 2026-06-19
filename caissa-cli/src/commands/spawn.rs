use caissa_core::agent::{
    assemble_workspace_md, load_domain_def, load_fondament_def, resolve_facet_name, AgentSpec,
};
use caissa_core::config::load_config;
use super::sandbox::resolve_workspace;

pub async fn run(
    agent: &str,
    project_override: Option<&str>,
    session: Option<&str>,
    generation_override: Option<&str>,
) -> anyhow::Result<()> {
    let config = load_config()?;
    let current_generation = generation_override.unwrap_or(&config.generation);
    let spec = AgentSpec::parse(agent, current_generation, project_override);
    let tag = format!("caissa-sandbox:{}", spec.image_tag);

    let workspace_path = session
        .map(|id| resolve_workspace(&config.workspaces_dir, id))
        .transpose()?;

    // Assemble /workspace/CLAUDE.md: domain + facet + live Farga context.
    let has_situational_context = spec.domain.is_some() || spec.facet.is_some() || spec.project.is_some();

    if let Some(ref ws) = workspace_path {
        if has_situational_context {
            let domain_def = if let Some(ref domain) = spec.domain {
                match load_domain_def(&config.fondament_path, domain) {
                    Ok(d) => Some(d),
                    Err(e) => {
                        eprintln!("[caissa] domain definition not found for '{}': {}", domain, e);
                        None
                    }
                }
            } else {
                None
            };

            let facet_def = if let Some(ref facet) = spec.facet {
                let fondament_name = resolve_facet_name(facet);
                match load_fondament_def(&config.fondament_path, fondament_name) {
                    Ok(f) => Some(f),
                    Err(e) => {
                        eprintln!("[caissa] facet definition not found for '{}' (tried '{}'): {}", facet, fondament_name, e);
                        None
                    }
                }
            } else {
                None
            };

            let farga_context = if let Some(ref proj) = spec.project {
                match fetch_farga_context(&config.farga_url, proj).await {
                    Ok(ctx) if !ctx.is_empty() => {
                        eprintln!("[caissa] farga context loaded for project: {}", proj);
                        Some(ctx)
                    }
                    Ok(_) => {
                        eprintln!("[caissa] farga returned empty context for project: {}", proj);
                        None
                    }
                    Err(e) => {
                        eprintln!("[caissa] farga context unavailable ({}): {}", proj, e);
                        None
                    }
                }
            } else {
                None
            };

            let workspace_md = assemble_workspace_md(
                domain_def.as_ref(),
                facet_def.as_ref(),
                farga_context.as_deref(),
            );

            if !workspace_md.is_empty() {
                std::fs::write(ws.join("CLAUDE.md"), &workspace_md)?;
                eprintln!("[caissa] workspace CLAUDE.md written ({} chars)", workspace_md.len());
            }
        } else if let Some(ref proj) = spec.project {
            // generation-only spawn with explicit --project: legacy Farga context injection
            match fetch_farga_context(&config.farga_url, proj).await {
                Ok(ctx) if !ctx.is_empty() => {
                    std::fs::write(ws.join("CLAUDE.md"), &ctx)?;
                    eprintln!("[caissa] farga context written to workspace CLAUDE.md");
                }
                Ok(_) => eprintln!("[caissa] farga returned empty context for project: {}", proj),
                Err(e) => eprintln!("[caissa] farga context unavailable ({}): {}", proj, e),
            }
        }

        eprintln!("[caissa] workspace: {}", ws.display());
    }

    let mut args = vec![
        "run".to_string(),
        "--rm".to_string(),
        "-it".to_string(),
        // MCP service URLs — entrypoint.sh writes these into the Claude Code config
        "-e".to_string(), format!("FARGA_MCP_URL={}", config.farga_mcp_url),
        "-e".to_string(), format!("DISPATCHER_MCP_URL={}", config.dispatcher_mcp_url),
        // Pass through the API key if set in the host environment
        "-e".to_string(), format!("ANTHROPIC_API_KEY={}", std::env::var("ANTHROPIC_API_KEY").unwrap_or_default()),
    ];

    if let Some(ref ws) = workspace_path {
        args.push("-v".to_string());
        args.push(format!("{}:/workspace", ws.display()));
    }

    args.push(tag.clone());
    eprintln!("[caissa] spawning: {}", tag);

    let status = tokio::process::Command::new("docker")
        .args(&args)
        .status()
        .await?;

    if !status.success() {
        anyhow::bail!("spawn exited with status: {}", status);
    }

    Ok(())
}

async fn fetch_farga_context(farga_url: &str, project: &str) -> anyhow::Result<String> {
    let url = format!("{}/context/project/{}", farga_url, project);
    let resp = reqwest::get(&url).await?;
    if resp.status() == reqwest::StatusCode::NOT_FOUND {
        return Ok(String::new());
    }
    Ok(resp.error_for_status()?.text().await?)
}
