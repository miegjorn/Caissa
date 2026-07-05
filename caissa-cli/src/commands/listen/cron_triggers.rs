use super::*;

/// Reads GH_TOKEN/GITHUB_TOKEN fresh from /creds/tokens.env at call time, so each
/// spawned `claude` subprocess picks up whatever the container's background refresh
/// loop most recently minted. This process's own inherited environment is fixed at
/// its own startup and never reflects later rewrites of that file, so every call
/// site that spawns `claude` must re-read here rather than relying on inherited env.
pub(crate) async fn run_sre_alert(state: &ListenState, anomalies: &[String]) -> anyhow::Result<()> {
    let mcp_config = serde_json::to_string(&serde_json::json!({
        "mcpServers": agent_mcp_servers(state)
    }))?;
    let mcp_path = std::env::temp_dir().join("guilhem-sre-alert-mcp.json");
    std::fs::write(&mcp_path, &mcp_config)?;

    let prompt = build_sre_alert_prompt(&state.fondament_path, anomalies);

    let output = tokio::process::Command::new("claude")
        .prefer_oauth_over_api_key()
        .args([
            "--print",
            &prompt,
            "--model",
            &state.dream_model,
            "--mcp-config",
            mcp_path.to_str().unwrap(),
            "--allowed-tools",
            "Bash,mcp__farga__search_signals,mcp__farga__write_signal,mcp__charradissa__matrix_send,mcp__nervi__nervi_subscribe",
        ])
        .env("FARGA_URL", &state.farga_url)
        .env("FARGA_PROJECT", &state.farga_project)
        .envs(github_token_envs())
        .output()
        .await?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("sre-alert claude exited with error: {}", stderr);
    }

    tracing::info!("sre-alert dispatch complete");
    Ok(())
}

pub(crate) fn build_sre_alert_prompt(fondament_path: &str, anomalies: &[String]) -> String {
    let anomaly_list = anomalies
        .iter()
        .map(|a| format!("  - {}", a))
        .collect::<Vec<_>>()
        .join("\n");

    format!(r###"You are Guilhem de Tudela, org agent. The SRE watchdog has detected health
anomalies, delivered directly to you (no fetch needed):

{anomalies}

{constraint}

---

## STEP 1 — Evaluate

For each anomaly identify the responsible component:
- "gardian" → dispatch subject: occitan.dispatch.gardian
- "farga" → dispatch subject: occitan.dispatch.farga
- "amassada" → dispatch subject: occitan.dispatch.amassada
- "charradissa" → dispatch subject: occitan.dispatch.charradissa
- "dispatcher" → dispatch subject: occitan.dispatch.caissa
- "nervi" → dispatch subject: occitan.dispatch.nervi
- "guilhem" → Escalate via Farga (cannot dispatch to yourself; write to Farga source="guilhem-sre-escalate")

## STEP 2 — Dispatch

For each affected component, publish a repair task to its Nervi dispatch subject:
nervi_publish(subject="occitan.dispatch.<component>", payload=JSON.stringify({{
  "type": "sre-repair",
  "anomaly": "<specific error description>",
  "task": "Investigate: check /health endpoint, review recent pod logs for errors, identify root cause. If a code fix is needed, open a PR following the standard issue→implement→PR→approval flow.",
  "class": 1,
  "dispatched_by": "guilhem-sre",
  "review_required": false
}}))

## STEP 3 — Record

Write a summary signal to Farga:
- source: "guilhem-sre-dispatch"
- content: "Dispatched SRE alerts to: <list>. Anomalies: <brief summary>. Timestamp: <now>"
"###,
        anomalies = anomaly_list,
        constraint = guilhem_dispatch_constraint(fondament_path),
    )
}

