use super::*;

pub(crate) async fn handle_matrix_reply(
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
/// Returns `(system_prompt, skills, models, is_aporia, thinking_budget)`.
/// Skills come from the role definition's `skills:` list; they are empty if
/// the definition is missing or declares none. The supply-chain decision for
/// vendoring skills into the image was tracked in Caissa#13 (now closed).
/// Decision: defer — the skills list is wired here so that the bake-in
/// doesn't require a code change (only an image change), but skills are not
/// currently baked into the image. See install.md for details.
///
/// Falls back to a bare prompt if fondament-server is unreachable or the
/// definition doesn't exist there.
///
/// Does NOT bake a composed-parts aporia preamble into the (spawn-time-only,
/// static) system prompt — this used to call
/// `build_aporia_preamble(&[])` unconditionally, which for every one of
/// these 9 single-role agents produces the same generic "[role: this
/// agent] — reason from your full context" fallback, indistinguishable in
/// substance from the pre-aporia hardcoded preamble it replaced. The actual
/// composed parts (this room's current Frontier nodes) aren't known until
/// `build_graph_context` runs, and — because the sidecar process is
/// long-lived and resumed, not respawned per turn — that has to happen on
/// every turn, not once at spawn. See `build_turn_context_block`, called
/// from `run_matrix_reply` on every message with the parts that actually
/// exist at that turn.
pub(crate) async fn resolve_guilhem_prompt(fondament_url: &str, generation: &str, room_id: &str) -> (String, Vec<String>, std::collections::HashMap<String, String>, bool, Option<u32>) {
    let (role_context, skills, models, modifiers) = match fetch_fondament_def(fondament_url, generation).await {
        Ok(def) => {
            let skills = def.skill_ids();
            let models = def.models;
            (def.context, skills, models, def.modifiers)
        }
        Err(e) => {
            tracing::warn!("fondament def not found for '{}' via '{}': {}; using bare prompt", generation, fondament_url, e);
            (String::from("You are Guilhem, the org agent for the Occitan stack."), vec![], std::collections::HashMap::new(), vec![])
        }
    };

    // Aporia is the default reasoning discipline for these 9 agents (Occitan
    // per-agent-matrix-independence follow-up) — each of their Fondament
    // definitions now declares `modifiers: [aporia]`.
    let is_aporia = modifiers.iter().any(|m| m == "aporia");
    let thinking_budget = is_aporia.then(|| {
        fondament_core::types::StructuredReasoning::from_parts_count(0).anthropic_budget()
    });

    let discipline_preamble = if is_aporia {
        "\
--- aporia reasoning discipline ---\n\
Every message you receive is prefixed with a snapshot of this room's current\n\
context graph: a list of session-node \"parts\" (this room's live frontier —\n\
open threads, unresolved tensions), each with an activation weight. Before\n\
responding:\n\
1. Become each listed part sequentially. Reason from it alone.\n\
2. Name the tensions between parts explicitly.\n\
3. If a gap surfaces that no part of you owns, output it typed:\n\
   GAP { domain: \"...\", question: \"...\", blocking: true/false }\n\
4. Recompose. Collapse to your public response from that synthesis.\n\
If no parts are listed (a fresh room, or a graph with no frontier yet),\n\
reason from your full context as one whole instead.\n\
Your public response reflects the recomposed whole. The internal debate is\n\
yours alone — it does not appear in output.\n\
--- end discipline ---"
    } else {
        ""
    };

    let context_graph_preamble = "\
--- context graph ---\n\
You have access to Farga's role-scoped context graph via:\n\
  mcp__farga__list_context_nodes (project: \"occitan\", role: \"org\")\n\
  mcp__farga__read_context_node  (path: \"[<component>][<type>]\", role: \"org\")\n\
\n\
When you need to understand a component (its code, its architecture, its constraints),\n\
read its context node before acting. Key nodes:\n\
  [occitan][system-rationale]     — stack-level design constraints (read before any architectural decision)\n\
  [<component>][codebase]         — where its code lives, what its CLAUDE.md says\n\
  [<component>][architecture]     — design summary, interfaces, invariants\n\
\n\
On your FIRST message in this session, call list_context_nodes to orient yourself.\n\
\n\
DISPATCH RULE: invoke_agent requires caller=\"guilhem\" and facet=\"architect\" only.\n\
For all code work, use nervi_publish to occitan.dispatch.<component>.\n\
The dispatcher will reject any other combination — this is a hard guard, not a suggestion.\n\
--- end context graph ---";

    let prompt = format!(
        "{}\n\n{}\n\n{}\n\nYou are replying in Matrix room {}.",
        discipline_preamble,
        role_context.trim_end(),
        context_graph_preamble,
        room_id,
    );
    (prompt, skills, models, is_aporia, thinking_budget)
}

/// Build the per-turn text block prepended to every message sent to a room's
/// sidecar — fresh on every turn, not only at session spawn. Combines the
/// current aporia composed-parts framing (if `is_aporia`) with the collapsed
/// graph context, both derived from `graph`'s Frontier nodes for *this* turn.
/// Returns an empty string when there is nothing to inject (no aporia and no
/// graph context) — callers should skip prepending in that case.
pub(crate) fn build_turn_context_block(is_aporia: bool, graph: Option<&caissa_core::graph_context::GraphContext>) -> String {
    let mut block = String::new();

    if is_aporia {
        let parts: Vec<fondament_core::types::ComposedPart> = graph
            .map(|g| {
                g.frontier_parts.iter()
                    .map(|(summary, weight)| fondament_core::types::ComposedPart {
                        kind: fondament_core::types::PartKind::SessionNode,
                        name: summary.clone(),
                        weight: *weight,
                        corpus_ref: None,
                    })
                    .collect()
            })
            .unwrap_or_default();
        block.push_str(&fondament_core::resolver::build_aporia_preamble(&parts));
    }

    if let Some(g) = graph {
        if !block.is_empty() {
            block.push_str("\n\n");
        }
        block.push_str("--- prior context (collapsed) ---\n");
        block.push_str(&g.collapsed);
        block.push_str("\n--- end prior context ---");
    }

    block
}

#[cfg(test)]
mod turn_context_block_tests {
    use super::*;
    use crate::commands::listen::*;
    use caissa_core::graph_context::GraphContext;

    #[test]
    fn non_aporia_no_graph_produces_empty_block() {
        assert_eq!(build_turn_context_block(false, None), "");
    }

    #[test]
    fn non_aporia_with_graph_carries_only_collapsed_context() {
        let g = GraphContext { collapsed: "N1: prior thread".into(), frontier_parts: vec![] };
        let block = build_turn_context_block(false, Some(&g));
        assert!(block.contains("prior context (collapsed)"));
        assert!(block.contains("N1: prior thread"));
        assert!(!block.contains("aporia"));
    }

    #[test]
    fn aporia_with_no_graph_uses_empty_parts_fallback() {
        let block = build_turn_context_block(true, None);
        assert!(block.contains("aporia reasoning discipline") || block.contains("composed of the following parts"));
        assert!(!block.contains("prior context (collapsed)"));
    }

    #[test]
    fn aporia_with_frontier_parts_names_each_session_node() {
        let g = GraphContext {
            collapsed: "N1: open thread about the license flip".into(),
            frontier_parts: vec![
                ("open thread about the license flip".into(), 0.9),
                ("pending review from caissa-agent".into(), 0.7),
            ],
        };
        let block = build_turn_context_block(true, Some(&g));
        assert!(block.contains("session-node"));
        assert!(block.contains("open thread about the license flip"));
        assert!(block.contains("pending review from caissa-agent"));
        assert!(block.contains("0.90") || block.contains("0.9"));
        assert!(block.contains("prior context (collapsed)"));
    }
}

pub(crate) async fn run_matrix_reply(state: &ListenState, req: &MatrixReplyReq) -> anyhow::Result<String> {
    // Phase 1: get or create the per-room process handle under the outer map
    // lock. The outer lock is held across the spawn() await (fast — just a
    // fork), but is released BEFORE the Claude API call so different rooms
    // can run in parallel.
    let (process_arc, is_aporia): (std::sync::Arc<tokio::sync::Mutex<SidecarProcess>>, bool) = {
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
            // Graph context and the aporia composed-parts preamble are NOT
            // baked in here — they're rebuilt fresh on every turn (see below,
            // after this block) so a long-running room keeps getting a
            // current snapshot instead of relying solely on the sidecar's own
            // accumulating (and, per Experiment 9/10, diluting) history.
            let (system_prompt, skills, def_models, session_is_aporia, thinking_budget) =
                resolve_guilhem_prompt(&state.fondament_url, &state.generation, &req.room_id).await;

            let model = def_models.get("matrix").cloned().unwrap_or_else(|| state.matrix_model.clone());
            let init = SidecarInit {
                system_prompt,
                model,
                allowed_tools: guilhem_allowed_tools(&state.fondament_url).await,
                skills,
                mcp_servers: guilhem_mcp_servers(state),
                max_thinking_tokens: thinking_budget,
            };

            // spawn() is async but fast (just a fork) — OK to await while
            // holding the outer map lock.
            let process = SidecarProcess::spawn(&init).await?;
            sessions.insert(
                req.room_id.clone(),
                RoomSession {
                    process: std::sync::Arc::new(tokio::sync::Mutex::new(process)),
                    last_activity: std::time::Instant::now(),
                    is_aporia: session_is_aporia,
                },
            );
        }

        let session = &sessions[&req.room_id];
        (std::sync::Arc::clone(&session.process), session.is_aporia)
        // outer map lock released here — other rooms can now run in parallel
    };

    // Recompute the graph context and (if aporia) the composed-parts preamble
    // fresh for THIS turn, every turn — including the very first one, which
    // subsumes what used to be a spawn-only injection. Prepended to the
    // message content itself, not the system prompt: the sidecar's `resume`
    // mechanism means the system prompt passed at spawn is fixed for the
    // life of the process, but each turn's own content is exactly where a
    // fresh snapshot belongs.
    let api_key = std::env::var("ANTHROPIC_API_KEY").ok();
    let graph_context = caissa_core::graph_context::build_graph_context(
        &state.farga_url, &req.room_id, &req.sender, &req.content, api_key,
    ).await;
    let turn_block = build_turn_context_block(is_aporia, graph_context.as_ref());
    let content = if turn_block.is_empty() {
        req.content.clone()
    } else {
        format!("{}\n\n{}", turn_block, req.content)
    };

    // Phase 2: Claude API call — no outer map lock held. Two messages for the
    // same room serialise on process_arc's Mutex; different rooms run freely.
    let reply = {
        let mut process = process_arc.lock().await;
        process.send(&req.room_id, &req.sender, &content).await?
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
pub(crate) fn guilhem_mcp_servers(state: &ListenState) -> serde_json::Value {
    serde_json::json!({
        "farga": { "type": "http", "url": state.farga_mcp_url },
        "dispatcher": { "type": "http", "url": state.dispatcher_mcp_url },
        "charradissa": { "type": "http", "url": state.charradissa_mcp_url },
        "nervi": { "type": "http", "url": state.nervi_mcp_url },
    })
}

/// The tool allow-list granted to every Guilhem sidecar session. Loaded live
/// from fondament-server's `guilhem` definition when reachable; hardcoded
/// fallback for local dev without fondament-server running.
pub(crate) async fn guilhem_allowed_tools(fondament_url: &str) -> Vec<String> {
    if let Ok(def) = fetch_fondament_def(fondament_url, "guilhem").await {
        let from_def: Vec<String> = def.tools.always_on.iter()
            .map(tool_to_claude_name)
            .collect();
        if !from_def.is_empty() {
            return from_def;
        }
    }
    // Hardcoded fallback (used in dev when Fondament checkout is absent)
    // Guilhem's role: read, formulate, dispatch, review — not implement in component repos.
    // Edit/Write are intentionally absent — code changes flow through component agents via dispatch.
    // WebSearch/WebFetch enable adversarial challenge evaluation and PR research.
    vec![
        "Bash".to_string(),
        "WebSearch".to_string(),
        "WebFetch".to_string(),
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
        "mcp__charradissa__matrix_request_approval".to_string(),
        "mcp__nervi__nervi_publish".to_string(),
        "mcp__nervi__nervi_subscribe".to_string(),
        "mcp__farga__write_context_node".to_string(),
        "mcp__farga__read_context_node".to_string(),
        "mcp__farga__list_context_nodes".to_string(),
    ]
}

// ── Component agent Nervi subscriber loop ────────────────────────────────────
//
// Non-Guilhem pods (generation ends with "-agent") run a Nervi polling loop
// alongside the HTTP server. The loop subscribes to:
//   occitan.issues.<component>   — GitHub issues synced by the daily issue-sync CronJob
//   occitan.dispatch.<component> — tasks published by Guilhem's dispatch cycle
// When messages arrive, it spawns a Claude session that acts as an orchestrator:
// reading the task, checking Farga context, and invoking specialist agents
// (developer / QA / reviewer / librarian) via the Dispatcher MCP.

