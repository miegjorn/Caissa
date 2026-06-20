/// Guilhem daemon — lightweight webhook listener.
///
/// Runs an HTTP server that accepts chronicle trigger events from Argo Workflows,
/// git webhooks, or cron. When triggered, it runs `claude --print "<task>"` as
/// a subprocess (non-interactive) and posts the output as a Signal to Farga.
/// (Matrix presence is Charradissa's appservice, not this listener.)
///
/// Token usage is proportional to actual events — the server itself costs nothing.

use axum::{extract::State, http::StatusCode, routing::post, Json, Router};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use caissa_core::config::load_config;

#[derive(Clone)]
struct ListenState {
    farga_url: String,
    farga_project: String,
    farga_mcp_url: String,
    chronicle_model: String,
    matrix_model: String,
    amassada_url: String,
}

#[derive(Deserialize)]
pub struct TriggerReq {
    /// Human-readable reason for the chronicle run.
    pub reason: String,
    /// Optional specific prompt override. If absent, uses the default chronicle prompt.
    pub prompt: Option<String>,
}

#[derive(Serialize)]
struct SignalPayload {
    project: String,
    signals: Vec<SignalItem>,
}

#[derive(Serialize)]
struct SignalItem {
    // Farga's Signal requires project on each item (not just the envelope).
    project: String,
    content: String,
    source: String,
}

pub async fn run(port: u16) -> anyhow::Result<()> {
    let config = load_config()?;

    let state = Arc::new(ListenState {
        farga_url: config.farga_url,
        farga_project: config.project,
        farga_mcp_url: config.farga_mcp_url,
        chronicle_model: config.chronicle_model,
        matrix_model: config.matrix_model,
        amassada_url: config.amassada_url,
    });

    let app = Router::new()
        .route("/trigger/chronicle", post(handle_chronicle))
        .route("/matrix/reply", post(handle_matrix_reply))
        .route("/health", axum::routing::get(|| async { "ok" }))
        .with_state(state);

    let addr = format!("0.0.0.0:{}", port);
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    eprintln!("[caissa] listening on {}", addr);

    axum::serve(listener, app).await?;
    Ok(())
}

async fn handle_chronicle(
    State(state): State<Arc<ListenState>>,
    Json(req): Json<TriggerReq>,
) -> StatusCode {
    tracing::info!("chronicle trigger received: {}", req.reason);

    let prompt = req
        .prompt
        .unwrap_or_else(|| build_chronicle_prompt(&req.reason, &state.farga_project));

    tokio::spawn(async move {
        match run_chronicle(&state, &prompt).await {
            Ok(_) => tracing::info!("chronicle run complete"),
            Err(e) => tracing::error!("chronicle run failed: {}", e),
        }
    });

    StatusCode::ACCEPTED
}

fn build_chronicle_prompt(reason: &str, project: &str) -> String {
    format!(
        r#"Chronicle trigger: {reason}

You are Guilhem de Tudela, chronicler of the Occitan stack. This is a scheduled
chronicle run for project "{project}".

You have the Farga MCP server attached. Ground your chronicle in real state — use its
read tools before writing:
- search_signals (project: "{project}") — recent signals / activity
- read_context (project: "{project}") — accumulated project context
- list_projects — what projects exist

Then write a concise chronicle entry: what happened, what it means for the trajectory,
what is now different from before. Your written response IS the chronicle — it is
recorded to Farga automatically, so do not try to post it yourself.

Be faithful, not verbose. The chronicle is for future agents (including your next
instance) to understand where the stack stands.
"#
    )
}

async fn run_chronicle(state: &ListenState, prompt: &str) -> anyhow::Result<()> {
    // Attach the Farga MCP server so Claude reads live state via tools instead of
    // shelling out (its bash tools are gated in headless --print runs). Only the read
    // tools are allowed — writes go through caissa's post_signal below.
    let mcp_config = format!(
        r#"{{"mcpServers":{{"farga":{{"type":"http","url":"{}"}}}}}}"#,
        state.farga_mcp_url
    );
    let mcp_path = std::env::temp_dir().join("guilhem-mcp.json");
    std::fs::write(&mcp_path, &mcp_config)?;

    let output = tokio::process::Command::new("claude")
        .args([
            "--print",
            prompt,
            "--model",
            &state.chronicle_model,
            "--mcp-config",
            mcp_path.to_str().unwrap(),
            "--allowed-tools",
            "mcp__farga__search_signals,mcp__farga__read_context,mcp__farga__list_projects",
        ])
        .env("FARGA_URL", &state.farga_url)
        .env("FARGA_PROJECT", &state.farga_project)
        .output()
        .await?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("claude exited with error: {}", stderr);
    }

    let chronicle = String::from_utf8_lossy(&output.stdout).to_string();

    if !chronicle.trim().is_empty() {
        post_signal(state, &chronicle).await?;
    }

    Ok(())
}

