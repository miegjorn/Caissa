use super::*;

pub(crate) fn is_component_agent(state: &ListenState) -> bool {
    state.generation.ends_with("-agent") && state.farga_project != "occitan"
}

pub(crate) async fn run_nervi_loop_if_component(state: Arc<ListenState>) {
    if !is_component_agent(&state) {
        return;
    }
    let component = state.farga_project.clone();
    tracing::info!("[component-agent/{component}] Nervi subscriber loop starting");
    loop {
        let issues_subject = format!("occitan.issues.{component}");
        let dispatch_subject = format!("occitan.dispatch.{component}");

        let mut tagged: Vec<serde_json::Value> = Vec::new();

        match poll_nervi_subject(&state, &issues_subject, 20).await {
            Ok(msgs) if !msgs.is_empty() => {
                tracing::info!("[component-agent/{component}] {} issue messages", msgs.len());
                for m in msgs {
                    tagged.push(serde_json::json!({"type": "issue", "data": m}));
                }
            }
            Err(e) => tracing::warn!("[component-agent/{component}] issues poll error: {e}"),
            _ => {}
        }

        match poll_nervi_subject(&state, &dispatch_subject, 10).await {
            Ok(msgs) if !msgs.is_empty() => {
                tracing::info!("[component-agent/{component}] {} dispatch messages", msgs.len());
                for m in msgs {
                    tagged.push(serde_json::json!({"type": "dispatch", "data": m}));
                }
            }
            Err(e) => tracing::warn!("[component-agent/{component}] dispatch poll error: {e}"),
            _ => {}
        }

        if !tagged.is_empty() {
            let payload = serde_json::to_string(&tagged).unwrap_or_default();
            if let Err(e) = run_component_agent(&state, &component, &payload).await {
                tracing::error!("[component-agent/{component}] agent run failed: {e}");
            }
        }

        tokio::time::sleep(std::time::Duration::from_secs(60)).await;
    }
}

/// Direct HTTP call to the Nervi MCP server to drain messages from a subject.
/// Returns the parsed message array; empty vec on any parse failure.
pub(crate) async fn poll_nervi_subject(state: &ListenState, subject: &str, max_messages: u32) -> anyhow::Result<Vec<serde_json::Value>> {
    if state.nervi_mcp_url.is_empty() {
        return Ok(vec![]);
    }
    // Derive a stable consumer name from the subject (durable — survives across polls).
    let consumer_name = subject.replace('.', "-");
    let client = reqwest::Client::new();
    let body = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/call",
        "params": {
            "name": "nervi_subscribe",
            "arguments": {
                "subject": subject,
                "consumer_name": consumer_name,
                "max_messages": max_messages
            }
        }
    });
    let resp = client
        .post(&state.nervi_mcp_url)
        // nervi-mcp TypeScript server requires SSE-capable Accept header.
        .header("Accept", "application/json, text/event-stream")
        .json(&body)
        .timeout(std::time::Duration::from_secs(15))
        .send()
        .await?
        .text()
        .await?;
    // SSE frames each response as "event: message\ndata: {...}\n\n"; unwrap the data line.
    let json_str = resp.lines()
        .find(|l| l.starts_with("data: "))
        .map(|l| &l["data: ".len()..])
        .unwrap_or(&resp);
    let resp: serde_json::Value = serde_json::from_str(json_str).unwrap_or(serde_json::Value::Null);
    Ok(parse_nervi_messages(&resp))
}

/// Parse the MCP tools/call response for nervi_subscribe.
///
/// nervi-mcp's actual response shape (confirmed live against a real
/// nervi_subscribe call) is NOT a bare array — it's an object with a
/// `messages` field, present in two equivalent places:
///   {"result": {
///     "structuredContent": {"subject":.., "count":.., "messages": [...]},
///     "content": [{"type": "text", "text": "<the same object, JSON-stringified>"}]
///   }}
/// Each element of `messages` is `{sequence, subject, qualifier, payload, timestamp}`,
/// where `payload` is itself a JSON string (the original nervi_publish payload) —
/// left as-is here; the component-agent prompt (build_component_agent_prompt)
/// dumps the whole message batch as JSON for the LLM to interpret directly.
///
/// Previously this parsed `text` as if it WERE the array directly, which always
/// failed silently (returning an empty vec, not an error) and both fallback
/// branches also missed the real shape — every dispatch/issue message delivered
/// this way was fetched-and-acked at the NATS layer, then silently dropped
/// before ever reaching a component agent's application logic.
pub(crate) fn parse_nervi_messages(resp: &serde_json::Value) -> Vec<serde_json::Value> {
    if let Some(msgs) = resp
        .get("result")
        .and_then(|r| r.get("structuredContent"))
        .and_then(|sc| sc.get("messages"))
        .and_then(|m| m.as_array())
    {
        return msgs.clone();
    }
    if let Some(text) = resp
        .get("result")
        .and_then(|r| r.get("content"))
        .and_then(|c| c.as_array())
        .and_then(|a| a.first())
        .and_then(|item| item.get("text"))
        .and_then(|t| t.as_str())
    {
        if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(text) {
            if let Some(msgs) = parsed.get("messages").and_then(|m| m.as_array()) {
                return msgs.clone();
            }
            // Back-compat: text itself is already a bare array.
            if let Some(arr) = parsed.as_array() {
                return arr.clone();
            }
        }
    }
    Vec::new()
}

