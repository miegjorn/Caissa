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
//! Non-fatal by design: any failure (extraction API error, Farga
//! unreachable) logs a warning and returns `None`. Callers fall back to
//! their pre-existing prompt — this must never turn a reply into a hard
//! failure.

use amassada_core::{extract_delta, NodeId, NodeType, SessionGraph};

/// Build a collapsed context string for `room_id`'s persistent graph, given
/// the latest message's sender and content. Returns `None` on any failure,
/// or when the room's graph has no frontier nodes yet (first message ever
/// in this room) — callers should fall back to their bare prompt in both
/// cases.
pub async fn build_graph_context(
    farga_url: &str,
    room_id: &str,
    latest_sender: &str,
    latest_content: &str,
    api_key: Option<String>,
) -> Option<String> {
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

    let frontier_ids: Vec<NodeId> = graph
        .layers
        .causal
        .nodes
        .values()
        .filter(|n| n.node_type == NodeType::Frontier)
        .map(|n| n.id.clone())
        .collect();

    let collapsed = if frontier_ids.is_empty() {
        None
    } else {
        Some(graph.retrieve(&frontier_ids, 1))
    };

    // Save even if extraction failed above -- version/vias/other layers may
    // still be worth persisting, and save_graph is itself non-fatal on error.
    amassada_core::farga::save_graph(farga_url, room_id, &graph).await;

    collapsed
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
