/// SRE watchdog — Level 1, no-LLM health probe.
///
/// Runs a loop every WATCHDOG_INTERVAL_SECS (default 300) checking:
///   - /health endpoints for Gardian, Farga, Amassada, Charradissa, Guilhem, Dispatcher
///   - Farga recent signals (at least one means the chronicle cron has run before)
///   - /room-status on Guilhem + all component agents (see ROOM_STATUS_STUCK_THRESHOLD_SECS)
///
/// On any anomaly, writes a bug-signal to Farga (project: occitan, source: sre-watchdog)
/// AND publishes structured alert to occitan.sre.alerts NATS subject via Nervi MCP so
/// Guilhem's sre-alert trigger can read and dispatch without polling Farga.
/// Exits only on fatal startup errors — probe failures are logged and signalled, not fatal.
///
/// All service URLs default to cluster-internal DNS and can be overridden via env vars:
///   FARGA_URL, NERVI_MCP_URL, GARDIAN_URL, AMASSADA_URL, CHARRADISSA_URL, GUILHEM_URL, DISPATCHER_URL
///   WATCHDOG_INTERVAL_SECS (default 300)
///   WATCHDOG_PROJECT (default: occitan)
///
/// `/health` on these six is a stateless "ok" with zero per-room awareness —
/// confirmed live (2026-07-04) that a hung (not crashed) agent-sidecar.js
/// process holding a room's mutex forever is invisible to it. `/room-status`
/// closes that gap: each Guilhem/component-agent pod reports per-room
/// `processing_secs` (see `commands::listen::session_management::
/// handle_room_status`), so the watchdog can catch a stuck turn even though
/// the in-process SIDECAR_TURN_TIMEOUT (5 minutes, in `matrix_reply.rs`)
/// should already have self-healed it by killing and respawning the sidecar.
/// A room still reporting `processing_secs` past that timeout plus a grace
/// margin means the self-heal itself didn't fire — worth paging on.

use caissa_core::config::load_config;
use serde_json::json;

/// Grace margin added on top of matrix_reply.rs's SIDECAR_TURN_TIMEOUT
/// (300s). A room stuck past this means the in-process timeout+kill did
/// not fire as expected — a worse condition than the already-handled case.
const ROOM_STATUS_STUCK_THRESHOLD_SECS: u64 = 360;

#[derive(serde::Deserialize)]
struct RoomStatusEntry {
    room_id: String,
    #[allow(dead_code)]
    alive: bool,
    #[allow(dead_code)]
    last_activity_secs_ago: u64,
    processing_secs: Option<u64>,
}

#[derive(serde::Deserialize)]
struct RoomStatusResponse {
    rooms: Vec<RoomStatusEntry>,
}

