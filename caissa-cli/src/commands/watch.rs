/// SRE watchdog — Level 1, no-LLM health probe.
///
/// Runs a loop every WATCHDOG_INTERVAL_SECS (default 300) checking:
///   - /health endpoints for Gardian, Farga, Amassada, Charradissa, Guilhem, Dispatcher
///   - Farga recent signals (at least one means the chronicle cron has run before)
///
/// On any anomaly, writes a bug-signal to Farga (project: occitan, source: sre-watchdog).
/// Exits only on fatal startup errors — probe failures are logged and signalled, not fatal.
///
/// All service URLs default to cluster-internal DNS and can be overridden via env vars:
///   FARGA_URL, GARDIAN_URL, AMASSADA_URL, CHARRADISSA_URL, GUILHEM_URL, DISPATCHER_URL
///   WATCHDOG_INTERVAL_SECS (default 300)
///   WATCHDOG_PROJECT (default: occitan)

use caissa_core::config::load_config;
use serde_json::json;

pub async fn run() -> anyhow::Result<()> {
    let config = load_config().unwrap_or_default();

    let farga_url = std::env::var("FARGA_URL").unwrap_or(config.farga_url.clone());
    let project = std::env::var("WATCHDOG_PROJECT").unwrap_or(config.project.clone());
    let interval_secs: u64 = std::env::var("WATCHDOG_INTERVAL_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(300);

    let services: Vec<(&str, String)> = vec![
        ("gardian",     std::env::var("GARDIAN_URL").unwrap_or_else(|_| "http://gardian.occitan-system.svc.cluster.local:7400".into())),
        ("farga",       farga_url.clone()),
        ("amassada",    std::env::var("AMASSADA_URL").unwrap_or_else(|_| "http://amassada.occitan-system.svc.cluster.local:7700".into())),
        ("charradissa", std::env::var("CHARRADISSA_URL").unwrap_or_else(|_| "http://charradissa.occitan-system.svc.cluster.local:8448".into())),
        ("guilhem",     std::env::var("GUILHEM_URL").unwrap_or_else(|_| "http://guilhem.agents.svc.cluster.local:8080".into())),
        ("dispatcher",  std::env::var("DISPATCHER_URL").unwrap_or_else(|_| "http://dispatcher.agents.svc.cluster.local:9090".into())),
    ];

    tracing::info!("sre-watchdog starting — interval {}s, project {}", interval_secs, project);

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
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
                tracing::error!("failed to post bug-signal: {}", e);
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