// ── Matrix reply ──────────────────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct MatrixReplyReq {
    pub room_id: String,
    pub sender: String,
    pub content: String,
    #[serde(default)]
    pub history: Vec<MatrixHistoryEntry>,
}

#[derive(Deserialize)]
pub struct MatrixHistoryEntry {
    pub sender: String,
    pub content: String,
}

#[derive(Serialize)]
pub struct MatrixReplyResp {
    pub text: String,
}

async fn handle_matrix_reply(
    State(state): State<Arc<ListenState>>,
    Json(req): Json<MatrixReplyReq>,
) -> (axum::http::StatusCode, Json<MatrixReplyResp>) {
    match run_matrix_reply(&state, &req).await {
        Ok(text) => {
            // Fire-and-forget: publish a BtwEmitted event to Amassada so subscribers
            // have cross-session visibility of matrix activity.
            let amassada_url = state.amassada_url.clone();
            let room = req.room_id.clone();
            let sender = req.sender.clone();
            let preview = text.chars().take(200).collect::<String>();
            tokio::spawn(async move {
                let event = serde_json::json!({
                    "BtwEmitted": {
                        "from": "guilhem",
                        "to": room,
                        "content": format!("[{}] {}", sender, preview)
                    }
                });
                let _ = reqwest::Client::new()
                    .post(format!("{}/events", amassada_url))
                    .json(&event)
                    .timeout(std::time::Duration::from_secs(2))
                    .send()
                    .await;
            });
            (axum::http::StatusCode::OK, Json(MatrixReplyResp { text }))
        }
        Err(e) => {
            tracing::error!("matrix reply failed: {}", e);
            (
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                Json(MatrixReplyResp { text: format!("(guilhem error: {})", e) }),
            )
        }
    }
}

/// Returns true only when the message explicitly needs live Farga state.
/// Most conversational replies are grounded in the passed history + GitHub tools —
/// skipping MCP saves the ~5-8s connection overhead on every casual message.
fn needs_farga_mcp(content: &str) -> bool {
    let lower = content.to_lowercase();
    lower.contains("farga")
        || lower.contains("read context")
        || lower.contains("search signal")
        || lower.contains("list project")
        || lower.contains("look up")
        || lower.contains("what's in")
}

async fn run_matrix_reply(state: &ListenState, req: &MatrixReplyReq) -> anyhow::Result<String> {
    let mut prompt = format!("Matrix room: {}\n\nConversation history (oldest first):\n", req.room_id);
    for entry in &req.history {
        prompt.push_str(&format!("{}: {}\n", entry.sender, entry.content));
    }
    prompt.push_str(&format!(
        "\nLatest message from {}:\n{}\n\nReply as Guilhem.",
        req.sender, req.content
    ));

    let mut cmd = tokio::process::Command::new("claude");
    cmd.args(["--print", &prompt, "--model", &state.matrix_model])
        .env("FARGA_URL", &state.farga_url)
        .env("FARGA_PROJECT", &state.farga_project);

    // Only pay the MCP connection cost when the message explicitly needs live Farga reads.
    let mcp_path = if needs_farga_mcp(&req.content) {
        let mcp_config = format!(
            r#"{{"mcpServers":{{"farga":{{"type":"http","url":"{}"}}}}}}"#,
            state.farga_mcp_url
        );
        let path = std::env::temp_dir().join(format!(
            "guilhem-matrix-mcp-{}.json",
            uuid::Uuid::new_v4()
        ));
        std::fs::write(&path, &mcp_config)?;
        tracing::debug!("matrix reply: attaching Farga MCP");
        cmd.args([
            "--mcp-config",
            path.to_str().unwrap(),
            "--allowed-tools",
            "Bash,Edit,Write,mcp__farga__search_signals,mcp__farga__read_context,mcp__farga__list_projects",
        ]);
        Some(path)
    } else {
        tracing::debug!("matrix reply: conversational path, no MCP");
        cmd.args(["--allowed-tools", "Bash,Edit,Write"]);
        None
    };

    let output = cmd.output().await?;

    if let Some(path) = &mcp_path {
        let _ = std::fs::remove_file(path);
    }

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("claude exited non-zero: {}", stderr);
    }

    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

async fn post_signal(state: &ListenState, content: &str) -> anyhow::Result<()> {
    let payload = SignalPayload {
        project: state.farga_project.clone(),
        signals: vec![SignalItem {
            project: state.farga_project.clone(),
            content: content.to_string(),
            source: "guilhem-daemon".into(),
        }],
    };

    reqwest::Client::new()
        .post(format!("{}/signals", state.farga_url))
        .json(&payload)
        .send()
        .await?
        .error_for_status()?;

    Ok(())
}
