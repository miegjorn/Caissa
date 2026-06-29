/// Guilhem daemon — HTTP listener with six routes:
///
/// `POST /turn` — Amassada orchestrates Guilhem as an agent-as-endpoint participant
/// (Option B-full). Single-shot: Amassada assembles the full context, sends one
/// user message, and the process tears down. No per-room session is created.
///
/// `POST /trigger/chronicle` — accepts chronicle trigger events from Argo
/// Workflows, git webhooks, or cron. One-shot: runs `claude --print "<task>"`
/// as a subprocess, posts the output as a Signal to Farga, exits.
///
/// `POST /trigger/sre-alert` — CronWorkflow-triggered (every 30min). Fetches
/// recent watchdog signals from Farga; if any are present and SRE_MATRIX_ROOM_ID
/// is configured, posts a formatted alert to the Matrix room.
///
/// `POST /trigger/backlog-review` — CronWorkflow-triggered (weekly). Guilhem
/// reads open GitHub issues across miegjorn repos, synthesizes a backlog review,
/// writes it to Farga, and optionally posts a summary to Matrix.
///
/// `POST /matrix/reply` — Charradissa (the Matrix appservice/bridge) forwards
/// every room message here; this listener generates the actual reply.
/// Per-room, NOT one-shot: the first message in a room spawns a persistent
/// `agent-sidecar.js` child process (Claude Agent SDK, `sandbox/agent-sidecar.js`),
/// and later messages in the same room are sent to that same process over
/// stdin/stdout, giving real conversational continuity via the SDK's `resume`
/// session mechanism. A background sweep reaps sessions idle past 30 minutes;
/// a dead/crashed sidecar is detected and respawned automatically on the next
/// message for that room. See `ListenState::room_sessions`.
///
/// `GET /health` — liveness probe; returns `200 ok`.
///
/// Token usage is proportional to actual events for chronicle; Matrix sessions
/// cost tokens for as long as a room stays active (up to the idle timeout).

use axum::{extract::State, http::StatusCode, routing::post, Json, Router};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::collections::HashMap;
use caissa_core::config::load_config;
use caissa_core::agent::load_fondament_def;
use super::handoff::{is_handoff_message, parse_handoff_message, HandoffRequest};

#[derive(Clone)]
struct ListenState {
    farga_url: String,
    farga_project: String,
    farga_mcp_url: String,
    chronicle_model: String,
    matrix_model: String,
    amassada_url: String,
    dispatcher_mcp_url: String,
    charradissa_mcp_url: String,
    nervi_mcp_url: String,
    fondament_path: String,
    generation: String,
    /// Matrix room ID for SRE alert posts. Empty string = alerting disabled.
    sre_matrix_room_id: String,
    /// Matrix room ID for backlog review posts. Empty string = posting disabled.
    backlog_matrix_room_id: String,
    dream_model: String,
    /// Matrix room ID for dream report posts. Empty = posting disabled.
    dream_matrix_room_id: String,
    /// One persistent agent-sidecar.js child process per actively-chatting
    /// Matrix room. Reaped by an idle-timeout sweep (see spawn_idle_reaper).
    /// The outer Mutex protects the map (held only for map operations, never
    /// across the Claude API call). Each entry's inner Mutex serialises
    /// concurrent messages for the same room while allowing different rooms
    /// to run in parallel.
    room_sessions: Arc<tokio::sync::Mutex<HashMap<String, RoomSession>>>,
}

/// A running agent-sidecar.js child process for one room.
struct SidecarProcess {
    child: tokio::process::Child,
    stdin: tokio::process::ChildStdin,
    stdout: tokio::io::BufReader<tokio::process::ChildStdout>,
}

impl SidecarProcess {
    async fn spawn(init: &SidecarInit) -> anyhow::Result<Self> {
        use tokio::io::AsyncWriteExt;

        let mut child = tokio::process::Command::new("node")
            .arg("/usr/local/bin/agent-sidecar.js")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::inherit())
            .spawn()?;

        let mut stdin = child.stdin.take().expect("stdin was piped");
        let stdout = child.stdout.take().expect("stdout was piped");

        let init_line = serde_json::to_string(init)?;
        stdin.write_all(init_line.as_bytes()).await?;
        stdin.write_all(b"\n").await?;

        Ok(Self {
            child,
            stdin,
            stdout: tokio::io::BufReader::new(stdout),
        })
    }

    async fn send(&mut self, room_id: &str, sender: &str, content: &str) -> anyhow::Result<String> {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt};

        let msg = serde_json::json!({ "room_id": room_id, "sender": sender, "content": content });
        let line = serde_json::to_string(&msg)?;
        self.stdin.write_all(line.as_bytes()).await?;
        self.stdin.write_all(b"\n").await?;

        let mut response_line = String::new();
        self.stdout.read_line(&mut response_line).await?;

        let parsed: serde_json::Value = serde_json::from_str(response_line.trim())?;
        if let Some(err) = parsed.get("error").and_then(|v| v.as_str()) {
            anyhow::bail!("sidecar error: {}", err);
        }
        Ok(parsed.get("reply").and_then(|v| v.as_str()).unwrap_or("").to_string())
    }

    fn kill(&mut self) {
        let _ = self.child.start_kill();
    }

    /// Returns true if the child process is still running. `try_wait()`
    /// returns `Ok(None)` while alive, `Ok(Some(_))` once it has exited, and
    /// `Err` if the OS-level status check itself fails — in that case we
    /// treat the process as dead (safer to respawn than keep using
    /// something we can't verify).
    fn is_alive(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }
}

#[derive(serde::Serialize)]
struct SidecarInit {
    #[serde(rename = "systemPrompt")]
    system_prompt: String,
    model: String,
    #[serde(rename = "allowedTools")]
    allowed_tools: Vec<String>,
    skills: Vec<String>,
    #[serde(rename = "mcpServers")]
    mcp_servers: serde_json::Value,
}

/// One room's live session: the running sidecar child process, and when it
/// last handled a message.
///
/// `process` is behind its own `Arc<Mutex>` so the outer map lock can be
/// released before the Claude API call. Different rooms run in parallel;
/// two messages for the same room serialise on the per-room Mutex.
/// `last_activity` is updated under the outer map lock so the idle reaper
/// can inspect it without touching the inner Mutex.
struct RoomSession {
    process: std::sync::Arc<tokio::sync::Mutex<SidecarProcess>>,
    last_activity: std::time::Instant,
}

impl RoomSession {
    #[cfg(test)]
    fn for_test(last_activity: std::time::Instant) -> Self {
        // tokio::process::Command::spawn() needs a live Tokio runtime (it
        // registers the child with the reactor for SIGCHLD), but these are
        // plain #[test] functions, not #[tokio::test]. Stand up a throwaway
        // current-thread runtime just for the spawn/take calls below.
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("build runtime for test");
        let (child, stdin, stdout) = rt.block_on(async {
            let mut cmd = tokio::process::Command::new("true");
            cmd.stdin(std::process::Stdio::piped()).stdout(std::process::Stdio::piped());
            let mut child = cmd.spawn().expect("spawn /bin/true for test");
            let stdin = child.stdin.take().expect("stdin was piped");
            let stdout = child.stdout.take().expect("stdout was piped");
            (child, stdin, stdout)
        });
        Self {
            process: std::sync::Arc::new(tokio::sync::Mutex::new(
                SidecarProcess { child, stdin, stdout: tokio::io::BufReader::new(stdout) },
            )),
            last_activity,
        }
    }

