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
///
/// It also polls GitHub Actions for a `failure` conclusion on `main` for each
/// tracked repo (see `BUILD_STATUS_REPOS`). Unlike the other checks, this one
/// doesn't just log an anomaly for a human to notice later: it publishes a
/// fix task straight to that repo's own `occitan.dispatch.<component>`
/// subject, the same channel and payload shape Guilhem's own dispatch cycle
/// uses (see `queue_triggers.rs`). The affected component agent picks it up
/// on its own next poll and dispatches to its own developer specialist
/// autonomously (class 1) — no Guilhem review round-trip for what's usually
/// a mechanical, time-sensitive fix. Requires `GITHUB_TOKEN`/`GH_TOKEN`; if
/// absent this check is skipped (logged once at startup), not fatal.

use caissa_core::config::load_config;
use crate::commands::sync::repo_to_component;
use serde_json::json;
use std::collections::HashMap;

/// Repos polled for build (CI) status, using the same repo_to_component
/// mapping github-sync (`caissa sync`) already uses. Occitan (the meta-repo)
/// is deliberately excluded — it has no deployed component agent to dispatch
/// a fix to.
const BUILD_STATUS_REPOS: &[&str] = &[
    "Gardian", "Fondament", "Farga", "Amassada", "Charradissa", "Cor", "Caissa", "nervi",
];

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

    let github_token = std::env::var("GITHUB_TOKEN").or_else(|_| std::env::var("GH_TOKEN")).ok();
    if github_token.is_none() {
        tracing::warn!("GITHUB_TOKEN/GH_TOKEN not set — build-failure monitor disabled (rate-limited unauthenticated calls would be unreliable)");
    }
    let mut github_headers = reqwest::header::HeaderMap::new();
    github_headers.insert(reqwest::header::USER_AGENT, "caissa-sre-watchdog/1.0".parse().unwrap());
    github_headers.insert(reqwest::header::ACCEPT, "application/vnd.github+json".parse().unwrap());
    github_headers.insert(
        reqwest::header::HeaderName::from_static("x-github-api-version"),
        "2022-11-28".parse().unwrap(),
    );
    if let Some(ref token) = github_token {
        github_headers.insert(
            reqwest::header::AUTHORIZATION,
            format!("Bearer {}", token).parse().unwrap(),
        );
    }
    let github_client = reqwest::Client::builder()
        .default_headers(github_headers)
        .timeout(std::time::Duration::from_secs(health_timeout_secs))
        .build()?;
    // Tracks the last failing run id we already dispatched a fix task for,
    // per repo -- so a build that's still red on the next cycle doesn't get
    // re-dispatched every WATCHDOG_INTERVAL_SECS until someone fixes it.
    let mut last_dispatched_failure: HashMap<String, u64> = HashMap::new();

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

        // Build (CI) failure check -- skipped entirely without a token rather
        // than hammering the unauthenticated 60 req/h GitHub rate limit.
        if github_token.is_some() {
            let build_anomalies = check_build_failures(
                &client,
                &github_client,
                &nervi_mcp_url,
                &mut last_dispatched_failure,
            ).await;
            anomalies.extend(build_anomalies);
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
                "qualifier": "info",
                "payload": alert_payload.to_string()
            }
        }
    });

    // Nervi MCP uses streamable-http (SSE response). HTTP 200 is NOT sufficient
    // to know the publish succeeded -- nervi-mcp returns 200 even when the
    // JSON-RPC payload inside the SSE `data:` line carries `isError: true`
    // (e.g. a tool-input validation failure). We have to read the body and
    // classify it, not just check the status code.
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

    let body = resp.text().await?;
    if let Err(e) = check_nervi_publish_response(&body) {
        anyhow::bail!("nervi MCP rejected occitan.sre.alerts publish: {}", e);
    }

    tracing::info!("published {} anomaly(-ies) to occitan.sre.alerts", anomalies.len());
    Ok(())
}

