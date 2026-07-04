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
/// `GET /health` — liveness probe; returns `200 ok`. Stateless -- has no
/// per-room awareness, so it cannot detect a hung (not crashed) sidecar. See
/// `GET /room-status` below for that.
///
/// `GET /room-status` — per-room diagnostic: whether each room's sidecar
/// process is alive, how long since its last message, and how long the
/// current turn (if any) has been in flight. Added after a hung sidecar went
/// undetected for hours in production (2026-07-04): the SRE watchdog only
/// polled the blanket `/health` above, which has no way to see a room stuck
/// mid-turn.
///
/// Token usage is proportional to actual events for chronicle; Matrix sessions
/// cost tokens for as long as a room stays active (up to the idle timeout).

use axum::{extract::State, http::StatusCode, routing::post, Json, Router};
use serde::{Deserialize, Serialize};

// External imports re-exported so submodules pick them up via `use super::*`.
pub(crate) use std::sync::Arc;
pub(crate) use std::collections::HashMap;
pub(crate) use caissa_core::config::load_config;
pub(crate) use caissa_core::agent::{fetch_fondament_def, tool_to_claude_name};
pub(crate) use super::handoff::{is_handoff_message, parse_handoff_message, HandoffRequest};

// Focused submodules split out of the former monolithic listen.rs (Caissa#55).
mod matrix_client;
mod cron_triggers;
mod queue_triggers;
mod matrix_reply;
mod component_agent;
mod handoff;
mod turn_endpoint;
mod session_management;

// Re-export submodule items so siblings resolve each other through `use super::*`.
pub(crate) use matrix_client::*;
pub(crate) use cron_triggers::*;
pub(crate) use queue_triggers::*;
pub(crate) use matrix_reply::*;
pub(crate) use component_agent::*;
pub(crate) use handoff::*;
pub(crate) use turn_endpoint::*;
pub(crate) use session_management::*;

#[derive(Clone)]
#[allow(dead_code)]
pub(crate) struct ListenState {
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
    /// fondament-server URL — resolved live at runtime via fetch_fondament_def,
    /// never a vendored local copy. See caissa_core::agent::fetch_fondament_def.
    fondament_url: String,
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
    /// This pod's own Matrix identity. Empty room_id disables the sync loop
    /// entirely (e.g. local dev without Matrix configured).
    matrix_user: String,
    matrix_room_id: String,
    matrix_homeserver: String,
    matrix_password: String,
    /// Updated in place on every successful login/re-login; read by both the
    /// sync loop and the post-reply call.
    matrix_access_token: Arc<tokio::sync::RwLock<String>>,
    /// Kroki server URL for rendering ```mermaid blocks in a reply as PNG
    /// images instead of posting raw diagram source as text.
    kroki_url: String,
}

/// A running agent-sidecar.js child process for one room.
#[derive(Deserialize)]
pub struct TriggerReq {
    /// Human-readable reason for the chronicle run.
    pub reason: String,
    /// Optional specific prompt override. If absent, uses the default chronicle prompt.
    pub prompt: Option<String>,
}

#[derive(Serialize)]
pub(crate) struct SignalPayload {
    project: String,
    signals: Vec<SignalItem>,
}

#[derive(Serialize)]
pub(crate) struct SignalItem {
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
        fondament_url: config.fondament_url,
        generation: config.generation,
        sre_matrix_room_id: std::env::var("SRE_MATRIX_ROOM_ID").unwrap_or_default(),
        backlog_matrix_room_id: std::env::var("BACKLOG_MATRIX_ROOM_ID").unwrap_or_default(),
        dream_model: config.dream_model,
        dream_matrix_room_id: std::env::var("DREAM_MATRIX_ROOM_ID").unwrap_or_default(),
        room_sessions: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
        matrix_user: std::env::var("MATRIX_USER").unwrap_or_default(),
        matrix_room_id: std::env::var("MATRIX_ROOM_ID").unwrap_or_default(),
        matrix_homeserver: std::env::var("MATRIX_HOMESERVER")
            .unwrap_or_else(|_| "http://synapse.occitan-system.svc.cluster.local:8008".into()),
        matrix_password: read_matrix_password(),
        matrix_access_token: Arc::new(tokio::sync::RwLock::new(String::new())),
        kroki_url: std::env::var("KROKI_URL")
            .unwrap_or_else(|_| "http://kroki.occitan-system.svc.cluster.local:8000".into()),
    });

    tokio::spawn(spawn_idle_reaper(Arc::clone(&state.room_sessions)));
    tokio::spawn(run_nervi_loop_if_component(Arc::clone(&state)));
    tokio::spawn(run_matrix_client_loop(Arc::clone(&state)));

    let app = Router::new()
        .route("/trigger/chronicle", post(handle_chronicle))
        .route("/trigger/sre-alert", post(handle_sre_alert))
        .route("/trigger/backlog-review", post(handle_backlog_review))
        .route("/trigger/dream", post(handle_dream))
        .route("/trigger/dispatch", post(handle_dispatch))
        .route("/trigger/mission-pulse", post(handle_mission_pulse))
        .route("/trigger/intake", post(handle_intake))
        .route("/trigger/scan", post(handle_scan))
        .route("/matrix/reply", post(handle_matrix_reply))
        .route("/turn", post(handle_turn))
        .route("/health", axum::routing::get(|| async { "ok" }))
        .route("/room-status", axum::routing::get(handle_room_status))
        .with_state(state);

    let addr = format!("0.0.0.0:{}", port);
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    eprintln!("[caissa] listening on {}", addr);

    axum::serve(listener, app).await?;
    Ok(())
}

#[derive(Deserialize)]
#[allow(dead_code)]
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
#[allow(dead_code)]
pub struct MatrixHistoryEntry {
    pub sender: String,
    pub content: String,
}

// ── Matrix reply ──────────────────────────────────────────────────────────────

#[derive(Serialize)]
pub struct MatrixReplyResp {
    pub text: String,
}

pub(crate) async fn post_signal(state: &ListenState, content: &str) -> anyhow::Result<()> {
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

