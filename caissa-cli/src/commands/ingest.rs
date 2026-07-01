/// Context ingestion pipeline — seeds Farga's role-scoped context graph for
/// Occitan components.
///
/// For each component (or a specific one), fetches CLAUDE.md and README from
/// GitHub, synthesizes an architecture summary via Claude, and writes typed
/// context nodes to Farga:
///   [<component>][codebase]     — codebase pointer (component-readable)
///   [<component>][architecture] — synthesized architecture (architect-readable)
///   [occitan][system-rationale] — stack-level rationale (org-readable)
///
/// Run manually or via the weekly ingestion CronJob in the component-agents chart.

use caissa_core::config::load_config;

const COMPONENTS: &[(&str, &str)] = &[
    ("gardian",     "Gardian"),
    ("fondament",   "Fondament"),
    ("farga",       "Farga"),
    ("amassada",    "Amassada"),
    ("cor",         "Cor"),
    ("caissa",      "Caissa"),
    ("charradissa", "Charradissa"),
    ("nervi",       "Nervi"),
];

pub async fn run(component: Option<&str>) -> anyhow::Result<()> {
    let config = load_config()?;
    let farga_mcp_url = config.farga_mcp_url;
    let client = reqwest::Client::new();

    let targets: Vec<(&str, &str)> = match component {
        Some(name) => COMPONENTS.iter()
            .find(|(id, _)| *id == name)
            .map(|&t| vec![t])
            .unwrap_or_else(|| {
                eprintln!("[ingest] unknown component: {name}. Known: {:?}",
                    COMPONENTS.iter().map(|(id, _)| id).collect::<Vec<_>>());
                vec![]
            }),
        None => COMPONENTS.to_vec(),
    };

    for (component_id, repo_name) in &targets {
        eprintln!("[ingest] processing {component_id}");

        // ── Codebase reference node (component-readable) ──────────────────
        let claude_md = match fetch_file_from_github(repo_name, "CLAUDE.md").await {
            Ok(s) => s,
            Err(_) => fetch_file_from_github(repo_name, "README.md").await
                .unwrap_or_else(|_| format!("No CLAUDE.md or README.md found in miegjorn/{repo_name}")),
        };

        let codebase_content = format!(
            "GitHub: https://github.com/miegjorn/{repo_name}\n\n{claude_md}"
        );

        write_context_node(
            &client,
            &farga_mcp_url,
            &format!("[{component_id}][codebase]"),
            "codebase-ref",
            &codebase_content,
            "component",
            "occitan",
            Some(component_id),
        ).await?;

        eprintln!("[ingest] {component_id}: codebase-ref written");

        // ── Architecture synthesis (architect-readable) ───────────────────
        let readme = fetch_file_from_github(repo_name, "README.md").await
            .unwrap_or_default();

        if !readme.is_empty() {
            let architecture = synthesize_architecture(component_id, repo_name, &readme).await
                .unwrap_or_else(|e| {
                    eprintln!("[ingest] {component_id}: synthesis failed ({e}), using README excerpt");
                    readme.chars().take(2000).collect()
                });

            write_context_node(
                &client,
                &farga_mcp_url,
                &format!("[{component_id}][architecture]"),
                "architecture",
                &architecture,
                "architect",
                "occitan",
                Some(component_id),
            ).await?;

            eprintln!("[ingest] {component_id}: architecture written");
        }
    }

    // ── Stack-level system rationale (org-readable) ───────────────────────
    if component.is_none() {
        ingest_system_rationale(&client, &farga_mcp_url).await?;
        eprintln!("[ingest] system-rationale written");
    }

    eprintln!("[ingest] done");
    Ok(())
}

async fn fetch_file_from_github(repo_name: &str, path: &str) -> anyhow::Result<String> {
    let output = tokio::process::Command::new("gh")
        .args([
            "api",
            &format!("repos/miegjorn/{repo_name}/contents/{path}"),
            "--jq", ".content",
        ])
        .output()
        .await?;

    if !output.status.success() {
        anyhow::bail!("gh api failed for {repo_name}/{path}");
    }

    let b64 = String::from_utf8_lossy(&output.stdout).trim().to_string();
    // GitHub API returns base64 with newlines
    let cleaned = b64.replace('\n', "");
    let bytes = base64_decode(&cleaned)?;
    Ok(String::from_utf8_lossy(&bytes).to_string())
}