/// Classify a raw Nervi MCP `tools/call` HTTP response body as success or
/// failure. Nervi's streamable-http transport frames each response as SSE:
/// `event: message\ndata: {<json-rpc response>}\n\n`. HTTP 200 is returned
/// even for a JSON-RPC-level or tool-input-validation failure, so the body
/// itself must be parsed and inspected for a top-level `error` field or a
/// `result.isError == true` (the shape nervi-mcp uses for e.g. a rejected
/// `nervi_publish` call missing a required argument).
///
/// Confirmed live against a real nervi-mcp `nervi_publish` call missing the
/// required `qualifier` argument, which returns HTTP 200 with body:
///   event: message
///   data: {"result":{"content":[{"type":"text","text":"MCP error -32602: ..."}],"isError":true},"jsonrpc":"2.0","id":1}
///
/// Returns `Ok(())` on a clean result, `Err(<message>)` on a detected failure.
/// Pure and network-free so it can be unit-tested with canned SSE bodies.
fn check_nervi_publish_response(body: &str) -> Result<(), String> {
    // SSE frames each response as "event: message\ndata: {...}\n\n"; unwrap
    // the data line (same convention as component_agent.rs::poll_nervi_subject).
    let json_str = body
        .lines()
        .find(|l| l.starts_with("data: "))
        .map(|l| &l["data: ".len()..])
        .unwrap_or(body);

    let parsed: serde_json::Value = serde_json::from_str(json_str)
        .map_err(|e| format!("could not parse nervi MCP response body: {} (body: {})", e, body))?;

    // Top-level JSON-RPC error (e.g. malformed request, unknown method).
    if let Some(err) = parsed.get("error") {
        let msg = err.get("message").and_then(|m| m.as_str()).unwrap_or("unknown error");
        return Err(format!("jsonrpc error: {}", msg));
    }

    // Tool-level error: result.isError == true, with the actual message in
    // result.content[0].text.
    let is_error = parsed
        .get("result")
        .and_then(|r| r.get("isError"))
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    if is_error {
        let text = parsed
            .get("result")
            .and_then(|r| r.get("content"))
            .and_then(|c| c.as_array())
            .and_then(|a| a.first())
            .and_then(|item| item.get("text"))
            .and_then(|t| t.as_str())
            .unwrap_or("(no error text in response)");
        return Err(text.to_string());
    }

    Ok(())
}

#[cfg(test)]
mod check_nervi_publish_response_tests {
    use super::*;

    /// Captured live against a real nervi-mcp `nervi_publish` call with the
    /// required `qualifier` argument omitted.
    fn real_validation_error_response() -> &'static str {
        "event: message\ndata: {\"result\":{\"content\":[{\"type\":\"text\",\"text\":\"MCP error -32602: Input validation error: Invalid arguments for tool nervi_publish: [\\n  {\\n    \\\"expected\\\": \\\"'info' | 'cross-project' | 'data'\\\",\\n    \\\"received\\\": \\\"undefined\\\",\\n    \\\"code\\\": \\\"invalid_type\\\",\\n    \\\"path\\\": [\\n      \\\"qualifier\\\"\\n    ],\\n    \\\"message\\\": \\\"Required\\\"\\n  }\\n]\"}],\"isError\":true},\"jsonrpc\":\"2.0\",\"id\":1}\n\n"
    }

    /// Captured live against a real nervi-mcp `nervi_publish` call with a
    /// valid `qualifier` argument.
    fn real_success_response() -> &'static str {
        "event: message\ndata: {\"result\":{\"content\":[{\"type\":\"text\",\"text\":\"{\\n  \\\"published\\\": true,\\n  \\\"subject\\\": \\\"occitan.sre.alerts\\\",\\n  \\\"qualifier\\\": \\\"info\\\",\\n  \\\"stream\\\": \\\"OCCITAN\\\",\\n  \\\"seq\\\": 79\\n}\"}],\"structuredContent\":{\"published\":true,\"subject\":\"occitan.sre.alerts\",\"qualifier\":\"info\",\"stream\":\"OCCITAN\",\"seq\":79}},\"jsonrpc\":\"2.0\",\"id\":1}\n\n"
    }

    #[test]
    fn detects_tool_input_validation_error() {
        let err = check_nervi_publish_response(real_validation_error_response())
            .expect_err("missing-qualifier response must be classified as a failure");
        assert!(err.contains("qualifier"), "error message should surface the actual validation failure: {}", err);
    }

    #[test]
    fn accepts_real_success_response() {
        check_nervi_publish_response(real_success_response())
            .expect("a real successful publish response must not be classified as an error");
    }

    #[test]
    fn detects_top_level_jsonrpc_error() {
        let body = "event: message\ndata: {\"jsonrpc\":\"2.0\",\"id\":1,\"error\":{\"code\":-32601,\"message\":\"Method not found\"}}\n\n";
        let err = check_nervi_publish_response(body).expect_err("top-level jsonrpc error must be classified as a failure");
        assert!(err.contains("Method not found"));
    }

    #[test]
    fn falls_back_to_raw_body_when_not_sse_framed() {
        // Defensive: if nervi-mcp ever returns plain JSON (no SSE framing),
        // the parser should still work rather than failing to find "data: ".
        let body = "{\"result\":{\"content\":[],\"isError\":true},\"jsonrpc\":\"2.0\",\"id\":1}";
        let err = check_nervi_publish_response(body).expect_err("plain JSON isError response must still be classified as a failure");
        assert!(err.contains("no error text"));
    }
}

