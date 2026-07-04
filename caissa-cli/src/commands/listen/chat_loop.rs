//! Each agent pod's turn loop, replacing the sidecar/session-map model
//! entirely (Occitan spec 2026-07-04-corrier-matrix-nervi-gateway-design.md).
//! Continuously consumes this component's inbound chat subject (all its
//! rooms, one subscription, per corrier_core::consume_inbound), builds fresh
//! context from Farga for every turn -- no SDK resume(), no per-room child
//! process, no in-memory session map -- and publishes the reply back to
//! Nervi's outbound side. Corrièr's write gateway delivers it to Matrix;
//! this pod never touches a Matrix credential.

use super::*;
use corrier_core::{consume_inbound, publish_outbound, ChatMessage, ChatReply};
use futures::StreamExt;

/// One non-resumed Claude Agent SDK turn: spawn `claude --print`, write the
/// full prompt to stdin, read the reply from stdout, exit. Replaces
/// SidecarProcess's persistent child-process-per-room model -- every turn
/// gets a fresh process, matching the one-shot pattern the existing
/// /trigger/* handlers (cron_triggers.rs, queue_triggers.rs) already use for
/// non-conversational work. No resume(), no continuity assumption beyond
/// what build_graph_context already reconstructs from Farga.
pub(crate) async fn run_single_turn(system_prompt: &str, content: &str) -> anyhow::Result<String> {
    let output = tokio::process::Command::new("claude")
        .args(["--print", content, "--append-system-prompt", system_prompt])
        .output()
        .await?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("claude --print exited with error: {}", stderr);
    }

    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

pub(crate) async fn run_chat_loop(state: Arc<ListenState>) {
    let nervi = match nervi_core::NerviClient::connect(&state.nats_url).await {
        Ok(c) => c,
        Err(e) => {
            tracing::error!("chat_loop: failed to connect to NATS at {}: {}", state.nats_url, e);
            return;
        }
    };

    let mut stream = match consume_inbound(&nervi, &state.component_name).await {
        Ok(s) => Box::pin(s),
        Err(e) => {
            tracing::error!("chat_loop: failed to open inbound consumer: {}", e);
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
            Err(e) => {
                tracing::warn!("chat_loop: failed to decode inbound message (non-fatal): {}", e);
            }
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

    let reply_text = crate::commands::listen::run_single_turn(&system_prompt, &content).await?;

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