    fn is_idle(&self, timeout: std::time::Duration) -> bool {
        self.last_activity.elapsed() >= timeout
    }
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
        dispatcher_mcp_url: config.dispatcher_mcp_url,
        charradissa_mcp_url: config.charradissa_mcp_url,
        nervi_mcp_url: config.nervi_mcp_url,
        fondament_path: config.fondament_path,
        generation: config.generation,
        sre_matrix_room_id: std::env::var("SRE_MATRIX_ROOM_ID").unwrap_or_default(),
        backlog_matrix_room_id: std::env::var("BACKLOG_MATRIX_ROOM_ID").unwrap_or_default(),
        dream_model: config.dream_model,
        dream_matrix_room_id: std::env::var("DREAM_MATRIX_ROOM_ID").unwrap_or_default(),
        room_sessions: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
    });

    tokio::spawn(spawn_idle_reaper(Arc::clone(&state.room_sessions)));

    let app = Router::new()
        .route("/trigger/chronicle", post(handle_chronicle))
        .route("/trigger/sre-alert", post(handle_sre_alert))
        .route("/trigger/backlog-review", post(handle_backlog_review))
        .route("/trigger/dream", post(handle_dream))
        .route("/trigger/scan", post(handle_scan))
        .route("/matrix/reply", post(handle_matrix_reply))
        .route("/turn", post(handle_turn))
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

/// POST /trigger/sre-alert — CronWorkflow-triggered (every 30min).
///
/// Fetches recent bug-signals written by the sre-watchdog from Farga.
/// If any are found and SRE_MATRIX_ROOM_ID is configured, posts a
/// formatted alert directly to the Matrix room using the SYNAPSE_ADMIN_TOKEN
/// and SYNAPSE_URL that the initContainer injects at pod startup.
/// Silent (202, no Matrix post) when all-clear or alerting is not configured.
async fn handle_sre_alert(
    State(state): State<Arc<ListenState>>,
    Json(req): Json<TriggerReq>,
) -> StatusCode {
    tracing::info!("sre-alert trigger received: {}", req.reason);

    let state_clone = Arc::clone(&state);
    tokio::spawn(async move {
        if let Err(e) = run_sre_alert(&state_clone).await {
            tracing::error!("sre-alert run failed: {}", e);
        }
    });

    StatusCode::ACCEPTED
}

async fn run_sre_alert(state: &ListenState) -> anyhow::Result<()> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()?;

    // Fetch recent signals — filter for watchdog bug-signals
    let url = format!("{}/signals/recent?project={}", state.farga_url, state.farga_project);
    let signals: Vec<serde_json::Value> = client.get(&url).send().await?.json().await.unwrap_or_default();

    let watchdog_signals: Vec<&str> = signals
        .iter()
        .filter(|s| s["source"].as_str() == Some("sre-watchdog"))
        .filter_map(|s| s["content"].as_str())
        .collect();

    if watchdog_signals.is_empty() {
        tracing::info!("sre-alert: all clear — no watchdog signals");
        return Ok(());
    }

    let alert_body = format!(
        "⚠️ SRE watchdog alert ({} issue(s) detected):\n\n{}",
        watchdog_signals.len(),
        watchdog_signals.join("\n\n---\n\n")
    );

    tracing::warn!("sre-alert: {} watchdog signal(s) found — posting to Matrix", watchdog_signals.len());

    if state.sre_matrix_room_id.is_empty() {
        tracing::warn!("sre-alert: SRE_MATRIX_ROOM_ID not set — alert not posted to Matrix");
        return Ok(());
    }

    let synapse_url = std::env::var("SYNAPSE_URL")
        .unwrap_or_else(|_| "http://synapse.occitan-system.svc.cluster.local:8008".into());
    let admin_token = std::env::var("SYNAPSE_ADMIN_TOKEN").unwrap_or_default();

    if admin_token.is_empty() {
        tracing::error!("sre-alert: SYNAPSE_ADMIN_TOKEN not set — cannot post Matrix alert");
        return Ok(());
    }

    let txn = uuid::Uuid::new_v4();
    let matrix_url = format!(
        "{}/_matrix/client/v3/rooms/{}/send/m.room.message/{}",
        synapse_url, state.sre_matrix_room_id, txn
    );

    client
        .put(&matrix_url)
        .bearer_auth(&admin_token)
        .json(&serde_json::json!({ "msgtype": "m.text", "body": alert_body }))
        .send()
        .await?
        .error_for_status()?;

    tracing::info!("sre-alert posted to Matrix room {}", state.sre_matrix_room_id);
    Ok(())
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

**SRE watchdog check** — before writing your chronicle, scan recent signals for any
with source "sre-watchdog". These are mechanical health alerts written by the no-LLM
watchdog when it detected an anomaly (unreachable service, missing signals, etc.).
If watchdog signals are present:
- Name each anomaly explicitly in your chronicle
- Assess whether it is still active or has self-resolved
- Note whether the SRE alert layer has already been triggered (source "sre-alert")
If no watchdog signals: note "watchdog: all clear" in one line and move on.

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
            "mcp__farga__search_signals,mcp__farga__read_context,mcp__farga__list_projects,mcp__farga__update_component_todo",
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

// ── Backlog review ────────────────────────────────────────────────────────────

/// POST /trigger/backlog-review — CronWorkflow-triggered (weekly).
///
/// Guilhem reads open GitHub issues across miegjorn repos via gh CLI, applies
/// staleness heuristics, synthesizes a backlog review, and writes it to Farga.
/// If BACKLOG_MATRIX_ROOM_ID is set, also posts a summary to that Matrix room.
async fn handle_backlog_review(
    State(state): State<Arc<ListenState>>,
    Json(req): Json<TriggerReq>,
) -> StatusCode {
    tracing::info!("backlog-review trigger received: {}", req.reason);

    tokio::spawn(async move {
        match run_backlog_review(&state).await {
            Ok(_) => tracing::info!("backlog-review complete"),
            Err(e) => tracing::error!("backlog-review failed: {}", e),
        }
    });

    StatusCode::ACCEPTED
}

async fn run_backlog_review(state: &ListenState) -> anyhow::Result<()> {
    let mcp_config = format!(
        r#"{{"mcpServers":{{"farga":{{"type":"http","url":"{}"}}}}}}"#,
        state.farga_mcp_url
    );
    let mcp_path = std::env::temp_dir().join("guilhem-backlog-mcp.json");
    std::fs::write(&mcp_path, &mcp_config)?;

    let prompt = build_backlog_review_prompt(&state.farga_project);

    let output = tokio::process::Command::new("claude")
        .args([
            "--print",
            &prompt,
            "--model",
            &state.matrix_model,
            "--mcp-config",
            mcp_path.to_str().unwrap(),
            "--allowed-tools",
            "Bash,mcp__farga__search_signals,mcp__farga__read_context,mcp__farga__write_signal",
        ])
        .env("FARGA_URL", &state.farga_url)
        .env("FARGA_PROJECT", &state.farga_project)
        .output()
        .await?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("backlog-review claude exited with error: {}", stderr);
    }

    let review = String::from_utf8_lossy(&output.stdout).to_string();

    if review.trim().is_empty() {
        tracing::warn!("backlog-review: empty output from claude");
        return Ok(());
    }

    // Write to Farga
    post_signal(state, &review).await?;
    tracing::info!("backlog-review written to Farga");

    // Post to Matrix if configured
    if !state.backlog_matrix_room_id.is_empty() {
        let synapse_url = std::env::var("SYNAPSE_URL")
            .unwrap_or_else(|_| "http://synapse.occitan-system.svc.cluster.local:8008".into());
        let admin_token = std::env::var("SYNAPSE_ADMIN_TOKEN").unwrap_or_default();

        if admin_token.is_empty() {
            tracing::warn!("backlog-review: SYNAPSE_ADMIN_TOKEN not set — skipping Matrix post");
            return Ok(());
        }

        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(15))
            .build()?;

        // Truncate to a readable Matrix summary (first 2000 chars)
        let summary = if review.len() > 2000 {
            format!("{}…\n\n(full review written to Farga)", &review[..2000])
        } else {
            review.clone()
        };

        let txn = uuid::Uuid::new_v4();
        let matrix_url = format!(
            "{}/_matrix/client/v3/rooms/{}/send/m.room.message/{}",
            synapse_url, state.backlog_matrix_room_id, txn
        );

        client
            .put(&matrix_url)
            .bearer_auth(&admin_token)
            .json(&serde_json::json!({ "msgtype": "m.text", "body": summary }))
            .send()
            .await?
            .error_for_status()?;

        tracing::info!("backlog-review posted to Matrix room {}", state.backlog_matrix_room_id);
    }

    Ok(())
}