fn base64_decode(s: &str) -> anyhow::Result<Vec<u8>> {
    use std::process::Command;
    let out = Command::new("base64")
        .args(["--decode"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .and_then(|mut child| {
            use std::io::Write;
            child.stdin.as_mut().unwrap().write_all(s.as_bytes())?;
            child.wait_with_output()
        })?;
    Ok(out.stdout)
}

async fn synthesize_architecture(
    component_id: &str,
    repo_name: &str,
    readme: &str,
) -> anyhow::Result<String> {
    let prompt = format!(
        r#"You are synthesizing an architecture summary for the Farga context graph.

Component: {component_id} (repo: miegjorn/{repo_name})

README content:
{readme}

Write a concise architecture summary (300-500 words) covering:
1. What this component does and its role in the Occitan stack
2. Key design decisions and why
3. Primary interfaces (HTTP API, MCP tools, NATS subjects, etc.)
4. What it depends on and what depends on it
5. Key invariants — what must always be true about this component

Be precise and factual. This will be stored as the architect-level context node for this component.
Output only the summary, no preamble."#,
        component_id = component_id,
        repo_name = repo_name,
        readme = readme.chars().take(8000).collect::<String>(),
    );

    let model = "claude-haiku-4-5-20251001"; // TODO: make configurable like chronicle_model, support grok*

    let output = if model.starts_with("grok") || model.starts_with("xai") {
        // basic grok for ingest
        let api_key = std::env::var("XAI_API_KEY")?;
        let client = reqwest::Client::new();
        let body = serde_json::json!({
            "model": model,
            "messages": [{"role": "user", "content": prompt}]
        });
        let resp = client.post("https://api.x.ai/v1/chat/completions")
            .header("Authorization", format!("Bearer {}", api_key))
            .json(&body)
            .send().await?.error_for_status()?;
        let j: serde_json::Value = resp.json().await?;
        let text = j["choices"][0]["message"]["content"].as_str().unwrap_or("").to_string();
        // simulate output
        tokio::process::Command::new("echo").arg(&text).output().await?
    } else {
        tokio::process::Command::new("claude")
            .args([
                "--print",
                &prompt,
                "--model",
                model,
            ])
            .output()
            .await?
    };

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("claude synthesis failed: {stderr}");
    }

    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

async fn ingest_system_rationale(client: &reqwest::Client, farga_mcp_url: &str) -> anyhow::Result<()> {
    // Read the project-level CLAUDE.md from ~/.claude/CLAUDE.md or the Caissa repo
    let home = std::env::var("HOME").unwrap_or_else(|_| "/root".to_string());
    let rationale = std::fs::read_to_string(
        std::path::Path::new(&home).join(".claude/CLAUDE.md")
    ).unwrap_or_else(|_| {
        // Fallback: synthesize from what we know
        "The Occitan stack is a self-maintaining, multi-agent platform built on:\n\
         - Rust microservices (Gardian, Farga, Amassada, Charradissa, Cor, Caissa)\n\
         - NATS JetStream (Nervi) as the event backbone\n\
         - Matrix (Synapse) as the human-agent communication layer\n\
         - Fondament as the persona/definition substrate\n\
         - Farga as the collective memory and context graph\n\
         - ArgoCD + Kubernetes for deployment\n\
         \nKey invariant: no agent takes architectural decisions alone. \
         All Class 3+ changes escalate to Pierre-Luc.".to_string()
    });

    write_context_node(
        client,
        farga_mcp_url,
        "[occitan][system-rationale]",
        "rationale",
        &rationale,
        "org",
        "occitan",
        None,
    ).await
}

async fn write_context_node(
    client: &reqwest::Client,
    farga_mcp_url: &str,
    path: &str,
    node_type: &str,
    content: &str,
    read_role: &str,
    project: &str,
    component: Option<&str>,
) -> anyhow::Result<()> {
    let mut args = serde_json::json!({
        "path": path,
        "node_type": node_type,
        "content": content,
        "read_role": read_role,
        "project": project,
    });
    if let Some(comp) = component {
        args["component"] = serde_json::Value::String(comp.to_string());
    }

    let body = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/call",
        "params": {
            "name": "write_context_node",
            "arguments": args
        }
    });

    let resp = client
        .post(farga_mcp_url)
        .json(&body)
        .timeout(std::time::Duration::from_secs(15))
        .send()
        .await?
        .json::<serde_json::Value>()
        .await?;

    if let Some(err) = resp.get("error") {
        anyhow::bail!("Farga write_context_node error: {err}");
    }
    Ok(())
}
