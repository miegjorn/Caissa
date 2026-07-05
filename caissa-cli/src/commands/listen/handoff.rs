use super::*;


// ── Handoff Bridge ──────────────────────────────────────────────────────────
//
// `@guilhem handoff domain:X facet:Y task:"..."` messages are intercepted in
// handle_matrix_reply (K-1) and dispatched mechanically: a Farga traceability
// signal is written first (O-4), the dispatcher's invoke_agent is called over
// JSON-RPC (O-2), and a background task polls invoke_agent's job to completion
// and posts the result — or a timeout / error — back to the room (O-3 / K-2).
// No conversational sidecar is spawned. Message parsing lives in handoff.rs.

/// How long the background poller waits for a dispatched job before declaring
/// a timeout and handing the job_id back for manual resumption (K-2).
pub(crate) const HANDOFF_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10 * 60);
/// First poll delay; grows geometrically up to HANDOFF_POLL_MAX.
pub(crate) const HANDOFF_POLL_INITIAL: std::time::Duration = std::time::Duration::from_secs(5);
/// Ceiling on the poll backoff.
pub(crate) const HANDOFF_POLL_MAX: std::time::Duration = std::time::Duration::from_secs(60);
/// Result summaries posted to the room are clipped to this many characters (O-3).
pub(crate) const HANDOFF_SUMMARY_MAX: usize = 500;

/// Terminal/intermediate classification of a dispatcher `get_agent_result` reply.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum JobStatus {
    /// Job done — carries the result text read back from Farga.
    Completed(String),
    /// Job failed — carries the dispatcher's stated reason.
    Failed(String),
    /// Still running or pending — keep polling.
    Pending,
}

/// Handle a `@guilhem handoff ...` message (K-1 interception target). Returns
/// the immediate Matrix reply text. Never errors out to a 500: every terminal
/// state — parse rejection, dispatch failure, accepted dispatch — produces a
/// Matrix message, honouring Caissa#36's "no silent termination" rule.
pub(crate) async fn handle_handoff(state: &Arc<ListenState>, req: &MatrixReplyReq) -> String {
    let handoff = match parse_handoff_message(&req.content) {
        Ok(h) => h,
        Err(e) => {
            // Validation rejection (unknown domain/facet, empty/missing task).
            // The error renders into an actionable Matrix message.
            tracing::info!("handoff rejected: {}", e);
            return e.to_string();
        }
    };

    // session_id: unique AND traceable — it doubles as the Farga project under
    // which both the O-4 trace signal and the agent's eventual result live, so
    // encode the target into it for at-a-glance scanning.
    let short = uuid::Uuid::new_v4().to_string()[..8].to_string();
    let session_id = format!("handoff-{}-{}-{}", handoff.domain, handoff.facet, short);

    // O-4: traceability signal BEFORE dispatch, so the parent context is
    // recorded even if the dispatch call itself fails.
    if let Err(e) = write_handoff_trace(state, &session_id, req, &handoff).await {
        tracing::warn!("handoff trace signal failed for {}: {}", session_id, e);
    }

    // O-2: dispatch via the dispatcher MCP (plain JSON-RPC over HTTP).
    let (job_id, assignment_id) = match dispatch_handoff(state, &session_id, &handoff).await {
        Ok(ids) => ids,
        Err(e) => {
            tracing::error!("handoff dispatch failed for {}: {}", session_id, e);
            let _ = write_handoff_error(state, &session_id, "(none)", &format!("dispatch failed: {}", e)).await;
            return format!(
                "Dispatch échoué — {} (domain:{} facet:{}). Signal Farga d'erreur écrit sous `{}`.",
                e, handoff.domain, handoff.facet, session_id
            );
        }
    };

    // O-3 / K-2: poll the job to completion out-of-band and post the result
    // (or timeout/error) to the room. The immediate reply below is the ack.
    let state_bg = Arc::clone(state);
    let room_id = req.room_id.clone();
    let job_bg = job_id.clone();
    let session_bg = session_id.clone();
    let assignment_bg = assignment_id.clone();
    tokio::spawn(async move {
        poll_and_report_handoff(&state_bg, &room_id, &job_bg, &session_bg, &assignment_bg).await;
    });

    format!("Dispatch lancé — job_id: `{}`, session: `{}`", job_id, session_id)
}