fn build_backlog_review_prompt(project: &str) -> String {
    format!(
        r#"You are Guilhem de Tudela, org agent for the Occitan stack. This is a scheduled
backlog review run for the miegjorn GitHub organisation.

## What to do

1. **Fetch open issues** across all miegjorn repos using Bash:
   ```
   gh issue list --repo miegjorn/Caissa --state open --json number,title,createdAt,updatedAt,labels,body --limit 100
   gh issue list --repo miegjorn/Farga --state open --json number,title,createdAt,updatedAt,labels,body --limit 100
   gh issue list --repo miegjorn/Fondament --state open --json number,title,createdAt,updatedAt,labels,body --limit 100
   gh issue list --repo miegjorn/Gardian --state open --json number,title,createdAt,updatedAt,labels,body --limit 100
   gh issue list --repo miegjorn/Amassada --state open --json number,title,createdAt,updatedAt,labels,body --limit 100
   gh issue list --repo miegjorn/Charradissa --state open --json number,title,createdAt,updatedAt,labels,body --limit 100
   gh issue list --repo miegjorn/Cor --state open --json number,title,createdAt,updatedAt,labels,body --limit 100
   ```

2. **Apply staleness heuristics** — flag each of the following explicitly:
   - Issues open **>14 days with no update** (updatedAt older than 14 days ago)
   - Issues with **no Epic parent** (no "Parent Epic" mention in body, not labelled as an Epic itself)
   - **Epics with no open sub-issues** (issues labelled Epic or titled "Epic:" with no referenced open child issues)
   - Issues with **no assignee and no recent activity** — potential blockers without owners

3. **Read Farga context** for recent signals (project: "{project}") to connect backlog state
   to what the stack has been doing lately.

4. **Synthesize** a concise backlog review:
   - Count open issues per repo
   - List flagged items (stale, orphan, blocked) with issue numbers
   - Note any priority drift — issues that should be moving but aren't
   - Note any structural gaps — missing Epics, issues with no clear parent

5. **Write the review to Farga** using mcp__farga__write_signal with:
   - project: "{project}"
   - source: "backlog-review"
   - content: your full synthesis

Your written response IS the review — keep it crisp and actionable, not exhaustive.
Today's date is available via `date` in Bash.
"#,
        project = project
    )
}

// ── Dream — nightly consolidation ─────────────────────────────────────────────

/// POST /trigger/dream — CronJob-triggered daily (03:00 UTC).
///
/// Three-phase session:
/// 1. GATHER — read Farga signals (past 24h) + GitHub state across all repos
/// 2. SYNTHESIZE — identify drift, improvement opportunities, patterns
/// 3. ACT — create GitHub issues for actionable gaps; write dream report to Farga
async fn handle_dream(
    State(state): State<Arc<ListenState>>,
    Json(req): Json<TriggerReq>,
) -> StatusCode {
    tracing::info!("dream trigger received: {}", req.reason);

    tokio::spawn(async move {
        match run_dream(&state).await {
            Ok(_) => tracing::info!("dream complete"),
            Err(e) => tracing::error!("dream failed: {}", e),
        }
    });

    StatusCode::ACCEPTED
}

async fn run_dream(state: &ListenState) -> anyhow::Result<()> {
    let mcp_config = format!(
        r#"{{"mcpServers":{{"farga":{{"type":"http","url":"{}"}}}}}}"#,
        state.farga_mcp_url
    );
    let mcp_path = std::env::temp_dir().join("guilhem-dream-mcp.json");
    std::fs::write(&mcp_path, &mcp_config)?;

    let prompt = build_dream_prompt(&state.farga_project);

    let output = tokio::process::Command::new("claude")
        .args([
            "--print",
            &prompt,
            "--model",
            &state.dream_model,
            "--mcp-config",
            mcp_path.to_str().unwrap(),
            "--allowed-tools",
            "Bash,mcp__farga__search_signals,mcp__farga__read_context,mcp__farga__write_signal,mcp__farga__update_component_todo",
        ])
        .env("FARGA_URL", &state.farga_url)
        .env("FARGA_PROJECT", &state.farga_project)
        .output()
        .await?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("dream claude exited with error: {}", stderr);
    }

    let report = String::from_utf8_lossy(&output.stdout).to_string();

    if report.trim().is_empty() {
        tracing::warn!("dream: empty output from claude");
        return Ok(());
    }

    tracing::info!("dream complete — report written to Farga by agent");

    // Post summary to Matrix if configured
    if !state.dream_matrix_room_id.is_empty() {
        let synapse_url = std::env::var("SYNAPSE_URL")
            .unwrap_or_else(|_| "http://synapse.occitan-system.svc.cluster.local:8008".into());
        let admin_token = std::env::var("SYNAPSE_ADMIN_TOKEN").unwrap_or_default();

        if admin_token.is_empty() {
            tracing::warn!("dream: SYNAPSE_ADMIN_TOKEN not set — skipping Matrix post");
            return Ok(());
        }

        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(15))
            .build()?;

        let summary = if report.len() > 2000 {
            format!("{}…\n\n(full dream report written to Farga)", &report[..2000])
        } else {
            report.clone()
        };

        let txn = uuid::Uuid::new_v4();
        let matrix_url = format!(
            "{}/_matrix/client/v3/rooms/{}/send/m.room.message/{}",
            synapse_url, state.dream_matrix_room_id, txn
        );

        client
            .put(&matrix_url)
            .bearer_auth(&admin_token)
            .json(&serde_json::json!({ "msgtype": "m.text", "body": summary }))
            .send()
            .await?
            .error_for_status()?;

        tracing::info!("dream report posted to Matrix room {}", state.dream_matrix_room_id);
    }

    Ok(())
}

