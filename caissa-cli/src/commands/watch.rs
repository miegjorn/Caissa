/// SRE watchdog — Level 1, no-LLM health probe.
///
/// Runs a loop every WATCHDOG_INTERVAL_SECS (default 300) checking:
///   - /health endpoints for Gardian, Farga, Amassada, Charradissa, Guilhem, Dispatcher
///   - Farga recent signals (at least one means the chronicle cron has run before)
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

use caissa_core::config::load_config;
use serde_json::json;

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
        ("amassada",    std::env::var("AMASSADA_URL").unwrap_or_else(|_| "http://amassada.occitan-system.svc.cluster.local:7700".into())),
        ("charradissa", std::env::var("CHARRADISSA_URL").unwrap_or_else(|_| "http://charradissa.occitan-system.svc.cluster.local:8448".into())),
        ("guilhem",     std::env::var("GUILHEM_URL").unwrap_or_else(|_| "http://guilhem.agents.svc.cluster.local:8080".into())),
        ("dispatcher",  std::env::var("DISPATCHER_URL").unwrap_or_else(|_| "http://dispatcher.agents.svc.cluster.local:9090".into())),
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