pub async fn run() -> anyhow::Result<()> {
    let config = load_config().unwrap_or_default();

    let farga_url = std::env::var("FARGA_URL").unwrap_or(config.farga_url.clone());
    let nervi_mcp_url = std::env::var("NERVI_MCP_URL").unwrap_or(config.nervi_mcp_url.clone());
    let project = std::env::var("WATCHDOG_PROJECT").unwrap_or(config.project.clone());
    // Env overrides take precedence over caissa.toml for k8s deployments.
    let interval_secs: u64 = std::env::var("WATCHDOG_INTERVAL_SECS")
        .ok().and_then(|v| v.parse().ok())
        .unwrap_or(config.sre_watchdog_interval_secs);
    let health_timeout_secs: u64 = std::env::var("HEALTH_TIMEOUT_SECS")
        .ok().and_then(|v| v.parse().ok())
        .unwrap_or(config.sre_health_timeout_secs);
    let chronicle_max_age_hours = config.sre_chronicle_max_age_hours;

    let services: Vec<(&str, String)> = vec![
        ("gardian",     std::env::var("GARDIAN_URL").unwrap_or_else(|_| "http://gardian.occitan-system.svc.cluster.local:7400".into())),
        ("farga",       farga_url.clone()),
        ("amassada",    std::env::var("AMASSADA_URL").unwrap_or_else(|_| "http://amassada.occitan-system.svc.cluster.local:7600".into())),
        ("charradissa", std::env::var("CHARRADISSA_URL").unwrap_or_else(|_| "http://charradissa.occitan-system.svc.cluster.local:8448".into())),
        ("guilhem",     std::env::var("GUILHEM_URL").unwrap_or_else(|_| "http://guilhem.agents.svc.cluster.local:8080".into())),
        ("dispatcher",  std::env::var("DISPATCHER_URL").unwrap_or_else(|_| "http://dispatcher.agents.svc.cluster.local:9090".into())),
    ];

    // Guilhem plus the 8 independent component-agent pods (deploy/charts/
    // component-agents/values.yaml: gardian, fondament, farga, amassada, cor,
    // caissa, charradissa, nervi) — every one of these runs the same `caissa
    // listen` binary with the same per-room RoomSession/GET /room-status
    // surface guilhem does, one Matrix room each (per-agent-matrix-
    // independence). Names double as the `agents` namespace service name:
    // guilhem is just `guilhem`; component agents are `{name}-agent`.
    let room_status_targets: Vec<(&str, String)> = vec![
        ("guilhem", std::env::var("GUILHEM_URL").unwrap_or_else(|_| "http://guilhem.agents.svc.cluster.local:8080".into())),
        ("gardian-agent", "http://gardian-agent.agents.svc.cluster.local:8080".into()),
        ("fondament-agent", "http://fondament-agent.agents.svc.cluster.local:8080".into()),
        ("farga-agent", "http://farga-agent.agents.svc.cluster.local:8080".into()),
        ("amassada-agent", "http://amassada-agent.agents.svc.cluster.local:8080".into()),
        ("cor-agent", "http://cor-agent.agents.svc.cluster.local:8080".into()),
        ("caissa-agent", "http://caissa-agent.agents.svc.cluster.local:8080".into()),
        ("charradissa-agent", "http://charradissa-agent.agents.svc.cluster.local:8080".into()),
        ("nervi-agent", "http://nervi-agent.agents.svc.cluster.local:8080".into()),
    ];

    tracing::info!(
        "sre-watchdog starting — interval {}s, health_timeout {}s, chronicle_max_age {}h, project {}",
        interval_secs, health_timeout_secs, chronicle_max_age_hours, project
    );
    // chronicle_max_age_hours is reserved — check requires Farga to return signal timestamps,
    // which the current /signals/recent endpoint does not. Filed against Farga.
    tracing::info!("note: chronicle_max_age check deferred — Farga timestamp API not yet available");

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(health_timeout_secs))
        .build()?;

    loop {
        let mut anomalies: Vec<String> = Vec::new();

        // Health endpoint checks
        for (name, base_url) in &services {
            let url = format!("{}/health", base_url);
            match client.get(&url).send().await {
                Ok(resp) if resp.status().is_success() => {
                    tracing::debug!("{} /health OK", name);
                }
                Ok(resp) => {
                    let msg = format!("{} /health returned {}", name, resp.status());
                    tracing::warn!("{}", msg);
                    anomalies.push(msg);
                }
                Err(e) => {
                    let msg = format!("{} /health unreachable: {}", name, e);
                    tracing::warn!("{}", msg);
                    anomalies.push(msg);
                }
            }
        }

        // Farga recent signals — at least one means the chronicle cron has ever run
        let signals_url = format!("{}/signals/recent?project={}", farga_url, project);
        match client.get(&signals_url).send().await {
            Ok(resp) if resp.status().is_success() => {
                let signals: Vec<serde_json::Value> = resp.json().await.unwrap_or_default();
                if signals.is_empty() {
                    let msg = format!("farga has no signals for project '{}' — chronicle may never have run", project);
                    tracing::warn!("{}", msg);
                    anomalies.push(msg);
                } else {
                    tracing::debug!("farga signals OK ({} recent)", signals.len());
                }
            }
            Ok(resp) => {
                anomalies.push(format!("farga /signals/recent returned {}", resp.status()));
            }
            Err(e) => {
                anomalies.push(format!("farga /signals/recent unreachable: {}", e));
            }
        }

        // Per-room stuck-turn check via /room-status. Unreachable pods are
        // NOT flagged here — that's already covered by the /health loop
        // above for guilhem, and a down component-agent pod isn't this
        // check's job to report on. Only a room that's alive enough to
        // answer /room-status but reports a turn stuck past the threshold
        // counts as an anomaly for this check.
        for (name, base_url) in &room_status_targets {
            let url = format!("{}/room-status", base_url);
            match client.get(&url).send().await {
                Ok(resp) if resp.status().is_success() => {
                    match resp.json::<RoomStatusResponse>().await {
                        Ok(status) => {
                            for room in status.rooms {
                                if let Some(secs) = room.processing_secs {
                                    if secs >= ROOM_STATUS_STUCK_THRESHOLD_SECS {
                                        let msg = format!(
                                            "{} room {} has been processing for {}s (>= {}s threshold) -- sidecar likely hung and did not self-heal",
                                            name, room.room_id, secs, ROOM_STATUS_STUCK_THRESHOLD_SECS
                                        );
                                        tracing::warn!("{}", msg);
                                        anomalies.push(msg);
                                    }
                                }
                            }
                        }
                        Err(e) => {
                            tracing::debug!("{} /room-status parse failed (non-fatal): {}", name, e);
                        }
                    }
                }
                Ok(resp) => {
                    tracing::debug!("{} /room-status returned {} (non-fatal, not flagged)", name, resp.status());
                }
                Err(e) => {
                    tracing::debug!("{} /room-status unreachable (non-fatal, not flagged): {}", name, e);
                }
            }
        }

        if !anomalies.is_empty() {
            let content = format!(
                "sre-watchdog detected {} anomaly(-ies):\n{}",
                anomalies.len(),
                anomalies.iter().map(|a| format!("  - {}", a)).collect::<Vec<_>>().join("\n")
            );
            tracing::warn!("posting bug-signal: {}", content);
            if let Err(e) = post_bug_signal(&client, &farga_url, &project, &content).await {
                tracing::error!("failed to post bug-signal to Farga: {}", e);
            }
            // Also publish to occitan.sre.alerts so Guilhem's sre-alert handler
            // can read and dispatch via nervi_subscribe without needing to poll Farga.
            if let Err(e) = publish_sre_alert(&client, &nervi_mcp_url, &project, &anomalies).await {
                tracing::warn!("failed to publish to occitan.sre.alerts (Guilhem will fall back to Farga): {}", e);
            }
        } else {
            tracing::info!("all clear");
        }

        tokio::time::sleep(std::time::Duration::from_secs(interval_secs)).await;
    }
}