/// O-4: write the parent-context traceability signal under the dispatch's own
/// Farga project (`session_id`), so it is colocated with the agent's result.
pub(crate) async fn write_handoff_trace(
    state: &ListenState,
    session_id: &str,
    req: &MatrixReplyReq,
    handoff: &HandoffRequest,
) -> anyhow::Result<()> {
    let triggered_by = req.event_id.clone().unwrap_or_else(|| req.sender.clone());
    let trace = serde_json::json!({
        "session_id": session_id,
        "parent_room": req.room_id,
        "task": handoff.task,
        "dispatched_to": { "domain": handoff.domain, "facet": handoff.facet },
        "triggered_by": triggered_by,
        "timestamp": chrono::Utc::now().to_rfc3339(),
    });
    post_signal_to(state, session_id, &trace.to_string(), "guilhem-handoff-trace").await
}

/// K-2: write an error signal `{job_id, session_id, reason, timestamp}` under
/// the dispatch's Farga project so a timeout or failure leaves a durable trace.
pub(crate) async fn write_handoff_error(
    state: &ListenState,
    session_id: &str,
    job_id: &str,
    reason: &str,
) -> anyhow::Result<()> {
    let err = serde_json::json!({
        "job_id": job_id,
        "session_id": session_id,
        "reason": reason,
        "timestamp": chrono::Utc::now().to_rfc3339(),
    });
    post_signal_to(state, session_id, &err.to_string(), "guilhem-handoff-error").await
}

/// O-2: call the dispatcher's `invoke_agent` and return (job_id, assignment_id).
/// The assignment_id identifies the Nervi reply queue that `get_agent_result`
/// now reads from — the caller must thread it through to the poller. Optional
/// handoff fields map to the dispatcher's arguments: `allowed_tools` passes
/// through verbatim; `context_ref` / `farga_project` are folded into the
/// pre-assembled `context` markdown the agent boots with.
pub(crate) async fn dispatch_handoff(
    state: &ListenState,
    session_id: &str,
    h: &HandoffRequest,
) -> anyhow::Result<(String, String)> {
    let mut arguments = serde_json::json!({
        "domain": h.domain,
        "facet": h.facet,
        "task": h.task,
        "session_id": session_id,
    });
    if let Some(tools) = &h.allowed_tools {
        arguments["allowed_tools"] = serde_json::Value::String(tools.clone());
    }
    let context = build_handoff_context(h);
    if !context.is_empty() {
        arguments["context"] = serde_json::Value::String(context);
    }

    let text = dispatcher_tool_call(state, "invoke_agent", arguments).await?;
    let job_id = parse_job_id(&text)
        .ok_or_else(|| anyhow::anyhow!("dispatcher returned no job_id (raw: {})", text))?;
    let assignment_id = parse_assignment_id(&text)
        .ok_or_else(|| anyhow::anyhow!("dispatcher returned no assignment_id (raw: {})", text))?;
    Ok((job_id, assignment_id))
}

/// Build the `context` markdown for invoke_agent from the optional handoff
/// fields. Empty when neither `context_ref` nor `farga_project` is present.
pub(crate) fn build_handoff_context(h: &HandoffRequest) -> String {
    let mut ctx = String::new();
    if let Some(cr) = &h.context_ref {
        ctx.push_str(&format!(
            "## Context reference\nBefore starting, load prior context from Farga project `{cr}` \
             with `mcp__farga__read_context` (project: \"{cr}\").\n\n"
        ));
    }
    if let Some(fp) = &h.farga_project {
        ctx.push_str(&format!(
            "## Farga project\nThis work pertains to Farga project `{fp}`. Record durable findings there.\n\n"
        ));
    }
    ctx
}