#[cfg(test)]
mod nervi_message_parsing_tests {
    use super::*;
    use crate::commands::listen::*;

    /// Captured live against a real nervi_subscribe call through nervi-mcp —
    /// this is the actual response shape, not an assumed one.
    fn real_response_with_one_message() -> serde_json::Value {
        serde_json::json!({
            "result": {
                "content": [{
                    "type": "text",
                    "text": "{\n  \"subject\": \"occitan.test.parsecheck\",\n  \"consumer_name\": \"test-parsecheck-consumer\",\n  \"count\": 1,\n  \"messages\": [\n    {\n      \"sequence\": 48,\n      \"subject\": \"occitan.test.parsecheck\",\n      \"qualifier\": \"info\",\n      \"payload\": \"{\\\"hello\\\":\\\"world\\\"}\",\n      \"timestamp\": \"2026-07-04T05:33:51Z\"\n    }\n  ]\n}"
                }],
                "structuredContent": {
                    "subject": "occitan.test.parsecheck",
                    "consumer_name": "test-parsecheck-consumer",
                    "count": 1,
                    "messages": [{
                        "sequence": 48,
                        "subject": "occitan.test.parsecheck",
                        "qualifier": "info",
                        "payload": "{\"hello\":\"world\"}",
                        "timestamp": "2026-07-04T05:33:51Z"
                    }]
                }
            },
            "jsonrpc": "2.0",
            "id": 1
        })
    }

    #[test]
    fn parses_structured_content_messages() {
        let resp = real_response_with_one_message();
        let msgs = parse_nervi_messages(&resp);
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0]["sequence"], 48);
        assert_eq!(msgs[0]["payload"], "{\"hello\":\"world\"}");
    }

    #[test]
    fn falls_back_to_text_content_when_structured_content_absent() {
        let mut resp = real_response_with_one_message();
        resp.as_object_mut().unwrap().get_mut("result").unwrap()
            .as_object_mut().unwrap().remove("structuredContent");
        let msgs = parse_nervi_messages(&resp);
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0]["sequence"], 48);
    }

    #[test]
    fn empty_messages_array_returns_empty_vec() {
        let resp = serde_json::json!({
            "result": {"structuredContent": {"count": 0, "messages": []}}
        });
        assert!(parse_nervi_messages(&resp).is_empty());
    }

    #[test]
    fn malformed_response_returns_empty_vec_not_panic() {
        let resp = serde_json::json!({"unexpected": "shape"});
        assert!(parse_nervi_messages(&resp).is_empty());
    }

    #[test]
    fn regression_bare_array_in_text_is_no_longer_the_only_supported_shape() {
        // The pre-fix code assumed `text` was directly a JSON array. That shape
        // never actually occurs from nervi-mcp, but keep supporting it as a
        // back-compat fallback rather than silently dropping it.
        let resp = serde_json::json!({
            "result": {
                "content": [{"type": "text", "text": "[{\"sequence\": 1}]"}]
            }
        });
        let msgs = parse_nervi_messages(&resp);
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0]["sequence"], 1);
    }
}

pub(crate) fn component_mcp_servers(state: &ListenState) -> serde_json::Value {
    let mut servers = serde_json::json!({
        "farga":      { "type": "http", "url": state.farga_mcp_url },
        "dispatcher": { "type": "http", "url": state.dispatcher_mcp_url },
        "nervi":      { "type": "http", "url": state.nervi_mcp_url },
    });
    // Wire Charradissa when configured — component agents that live in Matrix rooms
    // (e.g. nervi-agent) need matrix_request_approval and matrix_send.
    if !state.charradissa_mcp_url.is_empty() {
        servers["charradissa"] = serde_json::json!({ "type": "http", "url": state.charradissa_mcp_url });
    }
    servers
}

