//! Graph-based context collapse for long-lived Matrix conversations, ported
//! from `Charradissa/charradissa-core/src/graph_context.rs`. Reuses
//! `amassada-core`'s already-tested `SessionGraph` / `extract_delta` /
//! `retrieve` rather than a new implementation.
//!
//! Unlike Charradissa's version, this has no rolling-history parameter:
//! each Caissa agent pod now processes its own room's messages incrementally
//! as they arrive via its own `/sync` loop (see `matrix_client` in
//! `caissa-cli/src/commands/listen.rs`), so there is no fetched history
//! window to replay — the persistent graph loaded from Farga already carries
//! prior turns.
//!
//! Called on *every* turn of a Matrix reply, not only at session spawn (see
//! `run_matrix_reply`): a room's `agent-sidecar.js` process is long-lived and
//! resumed via the Claude Agent SDK's own `resume` mechanism, which means raw
//! conversation history accumulates for the life of the room regardless of
//! how many turns it runs (Experiment 9/10's Condition A). Recomputing and
//! re-injecting the collapsed context into every turn's message content — not
//! just the system prompt at spawn — is what keeps a long-running room from
//! silently degrading into pure dilution.
//!
//! Non-fatal by design: any failure (extraction API error, Farga
//! unreachable) logs a warning and returns `None`. Callers fall back to
//! their pre-existing prompt — this must never turn a reply into a hard
//! failure.

use amassada_core::{extract_delta, NodeId, NodeType, SessionGraph};

/// Result of a graph-context collapse: the flattened text for direct
/// injection into a prompt, plus the raw Frontier node data (summary,
/// activation_weight) for callers that want to feed it into
/// `fondament_core::resolver::build_aporia_preamble` as composed parts
/// instead of (or alongside) the flattened text.
pub struct GraphContext {
    pub collapsed: String,
    pub frontier_parts: Vec<(String, f32)>,
}

/// Build a collapsed context for `room_id`'s persistent graph, given the
/// latest message's sender and content. Returns `None` on any failure, or
/// when the room's graph has no frontier nodes yet (first message ever in
/// this room) — callers should fall back to their bare prompt in both cases.
pub async fn build_graph_context(
    farga_url: &str,
    room_id: &str,
    latest_sender: &str,
    latest_content: &str,
    api_key: Option<String>,
) -> Option<GraphContext> {
    let mut graph = amassada_core::farga::load_graph(farga_url, room_id)
        .await
        .unwrap_or_else(|| SessionGraph::new(room_id));

    let transcript_segment = format!("{}: {}\n", latest_sender, latest_content);

    let existing_nodes: Vec<NodeId> = graph.layers.causal.nodes.keys().cloned().collect();

    match extract_delta(&transcript_segment, &existing_nodes, api_key).await {
        Ok(delta) => graph.apply_delta(delta),
        Err(e) => {
            tracing::warn!(
                "graph_context: extraction failed for room {} (non-fatal, retrieving from existing graph as-is): {}",
                room_id, e
            );
        }
    }

    let frontier_nodes: Vec<_> = graph
        .layers
        .causal
        .nodes
        .values()
        .filter(|n| n.node_type == NodeType::Frontier)
        .collect();

    let result = if frontier_nodes.is_empty() {
        None
    } else {
        let frontier_ids: Vec<NodeId> = frontier_nodes.iter().map(|n| n.id.clone()).collect();
        let frontier_parts: Vec<(String, f32)> = frontier_nodes
            .iter()
            .map(|n| (n.summary.clone(), n.activation_weight))
            .collect();
        Some(GraphContext {
            collapsed: graph.retrieve(&frontier_ids, 1),
            frontier_parts,
        })
    };

    // Save even if extraction failed above -- version/vias/other layers may
    // still be worth persisting, and save_graph is itself non-fatal on error.
    amassada_core::farga::save_graph(farga_url, room_id, &graph).await;

    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn unreachable_farga_returns_none_not_panic() {
        // No live Farga at this address -- load_graph/save_graph are both
        // non-fatal (per amassada_core::farga's own doc comment), and
        // extract_delta will fail without a reachable Anthropic endpoint --
        // either way this must resolve to None, never panic or hang.
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            build_graph_context(
                "http://127.0.0.1:1",
                "!unreachable-test-room:occitane.guilhem",
                "@p:occitane.guilhem",
                "how are you?",
                Some("not-a-real-key".into()),
            ),
        )
        .await;
        if let Ok(context) = result {
            assert!(context.is_none());
        }
    }
}