/// O-3 / K-2: poll `get_agent_result` with geometric backoff until the job
/// completes, fails, or HANDOFF_TIMEOUT elapses — then post exactly one
/// terminal message to the room. Transient poll errors are tolerated (logged,
/// retried) so a blip doesn't masquerade as a job failure.
pub(crate) async fn poll_and_report_handoff(
    state: &ListenState,
    room_id: &str,
    job_id: &str,
    session_id: &str,
    assignment_id: &str,
) {
    let start = std::time::Instant::now();
    let mut backoff = HANDOFF_POLL_INITIAL;

    loop {
        tokio::time::sleep(backoff).await;
        backoff = std::cmp::min(backoff.mul_f32(1.6), HANDOFF_POLL_MAX);

        let args = serde_json::json!({ "job_id": job_id, "assignment_id": assignment_id });
        match dispatcher_tool_call(state, "get_agent_result", args).await {
            Ok(text) => match classify_job_status(&text) {
                JobStatus::Completed(summary) => {
                    let msg = format!(
                        "✓ Job `{}` terminé — session `{}`\n\n{}\n\nFarga : {}",
                        job_id,
                        session_id,
                        truncate_summary(&summary, HANDOFF_SUMMARY_MAX),
                        farga_link(state, session_id),
                    );
                    post_to_matrix_room(room_id, &msg).await;
                    return;
                }
                JobStatus::Failed(reason) => {
                    let _ = write_handoff_error(state, session_id, job_id, &reason).await;
                    let msg = format!(
                        "✗ Job `{}` échec — {} — reprise : `mcp__dispatcher__get_agent_result job_id:{} assignment_id:{}`",
                        job_id, reason, job_id, assignment_id
                    );
                    post_to_matrix_room(room_id, &msg).await;
                    return;
                }
                JobStatus::Pending => {}
            },
            Err(e) => {
                tracing::warn!("handoff poll error for job {}: {}", job_id, e);
            }
        }

        if start.elapsed() >= HANDOFF_TIMEOUT {
            let _ = write_handoff_error(state, session_id, job_id, "timeout (10 min)").await;
            let msg = format!(
                "Job `{}` timeout — reprise : `mcp__dispatcher__get_agent_result job_id:{} assignment_id:{}`",
                job_id, job_id, assignment_id
            );
            post_to_matrix_room(room_id, &msg).await;
            return;
        }
    }
}

/// Call a dispatcher MCP tool over plain JSON-RPC 2.0 (the dispatcher exposes a
/// stateless `POST /mcp`; no initialize handshake or SSE needed) and return the
/// tool's text content.
pub(crate) async fn dispatcher_tool_call(
    state: &ListenState,
    tool: &str,
    arguments: serde_json::Value,
) -> anyhow::Result<String> {
    let body = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/call",
        "params": { "name": tool, "arguments": arguments },
    });

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()?;
    let resp = client
        .post(&state.dispatcher_mcp_url)
        .json(&body)
        .send()
        .await?
        .error_for_status()?;
    let v: serde_json::Value = resp.json().await?;

    if let Some(err) = v.get("error") {
        let msg = err.get("message").and_then(|m| m.as_str()).unwrap_or("unknown dispatcher error");
        anyhow::bail!("dispatcher: {}", msg);
    }
    v["result"]["content"][0]["text"]
        .as_str()
        .map(|s| s.to_string())
        .ok_or_else(|| anyhow::anyhow!("dispatcher response missing text content"))
}

/// Extract the `job_id:` value from invoke_agent's text result.
pub(crate) fn parse_job_id(text: &str) -> Option<String> {
    text.lines()
        .find_map(|line| line.trim().strip_prefix("job_id:").map(|v| v.trim().to_string()))
        .filter(|s| !s.is_empty())
}

/// Extract the `assignment_id:` value from invoke_agent's text result.
pub(crate) fn parse_assignment_id(text: &str) -> Option<String> {
    text.lines()
        .find_map(|line| line.trim().strip_prefix("assignment_id:").map(|v| v.trim().to_string()))
        .filter(|s| !s.is_empty())
}

/// Map a `get_agent_result` reply to a [`JobStatus`]. The dispatcher prefixes
/// its reply with `status: completed|failed|running|pending`.
pub(crate) fn classify_job_status(text: &str) -> JobStatus {
    let t = text.trim_start();
    if let Some(rest) = t.strip_prefix("status: completed") {
        JobStatus::Completed(rest.trim().to_string())
    } else if let Some(rest) = t.strip_prefix("status: failed") {
        let reason = rest.trim();
        JobStatus::Failed(if reason.is_empty() { "job failed".to_string() } else { reason.to_string() })
    } else {
        JobStatus::Pending
    }
}