/// Poll GitHub Actions for the latest *completed*, *push*-triggered run on
/// `main` for each repo in `BUILD_STATUS_REPOS`. A `failure` conclusion not
/// already dispatched for this exact run id gets a fix task published
/// straight to that component's own `occitan.dispatch.<component>` --
/// bypassing Guilhem's review cycle entirely, since "the build is red" is
/// both urgent and (usually) mechanical to diagnose and fix. Per-repo
/// failures (network, rate limit, parse) are logged and skipped; this must
/// never abort the rest of the watchdog loop.
async fn check_build_failures(
    client: &reqwest::Client,
    github_client: &reqwest::Client,
    nervi_mcp_url: &str,
    last_dispatched: &mut HashMap<String, u64>,
) -> Vec<String> {
    let mut anomalies = Vec::new();

    for repo in BUILD_STATUS_REPOS {
        let component = repo_to_component(repo);
        let url = format!(
            "https://api.github.com/repos/miegjorn/{}/actions/runs?branch=main&event=push&status=completed&per_page=1",
            repo
        );

        let resp = match github_client.get(&url).send().await {
            Ok(r) => r,
            Err(e) => {
                tracing::debug!("build-status: {} unreachable (non-fatal): {}", repo, e);
                continue;
            }
        };

        if resp.status() == reqwest::StatusCode::TOO_MANY_REQUESTS
            || (resp.status() == reqwest::StatusCode::FORBIDDEN
                && resp.headers().contains_key("x-ratelimit-remaining"))
        {
            tracing::debug!("build-status: {} rate-limited (non-fatal), will retry next cycle", repo);
            continue;
        }

        if !resp.status().is_success() {
            tracing::debug!("build-status: {} returned {} (non-fatal)", repo, resp.status());
            continue;
        }

        let body: serde_json::Value = match resp.json().await {
            Ok(v) => v,
            Err(e) => {
                tracing::debug!("build-status: {} parse failed (non-fatal): {}", repo, e);
                continue;
            }
        };

        let run = match body["workflow_runs"].as_array().and_then(|a| a.first()) {
            Some(r) => r,
            None => continue, // no completed push runs on main yet
        };

        if run["conclusion"].as_str() != Some("failure") {
            continue;
        }

        let run_id = run["id"].as_u64().unwrap_or(0);
        if last_dispatched.get(*repo) == Some(&run_id) {
            continue; // already dispatched a fix task for this exact failing run
        }

        let run_url = run["html_url"].as_str().unwrap_or("");
        let head_sha = run["head_sha"].as_str().unwrap_or("");
        let short_sha = &head_sha[..head_sha.len().min(8)];
        let workflow_name = run["name"].as_str().unwrap_or("build");

        let msg = format!(
            "{} ({}) build failed on main: workflow '{}' at {} -- {}",
            repo, component, workflow_name, short_sha, run_url
        );
        tracing::warn!("{}", msg);
        anomalies.push(msg);

        let task = format!(
            "CI build failure on main: workflow '{}' failed at commit {}. \
             Diagnose via `gh run view --log-failed` against {} and fix the build.",
            workflow_name, short_sha, run_url
        );

        match publish_build_failure_dispatch(client, nervi_mcp_url, &component, &task).await {
            Ok(()) => {
                tracing::info!(
                    "dispatched build-failure fix task for {} to occitan.dispatch.{}",
                    repo, component
                );
                last_dispatched.insert(repo.to_string(), run_id);
            }
            Err(e) => {
                tracing::warn!("failed to dispatch build-failure fix task for {}: {}", repo, e);
            }
        }
    }

    anomalies
}

/// Publish a build-failure fix task directly to a component's own dispatch
/// subject. Same envelope shape Guilhem's dispatch cycle uses (queue_triggers.rs)
/// so component_agent.rs's existing dispatch-message handling needs no changes:
/// `class: 1` means the component agent dispatches to its own developer
/// specialist autonomously, no Guilhem review round-trip.
async fn publish_build_failure_dispatch(
    client: &reqwest::Client,
    nervi_mcp_url: &str,
    component: &str,
    task: &str,
) -> anyhow::Result<()> {
    let subject = format!("occitan.dispatch.{}", component);
    let payload = json!({
        "type": "dispatch",
        "task": task,
        "context": "detected by the SRE watchdog's build-status monitor -- this component's own CI build is currently red on main.",
        "outcome": "a PR that fixes the failing build, with CI green again on main",
        "class": 1,
        "dispatched_by": "sre-watchdog",
        "date": chrono::Utc::now().format("%Y-%m-%d").to_string(),
        "review_required": false
    });

    let mcp_body = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/call",
        "params": {
            "name": "nervi_publish",
            "arguments": {
                "subject": subject,
                "payload": payload.to_string()
            }
        }
    });

    let resp = client
        .post(nervi_mcp_url)
        .header("Content-Type", "application/json")
        .header("Accept", "application/json, text/event-stream")
        .json(&mcp_body)
        .send()
        .await?;

    if !resp.status().is_success() {
        anyhow::bail!("nervi MCP returned {} for subject {}", resp.status(), subject);
    }

    Ok(())
}
