use super::*;
use serde::{Deserialize, Serialize};

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
pub(crate) struct TurnReq {
    system_prompt: String,
    context: String,
    model: String,
    max_tokens: u32,
}

#[derive(Serialize)]
pub(crate) struct TurnResp {
    text: String,
    input_tokens: u32,
    output_tokens: u32,
}

pub(crate) async fn handle_turn(
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

pub(crate) async fn run_turn(state: &ListenState, req: &TurnReq) -> anyhow::Result<String> {
    // Skills come from the Fondament def (same source as the Matrix path); fall
    // back to none if fondament-server is unreachable or the definition
    // doesn't exist there.
    let skills = match fetch_fondament_def(&state.fondament_url, &state.generation).await {
        Ok(def) => def.skill_ids(),
        Err(e) => {
            tracing::warn!(
                "fondament def not found for '{}' via '{}': {}; turn runs without skills",
                state.generation, state.fondament_url, e
            );
            vec![]
        }
    };

    let init = SidecarInit {
        system_prompt: req.system_prompt.clone(),
        model: req.model.clone(),
        allowed_tools: guilhem_allowed_tools(&state.fondament_url).await,
        skills,
        mcp_servers: guilhem_mcp_servers(state),
        // Amassada assembles the full system_prompt for this path itself (it
        // is not resolve_guilhem_prompt's aporia-by-default output) — out of
        // scope for tonight's per-agent-matrix-independence follow-up, which
        // is specifically about the 9 agents' own persistent Matrix sessions.
        max_thinking_tokens: None,
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
#[cfg(test)]
mod turn_endpoint_tests {
    use super::*;
    use crate::commands::listen::*;

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

