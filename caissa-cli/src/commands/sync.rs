/// GitHub → NATS bridge — polling variant (Occitan#36).
///
/// Polls the GitHub REST API for new/updated issues on tracked miegjorn/*
/// repos and publishes each event to the OCCITAN JetStream stream under:
///   occitan.github.issues.<component>
/// where <component> is the lowercase repo name (e.g. "fondament", "caissa").
///
/// Runs as a long-running loop alongside the SRE watchdog sidecar in the
/// Guilhem pod. No webhook, no new ingress — pure polling.
///
/// Configuration (env vars take precedence over caissa.toml defaults):
///   GITHUB_TOKEN              — GitHub PAT; avoids the 60 req/h unauthenticated limit
///   GITHUB_POLL_REPOS         — comma-separated owner/repo pairs
///                               default: all 9 miegjorn/* component repos
///   GITHUB_POLL_INTERVAL_SECS — poll interval in seconds (default: 300)
///   NERVI_MCP_URL             — Nervi MCP HTTP endpoint for nervi_publish calls
///
/// State: last-polled timestamp tracked in memory per repo. On startup the
/// "since" window opens GITHUB_POLL_INTERVAL_SECS seconds before now, so
/// the first poll picks up recently-updated issues without replaying history.
///
/// Per-repo failures are logged and skipped — the loop continues for all
/// other repos. GitHub rate-limit responses (429 / 403 + x-ratelimit-remaining)
/// are respected with automatic back-off.

use caissa_core::config::load_config;
use serde_json::{json, Value};
use std::collections::HashMap;

/// Map a GitHub repo name to its Occitan component name.
/// Falls back to the lowercase repo name for unknown repos.
fn repo_to_component(repo_name: &str) -> String {
    match repo_name.to_lowercase().as_str() {
        "caissa"      => "caissa",
        "fondament"   => "fondament",
        "farga"       => "farga",
        "amassada"    => "amassada",
        "gardian"     => "gardian",
        "charradissa" => "charradissa",
        "nervi"       => "nervi",
        "cor"         => "cor",
        "occitan"     => "occitan",
        other         => return other.to_string(),
    }
    .to_string()
}