async fn post_bug_signal(
    client: &reqwest::Client,
    farga_url: &str,
    project: &str,
    content: &str,
) -> anyhow::Result<()> {
    let payload = json!({
        "project": project,
        "signals": [{
            "project": project,
            "content": content,
            "source": "sre-watchdog"
        }]
    });
    client
        .post(format!("{}/signals", farga_url))
        .json(&payload)
        .send()
        .await?
        .error_for_status()?;
    Ok(())
}

/// Publish structured SRE alert to occitan.sre.alerts via Nervi MCP.
/// Best-effort — failure is logged but does not abort the watchdog loop.
async fn publish_sre_alert(
    client: &reqwest::Client,
    nervi_mcp_url: &str,
    project: &str,
    anomalies: &[String],
) -> anyhow::Result<()> {
    let alert_payload = json!({
        "project": project,
        "source": "sre-watchdog",
        "anomaly_count": anomalies.len(),
        "anomalies": anomalies,
        "timestamp": chrono::Utc::now().to_rfc3339(),
    });

    let mcp_body = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/call",
        "params": {
            "name": "nervi_publish",
            "arguments": {
                "subject": "occitan.sre.alerts",
                "payload": alert_payload.to_string()
            }
        }
    });

    // Nervi MCP uses streamable-http (SSE response). We fire-and-forget:
    // send the request and consume enough of the response to release the connection.
    let resp = client
        .post(nervi_mcp_url)
        .header("Content-Type", "application/json")
        .header("Accept", "application/json, text/event-stream")
        .json(&mcp_body)
        .send()
        .await?;

    let status = resp.status();
    if !status.is_success() {
        anyhow::bail!("nervi MCP returned {}", status);
    }

    tracing::info!("published {} anomaly(-ies) to occitan.sre.alerts", anomalies.len());
    Ok(())
}