fn build_dream_prompt(project: &str) -> String {
    format!(
        r###"You are Guilhem de Tudela, org agent for the Occitan stack. This is the nightly
dream consolidation run. A dream has three phases — follow them in order.

---

## PHASE 1: GATHER (read, do not act yet)

1. Get today's date: `date -u '+%Y-%m-%d'`

2. Read Farga signals from the past 24 hours using mcp__farga__search_signals with
   since = yesterday's ISO 8601 timestamp. Note what changed: what was built,
   what was fixed, what was flagged.

3. Read the Farga project context (mcp__farga__read_context, project: "{project}") to
   understand the stack's current trajectory and open todos.

4. Fetch GitHub state across all 8 repos. Run these in sequence:
   ```
   for repo in Gardian Fondament Farga Amassada Charradissa Cor Caissa Occitan; do
     echo "=== $repo open issues ==="
     gh issue list --repo miegjorn/$repo --state open --json number,title,createdAt,updatedAt,labels --limit 50
     echo "=== $repo recent commits (24h) ==="
     gh api repos/miegjorn/$repo/commits --jq '.[0:5] | .[] | "\(.sha[:8]) \(.commit.message | split("\n")[0])"'
   done
   ```

5. Fetch open PRs across all repos:
   ```
   for repo in Gardian Fondament Farga Amassada Charradissa Cor Caissa Occitan; do
     gh pr list --repo miegjorn/$repo --state open --json number,title,createdAt,labels
   done
   ```

---

## PHASE 2: SYNTHESIZE (think before acting)

Using what you gathered, reason through:

- **What was actually built or fixed in the past 24h?** (from Farga signals + commits)
- **What improvement opportunities exist that are NOT already tracked as open issues?**
  Focus on: doc drift between code and README, unclosed stubs, missing integrations,
  architectural gaps that became visible from yesterday's activity.
- **Cross-repo implications**: does a change in one repo create a gap in another?
  (e.g. a new Amassada endpoint that Charradissa doesn't call yet)
- **Pattern signals**: recurring themes across multiple signals (e.g. "three signals
  about credential flow" → underlying structural issue)
- **What is the stack dreaming toward?** What does the trajectory imply about what
  should be built next?

For each opportunity you identify, decide:
- Is it actionable enough for a GitHub issue right now?
- Which repo does it belong in?
- What labels? (bug / enhancement / documentation / technical-debt)
- Does a similar open issue already exist? (check before creating)

---

## PHASE 3: ACT

**For each actionable improvement opportunity** (aim for 3–8, quality over quantity):

1. Verify no duplicate exists: `gh issue list --repo miegjorn/<repo> --state open --search "<key term>"`
2. Create the issue:
   ```
   gh issue create \
     --repo miegjorn/<repo> \
     --title "<concise, specific, actionable title>" \
     --body "Context\n<what was observed and why it matters>\n\nProposed approach\n<what the fix would involve>\n\nSource: identified during nightly dream consolidation {{date}}." \
     --label "<appropriate label(s)>"
   ```
3. Note the created issue URL for the dream report.

**Write the dream report to Farga** using mcp__farga__write_signal:
- project: "{project}"
- source: "dream"
- content: A structured summary including:
  - Date of dream
  - Key observations from the 24h window (3–5 bullet points)
  - Improvement opportunities identified (with reasoning)
  - GitHub issues created (with URLs)
  - Stack trajectory note: what does today's dream imply about where the stack is heading?

**Your written response** is the dream report — concise, substantive, forward-looking.
Do not just narrate what you did. Chronicle what the stack is becoming.
"###,
        project = project
    )
}

// ── Code scan + doc reconciliation ───────────────────────────────────────────

/// POST /trigger/scan — weekly CronJob per component agent.
///
/// Clones the component's GitHub repo, inspects code for TODOs / unimplemented
/// stubs / doc drift, deduplicates against open GitHub issues, creates new
/// issues for gaps found, optionally opens a README PR so Cartulari picks it
/// up, and writes a Farga signal summarising the run.
async fn handle_scan(
    State(state): State<Arc<ListenState>>,
    Json(req): Json<TriggerReq>,
) -> StatusCode {
    tracing::info!("scan trigger received: {}", req.reason);

    tokio::spawn(async move {
        match run_scan(&state).await {
            Ok(_) => tracing::info!("scan complete"),
            Err(e) => tracing::error!("scan failed: {}", e),
        }
    });

    StatusCode::ACCEPTED
}

async fn run_scan(state: &ListenState) -> anyhow::Result<()> {
    let mcp_config = format!(
        r#"{{"mcpServers":{{"farga":{{"type":"http","url":"{}"}}}}}}"#,
        state.farga_mcp_url
    );
    let mcp_path = std::env::temp_dir().join("caissa-scan-mcp.json");
    std::fs::write(&mcp_path, &mcp_config)?;

    let prompt = build_scan_prompt(&state.farga_project);

    let output = tokio::process::Command::new("claude")
        .args([
            "--print",
            &prompt,
            "--model",
            &state.chronicle_model,
            "--mcp-config",
            mcp_path.to_str().unwrap(),
            "--allowed-tools",
            "Bash,mcp__farga__write_signal,mcp__farga__read_context,mcp__farga__search_signals",
        ])
        .env("FARGA_URL", &state.farga_url)
        .env("FARGA_PROJECT", &state.farga_project)
        .output()
        .await?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("scan claude exited with error: {}", stderr);
    }

    let report = String::from_utf8_lossy(&output.stdout).to_string();
    if !report.trim().is_empty() {
        post_signal(state, &report).await?;
    }

    Ok(())
}

fn build_scan_prompt(component: &str) -> String {
    // GitHub repo name: capitalize first letter of component slug.
    let repo = {
        let mut c = component.chars();
        match c.next() {
            None => String::new(),
            Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
        }
    };

    format!(
        r###"You are the {component} component agent running a weekly code scan and
documentation reconciliation for the `miegjorn/{repo}` repository.

Your job has four phases. Complete them in order.

---

## PHASE 1: READ FARGA CONTEXT

Read the current project context so you understand what is intentionally deferred
vs genuinely missing:

- `mcp__farga__read_context` (project: "{component}")
- `mcp__farga__search_signals` (project: "{component}") — look for recent scan signals
  to avoid re-filing issues already created in the last 7 days

---

## PHASE 2: CLONE AND INSPECT THE CODEBASE

```bash
cd /tmp && rm -rf scan-{component} && gh repo clone miegjorn/{repo} scan-{component} -- --depth=1 2>&1
cd /tmp/scan-{component}
```

Run these inspections and collect the findings:

**2a. Code stubs and deferred work:**
```bash
grep -rn \
  --include="*.rs" --include="*.ts" --include="*.js" --include="*.py" --include="*.go" \
  -E "TODO|FIXME|HACK|unimplemented!\(\)|todo!\(\)|panic!\(\"not implemented\"\)|raise NotImplementedError" \
  . | grep -v "\.git/" | grep -v "/target/" | grep -v "node_modules/"
```

**2b. Spec/doc references in README that may not exist in code:**
```bash
# Extract endpoint paths, function names, config keys from README
grep -E "^#+|`[A-Z_]{{3,}}`|POST |GET |PUT |DELETE |\bfn [a-z]|\[.*\]\(#" README.md 2>/dev/null || true
```

**2c. Public API surface in code not mentioned in README:**
```bash
# Rust: public functions and structs
grep -rn --include="*.rs" -E "^pub (async )?fn |^pub struct " src/ 2>/dev/null | head -40 || true
# TypeScript: exported functions
grep -rn --include="*.ts" -E "^export (async )?function |^export class " src/ 2>/dev/null | head -40 || true
```

**2d. Config keys referenced in code vs documented:**
```bash
grep -rn --include="*.rs" --include="*.ts" -E \
  "env::var\(|process\.env\.|std::env::var" . | grep -v "\.git/" | grep -v target/ | head -30 || true
```

**2e. Read the full README:**
```bash
cat README.md 2>/dev/null || echo "No README.md found"
```

---

## PHASE 3: CLOSE RESOLVED ISSUES

Before creating new issues, check whether previously-filed scan issues are now resolved.
This prevents the queue from accumulating stale work.

```bash
# List open issues previously created by the scan (last 90 days)
gh issue list --repo miegjorn/{repo} --state open --label "technical-debt" \
  --search "weekly code scan" --json number,title,body --limit 30
```

For each open scan issue:
1. Extract the specific problem described (file path, symbol name, pattern).
2. Check whether it still exists in the cloned repo:
   ```bash
   grep -rn "<pattern from issue>" /tmp/scan-{component}/src/ 2>/dev/null | head -5
   ```
3. If the problem is **gone**: close the issue with evidence:
   ```bash
   gh issue close <number> --repo miegjorn/{repo} \
     --comment "Resolved: pattern no longer present in codebase as of $(date +%Y-%m-%d). Closing."
   ```
4. If it still exists: leave it open (do not re-comment).

---

## PHASE 4: RECONCILE AND CREATE ISSUES

For each gap you identify, before creating an issue:

1. Check for an existing open issue:
   ```bash
   gh issue list --repo miegjorn/{repo} --state open \
     --search "<key term from the gap>" --json number,title | head -5
   ```
2. If no duplicate exists, create an issue:
   ```bash
   gh issue create \
     --repo miegjorn/{repo} \
     --title "<concise, specific title>" \
     --body "## Context\n<what was found and why it matters>\n\n## Proposed fix\n<what the resolution would look like>\n\n## Source\nIdentified by weekly code scan ({component} agent, $(date +%Y-%m-%d))." \
     --label "technical-debt"
   ```

**Issue triage rules:**
- `unimplemented!()` / `todo!()` with no open issue → create `technical-debt` issue
- README documents an endpoint that doesn't exist in code → `documentation` + `bug`
- Public function exists but is undocumented in README → `documentation`
- Config key referenced in code but absent from README/docs → `documentation`
- FIXME/HACK comment that's been there for > 30 days → `technical-debt`

Aim for quality over quantity — file 3–8 issues maximum. If the codebase is clean,
say so and file nothing.

---

## PHASE 5: WRITE FARGA SIGNAL

Write a scan signal using `mcp__farga__write_signal`:
- project: "{component}"
- source: "scan"
- content: structured summary:
  - Date of scan
  - Stubs/TODOs found (count + worst offenders)
  - Doc drift identified (count)
  - Issues created (with URLs)
  - Issues skipped (already existed)
  - Overall health assessment: clean / minor gaps / significant gaps

Your written response IS the scan report. Be precise and brief.
"###,
        component = component,
        repo = repo,
    )
}

// ── Matrix reply ──────────────────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct MatrixReplyReq {
    pub room_id: String,
    pub sender: String,
    pub content: String,
    #[serde(default)]
    pub history: Vec<MatrixHistoryEntry>,
    /// Originating Matrix event id, when Charradissa forwards it. Used as the
    /// `triggered_by` handle in the O-4 handoff traceability signal; falls back
    /// to `sender` when absent.
    #[serde(default)]
    pub event_id: Option<String>,
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
    // K-1: intercept `@guilhem handoff ...` BEFORE the conversational flow.
    // A handoff is a mechanical dispatch — it must not spawn a sidecar or
    // consume conversational context. The reply text we return here is the
    // immediate acknowledgement (dispatch accepted / rejected); the job's
    // result is posted out-of-band by a background poller (O-3 / K-2).
    if is_handoff_message(&req.content) {
        let text = handle_handoff(&state, &req).await;
        return (axum::http::StatusCode::OK, Json(MatrixReplyResp { text }));
    }

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

/// Assemble the system prompt and skills for a Matrix reply session using the
/// Fondament resolver path for `fondament/guilhem+deconstructive`.
///
/// Returns `(system_prompt, skills)`. Skills come from the role definition's
/// `skills:` list; they are empty if the definition is missing or declares none.
/// The supply-chain decision for vendoring skills into the image was tracked in
/// Caissa#13 (now closed). Decision: defer — the skills list is wired here so
/// that the bake-in doesn't require a code change (only an image change), but
/// skills are not currently baked into the image. See install.md for details.
///
/// Falls back to a bare prompt if the definition file is missing (e.g. outside
/// the built image, in local dev without a Fondament checkout at fondament_path).
fn resolve_guilhem_prompt(fondament_path: &str, generation: &str, room_id: &str) -> (String, Vec<String>) {
    let (role_context, skills) = match load_fondament_def(fondament_path, generation) {
        Ok(def) => (def.context, def.skills),
        Err(e) => {
            tracing::warn!("fondament def not found for '{}' at '{}': {}; using bare prompt", generation, fondament_path, e);
            (String::from("You are Guilhem, the org agent for the Occitan stack."), vec![])
        }
    };

    let deconstructive_preamble = "\
--- injected by deconstructive discipline ---\n\
You are composed of the following parts:\n\
  - [role: guilhem]\n\
\n\
Before producing any response:\n\
1. Become each part sequentially. Reason from its corpus alone.\n\
2. Name the tensions between parts explicitly.\n\
3. If a gap surfaces that no part of you owns, output it typed:\n\
   GAP { domain: \"...\", question: \"...\", blocking: true/false }\n\
4. Recompose. Collapse to your public response from that synthesis.\n\
\n\
Your public response reflects the recomposed whole.\n\
The internal debate is yours alone — it does not appear in output.\n\
--- end injection ---";

    let prompt = format!(
        "{}\n\n{}\n\nYou are replying in Matrix room {}.",
        deconstructive_preamble,
        role_context.trim_end(),
        room_id,
    );
    (prompt, skills)
}

async fn run_matrix_reply(state: &ListenState, req: &MatrixReplyReq) -> anyhow::Result<String> {
    // Phase 1: get or create the per-room process handle under the outer map
    // lock. The outer lock is held across the spawn() await (fast — just a
    // fork), but is released BEFORE the Claude API call so different rooms
    // can run in parallel.
    let process_arc: std::sync::Arc<tokio::sync::Mutex<SidecarProcess>> = {
        let mut sessions = state.room_sessions.lock().await;

        // If a session exists but its sidecar has died (crash, OOM, fatal SDK
        // error), drop the stale entry so we fall through to a fresh spawn.
        // try_lock() is non-blocking: if another handler is mid-call the
        // process IS alive, so we skip the is_alive() check safely.
        if let Some(session) = sessions.get_mut(&req.room_id) {
            let dead = session.process.try_lock()
                .map(|mut p| !p.is_alive())
                .unwrap_or(false); // locked by another handler → alive
            if dead {
                tracing::warn!("sidecar for room {} has died; respawning", req.room_id);
                sessions.remove(&req.room_id);
            }
        }

        if !sessions.contains_key(&req.room_id) {
            // The sidecar process is long-lived (one per room, reused across
            // messages) — always attach the full tool/MCP set so capability
            // doesn't get frozen at whatever the room's first message needed.
            let (system_prompt, skills) = resolve_guilhem_prompt(&state.fondament_path, &state.generation, &req.room_id);
            let init = SidecarInit {
                system_prompt,
                model: state.matrix_model.clone(),
                allowed_tools: guilhem_allowed_tools(),
                skills,
                mcp_servers: guilhem_mcp_servers(state),
            };

            // spawn() is async but fast (just a fork) — OK to await while
            // holding the outer map lock.
            let process = SidecarProcess::spawn(&init).await?;
            sessions.insert(
                req.room_id.clone(),
                RoomSession {
                    process: std::sync::Arc::new(tokio::sync::Mutex::new(process)),
                    last_activity: std::time::Instant::now(),
                },
            );
        }

        std::sync::Arc::clone(&sessions[&req.room_id].process)
        // outer map lock released here — other rooms can now run in parallel
    };

    // Phase 2: Claude API call — no outer map lock held. Two messages for the
    // same room serialise on process_arc's Mutex; different rooms run freely.
    let reply = {
        let mut process = process_arc.lock().await;
        process.send(&req.room_id, &req.sender, &req.content).await?
    };

    // Phase 3: update last_activity under the outer lock (brief).
    if let Some(session) = state.room_sessions.lock().await.get_mut(&req.room_id) {
        session.last_activity = std::time::Instant::now();
    }

    Ok(reply)
}

/// The MCP servers attached to every Guilhem sidecar session — Farga (memory)
/// and the dispatcher (component-agent routing). Shared by the per-room Matrix
/// path and the single-shot `/turn` path so capability stays in lockstep.
fn guilhem_mcp_servers(state: &ListenState) -> serde_json::Value {
    serde_json::json!({
        "farga": { "type": "http", "url": state.farga_mcp_url },
        "dispatcher": { "type": "http", "url": state.dispatcher_mcp_url },
        "charradissa": { "type": "http", "url": state.charradissa_mcp_url },
        "nervi": { "type": "http", "url": state.nervi_mcp_url },
    })
}

/// The tool allow-list granted to every Guilhem sidecar session. Kept as a
/// single source of truth so the Matrix and `/turn` paths can't drift apart.
fn guilhem_allowed_tools() -> Vec<String> {
    vec![
        "Bash".to_string(), "Edit".to_string(), "Write".to_string(),
        "mcp__farga__search_signals".to_string(),
        "mcp__farga__read_context".to_string(),
        "mcp__farga__list_projects".to_string(),
        "mcp__farga__update_component_todo".to_string(),
        "mcp__farga__write_signal".to_string(),
        "mcp__dispatcher__invoke_agent".to_string(),
        "mcp__dispatcher__get_agent_result".to_string(),
        "mcp__dispatcher__list_agent_specs".to_string(),
        "mcp__charradissa__matrix_send".to_string(),
        "mcp__charradissa__matrix_invite".to_string(),
        "mcp__charradissa__matrix_kick".to_string(),
        "mcp__charradissa__matrix_get_dm".to_string(),
        "mcp__charradissa__matrix_leave".to_string(),
        "mcp__charradissa__matrix_read".to_string(),
        "mcp__nervi__nervi_publish".to_string(),
        "mcp__nervi__nervi_subscribe".to_string(),
        "mcp__charradissa__matrix_request_approval".to_string(),
    ]
}

// ── Handoff Bridge ──────────────────────────────────────────────────────────
//
// `@guilhem handoff domain:X facet:Y task:"..."` messages are intercepted in
// handle_matrix_reply (K-1) and dispatched mechanically: a Farga traceability
// signal is written first (O-4), the dispatcher's invoke_agent is called over
// JSON-RPC (O-2), and a background task polls invoke_agent's job to completion
// and posts the result — or a timeout / error — back to the room (O-3 / K-2).
// No conversational sidecar is spawned. Message parsing lives in handoff.rs.

/// How long the background poller waits for a dispatched job before declaring
/// a timeout and handing the job_id back for manual resumption (K-2).
const HANDOFF_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10 * 60);
/// First poll delay; grows geometrically up to HANDOFF_POLL_MAX.
const HANDOFF_POLL_INITIAL: std::time::Duration = std::time::Duration::from_secs(5);
/// Ceiling on the poll backoff.
const HANDOFF_POLL_MAX: std::time::Duration = std::time::Duration::from_secs(60);
/// Result summaries posted to the room are clipped to this many characters (O-3).
const HANDOFF_SUMMARY_MAX: usize = 500;