pub(crate) async fn component_allowed_tools(fondament_url: &str, component: &str) -> Vec<String> {
    let def_name = format!("{}-agent", component);
    if let Ok(def) = fetch_fondament_def(fondament_url, &def_name).await {
        let from_def: Vec<String> = def.tools.always_on.iter()
            .map(tool_to_claude_name)
            .collect();
        if !from_def.is_empty() {
            return from_def;
        }
    }
    // Hardcoded fallback
    vec![
        "Bash".to_string(),
        "mcp__farga__search_signals".to_string(),
        "mcp__farga__read_context".to_string(),
        "mcp__farga__write_signal".to_string(),
        "mcp__farga__update_component_todo".to_string(),
        "mcp__farga__read_context_node".to_string(),
        "mcp__farga__list_context_nodes".to_string(),
        "mcp__dispatcher__invoke_agent".to_string(),
        "mcp__dispatcher__get_agent_result".to_string(),
        "mcp__dispatcher__list_agent_specs".to_string(),
        "mcp__charradissa__matrix_send".to_string(),
        "mcp__charradissa__matrix_request_approval".to_string(),
        "mcp__nervi__nervi_publish".to_string(),
        "mcp__nervi__nervi_subscribe".to_string(),
    ]
}

/// Invoke the component agent orchestrator for a batch of Nervi messages.
/// Supports complementary models: claude* via claude CLI (full MCP/tools),
/// grok* via basic xAI API (no MCP/tools in basic path; use endpoint for full agentic).
pub(crate) async fn run_component_agent(state: &ListenState, component: &str, payload: &str) -> anyhow::Result<()> {
    let def_name = format!("{component}-agent");
    let effective_model = match fetch_fondament_def(&state.fondament_url, &def_name).await {
        Ok(def) => def.default_model.unwrap_or_else(|| state.chronicle_model.clone()),
        Err(_) => state.chronicle_model.clone(),
    };

    let persona_context = load_component_persona(state, component).await;
    let prompt = build_component_agent_prompt(component, &state.farga_project, payload, &persona_context);

    if effective_model.starts_with("grok") || effective_model.starts_with("xai") {
        // Basic Grok path for complementary support. No MCP/tool loop in this path.
        // For full agentic with tools for Grok, configure the component participant in Amassada
        // canvas to use an endpoint pointing at a Grok-backed service.
        let api_key = std::env::var("XAI_API_KEY")
            .map_err(|_| anyhow::anyhow!("XAI_API_KEY not set for grok model in component agent"))?;

        let client = reqwest::Client::new();
        let body = serde_json::json!({
            "model": effective_model,
            "messages": [
                {"role": "system", "content": persona_context},
                {"role": "user", "content": prompt}
            ]
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
        // Claude path (full MCP/tools support)
        let mcp_config = serde_json::to_string(&serde_json::json!({
            "mcpServers": component_mcp_servers(state)
        }))?;
        let mcp_path = std::env::temp_dir().join(format!("{component}-agent-mcp.json"));
        std::fs::write(&mcp_path, &mcp_config)?;

        let tools = component_allowed_tools(&state.fondament_url, component).await.join(",");

        let output = tokio::process::Command::new("claude")
            .args([
                "--print",
                &prompt,
                "--model",
                &effective_model,
                "--mcp-config",
                mcp_path.to_str().unwrap(),
                "--allowed-tools",
                &tools,
            ])
            .env("FARGA_URL", &state.farga_url)
            .env("FARGA_PROJECT", &state.farga_project)
            .envs(github_token_envs())
            .output()
            .await?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("component agent exited non-zero: {stderr}");
        }

        let response = String::from_utf8_lossy(&output.stdout).to_string();
        if !response.trim().is_empty() {
            post_signal(state, &response).await?;
        }
    }
    Ok(())
}

pub(crate) async fn load_component_persona(state: &ListenState, component: &str) -> String {
    let def_name = format!("{component}-agent");
    match fetch_fondament_def(&state.fondament_url, &def_name).await {
        Ok(def) => def.context,
        Err(_) => format!("You are the {component} component agent for the Occitan stack."),
    }
}

pub(crate) fn build_component_agent_prompt(component: &str, project: &str, payload: &str, persona_context: &str) -> String {
    format!(
        r###"{persona_context}

---

## Nervi message batch

Messages received from Nervi subjects for component "{component}" (project "{project}"):

```json
{payload}
```

Each entry has: `"type"` ("issue" or "dispatch") and `"data"` containing the payload.

---

## Your task

You are the orchestrator for {component}. You do NOT implement code directly —
you read each message, decide what work it requires, and spawn the right specialist
agents via the Dispatcher MCP.

## COMPONENT AGENT CONSTRAINT

Same rule as Guilhem: you orchestrate, you do not implement.
Permitted: mcp__dispatcher__invoke_agent, nervi_publish, Farga reads/writes, Bash for `gh`.
Not permitted: editing source files, running builds, committing code.

**Dispatch scope — hard rule:**

You are the `{component}` component agent. You may ONLY dispatch to specialists within
your own domain. Every invoke_agent call must have:
- domain: "{component}"
- caller: "{component}"

Never use a domain other than "{component}". If work requires another component, publish
via nervi_publish to `occitan.issues.<other-component>` and let Guilhem coordinate —
you are not the coordinator for other domains. The dispatcher will reject out-of-scope
calls and you will not be able to override it.

---

### Step 1 — Load your context

Call mcp__farga__read_context_node (path: "[{component}][codebase]", role: "component")
to learn where your code lives and what your CLAUDE.md says. This is your primary
self-knowledge — what you are, where your code is, how it is structured.

Call mcp__farga__read_context_node (path: "[{component}][architecture]", role: "architect")
if you need to understand design constraints before routing work to specialists.

Call mcp__farga__read_context (project: "{project}") to orient yourself in the stack.
Call mcp__farga__search_signals (project: "{project}") to see recent activity and
your current mission (source="mission-pulse" or source="component-mission").

### Step 2 — Process each message by issue label

**`epic` label — Mission assignment from Guilhem:**
This is a component-level goal for your current cycle. Your response:
1. Read and understand the Epic fully (body, parent Initiative reference, definition of done).
2. Consult your architect: invoke `{component}/architect` via Dispatcher with task:
   "Break this Epic into Stories for execution. Epic: '<title>'. Body: '<body>'.
   Return 3-6 Stories: each independently testable, clearly scoped, with concrete acceptance criteria."
3. Wait for architect result (mcp__dispatcher__get_agent_result).
4. Create each Story as a GitHub issue:
   ```
   gh issue create --repo miegjorn/<Component> \
     --title "<story title>" \
     --body "<story description>\n\nAcceptance criteria:\n- <criterion>\n\nParent Epic: <repo>#<number>" \
     --label "story"
   ```
5. Write your adopted mission to Farga: source="component-mission",
   content="Epic adopted: '<title>'. Stories created: <list>. This defines {component}'s direction this cycle."

**`story` label — Work package to decompose:**
1. Read the Story and its parent Epic (from the body's "Parent Epic" reference).
2. Determine what specialist(s) are needed:
   - Code implementation → invoke `{component}/developer`
   - Tests or quality gate → invoke `{component}/qa`
   - Architecture decision → invoke `{component}/architect`
   - Documentation → invoke `{component}/librarian`
3. Create Task issues for each piece:
   ```
   gh issue create --repo miegjorn/<Component> \
     --title "<task title>" \
     --body "<task description>\n\nParent Story: <repo>#<number>" \
     --label "task"
   ```
4. Dispatch each task to the appropriate specialist via mcp__dispatcher__invoke_agent.

**`task` label — Executable order:**
Dispatch directly to the appropriate specialist agent. No decomposition needed.
- Code → `{component}/developer`
- Tests → `{component}/qa`
- Review → `{component}/reviewer`
- Docs → `{component}/librarian`

**No label or `bug`/`enhancement` — treat as Story:**
Assess, decompose if needed, dispatch appropriately.

**`initiative` label — Escalate to Guilhem:**
Initiatives are Guilhem's domain. Write a Farga signal source="component-escalation":
"Received an Initiative issue directly — routing to Guilhem: <title>."
Do not process it yourself.

---

**For dispatch messages** (tasks published by Guilhem via `occitan.dispatch.{component}`):
- The payload includes: type, task, context, outcome, class, review_required
- Class 1: dispatch autonomously to the appropriate specialist.
- Class 2: dispatch, then write Farga signal source="component-class2-ready" for Guilhem review.

---

### Escalation threshold

Escalate to Guilhem (source="component-escalation") when:
- Work touches interfaces other components depend on
- Confidence is below 70%
- The issue or Epic implies architectural change beyond this component

For routine, well-scoped work within {component}'s domain: act autonomously.

### Step 3 — Record results

Write a summary signal to Farga:
- source: "component-agent"
- content: "Processed N messages. Epics adopted: <list>. Stories created: <list>. Tasks dispatched: <list>. Skipped: <reasons>. Escalated: <list>."

Your written response IS the summary — recorded to Farga automatically.
"###,
        persona_context = persona_context,
        component = component,
        project = project,
        payload = payload,
    )
}