/// Clip a result summary to `max` characters (char-safe), appending an ellipsis
/// when truncated.
pub(crate) fn truncate_summary(s: &str, max: usize) -> String {
    let s = s.trim();
    if s.chars().count() <= max {
        return s.to_string();
    }
    let kept: String = s.chars().take(max.saturating_sub(1)).collect();
    format!("{}…", kept)
}

/// A retrievable link to the dispatch's Farga signals (parent trace + result).
pub(crate) fn farga_link(state: &ListenState, session_id: &str) -> String {
    format!("{}/signals/recent?project={}", state.farga_url, session_id)
}

/// Post a message directly to a Matrix room via the Synapse admin API — the
/// same credential path the SRE/backlog/dream posts use. Best-effort: failures
/// are logged, not propagated, since this runs in a detached poller.
#[cfg(test)]
mod handoff_helper_tests {
    use super::*;
    use crate::commands::listen::*;

    #[test]
    fn parse_job_id_extracts_from_dispatcher_text() {
        let text = "Agent job dispatched.\njob_id: agent-gardian-developer-ab12cd34\nsession_id: handoff-gardian-developer-ab12cd34\nassignment_id: assign-ab12cd34ef56\n\nPoll with get_agent_result(...).";
        assert_eq!(
            parse_job_id(text).as_deref(),
            Some("agent-gardian-developer-ab12cd34")
        );
    }

    #[test]
    fn parse_job_id_none_when_absent() {
        assert_eq!(parse_job_id("no id here\nsession_id: x"), None);
    }

    #[test]
    fn parse_assignment_id_extracts_from_dispatcher_text() {
        let text = "Agent job dispatched.\njob_id: agent-gardian-developer-ab12cd34\nsession_id: handoff-gardian-developer-ab12cd34\nassignment_id: assign-ab12cd34ef56\n\nPoll with get_agent_result(...).";
        assert_eq!(
            parse_assignment_id(text).as_deref(),
            Some("assign-ab12cd34ef56")
        );
    }

    #[test]
    fn parse_assignment_id_none_when_absent() {
        assert_eq!(parse_assignment_id("no id here\nsession_id: x"), None);
    }

    #[test]
    fn classify_completed_carries_result_body() {
        let status = classify_job_status("status: completed\n\nThe token cache now resolves in two hops.");
        assert_eq!(
            status,
            JobStatus::Completed("The token cache now resolves in two hops.".to_string())
        );
    }

    #[test]
    fn classify_failed_carries_reason() {
        let status = classify_job_status("status: failed (check pod logs: kubectl logs ...)");
        assert_eq!(
            status,
            JobStatus::Failed("(check pod logs: kubectl logs ...)".to_string())
        );
    }

    #[test]
    fn classify_running_and_pending_are_pending() {
        assert_eq!(classify_job_status("status: running"), JobStatus::Pending);
        assert_eq!(classify_job_status("status: pending"), JobStatus::Pending);
    }

    #[test]
    fn truncate_summary_leaves_short_text_untouched() {
        assert_eq!(truncate_summary("  short result  ", 500), "short result");
    }

    #[test]
    fn truncate_summary_clips_long_text_with_ellipsis() {
        let long = "x".repeat(600);
        let out = truncate_summary(&long, 500);
        assert_eq!(out.chars().count(), 500);
        assert!(out.ends_with('…'));
    }

    #[test]
    fn build_handoff_context_empty_without_optionals() {
        let h = HandoffRequest {
            domain: "gardian".into(),
            facet: "developer".into(),
            task: "do it".into(),
            context_ref: None,
            farga_project: None,
            allowed_tools: None,
        };
        assert!(build_handoff_context(&h).is_empty());
    }

    #[test]
    fn build_handoff_context_folds_in_optionals() {
        let h = HandoffRequest {
            domain: "gardian".into(),
            facet: "developer".into(),
            task: "do it".into(),
            context_ref: Some("gardian".into()),
            farga_project: Some("proj-7".into()),
            allowed_tools: None,
        };
        let ctx = build_handoff_context(&h);
        assert!(ctx.contains("Farga project `gardian`"));
        assert!(ctx.contains("project `proj-7`"));
    }
}
