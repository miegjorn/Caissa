/// Agent pod daemon — HTTP listener plus a Nervi-driven chat loop.
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
/// `GET /health` — liveness probe; returns `200 ok`.
///
/// Chat turns no longer arrive over HTTP. `chat_loop::run_chat_loop` (spawned
/// below, alongside the HTTP server) continuously consumes this component's
/// Nervi inbound chat subject (`corrier_core::consume_inbound`) -- every room
/// this component is in, one subscription -- builds fresh context from Farga
/// for each message (no SDK `resume()`, no per-room child process, no
/// in-memory session map), and publishes the reply to Nervi's outbound side
/// (`corrier_core::publish_outbound`). Corrièr's write gateway delivers it to
/// Matrix; this pod never touches a Matrix credential. This replaces the
/// former `agent-sidecar.js` child-process-per-room model and its
/// `/matrix/reply`, `/turn`, and `/room-status` HTTP routes entirely -- see
/// `chat_loop.rs`.
///
/// Token usage is proportional to actual events for chronicle triggers; chat
/// turns cost tokens per message (no idle-session cost, since there is no
/// idle session).

use axum::{extract::State, http::StatusCode, routing::post, Json, Router};
use serde::{Deserialize, Serialize};

// External imports re-exported so submodules pick them up via `use super::*`.
pub(crate) use std::sync::Arc;
pub(crate) use caissa_core::config::load_config;
pub(crate) use caissa_core::agent::{fetch_fondament_def, tool_to_claude_name};
pub(crate) use super::handoff::{is_handoff_message, parse_handoff_message, HandoffRequest};

// Focused submodules split out of the former monolithic listen.rs (Caissa#55).
mod cron_triggers;
mod queue_triggers;
mod chat_loop;
mod agent_prompt;
mod matrix_admin;
mod component_agent;
mod handoff;

// Re-export submodule items so siblings resolve each other through `use super::*`.
pub(crate) use cron_triggers::*;
pub(crate) use queue_triggers::*;
pub(crate) use chat_loop::*;
pub(crate) use agent_prompt::*;
pub(crate) use matrix_admin::*;
pub(crate) use component_agent::*;
pub(crate) use handoff::*;

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
    /// This pod's own component identity — used to resolve its Fondament
    /// persona (see agent_prompt::resolve_agent_prompt) and as the routing
    /// key for its Nervi inbound/outbound chat subjects (see chat_loop.rs,
    /// corrier_core::{consume_inbound, publish_outbound}). Defaults from
    /// `config.generation` with any `-agent` suffix trimmed, matching
    /// `repo_to_component`'s convention in `sync.rs`.
    component_name: String,
    /// NATS/JetStream broker URL for this pod's Nervi chat-loop connection
    /// (see chat_loop::run_chat_loop). Corrièr's own gateways connect to the
    /// same broker under the same env var.
    nats_url: String,
}

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
        component_name: config.generation.strip_suffix("-agent").unwrap_or(&config.generation).to_string(),
        generation: config.generation,
        sre_matrix_room_id: std::env::var("SRE_MATRIX_ROOM_ID").unwrap_or_default(),
        backlog_matrix_room_id: std::env::var("BACKLOG_MATRIX_ROOM_ID").unwrap_or_default(),
        dream_model: config.dream_model,
        dream_matrix_room_id: std::env::var("DREAM_MATRIX_ROOM_ID").unwrap_or_default(),
        nats_url: std::env::var("NATS_URL")
            .unwrap_or_else(|_| "nats://nats.occitan-system.svc.cluster.local:4222".into()),
    });

    tokio::spawn(run_nervi_loop_if_component(Arc::clone(&state)));
    tokio::spawn(chat_loop::run_chat_loop(Arc::clone(&state)));

    let app = Router::new()
        .route("/trigger/chronicle", post(handle_chronicle))
        .route("/trigger/sre-alert", post(handle_sre_alert))
        .route("/trigger/backlog-review", post(handle_backlog_review))
        .route("/trigger/dream", post(handle_dream))
        .route("/trigger/dispatch", post(handle_dispatch))
        .route("/trigger/mission-pulse", post(handle_mission_pulse))
        .route("/trigger/intake", post(handle_intake))
        .route("/trigger/scan", post(handle_scan))
        .route("/health", axum::routing::get(|| async { "ok" }))
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

