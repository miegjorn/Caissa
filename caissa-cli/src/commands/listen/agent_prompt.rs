use super::*;

/// Assemble the system prompt and skills for a chat turn using the
/// Fondament resolver path for `fondament/<component>+deconstructive`.
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
pub(crate) async fn resolve_agent_prompt(fondament_url: &str, generation: &str, room_id: &str) -> (String, Vec<String>, std::collections::HashMap<String, String>, bool, Option<u32>) {
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

    let context_graph_preamble = context_graph_preamble();

    let skill_constraints = resolve_skill_constraints(fondament_url, &skills).await;

    let prompt = format!(
        "{}\n\n{}\n\n{}\n\n{}\n\nYou are replying in Matrix room {}.",
        discipline_preamble,
        role_context.trim_end(),
        context_graph_preamble,
        skill_constraints,
        room_id,
    );
    (prompt, skills, models, is_aporia, thinking_budget)
}

/// The two real dispatcher scope rules, stated accurately for every agent's
/// prompt: Guilhem may invoke any domain's `architect` or `axiom-evaluator`
/// facet only (cross-domain, consultative/evaluative work); component
/// agents may invoke any facet within their own domain only (self-domain).
/// Both are enforced by `caissa-cli/src/commands/dispatch.rs::ScopeRules`,
/// loaded from `caissa/scope-org-orchestrator` and
/// `caissa/scope-component-orchestrator` respectively — this text
/// previously claimed only the first rule existed and that any other
/// combination was rejected outright, which left every component agent's
/// own prompt self-contradicting its own attached
/// `caissa/scope-component-orchestrator` skill content.
fn context_graph_preamble() -> &'static str {
    "\
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
DISPATCH RULE: Guilhem may invoke_agent for any domain's \"architect\" or\n\
\"axiom-evaluator\" facet only (cross-domain, consultative/evaluative work).\n\
Component agents may invoke_agent for any facet within their own domain only\n\
(self-domain). For all cross-component code work, use nervi_publish to\n\
occitan.dispatch.<component> instead. The dispatcher enforces both rules —\n\
this is a hard guard, not a suggestion.\n\
--- end context graph ---"
}

/// Fetch each declared skill's `rules.prompt_constraint` text from
/// fondament-server and concatenate them, in declaration order, each
/// separated by a blank line. Missing/unreachable skills are skipped
/// (logged, not fatal) -- matches `dispatch.rs::load_scope_rules`'s
/// existing fetch-by-id pattern (`GET {fondament_url}/raw/{id}@latest`),
/// generalized from two hardcoded IDs to whatever a persona actually
/// declares.
pub(crate) async fn resolve_skill_constraints(fondament_url: &str, skill_ids: &[String]) -> String {
    if skill_ids.is_empty() {
        return String::new();
    }

    #[derive(serde::Deserialize, Default)]
    struct SkillRulesBlock {
        prompt_constraint: Option<String>,
    }
    #[derive(serde::Deserialize)]
    struct SkillFile {
        #[serde(default)]
        rules: Option<SkillRulesBlock>,
    }

    let client = reqwest::Client::new();
    let mut constraints = Vec::new();

    for skill_id in skill_ids {
        let url = format!("{}/raw/{}@latest", fondament_url.trim_end_matches('/'), skill_id);
        let result = client
            .get(&url)
            .timeout(std::time::Duration::from_secs(5))
            .send()
            .await;

        match result {
            Ok(resp) if resp.status().is_success() => match resp.text().await {
                Ok(text) => match serde_yaml::from_str::<SkillFile>(&text) {
                    Ok(skill) => {
                        if let Some(c) = skill.rules.and_then(|r| r.prompt_constraint) {
                            constraints.push(c);
                        }
                    }
                    Err(e) => tracing::warn!("skill {} failed to parse: {}", skill_id, e),
                },
                Err(e) => tracing::warn!("skill {} response body unreadable: {}", skill_id, e),
            },
            Ok(resp) => tracing::warn!("skill {} returned {}", skill_id, resp.status()),
            Err(e) => tracing::warn!("skill {} unreachable (non-fatal): {}", skill_id, e),
        }
    }

    constraints.join("\n\n")
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

/// The MCP servers attached to every one-shot `claude --print` invocation
/// this pod spawns — Farga (memory) and the dispatcher (component-agent
/// routing). Shared by the `/trigger/*` handlers (cron_triggers.rs,
/// queue_triggers.rs) so capability stays in lockstep across trigger types.
/// Renamed from `guilhem_mcp_servers` (this loop now runs for every
/// component, not just Guilhem).
pub(crate) fn agent_mcp_servers(state: &ListenState) -> serde_json::Value {
    serde_json::json!({
        "farga": { "type": "http", "url": state.farga_mcp_url },
        "dispatcher": { "type": "http", "url": state.dispatcher_mcp_url },
        "charradissa": { "type": "http", "url": state.charradissa_mcp_url },
        "nervi": { "type": "http", "url": state.nervi_mcp_url },
    })
}

/// The tool allow-list granted to every one-shot `claude --print` invocation
/// this pod spawns (see `agent_mcp_servers` above for callers). Loaded live
/// from fondament-server's `state.component_name` definition when reachable;
/// hardcoded fallback for local dev without fondament-server running.
/// Renamed from `guilhem_allowed_tools`, and its Fondament-address lookup
/// changed from the hardcoded `"guilhem"` to `&state.component_name` so it
/// resolves whichever agent's persona is actually running in this pod.
pub(crate) async fn agent_allowed_tools(fondament_url: &str, state: &ListenState) -> Vec<String> {
    if let Ok(def) = fetch_fondament_def(fondament_url, &state.component_name).await {
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
        // matrix_send/matrix_invite/matrix_kick/matrix_leave/matrix_read
        // deliberately removed (2026-07-04), matching guilhem.yaml: all five
        // are dead weight or actively harmful here. Real replies to #occitan
        // go through the built-in post_reply path, never an MCP tool call;
        // matrix_send in particular always 403s (charradissa MCP server's
        // backing identity has no standing in #occitan).
        "mcp__charradissa__matrix_get_dm".to_string(),
        "mcp__charradissa__matrix_request_approval".to_string(),
        "mcp__nervi__nervi_publish".to_string(),
        "mcp__nervi__nervi_subscribe".to_string(),
        "mcp__farga__write_context_node".to_string(),
        "mcp__farga__read_context_node".to_string(),
        "mcp__farga__list_context_nodes".to_string(),
    ]
}

#[cfg(test)]
mod skill_resolution_tests {
    use super::*;

    #[tokio::test]
    async fn resolve_skill_constraints_against_unreachable_fondament_returns_empty_not_panic() {
        let result = resolve_skill_constraints("http://127.0.0.1:1", &["occitan/amassada".to_string()]).await;
        assert_eq!(result, "");
    }

    #[tokio::test]
    async fn resolve_skill_constraints_with_no_skills_returns_empty() {
        let result = resolve_skill_constraints("http://127.0.0.1:1", &[]).await;
        assert_eq!(result, "");
    }

    #[test]
    fn context_graph_preamble_states_both_real_dispatch_rules() {
        let text = context_graph_preamble();
        assert!(text.contains("architect"));
        assert!(text.contains("axiom-evaluator"));
        assert!(text.contains("own domain only"));
        assert!(!text.contains("requires caller=\"guilhem\" and facet=\"architect\" only"));
        assert!(!text.contains("The dispatcher will reject any other combination"));
    }
}
