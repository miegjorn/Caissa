//! Surviving subset of the now-deleted `matrix_client.rs` (Task 10 of the
//! sidecar/session-map removal). Everything else in that file existed only
//! to support `run_matrix_client_loop`'s own login/sync/reply cycle (Matrix
//! login, `/sync` long-polling, markdown rendering, Kroki diagram upload) --
//! all deleted along with it, since this pod no longer touches a Matrix
//! credential (Corrièr's gateways own that now). These three functions have
//! independent, still-live callers outside that deleted loop:
//! `github_token_envs` (cron_triggers.rs, queue_triggers.rs,
//! component_agent.rs -- GH token env for one-shot `claude --print`
//! subprocesses), and `post_to_matrix_room` / `post_signal_to` (handoff.rs --
//! posting via the Synapse admin token, an entirely separate identity from
//! the deleted per-pod Matrix login). Bodies carried over unchanged.

use super::*;

pub(crate) fn github_token_envs() -> Vec<(String, String)> {
    let content = match std::fs::read_to_string("/creds/tokens.env") {
        Ok(c) => c,
        Err(_) => return Vec::new(),
    };
    content
        .lines()
        .filter_map(|line| {
            let rest = line.strip_prefix("export ")?;
            let (key, value) = rest.split_once('=')?;
            if key != "GH_TOKEN" && key != "GITHUB_TOKEN" {
                return None;
            }
            Some((key.to_string(), value.trim_matches('\'').to_string()))
        })
        .collect()
}

pub(crate) async fn post_to_matrix_room(room_id: &str, body: &str) {
    let synapse_url = std::env::var("SYNAPSE_URL")
        .unwrap_or_else(|_| "http://synapse.occitan-system.svc.cluster.local:8008".into());
    let admin_token = std::env::var("SYNAPSE_ADMIN_TOKEN").unwrap_or_default();
    if admin_token.is_empty() {
        tracing::error!("handoff: SYNAPSE_ADMIN_TOKEN not set — cannot post to room {}", room_id);
        return;
    }

    let client = match reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            tracing::error!("handoff: matrix client build failed: {}", e);
            return;
        }
    };

    let txn = uuid::Uuid::new_v4();
    let url = format!(
        "{}/_matrix/client/v3/rooms/{}/send/m.room.message/{}",
        synapse_url, room_id, txn
    );
    let res = client
        .put(&url)
        .bearer_auth(&admin_token)
        .json(&serde_json::json!({ "msgtype": "m.text", "body": body }))
        .send()
        .await
        .and_then(|r| r.error_for_status());
    if let Err(e) = res {
        tracing::error!("handoff: failed to post to room {}: {}", room_id, e);
    }
}

/// Like [`post_signal`] but with an explicit project and source — used by the
/// handoff traceability (O-4) and error (K-2) signals, which are written under
/// the per-dispatch `session_id` project rather than the daemon's own project.
pub(crate) async fn post_signal_to(
    state: &ListenState,
    project: &str,
    content: &str,
    source: &str,
) -> anyhow::Result<()> {
    let payload = SignalPayload {
        project: project.to_string(),
        signals: vec![SignalItem {
            project: project.to_string(),
            content: content.to_string(),
            source: source.to_string(),
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