/// Constraint block injected at the top of every Guilhem prompt.
/// Loaded from Fondament skill YAML when available; hardcoded fallback for local dev.
pub(crate) fn guilhem_dispatch_constraint(fondament_path: &str) -> String {
    let skill_path = std::path::Path::new(fondament_path)
        .join("definitions/skills/caissa/scope-org-orchestrator.yaml");
    if let Ok(text) = std::fs::read_to_string(&skill_path) {
        #[derive(serde::Deserialize)]
        struct SkillRules { prompt_constraint: Option<String> }
        #[derive(serde::Deserialize)]
        struct SkillFile { rules: Option<SkillRules> }
        if let Ok(skill) = serde_yaml::from_str::<SkillFile>(&text) {
            if let Some(constraint) = skill.rules.and_then(|r| r.prompt_constraint) {
                return constraint;
            }
        }
    }
    // Hardcoded fallback (used in dev when Fondament checkout is absent)
    r#"## DISPATCH CONSTRAINT — read before acting

You are an orchestrator. You observe, classify, and route. You do NOT implement.

**Permitted Bash uses:**
- `gh issue list/create/view/edit` — read and manage GitHub issues and Initiatives/Epics
- `gh pr list/view` — check PR status
- `cat`, `ls`, `find`, `date`, `git log/status/diff` — read context

**Not permitted — dispatch to component agents instead:**
- Editing source files (no `sed`, `awk`, `patch`, `echo > file.rs`)
- Building or testing (`cargo`, `npm`, `make`, `pytest`, `go build`)
- Creating PRs that contain code changes
- Running migrations or deployments

**Dispatch hierarchy — this is a hard rule:**

To route work to a component: publish via nervi_publish to `occitan.dispatch.<component>`.
The component agent picks it up and routes it to its own specialists.

`invoke_agent` is ONLY for architect consultation (facet: "architect"). You may never
directly invoke a developer, qa, infra, db, or security specialist — that is the component
agent's responsibility. Always pass caller: "guilhem" so the dispatcher can enforce this.

Think of it as hockey: you put the line on the ice (nervi_publish → component agent).
The component agent distributes the puck to their own wingers. You do not reach over the
boards to hand the puck to a winger yourself.

If you catch yourself about to write code or spawn a code-writing agent: stop.
Formulate the task precisely and publish it via nervi_publish to `occitan.dispatch.<component>`.

---
"#.to_string()
}

pub(crate) fn build_chronicle_prompt(fondament_path: &str, reason: &str, project: &str) -> String {
    format!(
        r#"Chronicle trigger: {reason}

You are Guilhem de Tudela, chronicler of the Occitan stack. This is a scheduled
chronicle run for project "{project}".

{constraint}

## BEFORE ANYTHING ELSE — Load the context graph

Call mcp__farga__list_context_nodes (project: "{project}", role: "org") to see what
the stack knows about itself: codebase references, architecture summaries, design
rationale. This is your institutional memory. Read it before reading signals.

You have the Farga MCP server attached. Ground your chronicle in real state — use its
read tools before writing:
- list_context_nodes (project: "{project}", role: "org") — stack context graph (read first)
- search_signals (project: "{project}") — recent signals / activity
- read_context (project: "{project}") — accumulated project context
- list_projects — what projects exist

**SRE watchdog check** — before writing your chronicle, scan recent signals for any
with source "sre-watchdog". These are mechanical health alerts written by the no-LLM
watchdog when it detected an anomaly (unreachable service, missing signals, etc.).
If watchdog signals are present:
- Name each anomaly explicitly in your chronicle
- Assess whether it is still active or has self-resolved
- Note whether the SRE alert layer has already been triggered (source "sre-alert")
If no watchdog signals: note "watchdog: all clear" in one line and move on.

Then write a concise chronicle entry: what happened, what it means for the trajectory,
what is now different from before. Your written response IS the chronicle — it is
recorded to Farga automatically, so do not try to post it yourself.

Be faithful, not verbose. The chronicle is for future agents (including your next
instance) to understand where the stack stands.
"#,
        reason = reason,
        project = project,
        constraint = guilhem_dispatch_constraint(fondament_path),
    )
}

