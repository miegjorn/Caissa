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

/// Claude Code strictly prefers `ANTHROPIC_API_KEY` over
/// `CLAUDE_CODE_OAUTH_TOKEN` when both are present in a process's
/// environment -- confirmed empirically (2026-07-04): a deliberately invalid
/// `ANTHROPIC_API_KEY` produced "Invalid API key" even with a valid
/// `CLAUDE_CODE_OAUTH_TOKEN` also set, rather than falling back to OAuth.
/// For an OAuth token (a `claude setup-token` credential tied to a Claude
/// subscription, cheaper/included usage vs. metered API billing) to actually
/// take effect over a pod's API key, `ANTHROPIC_API_KEY` must be removed
/// from the *spawned* `claude` process's own environment -- it stays wired
/// as the failover for any pod where no OAuth token is configured, since
/// `env_remove` here only affects this one child process, not the parent's
/// environment or the k8s Secret backing either variable.
pub(crate) trait ClaudeCommandExt {
    fn prefer_oauth_over_api_key(&mut self) -> &mut Self;
}

impl ClaudeCommandExt for tokio::process::Command {
    // &mut self -> &mut Self, matching Command's own builder convention
    // (.arg()/.args()/.env() are all &mut self -> &mut Self, not self -> Self)
    // -- this is what actually makes chaining work: `Command::new("claude")`
    // yields an owned Command that auto-refs to call this method, and the
    // `&mut Self` this returns is what the next chained call (.args(), etc.)
    // needs to continue on.
    fn prefer_oauth_over_api_key(&mut self) -> &mut Self {
        if std::env::var("CLAUDE_CODE_OAUTH_TOKEN").is_ok() {
            self.env_remove("ANTHROPIC_API_KEY");
        }
        self
    }
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

#[cfg(test)]
mod claude_command_ext_tests {
    use super::*;

    // Both scenarios live in one test function, run strictly sequentially,
    // rather than two separate #[tokio::test] functions: Rust runs tests in
    // parallel within a binary by default, and both scenarios mutate the
    // same process-global env vars (ANTHROPIC_API_KEY/CLAUDE_CODE_OAUTH_TOKEN)
    // -- as two separate tests this raced and failed intermittently
    // (confirmed live), since env::set_var/remove_var affect the whole
    // process, not per-thread state. No other test in this workspace reads
    // or writes these two vars, and both are restored to their prior state
    // before the function returns.
    #[tokio::test]
    async fn prefer_oauth_over_api_key_removes_or_keeps_api_key_correctly() {
        let prior_api_key = std::env::var("ANTHROPIC_API_KEY").ok();
        let prior_oauth = std::env::var("CLAUDE_CODE_OAUTH_TOKEN").ok();

        // Scenario 1: OAuth token present -> ANTHROPIC_API_KEY must be
        // removed from the child's environment.
        std::env::set_var("ANTHROPIC_API_KEY", "sk-ant-parent-value");
        std::env::set_var("CLAUDE_CODE_OAUTH_TOKEN", "sk-ant-oat-parent-value");
        let output = tokio::process::Command::new("sh")
            .args(["-c", "echo \"[$ANTHROPIC_API_KEY]\""])
            .prefer_oauth_over_api_key()
            .output()
            .await
            .expect("sh must be available to run this test");
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert_eq!(stdout.trim(), "[]", "ANTHROPIC_API_KEY must be empty in the child when an OAuth token is present");

        // Scenario 2: no OAuth token -> ANTHROPIC_API_KEY passes through
        // unchanged as the failover.
        std::env::set_var("ANTHROPIC_API_KEY", "sk-ant-parent-value");
        std::env::remove_var("CLAUDE_CODE_OAUTH_TOKEN");
        let output = tokio::process::Command::new("sh")
            .args(["-c", "echo \"[$ANTHROPIC_API_KEY]\""])
            .prefer_oauth_over_api_key()
            .output()
            .await
            .expect("sh must be available to run this test");
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert_eq!(stdout.trim(), "[sk-ant-parent-value]", "ANTHROPIC_API_KEY must pass through unchanged as the failover when no OAuth token is set");

        match prior_api_key {
            Some(v) => std::env::set_var("ANTHROPIC_API_KEY", v),
            None => std::env::remove_var("ANTHROPIC_API_KEY"),
        }
        match prior_oauth {
            Some(v) => std::env::set_var("CLAUDE_CODE_OAUTH_TOKEN", v),
            None => std::env::remove_var("CLAUDE_CODE_OAUTH_TOKEN"),
        }
    }
}