/// Terminal/intermediate classification of a dispatcher `get_agent_result` reply.
#[derive(Debug, PartialEq, Eq)]
enum JobStatus {
    /// Job done — carries the result text read back from Farga.
    Completed(String),
    /// Job failed — carries the dispatcher's stated reason.
    Failed(String),
    /// Still running or pending — keep polling.
    Pending,
}

/// Handle a `@guilhem handoff ...` message (K-1 interception target). Returns
/// the immediate Matrix reply text. Never errors out to a 500: every terminal
/// state — parse rejection, dispatch failure, accepted dispatch — produces a
/// Matrix message, honouring Caissa#36's "no silent termination" rule.
async fn handle_handoff(state: &Arc<ListenState>, req: &MatrixReplyReq) -> String {
    let handoff = match parse_handoff_message(&req.content) {
        Ok(h) => h,
        Err(e) => {
            // Validation rejection (unknown domain/facet, empty/missing task).
            // The error renders into an actionable Matrix message.
            tracing::info!("handoff rejected: {}", e);
            return e.to_string();
        }
    };

    // session_id: unique AND traceable — it doubles as the Farga project under
    // which both the O-4 trace signal and the agent's eventual result live, so
    // encode the target into it for at-a-glance scanning.
    let short = uuid::Uuid::new_v4().to_string()[..8].to_string();
    let session_id = format!("handoff-{}-{}-{}", handoff.domain, handoff.facet, short);

    // O-4: traceability signal BEFORE dispatch, so the parent context is
    // recorded even if the dispatch call itself fails.
    if let Err(e) = write_handoff_trace(state, &session_id, req, &handoff).await {
        tracing::warn!("handoff trace signal failed for {}: {}", session_id, e);
    }

    // O-2: dispatch via the dispatcher MCP (plain JSON-RPC over HTTP).
    let job_id = match dispatch_handoff(state, &session_id, &handoff).await {
        Ok(id) => id,
        Err(e) => {
            tracing::error!("handoff dispatch failed for {}: {}", session_id, e);
            let _ = write_handoff_error(state, &session_id, "(none)", &format!("dispatch failed: {}", e)).await;
            return format!(
                "Dispatch échoué — {} (domain:{} facet:{}). Signal Farga d'erreur écrit sous `{}`.",
                e, handoff.domain, handoff.facet, session_id
            );
        }
    };

    // O-3 / K-2: poll the job to completion out-of-band and post the result
    // (or timeout/error) to the room. The immediate reply below is the ack.
    let state_bg = Arc::clone(state);
    let room_id = req.room_id.clone();
    let job_bg = job_id.clone();
    let session_bg = session_id.clone();
    tokio::spawn(async move {
        poll_and_report_handoff(&state_bg, &room_id, &job_bg, &session_bg).await;
    });

    format!("Dispatch lancé — job_id: `{}`, session: `{}`", job_id, session_id)
}