pub(crate) async fn run_chronicle(state: &ListenState, prompt: &str) -> anyhow::Result<()> {
    let model = &state.chronicle_model;

    if model.starts_with("grok") || model.starts_with("xai") {
        // Basic Grok path for complementary support
        let api_key = std::env::var("XAI_API_KEY")
            .map_err(|_| anyhow::anyhow!("XAI_API_KEY not set for grok chronicle model"))?;

        let client = reqwest::Client::new();
        let body = serde_json::json!({
            "model": model,
            "messages": [{"role": "user", "content": prompt}]
        });

        let resp = client
            .post("https://api.x.ai/v1/chat/completions")
            .header("Authorization", format!("Bearer {}", api_key))
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await?
            .error_for_status()?;

        let resp_json: serde_json::Value = resp.json().await?;
        let response = resp_json["choices"][0]["message"]["content"]
            .as_str()
            .unwrap_or("")
            .to_string();

        if !response.trim().is_empty() {
            post_signal(state, &response).await?;
        }
    } else {
        // Claude path with MCP
        let mcp_config = format!(
            r#"{{"mcpServers":{{"farga":{{"type":"http","url":"{}"}}}}}}"#,
            state.farga_mcp_url
        );
        let mcp_path = std::env::temp_dir().join("guilhem-mcp.json");
        std::fs::write(&mcp_path, &mcp_config)?;

        let output = tokio::process::Command::new("claude")
            .prefer_oauth_over_api_key()
            .args([
                "--print",
                prompt,
                "--model",
                model,
                "--mcp-config",
                mcp_path.to_str().unwrap(),
                "--allowed-tools",
                "mcp__farga__search_signals,mcp__farga__read_context,mcp__farga__list_projects,mcp__farga__update_component_todo",
            ])
            .env("FARGA_URL", &state.farga_url)
            .env("FARGA_PROJECT", &state.farga_project)
            .envs(github_token_envs())
            .output()
            .await?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("claude exited with error: {}", stderr);
        }

        let response = String::from_utf8_lossy(&output.stdout).to_string();

        if !response.trim().is_empty() {
            post_signal(state, &response).await?;
        }
    }

    Ok(())
}

// ── Backlog review ────────────────────────────────────────────────────────────

/// POST /trigger/backlog-review — CronWorkflow-triggered (weekly).
///
/// Guilhem reads open GitHub issues across miegjorn repos via gh CLI, applies
/// staleness heuristics, synthesizes a backlog review, and writes it to Farga.
/// If BACKLOG_MATRIX_ROOM_ID is set, also posts a summary to that Matrix room.
pub(crate) async fn handle_backlog_review(
    State(state): State<Arc<ListenState>>,
    Json(req): Json<TriggerReq>,
) -> StatusCode {
    tracing::info!("backlog-review trigger received: {}", req.reason);

    tokio::spawn(async move {
        match run_backlog_review(&state).await {
            Ok(_) => tracing::info!("backlog-review complete"),
            Err(e) => tracing::error!("backlog-review failed: {}", e),
        }
    });

    StatusCode::ACCEPTED
}

