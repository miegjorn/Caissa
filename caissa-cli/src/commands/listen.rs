/// Guilhem daemon — lightweight webhook listener.
///
/// Runs an HTTP server that accepts chronicle trigger events from Argo Workflows,
/// git webhooks, or Matrix. When triggered, it runs `claude --print "<task>"` as
/// a subprocess (non-interactive) and posts the output as a Signal to Farga.
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
    content: String,
    source: String,
}

pub async fn run(port: u16) -> anyhow::Result<()> {
    let config = load_config()?;

    let state = Arc::new(ListenState {
        farga_url: config.farga_url,
        farga_project: config.project,
    });

    let app = Router::new()
        .route("/trigger/chronicle", post(handle_chronicle))
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

    let prompt = req.prompt.unwrap_or_else(|| build_chronicle_prompt(&req.reason));

    tokio::spawn(async move {
        match run_chronicle(&state, &prompt).await {
            Ok(_) => tracing::info!("chronicle run complete"),
            Err(e) => tracing::error!("chronicle run failed: {}", e),
        }
    });

    StatusCode::ACCEPTED
}

fn build_chronicle_prompt(reason: &str) -> String {
    format!(
        r#"Chronicle trigger: {reason}

You are Guilhem de Tudela, chronicler of the Occitan stack. This is a scheduled
chronicle run.

Your task:
1. Check recent signals in Farga: curl $FARGA_URL/signals/recent?project=$FARGA_PROJECT
2. Review any significant activity (git logs if repos are accessible, recent session
   outputs, decisions made).
3. Write a concise chronicle entry — what happened, what it means for the trajectory,
   what is now different from before.
4. Post your chronicle as a signal:
   curl -X POST $FARGA_URL/signals \
     -H 'Content-Type: application/json' \
     -d '{{"project":"$FARGA_PROJECT","signals":[{{"content":"<your chronicle>","source":"guilhem"}}]}}'

Be faithful, not verbose. The chronicle is for future agents (including your next
instance) to understand where the stack stands.
"#
    )
}

async fn run_chronicle(state: &ListenState, prompt: &str) -> anyhow::Result<()> {
    let output = tokio::process::Command::new("claude")
        .args(["--print", prompt])
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

async fn post_signal(state: &ListenState, content: &str) -> anyhow::Result<()> {
    let payload = SignalPayload {
        project: state.farga_project.clone(),
        signals: vec![SignalItem {
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
