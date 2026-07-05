//! Each agent pod's perceive loop, replacing the sidecar/session-map model
//! entirely (Occitan spec 2026-07-04-corrier-matrix-nervi-gateway-design.md).
//! `run_perceive_loop` fans out into four sibling streams sharing one Nervi
//! connection: chat (this component's inbound chat subject, all its rooms,
//! per corrier_core::consume_inbound), dispatch orders
//! (occitan.dispatch.<component>), self-scheduled ticks
//! (occitan.tick.<component>.*), and (guilhem only) reactive SRE alerts
//! (occitan.sre.alerts). Every turn builds fresh context from Farga -- no
//! SDK resume(), no per-room child process, no in-memory session map. Chat
//! replies publish back to Nervi's outbound side; Corrièr's write gateway
//! delivers them to Matrix -- this pod never touches a Matrix credential.
//! Dispatch/tick/sre-alert turns have no reply channel -- their tool use is
//! their visible effect.

use super::*;
use corrier_core::{
    consume_inbound, dispatch_subject, publish_outbound, ChatMessage, ChatReply, PerceivedMessage,
    SRE_ALERT_SUBJECT,
};
use futures::StreamExt;

/// One non-resumed Claude Agent SDK turn: spawn `claude --print`, write the
/// full prompt to stdin, read the reply from stdout, exit. Replaces
/// SidecarProcess's persistent child-process-per-room model -- every turn
/// gets a fresh process, matching the one-shot pattern the existing
/// /trigger/* handlers (cron_triggers.rs, queue_triggers.rs) already use for
/// non-conversational work. No resume(), no continuity assumption beyond
/// what build_graph_context already reconstructs from Farga.
///
/// Wires `--mcp-config`/`--allowed-tools` the same way every other trigger
/// path in this module does (see cron_triggers::run_sre_alert,
/// queue_triggers::run_mission_pulse) -- Task 10's original rewrite spawned
/// `claude` directly with neither, so conversational turns had zero MCP
/// tools (Farga, Nervi, dispatcher, charradissa) available at all. The mcp
/// config content is a pure function of `state`, so concurrent turns sharing
/// this fixed temp path is safe -- every writer produces identical bytes.
pub(crate) async fn run_single_turn(
    state: &ListenState,
    system_prompt: &str,
    content: &str,
) -> anyhow::Result<String> {
    let mcp_config = serde_json::to_string(&serde_json::json!({
        "mcpServers": agent_mcp_servers(state)
    }))?;
    let mcp_path = std::env::temp_dir().join(format!("{}-chat-loop-mcp.json", state.component_name));
    std::fs::write(&mcp_path, &mcp_config)?;

    let tools = agent_allowed_tools(&state.fondament_url, state).await.join(",");

    let output = tokio::process::Command::new("claude")
        .prefer_oauth_over_api_key()
        .args([
            "--print",
            content,
            "--append-system-prompt",
            system_prompt,
            "--mcp-config",
            mcp_path.to_str().unwrap(),
            "--allowed-tools",
            &tools,
        ])
        .env("FARGA_URL", &state.farga_url)
        .env("FARGA_PROJECT", &state.farga_project)
        .envs(github_token_envs())
        .output()
        .await?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("claude --print exited with error: {}", stderr);
    }

    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

pub(crate) async fn run_perceive_loop(state: Arc<ListenState>) {
    let nervi = match nervi_core::NerviClient::connect(&state.nats_url).await {
        Ok(c) => c,
        Err(e) => {
            tracing::error!("perceive_loop: failed to connect to NATS at {}: {}", state.nats_url, e);
            return;
        }
    };

    tokio::spawn(run_chat_stream(Arc::clone(&state), nervi.clone()));
    tokio::spawn(run_dispatch_stream(Arc::clone(&state), nervi.clone()));
    tokio::spawn(run_tick_stream(Arc::clone(&state), nervi.clone()));
    tokio::spawn(run_sre_alert_stream(Arc::clone(&state), nervi));
}

async fn run_chat_stream(state: Arc<ListenState>, nervi: nervi_core::NerviClient) {
    let mut stream = match consume_inbound(&nervi, &state.component_name).await {
        Ok(s) => Box::pin(s),
        Err(e) => {
            tracing::error!("chat stream: failed to open inbound consumer: {}", e);
            return;
        }
    };

    while let Some(result) = stream.next().await {
        match result {
            Ok(msg) => {
                let state = Arc::clone(&state);
                let nervi = nervi.clone();
                tokio::spawn(async move {
                    if let Err(e) = handle_chat_message(&state, &nervi, msg).await {
                        tracing::error!("chat_loop: turn failed: {}", e);
                    }
                });
            }
            Err(e) => tracing::warn!("chat stream: failed to decode inbound message (non-fatal): {}", e),
        }
    }
}