pub(crate) async fn run_backlog_review(state: &ListenState) -> anyhow::Result<()> {
    let mcp_config = format!(
        r#"{{"mcpServers":{{"farga":{{"type":"http","url":"{}"}}}}}}"#,
        state.farga_mcp_url
    );
    let mcp_path = std::env::temp_dir().join("guilhem-backlog-mcp.json");

    let model = &state.matrix_model;
    let prompt = build_backlog_review_prompt(&state.fondament_path, &state.farga_project);
    let review: String;

    if model.starts_with("grok") || model.starts_with("xai") {
        let api_key = std::env::var("XAI_API_KEY").map_err(|_| anyhow::anyhow!("XAI_API_KEY for grok"))?;
        let client = reqwest::Client::new();
        let body = serde_json::json!({
            "model": model,
            "messages": [{"role": "user", "content": prompt}]
        });
        let resp = client.post("https://api.x.ai/v1/chat/completions")
            .header("Authorization", format!("Bearer {}", api_key))
            .json(&body)
            .send().await?.error_for_status()?;
        let j: serde_json::Value = resp.json().await?;
        review = j["choices"][0]["message"]["content"].as_str().unwrap_or("").to_string();
        if review.trim().is_empty() {
            tracing::warn!("backlog-review: empty from grok");
            return Ok(());
        }
        post_signal(state, &review).await?;
    } else {
        std::fs::write(&mcp_path, &mcp_config)?;
        let output = tokio::process::Command::new("claude")
            .prefer_oauth_over_api_key()
            .args([
                "--print",
                &prompt,
                "--model",
                model,
                "--mcp-config",
                mcp_path.to_str().unwrap(),
                "--allowed-tools",
                "Bash,mcp__farga__search_signals,mcp__farga__read_context,mcp__farga__write_signal",
            ])
            .env("FARGA_URL", &state.farga_url)
            .env("FARGA_PROJECT", &state.farga_project)
            .envs(github_token_envs())
            .output()
            .await?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("backlog-review claude exited with error: {}", stderr);
        }

        review = String::from_utf8_lossy(&output.stdout).to_string();

        if review.trim().is_empty() {
            tracing::warn!("backlog-review: empty output from claude");
            return Ok(());
        }
        post_signal(state, &review).await?;
    }

    tracing::info!("backlog-review written to Farga");

    // Post to Matrix if configured
    if !state.backlog_matrix_room_id.is_empty() {
        let synapse_url = std::env::var("SYNAPSE_URL")
            .unwrap_or_else(|_| "http://synapse.occitan-system.svc.cluster.local:8008".into());
        let admin_token = std::env::var("SYNAPSE_ADMIN_TOKEN").unwrap_or_default();

        if admin_token.is_empty() {
            tracing::warn!("backlog-review: SYNAPSE_ADMIN_TOKEN not set — skipping Matrix post");
            return Ok(());
        }

        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(15))
            .build()?;

        // Truncate to a readable Matrix summary (first 2000 chars)
        let summary = if review.len() > 2000 {
            format!("{}…\n\n(full review written to Farga)", &review[..2000])
        } else {
            review.clone()
        };

        let txn = uuid::Uuid::new_v4();
        let matrix_url = format!(
            "{}/_matrix/client/v3/rooms/{}/send/m.room.message/{}",
            synapse_url, state.backlog_matrix_room_id, txn
        );

        client
            .put(&matrix_url)
            .bearer_auth(&admin_token)
            .json(&serde_json::json!({ "msgtype": "m.text", "body": summary }))
            .send()
            .await?
            .error_for_status()?;

        tracing::info!("backlog-review posted to Matrix room {}", state.backlog_matrix_room_id);
    }

    Ok(())
}

pub(crate) fn build_backlog_review_prompt(fondament_path: &str, project: &str) -> String {
    format!(
        r#"You are Guilhem de Tudela, org agent for the Occitan stack. This is a scheduled
backlog review run for the miegjorn GitHub organisation.

{constraint}

## What to do

1. **Fetch open issues** across all miegjorn repos using Bash:
   ```
   gh issue list --repo miegjorn/Caissa --state open --json number,title,createdAt,updatedAt,labels,body --limit 100
   gh issue list --repo miegjorn/Farga --state open --json number,title,createdAt,updatedAt,labels,body --limit 100
   gh issue list --repo miegjorn/Fondament --state open --json number,title,createdAt,updatedAt,labels,body --limit 100
   gh issue list --repo miegjorn/Gardian --state open --json number,title,createdAt,updatedAt,labels,body --limit 100
   gh issue list --repo miegjorn/Amassada --state open --json number,title,createdAt,updatedAt,labels,body --limit 100
   gh issue list --repo miegjorn/Charradissa --state open --json number,title,createdAt,updatedAt,labels,body --limit 100
   gh issue list --repo miegjorn/Cor --state open --json number,title,createdAt,updatedAt,labels,body --limit 100
   ```

2. **Apply staleness heuristics** — flag each of the following explicitly:
   - Issues open **>14 days with no update** (updatedAt older than 14 days ago)
   - Issues with **no Epic parent** (no "Parent Epic" mention in body, not labelled as an Epic itself)
   - **Epics with no open sub-issues** (issues labelled Epic or titled "Epic:" with no referenced open child issues)
   - Issues with **no assignee and no recent activity** — potential blockers without owners

3. **Read Farga context** for recent signals (project: "{project}") to connect backlog state
   to what the stack has been doing lately.

4. **Synthesize** a concise backlog review:
   - Count open issues per repo
   - List flagged items (stale, orphan, blocked) with issue numbers
   - Note any priority drift — issues that should be moving but aren't
   - Note any structural gaps — missing Epics, issues with no clear parent

5. **Write the review to Farga** using mcp__farga__write_signal with:
   - project: "{project}"
   - source: "backlog-review"
   - content: your full synthesis

Your written response IS the review — keep it crisp and actionable, not exhaustive.
Today's date is available via `date` in Bash.
"#,
        project = project,
        constraint = guilhem_dispatch_constraint(fondament_path),
    )
}