/// O-4: write the parent-context traceability signal under the dispatch's own
/// Farga project (`session_id`), so it is colocated with the agent's result.
async fn write_handoff_trace(
    state: &ListenState,
    session_id: &str,
    req: &MatrixReplyReq,
    handoff: &HandoffRequest,
) -> anyhow::Result<()> {
    let triggered_by = req.event_id.clone().unwrap_or_else(|| req.sender.clone());
    let trace = serde_json::json!({
        "session_id": session_id,
        "parent_room": req.room_id,
        "task": handoff.task,
        "dispatched_to": { "domain": handoff.domain, "facet": handoff.facet },
        "triggered_by": triggered_by,
        "timestamp": chrono::Utc::now().to_rfc3339(),
    });
    post_signal_to(state, session_id, &trace.to_string(), "guilhem-handoff-trace").await
}

/// K-2: write an error signal `{job_id, session_id, reason, timestamp}` under
/// the dispatch's Farga project so a timeout or failure leaves a durable trace.
async fn write_handoff_error(
    state: &ListenState,
    session_id: &str,
    job_id: &str,
    reason: &str,
) -> anyhow::Result<()> {
    let err = serde_json::json!({
        "job_id": job_id,
        "session_id": session_id,
        "reason": reason,
        "timestamp": chrono::Utc::now().to_rfc3339(),
    });
    post_signal_to(state, session_id, &err.to_string(), "guilhem-handoff-error").await
}

/// O-2: call the dispatcher's `invoke_agent` and return the job_id. Optional
/// handoff fields map to the dispatcher's arguments: `allowed_tools` passes
/// through verbatim; `context_ref` / `farga_project` are folded into the
/// pre-assembled `context` markdown the agent boots with.
async fn dispatch_handoff(
    state: &ListenState,
    session_id: &str,
    h: &HandoffRequest,
) -> anyhow::Result<String> {
    let mut arguments = serde_json::json!({
        "domain": h.domain,
        "facet": h.facet,
        "task": h.task,
        "session_id": session_id,
    });
    if let Some(tools) = &h.allowed_tools {
        arguments["allowed_tools"] = serde_json::Value::String(tools.clone());
    }
    let context = build_handoff_context(h);
    if !context.is_empty() {
        arguments["context"] = serde_json::Value::String(context);
    }

    let text = dispatcher_tool_call(state, "invoke_agent", arguments).await?;
    parse_job_id(&text)
        .ok_or_else(|| anyhow::anyhow!("dispatcher returned no job_id (raw: {})", text))
}

/// Build the `context` markdown for invoke_agent from the optional handoff
/// fields. Empty when neither `context_ref` nor `farga_project` is present.
fn build_handoff_context(h: &HandoffRequest) -> String {
    let mut ctx = String::new();
    if let Some(cr) = &h.context_ref {
        ctx.push_str(&format!(
            "## Context reference\nBefore starting, load prior context from Farga project `{cr}` \
             with `mcp__farga__read_context` (project: \"{cr}\").\n\n"
        ));
    }
    if let Some(fp) = &h.farga_project {
        ctx.push_str(&format!(
            "## Farga project\nThis work pertains to Farga project `{fp}`. Record durable findings there.\n\n"
        ));
    }
    ctx
}