/// Perceives dispatch orders addressed to this component (today: Guilhem's
/// dream/sre-alert prompts publishing to occitan.dispatch.<component>).
/// Dispatch orders spawn a turn the same shape as a chat message -- built
/// fresh from Farga context, no session resume -- but with no Matrix room
/// to reply into; the turn's own tool use (nervi_publish, gh, etc.) is its
/// visible effect, not a queued reply.
async fn run_dispatch_stream(state: Arc<ListenState>, nervi: nervi_core::NerviClient) {
    let subject = dispatch_subject(&state.component_name);
    let durable_name = format!("perceive-dispatch-{}", state.component_name);
    let mut stream = match nervi.consume_durable(&subject, &durable_name).await {
        Ok(s) => Box::pin(s),
        Err(e) => {
            tracing::error!("dispatch stream: failed to open consumer for {}: {}", subject, e);
            return;
        }
    };

    while let Some(result) = stream.next().await {
        match result {
            Ok(raw) => {
                let parsed: Result<PerceivedMessage, _> = serde_json::from_str(&raw.payload);
                match parsed {
                    Ok(PerceivedMessage::Dispatch { task, dispatched_by, risk_class }) => {
                        let state = Arc::clone(&state);
                        tokio::spawn(async move {
                            if let Err(e) = handle_dispatch_order(&state, &task, &dispatched_by, risk_class).await {
                                tracing::error!("dispatch_stream: turn failed: {}", e);
                            }
                        });
                    }
                    Ok(other) => tracing::warn!("dispatch stream: unexpected message variant on {}: {:?}", subject, other),
                    Err(e) => tracing::warn!("dispatch stream: failed to decode message (non-fatal): {}", e),
                }
            }
            Err(e) => tracing::warn!("dispatch stream: delivery error (non-fatal): {}", e),
        }
    }
}

async fn handle_dispatch_order(
    state: &ListenState,
    task: &str,
    dispatched_by: &str,
    risk_class: u8,
) -> anyhow::Result<()> {
    let (system_prompt, _skills, _models, _is_aporia, _budget) =
        crate::commands::listen::resolve_agent_prompt(&state.fondament_url, &state.generation, "dispatch").await;
    let content = format!(
        "Dispatch order from {} (risk class {}): {}",
        dispatched_by, risk_class, task
    );
    crate::commands::listen::run_single_turn(state, &system_prompt, &content).await?;
    Ok(())
}

/// Perceives this component's self-scheduled periodic-skill ticks
/// (Task 2's tick-poller). One durable consumer covers every skill this
/// component runs (chronicle.dream.mission-pulse), routed by `skill` name.
async fn run_tick_stream(state: Arc<ListenState>, nervi: nervi_core::NerviClient) {
    let wildcard = format!("occitan.tick.{}.>", state.component_name);
    let durable_name = format!("perceive-tick-{}", state.component_name);
    let mut stream = match nervi.consume_durable(&wildcard, &durable_name).await {
        Ok(s) => Box::pin(s),
        Err(e) => {
            tracing::error!("tick stream: failed to open consumer for {}: {}", wildcard, e);
            return;
        }
    };

    while let Some(result) = stream.next().await {
        match result {
            Ok(raw) => {
                let parsed: Result<PerceivedMessage, _> = serde_json::from_str(&raw.payload);
                match parsed {
                    Ok(PerceivedMessage::Tick { skill }) => {
                        let state = Arc::clone(&state);
                        tokio::spawn(async move {
                            let result = match skill.as_str() {
                                "chronicle" => {
                                    let prompt = crate::commands::listen::build_chronicle_prompt(
                                        &state.fondament_path, "self-paced tick", &state.farga_project,
                                    );
                                    crate::commands::listen::run_chronicle(&state, &prompt).await
                                }
                                "dream" => crate::commands::listen::run_dream(&state).await,
                                "mission-pulse" => crate::commands::listen::run_mission_pulse(&state).await,
                                other => {
                                    tracing::warn!("tick stream: unknown skill '{}', ignoring", other);
                                    Ok(())
                                }
                            };
                            if let Err(e) = result {
                                tracing::error!("tick_stream: {} run failed: {}", skill, e);
                            }
                        });
                    }
                    Ok(other) => tracing::warn!("tick stream: unexpected message variant on {}: {:?}", wildcard, other),
                    Err(e) => tracing::warn!("tick stream: failed to decode message (non-fatal): {}", e),
                }
            }
            Err(e) => tracing::warn!("tick stream: delivery error (non-fatal): {}", e),
        }
    }
}