// ── Dream — nightly consolidation ─────────────────────────────────────────────

/// Self-paced daily (03:00 UTC) via the tick-poller (Task 2/6) publishing a
/// `PerceivedMessage::Tick { skill: "dream" }` on this component's tick
/// subject, perceived by chat_loop::run_tick_stream and routed here directly
/// -- no HTTP trigger anymore.
///
/// Four-phase session (see build_dream_prompt for the full text):
/// 1. GATHER — read Farga signals (past 24h) + GitHub state across all repos
/// 2. SYNTHESIZE — identify drift, improvement opportunities, patterns
/// 3. ACT — create GitHub issues for actionable gaps; write dream report to Farga
/// 4. ADVERSARIAL CHALLENGE — read system-defence.md live from fondament-server,
///    web-search prior art for architecturally non-obvious findings, classify
///    against Class 1-4, write dream-adversarial signals that run_dispatch
///    (1h later) picks up and routes autonomously (Class 1/2) or defers to
///    Pierre-Luc (Class 3/4)
pub(crate) async fn run_dream(state: &ListenState) -> anyhow::Result<()> {
    // Was farga-only until 2026-07-04: the dream prompt (build_dream_prompt,
    // Phase 4) instructs Guilhem to dispatch via nervi_publish, but the nervi
    // MCP server was never actually registered here, so every nervi_publish
    // call failed and the dream silently fell back to writing dispatch intent
    // as plain Farga signals instead — confirmed live, in a real dream
    // report ("Dispatches executed (via Farga signals — nervi_publish
    // unavailable)"). Reuse agent_mcp_servers(state), the same full server
    // set run_dispatch/run_mission_pulse/run_intake already use, instead of a
    // second hand-rolled farga-only config drifting out of sync with them.
    let mcp_config = serde_json::to_string(&serde_json::json!({
        "mcpServers": agent_mcp_servers(state)
    }))?;
    let mcp_path = std::env::temp_dir().join("guilhem-dream-mcp.json");
    std::fs::write(&mcp_path, &mcp_config)?;

    let prompt = build_dream_prompt(&state.fondament_path, &state.farga_project);

    let output = tokio::process::Command::new("claude")
        .prefer_oauth_over_api_key()
        .args([
            "--print",
            &prompt,
            "--model",
            &state.dream_model,
            "--mcp-config",
            mcp_path.to_str().unwrap(),
            "--allowed-tools",
            "Bash,WebSearch,WebFetch,mcp__farga__search_signals,mcp__farga__read_context,mcp__farga__write_signal,mcp__farga__update_component_todo,mcp__nervi__nervi_publish,mcp__nervi__nervi_subscribe",
        ])
        .env("FARGA_URL", &state.farga_url)
        .env("FARGA_PROJECT", &state.farga_project)
        .envs(github_token_envs())
        .output()
        .await?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("dream claude exited with error: {}", stderr);
    }

    let report = String::from_utf8_lossy(&output.stdout).to_string();

    if report.trim().is_empty() {
        tracing::warn!("dream: empty output from claude");
        return Ok(());
    }

    tracing::info!("dream complete — report written to Farga by agent");

    // Post summary to Matrix if configured
    if !state.dream_matrix_room_id.is_empty() {
        let synapse_url = std::env::var("SYNAPSE_URL")
            .unwrap_or_else(|_| "http://synapse.occitan-system.svc.cluster.local:8008".into());
        let admin_token = std::env::var("SYNAPSE_ADMIN_TOKEN").unwrap_or_default();

        if admin_token.is_empty() {
            tracing::warn!("dream: SYNAPSE_ADMIN_TOKEN not set — skipping Matrix post");
            return Ok(());
        }

        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(15))
            .build()?;

        let summary = if report.len() > 2000 {
            format!("{}…\n\n(full dream report written to Farga)", &report[..2000])
        } else {
            report.clone()
        };

        let txn = uuid::Uuid::new_v4();
        let matrix_url = format!(
            "{}/_matrix/client/v3/rooms/{}/send/m.room.message/{}",
            synapse_url, state.dream_matrix_room_id, txn
        );

        client
            .put(&matrix_url)
            .bearer_auth(&admin_token)
            .json(&serde_json::json!({ "msgtype": "m.text", "body": summary }))
            .send()
            .await?
            .error_for_status()?;

        tracing::info!("dream report posted to Matrix room {}", state.dream_matrix_room_id);
    }

    Ok(())
}