/// O-3 / K-2: poll `get_agent_result` with geometric backoff until the job
/// completes, fails, or HANDOFF_TIMEOUT elapses — then post exactly one
/// terminal message to the room. Transient poll errors are tolerated (logged,
/// retried) so a blip doesn't masquerade as a job failure.
async fn poll_and_report_handoff(
    state: &ListenState,
    room_id: &str,
    job_id: &str,
    session_id: &str,
) {
    let start = std::time::Instant::now();
    let mut backoff = HANDOFF_POLL_INITIAL;

    loop {
        tokio::time::sleep(backoff).await;
        backoff = std::cmp::min(backoff.mul_f32(1.6), HANDOFF_POLL_MAX);

        let args = serde_json::json!({ "job_id": job_id, "session_id": session_id });
        match dispatcher_tool_call(state, "get_agent_result", args).await {
            Ok(text) => match classify_job_status(&text) {
                JobStatus::Completed(summary) => {
                    let msg = format!(
                        "✓ Job `{}` terminé — session `{}`\n\n{}\n\nFarga : {}",
                        job_id,
                        session_id,
                        truncate_summary(&summary, HANDOFF_SUMMARY_MAX),
                        farga_link(state, session_id),
                    );
                    post_to_matrix_room(room_id, &msg).await;
                    return;
                }
                JobStatus::Failed(reason) => {
                    let _ = write_handoff_error(state, session_id, job_id, &reason).await;
                    let msg = format!(
                        "✗ Job `{}` échec — {} — reprise : `mcp__dispatcher__get_agent_result job_id:{} session_id:{}`",
                        job_id, reason, job_id, session_id
                    );
                    post_to_matrix_room(room_id, &msg).await;
                    return;
                }
                JobStatus::Pending => {}
            },
            Err(e) => {
                tracing::warn!("handoff poll error for job {}: {}", job_id, e);
            }
        }

        if start.elapsed() >= HANDOFF_TIMEOUT {
            let _ = write_handoff_error(state, session_id, job_id, "timeout (10 min)").await;
            let msg = format!(
                "Job `{}` timeout — reprise : `mcp__dispatcher__get_agent_result job_id:{} session_id:{}`",
                job_id, job_id, session_id
            );
            post_to_matrix_room(room_id, &msg).await;
            return;
        }
    }
}

/// Call a dispatcher MCP tool over plain JSON-RPC 2.0 (the dispatcher exposes a
/// stateless `POST /mcp`; no initialize handshake or SSE needed) and return the
/// tool's text content.
async fn dispatcher_tool_call(
    state: &ListenState,
    tool: &str,
    arguments: serde_json::Value,
) -> anyhow::Result<String> {
    let body = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/call",
        "params": { "name": tool, "arguments": arguments },
    });

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()?;
    let resp = client
        .post(&state.dispatcher_mcp_url)
        .json(&body)
        .send()
        .await?
        .error_for_status()?;
    let v: serde_json::Value = resp.json().await?;

    if let Some(err) = v.get("error") {
        let msg = err.get("message").and_then(|m| m.as_str()).unwrap_or("unknown dispatcher error");
        anyhow::bail!("dispatcher: {}", msg);
    }
    v["result"]["content"][0]["text"]
        .as_str()
        .map(|s| s.to_string())
        .ok_or_else(|| anyhow::anyhow!("dispatcher response missing text content"))
}

/// Extract the `job_id:` value from invoke_agent's text result.
fn parse_job_id(text: &str) -> Option<String> {
    text.lines()
        .find_map(|line| line.trim().strip_prefix("job_id:").map(|v| v.trim().to_string()))
        .filter(|s| !s.is_empty())
}

/// Map a `get_agent_result` reply to a [`JobStatus`]. The dispatcher prefixes
/// its reply with `status: completed|failed|running|pending`.
fn classify_job_status(text: &str) -> JobStatus {
    let t = text.trim_start();
    if let Some(rest) = t.strip_prefix("status: completed") {
        JobStatus::Completed(rest.trim().to_string())
    } else if let Some(rest) = t.strip_prefix("status: failed") {
        let reason = rest.trim();
        JobStatus::Failed(if reason.is_empty() { "job failed".to_string() } else { reason.to_string() })
    } else {
        JobStatus::Pending
    }
}

/// Clip a result summary to `max` characters (char-safe), appending an ellipsis
/// when truncated.
fn truncate_summary(s: &str, max: usize) -> String {
    let s = s.trim();
    if s.chars().count() <= max {
        return s.to_string();
    }
    let kept: String = s.chars().take(max.saturating_sub(1)).collect();
    format!("{}…", kept)
}

/// A retrievable link to the dispatch's Farga signals (parent trace + result).
fn farga_link(state: &ListenState, session_id: &str) -> String {
    format!("{}/signals/recent?project={}", state.farga_url, session_id)
}

/// Post a message directly to a Matrix room via the Synapse admin API — the
/// same credential path the SRE/backlog/dream posts use. Best-effort: failures
/// are logged, not propagated, since this runs in a detached poller.
async fn post_to_matrix_room(room_id: &str, body: &str) {
    let synapse_url = std::env::var("SYNAPSE_URL")
        .unwrap_or_else(|_| "http://synapse.occitan-system.svc.cluster.local:8008".into());
    let admin_token = std::env::var("SYNAPSE_ADMIN_TOKEN").unwrap_or_default();
    if admin_token.is_empty() {
        tracing::error!("handoff: SYNAPSE_ADMIN_TOKEN not set — cannot post to room {}", room_id);
        return;
    }

    let client = match reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            tracing::error!("handoff: matrix client build failed: {}", e);
            return;
        }
    };

    let txn = uuid::Uuid::new_v4();
    let url = format!(
        "{}/_matrix/client/v3/rooms/{}/send/m.room.message/{}",
        synapse_url, room_id, txn
    );
    let res = client
        .put(&url)
        .bearer_auth(&admin_token)
        .json(&serde_json::json!({ "msgtype": "m.text", "body": body }))
        .send()
        .await
        .and_then(|r| r.error_for_status());
    if let Err(e) = res {
        tracing::error!("handoff: failed to post to room {}: {}", room_id, e);
    }
}

