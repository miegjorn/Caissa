/// Guilhem daemon — lightweight webhook listener with two independent handlers.
///
/// `POST /trigger/chronicle` — accepts chronicle trigger events from Argo
/// Workflows, git webhooks, or cron. One-shot: runs `claude --print "<task>"`
/// as a subprocess, posts the output as a Signal to Farga, exits.
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
/// Token usage is proportional to actual events for chronicle; Matrix sessions
/// cost tokens for as long as a room stays active (up to the idle timeout).

use axum::{extract::State, http::StatusCode, routing::post, Json, Router};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::collections::HashMap;
use caissa_core::config::load_config;

#[derive(Clone)]
struct ListenState {
    farga_url: String,
    farga_project: String,
    farga_mcp_url: String,
    chronicle_model: String,
    matrix_model: String,
    amassada_url: String,
    dispatcher_mcp_url: String,
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
        room_sessions: Arc::new(tokio::sync::RwLock::new(HashMap::new())),
    });

    tokio::spawn(spawn_idle_reaper(Arc::clone(&state.room_sessions)));

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
        let mcp_servers = serde_json::json!({
            "farga": { "type": "http", "url": state.farga_mcp_url },
            "dispatcher": { "type": "http", "url": state.dispatcher_mcp_url },
        });
        let allowed_tools = vec![
            "Bash".to_string(), "Edit".to_string(), "Write".to_string(),
            "mcp__farga__search_signals".to_string(),
            "mcp__farga__read_context".to_string(),
            "mcp__farga__list_projects".to_string(),
            "mcp__farga__update_component_todo".to_string(),
            "mcp__dispatcher__invoke_agent".to_string(),
            "mcp__dispatcher__get_agent_result".to_string(),
            "mcp__dispatcher__list_agent_specs".to_string(),
        ];

        let init = SidecarInit {
            system_prompt: format!("You are Guilhem, replying in Matrix room {}.", req.room_id),
            model: state.matrix_model.clone(),
            allowed_tools,
            skills: vec![], // populated from the resolved facet's `skills` list by the caller; empty until Fondament-resolver wiring exists (out of scope, matches tools.always_on's existing manual-relay model)
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