/// Perceives the SRE watchdog's reactive anomaly pushes -- purely
/// event-driven, no poll cadence (Task 4 makes the watchdog publish here
/// the instant it detects something, replacing the former 30-minute
/// CronWorkflow poll entirely).
async fn run_sre_alert_stream(state: Arc<ListenState>, nervi: nervi_core::NerviClient) {
    if state.component_name != "guilhem" {
        return; // SRE alerting is guilhem-only, matching today's behavior
    }
    let durable_name = "perceive-sre-alert-guilhem".to_string();
    let mut stream = match nervi.consume_durable(SRE_ALERT_SUBJECT, &durable_name).await {
        Ok(s) => Box::pin(s),
        Err(e) => {
            tracing::error!("sre-alert stream: failed to open consumer for {}: {}", SRE_ALERT_SUBJECT, e);
            return;
        }
    };

    while let Some(result) = stream.next().await {
        match result {
            Ok(raw) => {
                let parsed: Result<PerceivedMessage, _> = serde_json::from_str(&raw.payload);
                match parsed {
                    Ok(PerceivedMessage::SreAlert { anomalies }) => {
                        let state = Arc::clone(&state);
                        tokio::spawn(async move {
                            if let Err(e) = crate::commands::listen::run_sre_alert(&state, &anomalies).await {
                                tracing::error!("sre_alert_stream: run failed: {}", e);
                            }
                        });
                    }
                    Ok(other) => tracing::warn!("sre-alert stream: unexpected message variant: {:?}", other),
                    Err(e) => tracing::warn!("sre-alert stream: delivery error (non-fatal): {}", e),
                }
            }
            Err(e) => tracing::warn!("sre-alert stream: delivery error (non-fatal): {}", e),
        }
    }
}

async fn handle_chat_message(
    state: &Arc<ListenState>,
    nervi: &nervi_core::NerviClient,
    msg: ChatMessage,
) -> anyhow::Result<()> {
    // K-1: intercept `@guilhem handoff ...` BEFORE the conversational flow.
    // A handoff is a mechanical dispatch -- it must not consume conversational
    // context or spawn a full turn. Restored here after Task 10's chat_loop.rs
    // rewrite dropped this check (a real regression caught in review, not an
    // intentional removal) -- same interception point as the old
    // handle_matrix_reply, just moved into the new Nervi-driven message path.
    if is_handoff_message(&msg.content) {
        let req = MatrixReplyReq {
            room_id: msg.conversation_id.clone(),
            sender: msg.sender.clone(),
            content: msg.content.clone(),
            history: vec![],
            event_id: msg.external_event_id.clone(),
        };
        let text = handle_handoff(state, &req).await;
        let reply = ChatReply {
            conversation_id: msg.conversation_id,
            content: text,
            adapter: msg.adapter,
        };
        return publish_outbound(nervi, &state.component_name, &reply).await;
    }

    // Fresh context every turn, from Farga -- no resumed SDK session to lean
    // on. This is the seam the spec's Agent pods section names as the
    // natural home for trajectory-conditioned context collapse; this task
    // only needs to remove the structural reason collapse couldn't
    // previously be tried here, not implement it.
    let api_key = std::env::var("ANTHROPIC_API_KEY").ok();
    let graph_context = caissa_core::graph_context::build_graph_context(
        &state.farga_url, &msg.conversation_id, &msg.sender, &msg.content, api_key,
    ).await;

    let (system_prompt, _skills, _def_models, _is_aporia, _thinking_budget) =
        crate::commands::listen::resolve_agent_prompt(&state.fondament_url, &state.generation, &msg.conversation_id).await;

    let turn_block = crate::commands::listen::build_turn_context_block(false, graph_context.as_ref());
    let content = if turn_block.is_empty() {
        msg.content.clone()
    } else {
        format!("{}\n\n{}", turn_block, msg.content)
    };

    let reply_text = crate::commands::listen::run_single_turn(state, &system_prompt, &content).await?;

    let reply = ChatReply {
        conversation_id: msg.conversation_id,
        content: reply_text,
        adapter: msg.adapter,
    };
    publish_outbound(nervi, &state.component_name, &reply).await
}

#[cfg(test)]
mod tests {
    // handle_chat_message's own network calls (Farga, fondament-server,
    // Anthropic) make it unsuitable for a fast unit test without a mock
    // server -- covered instead by the existing graph_context and
    // resolve_agent_prompt test suites (unchanged by this task) plus this
    // plan's Task 13 live-verification step. This module intentionally has
    // no inline tests beyond confirming it compiles against the real
    // corrier_core/nervi_core types, which `cargo build` already proves.
}