pub(crate) fn build_dream_prompt(fondament_path: &str, project: &str) -> String {
    format!(
        r###"You are Guilhem de Tudela, org agent for the Occitan stack. This is the nightly
dream consolidation run. A dream has three phases — follow them in order.

{constraint}

---

## PHASE 1: GATHER (read, do not act yet)

1. Get today's date: `date -u '+%Y-%m-%d'`

2. Load the context graph: mcp__farga__list_context_nodes (project: "{project}", role: "org").
   For each node that looks relevant to tonight's synthesis, call mcp__farga__read_context_node
   to fetch its content. Pay particular attention to [occitan][system-rationale] — it defines
   the constraints that govern all architectural decisions.

3. Read Farga signals from the past 24 hours using mcp__farga__search_signals with
   since = yesterday's ISO 8601 timestamp. Note what changed: what was built,
   what was fixed, what was flagged.

4. Read the Farga project context (mcp__farga__read_context, project: "{project}") to
   understand the stack's current trajectory and open todos.

5. Fetch GitHub state across all 8 repos. Run these in sequence:
   ```
   for repo in Gardian Fondament Farga Amassada Charradissa Cor Caissa Occitan; do
     echo "=== $repo open issues ==="
     gh issue list --repo miegjorn/$repo --state open --json number,title,createdAt,updatedAt,labels --limit 50
     echo "=== $repo recent commits (24h) ==="
     gh api repos/miegjorn/$repo/commits --jq '.[0:5] | .[] | "\(.sha[:8]) \(.commit.message | split("\n")[0])"'
   done
   ```

6. Fetch open PRs across all repos:
   ```
   for repo in Gardian Fondament Farga Amassada Charradissa Cor Caissa Occitan; do
     gh pr list --repo miegjorn/$repo --state open --json number,title,createdAt,labels
   done
   ```

---

## PHASE 2: SYNTHESIZE (think before acting)

Using what you gathered, reason through:

- **What was actually built or fixed in the past 24h?** (from Farga signals + commits)
- **What improvement opportunities exist that are NOT already tracked as open issues?**
  Focus on: doc drift between code and README, unclosed stubs, missing integrations,
  architectural gaps that became visible from yesterday's activity.
- **Cross-repo implications**: does a change in one repo create a gap in another?
  (e.g. a new Amassada endpoint that Charradissa doesn't call yet)
- **Pattern signals**: recurring themes across multiple signals (e.g. "three signals
  about credential flow" → underlying structural issue)
- **What is the stack dreaming toward?** What does the trajectory imply about what
  should be built next?

For each opportunity you identify, decide:
- Is it actionable enough for a GitHub issue right now?
- Which repo does it belong in?
- What labels? (bug / enhancement / documentation / technical-debt)
- Does a similar open issue already exist? (check before creating)

---

## PHASE 3: ACT

**For each actionable improvement opportunity** (aim for 3–8, quality over quantity):

1. Verify no duplicate exists: `gh issue list --repo miegjorn/<repo> --state open --search "<key term>"`
2. Create the issue:
   ```
   gh issue create \
     --repo miegjorn/<repo> \
     --title "<concise, specific, actionable title>" \
     --body "Context\n<what was observed and why it matters>\n\nProposed approach\n<what the fix would involve>\n\nSource: identified during nightly dream consolidation {{date}}." \
     --label "<appropriate label(s)>"
   ```
3. Note the created issue URL for the dream report.

**Write the dream report to Farga** using mcp__farga__write_signal:
- project: "{project}"
- source: "dream"
- content: A structured summary including:
  - Date of dream
  - Key observations from the 24h window (3–5 bullet points)
  - Improvement opportunities identified (with reasoning)
  - GitHub issues created (with URLs)
  - Stack trajectory note: what does today's dream imply about where the stack is heading?

---

## PHASE 4: ADVERSARIAL CHALLENGE

The dream is not only consolidation — it is also where the stack challenges itself.
Prior art exists. Other systems have solved similar problems. Not consulting it is a
form of local-optimum convergence. This phase is mandatory.

1. Read the system-defence axioms before evaluating anything:
   `curl -s http://fondament.occitan-system.svc.cluster.local:7800/file/fondament/system-defence.md`

2. For each architecturally non-obvious finding from Phase 2 (patterns, recurring signals,
   structural gaps — not simple bug fixes), search the web for prior art:
   - Query pattern: "how do [Rust async / event-sourced / multi-agent] systems handle [the pattern]?"
   - Search: mature implementations of agent orchestration, distributed chronicle/memory,
     CQRS event sourcing, Rust actor patterns, MCP multi-server composition.
   - You are looking for: materially better approaches, known failure modes of the current
     approach, standard patterns the stack might be missing or misapplying.

3. For each finding where web search reveals a materially better approach:
   a. Evaluate it against the system-defence axioms (A-1 through A-8).
   b. Classify it using the risk table (Class 1–4).
   c. Write a "challenge proposal" signal to Farga using mcp__farga__write_signal:
      - source: "dream-adversarial"
      - content (structured):
        * Current approach: [one sentence]
        * Prior art suggests: [1-2 sentences, include search reference]
        * Axiom check: [which axioms are relevant, any conflicts?]
        * Risk class: [1/2/3/4]
        * Recommended action: [dispatch / review / escalate to Pierre-Luc / reject]
   d. Do NOT act on Class 3 or Class 4 proposals autonomously — the signal is the action.

4. If no materially better approaches were found for any finding:
   Write a brief Farga signal (source: "dream-adversarial") confirming the stack's current
   patterns align with prior art. This is meaningful evidence of intentional design.

Note: A challenge that gets rejected (Class 4) is worth recording — it is evidence of
deliberate design choice over convenience. The adversarial phase is not skepticism for its
own sake; it is how the stack avoids mistaking inertia for wisdom.

**Your written response** is the dream report — concise, substantive, forward-looking.
Do not just narrate what you did. Chronicle what the stack is becoming.
"###,
        project = project,
        constraint = guilhem_dispatch_constraint(fondament_path),
    )
}

// ── Farcaster — cross-component pattern detection ─────────────────────────────

/// Self-paced periodic skill (see corrier_core's schedule_tick convention,
/// armed via a PUT to {farga_url}/kv/schedule/guilhem__farcaster). Reads
/// recent signals across every known project, looks for the cross-component
/// pattern occitan/amassada's "Cross-X" section describes, and for anything
/// worth sharing, dispatches via nervi_publish or writes a Farga signal --
/// the same delivery convention every other Guilhem skill already uses, not
/// a new mechanism.
pub(crate) async fn run_farcaster(state: &ListenState) -> anyhow::Result<()> {
    let mcp_config = serde_json::to_string(&serde_json::json!({
        "mcpServers": agent_mcp_servers(state)
    }))?;
    let mcp_path = std::env::temp_dir().join("guilhem-farcaster-mcp.json");
    std::fs::write(&mcp_path, &mcp_config)?;

    let prompt = build_farcaster_prompt(&state.fondament_path, &state.farga_project);

    let output = tokio::process::Command::new("claude")
        .prefer_oauth_over_api_key()
        .args([
            "--print",
            &prompt,
            "--model",
            &state.dream_model,
            "--mcp-config",
            mcp_path.to_str().unwrap(),
            "--allowed-tools",
            "Bash,mcp__farga__search_signals,mcp__farga__read_context,mcp__farga__write_signal,mcp__farga__list_projects,mcp__nervi__nervi_publish,mcp__nervi__nervi_subscribe",
        ])
        .env("FARGA_URL", &state.farga_url)
        .env("FARGA_PROJECT", &state.farga_project)
        .envs(github_token_envs())
        .output()
        .await?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("farcaster claude exited with error: {}", stderr);
    }

    let report = String::from_utf8_lossy(&output.stdout).to_string();

    if report.trim().is_empty() {
        tracing::warn!("farcaster: empty output from claude");
        return Ok(());
    }

    tracing::info!("farcaster: cross-component pass complete");
    Ok(())
}

pub(crate) fn build_farcaster_prompt(fondament_path: &str, project: &str) -> String {
    format!(
        r###"You are Guilhem de Tudela, org agent for the Occitan stack. This is your
self-paced Farcaster tick -- a cross-component pattern-detection pass, per
your occitan/amassada skill's "Cross-X" section.

{constraint}

## STEP 1 — Gather

List all known projects: mcp__farga__list_projects. For each, read recent
signals: mcp__farga__search_signals (project: "<project>", since: last 6
hours or your last farcaster run, whichever you can determine -- if unsure,
use the last 6 hours as a safe default).

## STEP 2 — Look for cross-component patterns

At your level (component owner across all 8: Gardian, Fondament, Farga,
Amassada, Charradissa, Cor, Caissa, Nervi), "sibling" means these 8
components. Look specifically for:
- A pattern repeating across more than one component's recent signals
- A lesson one component already learned (a fix, a workaround) that another
  is about to relearn independently
- A connection between two components' recent activity that isn't visible
  from inside either one alone

Do not just summarize what you read -- that is chronicle's job, not this
one. Only what a single-component view would have missed counts here.

## STEP 3 — Propagate, if warranted

For each finding worth sharing:
- If it's actionable now for a specific component: nervi_publish(subject=
  "occitan.dispatch.<component>", payload=JSON.stringify({{"type":"cross-
  component-note","task":"<what to check or apply>","dispatched_by":
  "guilhem-farcaster","class":1}}))
- If it's context for later, not an instruction: mcp__farga__write_signal
  (project: "{project}", source: "guilhem-farcaster", content: "<the
  finding, and why it matters across components>")

Not every pass finds something. If nothing crosses the bar this run, write
one brief Farga signal saying so (source: "guilhem-farcaster", content:
"cross-component pass: no findings this cycle") and stop -- do not force a
finding to justify the tick.

## STEP 4 — Re-arm your own next tick

Near the end of this run, PUT your next scheduled wake to Farga's KV store:

curl -X PUT {{farga_url}}/kv/schedule/guilhem__farcaster \
  -H "Content-Type: application/json" \
  -d '{{"value": {{"next_due": "<ISO8601 UTC, your own judgment -- default
  roughly 6 hours out if nothing suggests otherwise>", "note": "<one line on
  why this interval>"}}, "ttl_seconds": 2592000}}'

Use the actual {{farga_url}} value from your own environment (FARGA_URL),
not the literal string above.
"###,
        constraint = guilhem_dispatch_constraint(fondament_path),
        project = project,
    )
}