/// Like [`post_signal`] but with an explicit project and source — used by the
/// handoff traceability (O-4) and error (K-2) signals, which are written under
/// the per-dispatch `session_id` project rather than the daemon's own project.
async fn post_signal_to(
    state: &ListenState,
    project: &str,
    content: &str,
    source: &str,
) -> anyhow::Result<()> {
    let payload = SignalPayload {
        project: project.to_string(),
        signals: vec![SignalItem {
            project: project.to_string(),
            content: content.to_string(),
            source: source.to_string(),
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

// ── Amassada turn ─────────────────────────────────────────────────────────────

/// POST /turn — Amassada orchestrates Guilhem as an "agent-as-endpoint"
/// participant (Option B-full). Unlike `/matrix/reply`, this is single-shot:
/// Amassada owns the conversation and assembles the full context, so each turn
/// spawns a fresh `agent-sidecar.js`, sends one user message, and tears the
/// process down. No per-room session is created or reused.
///
/// The request's `system_prompt` is used verbatim as the sidecar system prompt
/// (Amassada assembles the persona/context, including any deconstructive
/// preamble it wants), while the tool/MCP set and skills mirror the Matrix path
/// so Guilhem has the same capabilities here as in a room.
#[derive(Deserialize)]
struct TurnReq {
    system_prompt: String,
    context: String,
    model: String,
    max_tokens: u32,
}

#[derive(Serialize)]
struct TurnResp {
    text: String,
    input_tokens: u32,
    output_tokens: u32,
}

async fn handle_turn(
    State(state): State<Arc<ListenState>>,
    Json(req): Json<TurnReq>,
) -> Result<Json<TurnResp>, StatusCode> {
    tracing::info!("turn request: model={}, max_tokens={}", req.model, req.max_tokens);

    match run_turn(&state, &req).await {
        Ok(text) => Ok(Json(TurnResp {
            text,
            // The sidecar does not surface token counts yet; see Caissa Farga
            // TODO (caissa-listen). Reported as 0 until that lands.
            input_tokens: 0,
            output_tokens: 0,
        })),
        Err(e) => {
            tracing::error!("turn failed: {}", e);
            Err(StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
}

async fn run_turn(state: &ListenState, req: &TurnReq) -> anyhow::Result<String> {
    // Skills come from the Fondament def (same source as the Matrix path); fall
    // back to none if the definition isn't present (e.g. local dev without a
    // Fondament checkout at fondament_path).
    let skills = match load_fondament_def(&state.fondament_path, &state.generation) {
        Ok(def) => def.skills,
        Err(e) => {
            tracing::warn!(
                "fondament def not found for '{}' at '{}': {}; turn runs without skills",
                state.generation, state.fondament_path, e
            );
            vec![]
        }
    };

    let init = SidecarInit {
        system_prompt: req.system_prompt.clone(),
        model: req.model.clone(),
        allowed_tools: guilhem_allowed_tools(),
        skills,
        mcp_servers: guilhem_mcp_servers(state),
    };

    // Single-shot: spawn, send the assembled context as one user message from
    // "amassada", then tear the process down regardless of outcome.
    let mut process = SidecarProcess::spawn(&init).await?;
    let result = process.send("", "amassada", &req.context).await;
    process.kill();
    result
}

/// Periodically kills and removes any RoomSession that's been idle past
/// the timeout, releasing its sidecar process. Spawned once at startup
/// alongside the existing chronicle/archival background loops.
async fn spawn_idle_reaper(room_sessions: Arc<tokio::sync::Mutex<HashMap<String, RoomSession>>>) {
    const IDLE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30 * 60);
    const SWEEP_INTERVAL: std::time::Duration = std::time::Duration::from_secs(60);

    loop {
        tokio::time::sleep(SWEEP_INTERVAL).await;
        let mut sessions = room_sessions.lock().await;
        let idle_rooms: Vec<String> = sessions
            .iter()
            .filter(|(_, s)| s.is_idle(IDLE_TIMEOUT))
            .map(|(room, _)| room.clone())
            .collect();
        for room in idle_rooms {
            if let Some(session) = sessions.remove(&room) {
                tracing::info!("reaping idle session for room {}", room);
                // try_lock: if a handler is mid-call the Arc keeps the process
                // alive until it finishes; the pipes close when the last Arc
                // clone is dropped, sending EOF/EPIPE to the sidecar naturally.
                if let Ok(mut proc) = session.process.try_lock() {
                    proc.kill();
                }
            }
        }
    }
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

#[cfg(test)]
mod session_supervisor_tests {
    use super::*;
    use std::time::{Duration, Instant};

    #[test]
    fn room_session_is_not_idle_when_recently_active() {
        let session = RoomSession::for_test(Instant::now());
        assert!(!session.is_idle(Duration::from_secs(1800)));
    }

    #[test]
    fn room_session_is_idle_after_timeout_elapsed() {
        let session = RoomSession::for_test(Instant::now() - Duration::from_secs(1801));
        assert!(session.is_idle(Duration::from_secs(1800)));
    }

    #[test]
    fn sidecar_process_is_not_alive_after_child_exits() {
        let session = RoomSession::for_test(Instant::now());

        // /bin/true exits immediately; poll try_wait until the exit is
        // observed (avoids a flaky fixed sleep) using a throwaway runtime,
        // mirroring the pattern RoomSession::for_test uses to spawn it.
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("build runtime for test");
        rt.block_on(async {
            for _ in 0..100 {
                let mut proc = session.process.lock().await;
                if matches!(proc.child.try_wait(), Ok(Some(_))) {
                    break;
                }
                drop(proc);
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            assert!(!session.process.lock().await.is_alive());
        });
    }
}

#[cfg(test)]
mod turn_endpoint_tests {
    use super::*;

    #[test]
    fn turn_req_deserializes_from_amassada_payload() {
        // Mirrors the body Amassada POSTs to /turn.
        let body = r#"{
            "system_prompt": "You are Guilhem.",
            "context": "Pierre-Luc: situate Amassada in the trajectory.",
            "model": "claude-sonnet-4-6",
            "max_tokens": 4096
        }"#;
        let req: TurnReq = serde_json::from_str(body).expect("TurnReq should deserialize");
        assert_eq!(req.system_prompt, "You are Guilhem.");
        assert_eq!(req.context, "Pierre-Luc: situate Amassada in the trajectory.");
        assert_eq!(req.model, "claude-sonnet-4-6");
        assert_eq!(req.max_tokens, 4096);
    }

    #[test]
    fn turn_resp_serializes_with_token_fields() {
        let resp = TurnResp {
            text: "Amassada is the session engine.".to_string(),
            input_tokens: 0,
            output_tokens: 0,
        };
        let value: serde_json::Value =
            serde_json::to_value(&resp).expect("TurnResp should serialize");
        assert_eq!(value["text"], "Amassada is the session engine.");
        assert_eq!(value["input_tokens"], 0);
        assert_eq!(value["output_tokens"], 0);
    }
}

#[cfg(test)]
mod handoff_helper_tests {
    use super::*;

    #[test]
    fn parse_job_id_extracts_from_dispatcher_text() {
        let text = "Agent job dispatched.\njob_id: agent-gardian-developer-ab12cd34\nsession_id: handoff-gardian-developer-ab12cd34\n\nPoll with get_agent_result(...).";
        assert_eq!(
            parse_job_id(text).as_deref(),
            Some("agent-gardian-developer-ab12cd34")
        );
    }

    #[test]
    fn parse_job_id_none_when_absent() {
        assert_eq!(parse_job_id("no id here\nsession_id: x"), None);
    }

    #[test]
    fn classify_completed_carries_result_body() {
        let status = classify_job_status("status: completed\n\nThe token cache now resolves in two hops.");
        assert_eq!(
            status,
            JobStatus::Completed("The token cache now resolves in two hops.".to_string())
        );
    }

    #[test]
    fn classify_failed_carries_reason() {
        let status = classify_job_status("status: failed (check pod logs: kubectl logs ...)");
        assert_eq!(
            status,
            JobStatus::Failed("(check pod logs: kubectl logs ...)".to_string())
        );
    }

    #[test]
    fn classify_running_and_pending_are_pending() {
        assert_eq!(classify_job_status("status: running"), JobStatus::Pending);
        assert_eq!(classify_job_status("status: pending"), JobStatus::Pending);
    }

    #[test]
    fn truncate_summary_leaves_short_text_untouched() {
        assert_eq!(truncate_summary("  short result  ", 500), "short result");
    }

    #[test]
    fn truncate_summary_clips_long_text_with_ellipsis() {
        let long = "x".repeat(600);
        let out = truncate_summary(&long, 500);
        assert_eq!(out.chars().count(), 500);
        assert!(out.ends_with('…'));
    }

    #[test]
    fn build_handoff_context_empty_without_optionals() {
        let h = HandoffRequest {
            domain: "gardian".into(),
            facet: "developer".into(),
            task: "do it".into(),
            context_ref: None,
            farga_project: None,
            allowed_tools: None,
        };
        assert!(build_handoff_context(&h).is_empty());
    }

    #[test]
    fn build_handoff_context_folds_in_optionals() {
        let h = HandoffRequest {
            domain: "gardian".into(),
            facet: "developer".into(),
            task: "do it".into(),
            context_ref: Some("gardian".into()),
            farga_project: Some("proj-7".into()),
            allowed_tools: None,
        };
        let ctx = build_handoff_context(&h);
        assert!(ctx.contains("Farga project `gardian`"));
        assert!(ctx.contains("project `proj-7`"));
    }
}