/// Default tracked repos — the full miegjorn/* component roster.
fn default_repos() -> Vec<String> {
    [
        "miegjorn/Caissa",
        "miegjorn/Fondament",
        "miegjorn/Farga",
        "miegjorn/Amassada",
        "miegjorn/Gardian",
        "miegjorn/Charradissa",
        "miegjorn/Nervi",
        "miegjorn/Cor",
        "miegjorn/Occitan",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}

pub async fn run() -> anyhow::Result<()> {
    let config = load_config().unwrap_or_default();

    let nervi_mcp_url = std::env::var("NERVI_MCP_URL")
        .unwrap_or_else(|_| config.nervi_mcp_url.clone());

    let interval_secs: u64 = std::env::var("GITHUB_POLL_INTERVAL_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(config.github_poll_interval_secs);

    let repos: Vec<String> = std::env::var("GITHUB_POLL_REPOS")
        .ok()
        .map(|v| v.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect())
        .unwrap_or_else(|| {
            if config.github_poll_repos.is_empty() {
                default_repos()
            } else {
                config.github_poll_repos.clone()
            }
        });

    let github_token = std::env::var("GITHUB_TOKEN")
        .or_else(|_| std::env::var("GH_TOKEN"))
        .ok();

    tracing::info!(
        "github-sync starting — {} repos, interval {}s, token: {}",
        repos.len(),
        interval_secs,
        if github_token.is_some() { "present" } else { "absent (rate-limited to 60 req/h)" }
    );
    tracing::info!("tracking: {}", repos.join(", "));

    let mut headers = reqwest::header::HeaderMap::new();
    headers.insert(
        reqwest::header::USER_AGENT,
        "caissa-github-sync/1.0".parse().unwrap(),
    );
    headers.insert(
        reqwest::header::ACCEPT,
        "application/vnd.github+json".parse().unwrap(),
    );
    headers.insert(
        reqwest::header::HeaderName::from_static("x-github-api-version"),
        "2022-11-28".parse().unwrap(),
    );
    if let Some(ref token) = github_token {
        headers.insert(
            reqwest::header::AUTHORIZATION,
            format!("Bearer {}", token).parse().unwrap(),
        );
    }

    let client = reqwest::Client::builder()
        .default_headers(headers)
        .timeout(std::time::Duration::from_secs(30))
        .build()?;

    // Per-repo "since" cursor — initialised to (now - interval) so the first
    // poll picks up recently-active issues without replaying all history.
    let mut since_map: HashMap<String, chrono::DateTime<chrono::Utc>> = repos
        .iter()
        .map(|r| {
            let since = chrono::Utc::now()
                - chrono::Duration::seconds(interval_secs as i64);
            (r.clone(), since)
        })
        .collect();

    loop {
        for repo in &repos {
            let parts: Vec<&str> = repo.splitn(2, '/').collect();
            if parts.len() != 2 {
                tracing::warn!("skipping malformed repo entry: {}", repo);
                continue;
            }
            let (owner, repo_name) = (parts[0], parts[1]);
            let component = repo_to_component(repo_name);
            let subject = format!("occitan.github.issues.{}", component);

            let since = since_map.get(repo).copied().unwrap_or_else(chrono::Utc::now);
            let since_str = since.to_rfc3339();

            let url = format!(
                "https://api.github.com/repos/{}/{}/issues?state=open&sort=updated&direction=desc&since={}&per_page=100",
                owner, repo_name, since_str
            );

            tracing::debug!("polling {} since {}", repo, since_str);

            let resp = match client.get(&url).send().await {
                Ok(r) => r,
                Err(e) => {
                    tracing::warn!("network error polling {}: {}", repo, e);
                    continue;
                }
            };

            // Respect rate limits.
            if resp.status() == reqwest::StatusCode::TOO_MANY_REQUESTS
                || (resp.status() == reqwest::StatusCode::FORBIDDEN
                    && resp.headers().contains_key("x-ratelimit-remaining"))
            {
                let retry_after = resp
                    .headers()
                    .get("retry-after")
                    .and_then(|v| v.to_str().ok())
                    .and_then(|v| v.parse::<u64>().ok())
                    .unwrap_or(60);
                tracing::warn!(
                    "GitHub rate limit hit for {} — backing off {}s",
                    repo,
                    retry_after
                );
                tokio::time::sleep(std::time::Duration::from_secs(retry_after)).await;
                continue;
            }

            if !resp.status().is_success() {
                tracing::warn!("GitHub API returned {} for {}", resp.status(), repo);
                continue;
            }

            let issues: Vec<Value> = match resp.json().await {
                Ok(v) => v,
                Err(e) => {
                    tracing::warn!("failed to parse GitHub response for {}: {}", repo, e);
                    continue;
                }
            };

            if issues.is_empty() {
                tracing::debug!("{}: no new/updated issues", repo);
            } else {
                tracing::info!("{}: {} issue(s) to publish", repo, issues.len());
            }

            let mut latest_updated: Option<chrono::DateTime<chrono::Utc>> = None;

            for issue in &issues {
                // Skip pull requests — GitHub issues API includes PRs.
                if issue.get("pull_request").is_some() {
                    continue;
                }

                let number = issue["number"].as_u64().unwrap_or(0);
                let title = issue["title"].as_str().unwrap_or("").to_string();
                let body = issue["body"].as_str().unwrap_or("").to_string();
                let html_url = issue["html_url"].as_str().unwrap_or("").to_string();
                let state = issue["state"].as_str().unwrap_or("open").to_string();
                let updated_at = issue["updated_at"].as_str().unwrap_or("").to_string();

                let labels: Vec<String> = issue["labels"]
                    .as_array()
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|l| l["name"].as_str().map(|s| s.to_string()))
                            .collect()
                    })
                    .unwrap_or_default();

                let payload = json!({
                    "number": number,
                    "title": title,
                    "body": body,
                    "labels": labels,
                    "url": html_url,
                    "state": state,
                    "updated_at": updated_at,
                    "component": component,
                    "repo": repo,
                });

                if let Err(e) =
                    publish_issue(&client, &nervi_mcp_url, &subject, &payload).await
                {
                    tracing::warn!(
                        "failed to publish issue #{} from {} to {}: {}",
                        number,
                        repo,
                        subject,
                        e
                    );
                } else {
                    tracing::info!("published {}#{} → {}", repo, number, subject);
                }

                // Track the latest updated_at seen to advance the cursor.
                if let Ok(ts) = chrono::DateTime::parse_from_rfc3339(&updated_at) {
                    let ts_utc = ts.with_timezone(&chrono::Utc);
                    match latest_updated {
                        Some(prev) if ts_utc > prev => latest_updated = Some(ts_utc),
                        None => latest_updated = Some(ts_utc),
                        _ => {}
                    }
                }
            }

            // Advance cursor to just after the latest issue seen — prevents re-publishing.
            if let Some(latest) = latest_updated {
                let next_since = latest + chrono::Duration::seconds(1);
                since_map.insert(repo.clone(), next_since);
                tracing::debug!("{}: cursor advanced to {}", repo, next_since.to_rfc3339());
            }
        }

        tracing::debug!("github-sync sleeping {}s", interval_secs);
        tokio::time::sleep(std::time::Duration::from_secs(interval_secs)).await;
    }
}

/// Publish a GitHub issue payload to a NATS subject via the Nervi MCP endpoint.
async fn publish_issue(
    client: &reqwest::Client,
    nervi_mcp_url: &str,
    subject: &str,
    payload: &Value,
) -> anyhow::Result<()> {
    let mcp_body = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/call",
        "params": {
            "name": "nervi_publish",
            "arguments": {
                "subject": subject,
                "qualifier": "data",
                "payload": payload
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
