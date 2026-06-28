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

#[derive(Clone)]
struct ListenState {
    farga_url: String,
    farga_project: String,
    farga_mcp_url: String,
    chronicle_model: String,
    matrix_model: String,
    amassada_url: String,
    dispatcher_mcp_url: String,
    fondament_path: String,
    generation: String,
    /// Matrix room ID for SRE alert posts. Empty string = alerting disabled.
    sre_matrix_room_id: String,
    /// Matrix room ID for backlog review posts. Empty string = posting disabled.
    backlog_matrix_room_id: String,
    /// One persistent agent-sidecar.js child process per actively-chatting
    /// Matrix room. Reaped by an idle-timeout sweep (see spawn_idle_reaper).
    room_sessions: Arc<tokio::sync::RwLock<HashMap<String, RoomSession>>>,
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

    async fn send(&mut self, sender: &str, content: &str) -> anyhow::Result<String> {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt};

        let msg = serde_json::json!({ "sender": sender, "content": content });
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
struct RoomSession {
    process: SidecarProcess,
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
            process: SidecarProcess { child, stdin, stdout: tokio::io::BufReader::new(stdout) },
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
        fondament_path: config.fondament_path,
        generation: config.generation,
        sre_matrix_room_id: std::env::var("SRE_MATRIX_ROOM_ID").unwrap_or_default(),
        backlog_matrix_room_id: std::env::var("BACKLOG_MATRIX_ROOM_ID").unwrap_or_default(),
        room_sessions: Arc::new(tokio::sync::RwLock::new(HashMap::new())),
    });

    tokio::spawn(spawn_idle_reaper(Arc::clone(&state.room_sessions)));

    let app = Router::new()
        .route("/trigger/chronicle", post(handle_chronicle))
        .route("/trigger/sre-alert", post(handle_sre_alert))
        .route("/trigger/backlog-review", post(handle_backlog_review))
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
    let mut sessions = state.room_sessions.write().await;

    // If a session exists but its sidecar process has died (crash, OOM, fatal
    // SDK error), drop the stale entry so we fall through to a fresh spawn
    // below instead of writing/reading on a dead pipe.
    if let Some(session) = sessions.get_mut(&req.room_id) {
        if !session.process.is_alive() {
            tracing::warn!("sidecar for room {} has died; respawning", req.room_id);
            sessions.remove(&req.room_id);
        }
    }

    if !sessions.contains_key(&req.room_id) {
        // The sidecar process is now long-lived (one per room, reused across
        // messages), so the MCP handshake cost is paid once per session
        // rather than once per message — always attach the full tool/MCP
        // set so capability doesn't get frozen at whatever the room's first
        // message happened to need.
        let mcp_servers = guilhem_mcp_servers(state);
        let allowed_tools = guilhem_allowed_tools();

        let (system_prompt, skills) = resolve_guilhem_prompt(&state.fondament_path, &state.generation, &req.room_id);
        let init = SidecarInit {
            system_prompt,
            model: state.matrix_model.clone(),
            allowed_tools,
            skills,
            mcp_servers,
        };

        let process = SidecarProcess::spawn(&init).await?;
        sessions.insert(
            req.room_id.clone(),
            RoomSession { process, last_activity: std::time::Instant::now() },
        );
    }

    let session = sessions.get_mut(&req.room_id).expect("just inserted or already present");
    let reply = session.process.send(&req.sender, &req.content).await?;
    session.last_activity = std::time::Instant::now();

    Ok(reply)
}

/// The MCP servers attached to every Guilhem sidecar session — Farga (memory)
/// and the dispatcher (component-agent routing). Shared by the per-room Matrix
/// path and the single-shot `/turn` path so capability stays in lockstep.
fn guilhem_mcp_servers(state: &ListenState) -> serde_json::Value {
    serde_json::json!({
        "farga": { "type": "http", "url": state.farga_mcp_url },
        "dispatcher": { "type": "http", "url": state.dispatcher_mcp_url },
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
    ]
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
    let result = process.send("amassada", &req.context).await;
    process.kill();
    result
}

/// Periodically kills and removes any RoomSession that's been idle past
/// the timeout, releasing its sidecar process. Spawned once at startup
/// alongside the existing chronicle/archival background loops.
async fn spawn_idle_reaper(room_sessions: Arc<tokio::sync::RwLock<HashMap<String, RoomSession>>>) {
    const IDLE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30 * 60);
    const SWEEP_INTERVAL: std::time::Duration = std::time::Duration::from_secs(60);

    loop {
        tokio::time::sleep(SWEEP_INTERVAL).await;
        let mut sessions = room_sessions.write().await;
        let idle_rooms: Vec<String> = sessions
            .iter()
            .filter(|(_, s)| s.is_idle(IDLE_TIMEOUT))
            .map(|(room, _)| room.clone())
            .collect();
        for room in idle_rooms {
            if let Some(mut session) = sessions.remove(&room) {
                tracing::info!("reaping idle session for room {}", room);
                session.process.kill();
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
        let mut session = RoomSession::for_test(Instant::now());

        // /bin/true exits immediately; poll try_wait until the exit is
        // observed (avoids a flaky fixed sleep) using a throwaway runtime,
        // mirroring the pattern RoomSession::for_test uses to spawn it.
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("build runtime for test");
        rt.block_on(async {
            for _ in 0..100 {
                if matches!(session.process.child.try_wait(), Ok(Some(_))) {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        });

        assert!(!session.process.is_alive());
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
