/// Guilhem daemon — HTTP listener with six routes:
///
/// `POST /turn` — Amassada orchestrates Guilhem as an agent-as-endpoint participant
/// (Option B-full). Single-shot: Amassada assembles the full context, sends one
/// user message, and the process tears down. No per-room session is created.
///
/// `POST /trigger/chronicle` — accepts chronicle trigger events from Argo
/// Workflows, git webhooks, or cron. One-shot: runs `claude --print "<task>"`
/// as a subprocess, posts the output as a Signal to Farga, exits.
///
/// `POST /trigger/sre-alert` — CronWorkflow-triggered (every 30min). Fetches
/// recent watchdog signals from Farga; if any are present and SRE_MATRIX_ROOM_ID
/// is configured, posts a formatted alert to the Matrix room.
///
/// `POST /trigger/backlog-review` — CronWorkflow-triggered (weekly). Guilhem
/// reads open GitHub issues across miegjorn repos, synthesizes a backlog review,
/// writes it to Farga, and optionally posts a summary to Matrix.
///
/// `POST /matrix/reply` — Charradissa (the Matrix appservice/bridge) forwards
/// every room message here; this listener generates the actual reply.
/// Per-room, NOT one-shot: the first message in a room spawns a persistent
/// `agent-sidecar.js` child process (Claude Agent SDK, `sandbox/agent-sidecar.js`),
/// and later messages in the same room are sent to that same process over
/// stdin/stdout, giving real conversational continuity via the SDK's `resume`
/// session mechanism. A background sweep reaps sessions idle past 30 minutes;
/// a dead/crashed sidecar is detected and respawned automatically on the next
/// message for that room. See `ListenState::room_sessions`.
///
/// `GET /health` — liveness probe; returns `200 ok`.
///
/// Token usage is proportional to actual events for chronicle; Matrix sessions
/// cost tokens for as long as a room stays active (up to the idle timeout).

use axum::{extract::State, http::StatusCode, routing::post, Json, Router};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::collections::HashMap;
use caissa_core::config::load_config;
use caissa_core::agent::{load_fondament_def, tool_to_claude_name};
use super::handoff::{is_handoff_message, parse_handoff_message, HandoffRequest};

#[derive(Clone)]
#[allow(dead_code)]
struct ListenState {
    farga_url: String,
    farga_project: String,
    farga_mcp_url: String,
    chronicle_model: String,
    matrix_model: String,
    amassada_url: String,
    dispatcher_mcp_url: String,
    charradissa_mcp_url: String,
    nervi_mcp_url: String,
    fondament_path: String,
    generation: String,
    /// Matrix room ID for SRE alert posts. Empty string = alerting disabled.
    sre_matrix_room_id: String,
    /// Matrix room ID for backlog review posts. Empty string = posting disabled.
    backlog_matrix_room_id: String,
    dream_model: String,
    /// Matrix room ID for dream report posts. Empty = posting disabled.
    dream_matrix_room_id: String,
    /// One persistent agent-sidecar.js child process per actively-chatting
    /// Matrix room. Reaped by an idle-timeout sweep (see spawn_idle_reaper).
    /// The outer Mutex protects the map (held only for map operations, never
    /// across the Claude API call). Each entry's inner Mutex serialises
    /// concurrent messages for the same room while allowing different rooms
    /// to run in parallel.
    room_sessions: Arc<tokio::sync::Mutex<HashMap<String, RoomSession>>>,
    /// This pod's own Matrix identity. Empty room_id disables the sync loop
    /// entirely (e.g. local dev without Matrix configured).
    matrix_user: String,
    matrix_room_id: String,
    matrix_homeserver: String,
    matrix_password: String,
    /// Updated in place on every successful login/re-login; read by both the
    /// sync loop and the post-reply call.
    matrix_access_token: Arc<tokio::sync::RwLock<String>>,
    /// Kroki server URL for rendering ```mermaid blocks in a reply as PNG
    /// images instead of posting raw diagram source as text.
    kroki_url: String,
}

/// A running agent-sidecar.js child process for one room.
struct SidecarProcess {
    child: tokio::process::Child,
    stdin: tokio::process::ChildStdin,
    stdout: tokio::io::BufReader<tokio::process::ChildStdout>,
}

impl SidecarProcess {
    async fn spawn(init: &SidecarInit) -> anyhow::Result<Self> {
        use tokio::io::AsyncWriteExt;

        let mut child = tokio::process::Command::new("node")
            .arg("/usr/local/bin/agent-sidecar.js")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::inherit())
            .spawn()?;

        let mut stdin = child.stdin.take().expect("stdin was piped");
        let stdout = child.stdout.take().expect("stdout was piped");

        let init_line = serde_json::to_string(init)?;
        stdin.write_all(init_line.as_bytes()).await?;
        stdin.write_all(b"\n").await?;

        Ok(Self {
            child,
            stdin,
            stdout: tokio::io::BufReader::new(stdout),
        })
    }

    async fn send(&mut self, room_id: &str, sender: &str, content: &str) -> anyhow::Result<String> {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt};

        let msg = serde_json::json!({ "room_id": room_id, "sender": sender, "content": content });
        let line = serde_json::to_string(&msg)?;
        self.stdin.write_all(line.as_bytes()).await?;
        self.stdin.write_all(b"\n").await?;

        let mut response_line = String::new();
        self.stdout.read_line(&mut response_line).await?;

        let parsed: serde_json::Value = serde_json::from_str(response_line.trim())?;
        if let Some(err) = parsed.get("error").and_then(|v| v.as_str()) {
            anyhow::bail!("sidecar error: {}", err);
        }
        Ok(parsed.get("reply").and_then(|v| v.as_str()).unwrap_or("").to_string())
    }

    fn kill(&mut self) {
        let _ = self.child.start_kill();
    }

    /// Returns true if the child process is still running. `try_wait()`
    /// returns `Ok(None)` while alive, `Ok(Some(_))` once it has exited, and
    /// `Err` if the OS-level status check itself fails — in that case we
    /// treat the process as dead (safer to respawn than keep using
    /// something we can't verify).
    fn is_alive(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }
}

#[derive(serde::Serialize)]
struct SidecarInit {
    #[serde(rename = "systemPrompt")]
    system_prompt: String,
    model: String,
    #[serde(rename = "allowedTools")]
    allowed_tools: Vec<String>,
    skills: Vec<String>,
    #[serde(rename = "mcpServers")]
    mcp_servers: serde_json::Value,
    /// Extended-thinking token budget for the aporia discipline (Occitan
    /// per-agent-matrix-independence follow-up: aporia is now the default
    /// reasoning mode for these 9 agents, not opt-in). `None` when the
    /// agent's Fondament definition doesn't declare the `aporia` modifier —
    /// omitted entirely from the JSON in that case so agent-sidecar.js's
    /// default (no extended thinking) is unchanged for those.
    #[serde(rename = "maxThinkingTokens", skip_serializing_if = "Option::is_none")]
    max_thinking_tokens: Option<u32>,
}

/// One room's live session: the running sidecar child process, and when it
/// last handled a message.
///
/// `process` is behind its own `Arc<Mutex>` so the outer map lock can be
/// released before the Claude API call. Different rooms run in parallel;
/// two messages for the same room serialise on the per-room Mutex.
/// `last_activity` is updated under the outer map lock so the idle reaper
/// can inspect it without touching the inner Mutex.
struct RoomSession {
    process: std::sync::Arc<tokio::sync::Mutex<SidecarProcess>>,
    last_activity: std::time::Instant,
}

impl RoomSession {
    #[cfg(test)]
    fn for_test(last_activity: std::time::Instant) -> Self {
        // tokio::process::Command::spawn() needs a live Tokio runtime (it
        // registers the child with the reactor for SIGCHLD), but these are
        // plain #[test] functions, not #[tokio::test]. Stand up a throwaway
        // current-thread runtime just for the spawn/take calls below.
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("build runtime for test");
        let (child, stdin, stdout) = rt.block_on(async {
            let mut cmd = tokio::process::Command::new("true");
            cmd.stdin(std::process::Stdio::piped()).stdout(std::process::Stdio::piped());
            let mut child = cmd.spawn().expect("spawn /bin/true for test");
            let stdin = child.stdin.take().expect("stdin was piped");
            let stdout = child.stdout.take().expect("stdout was piped");
            (child, stdin, stdout)
        });
        Self {
            process: std::sync::Arc::new(tokio::sync::Mutex::new(
                SidecarProcess { child, stdin, stdout: tokio::io::BufReader::new(stdout) },
            )),
            last_activity,
        }
    }

    fn is_idle(&self, timeout: std::time::Duration) -> bool {
        self.last_activity.elapsed() >= timeout
    }
}

#[derive(Deserialize)]
pub struct TriggerReq {
    /// Human-readable reason for the chronicle run.
    pub reason: String,
    /// Optional specific prompt override. If absent, uses the default chronicle prompt.
    pub prompt: Option<String>,
}

#[derive(Serialize)]
struct SignalPayload {
    project: String,
    signals: Vec<SignalItem>,
}

#[derive(Serialize)]
struct SignalItem {
    // Farga's Signal requires project on each item (not just the envelope).
    project: String,
    content: String,
    source: String,
}

pub async fn run(port: u16) -> anyhow::Result<()> {
    let config = load_config()?;

    let state = Arc::new(ListenState {
        farga_url: config.farga_url,
        farga_project: config.project,
        farga_mcp_url: config.farga_mcp_url,
        chronicle_model: config.chronicle_model,
        matrix_model: config.matrix_model,
        amassada_url: config.amassada_url,
        dispatcher_mcp_url: config.dispatcher_mcp_url,
        charradissa_mcp_url: config.charradissa_mcp_url,
        nervi_mcp_url: config.nervi_mcp_url,
        fondament_path: config.fondament_path,
        generation: config.generation,
        sre_matrix_room_id: std::env::var("SRE_MATRIX_ROOM_ID").unwrap_or_default(),
        backlog_matrix_room_id: std::env::var("BACKLOG_MATRIX_ROOM_ID").unwrap_or_default(),
        dream_model: config.dream_model,
        dream_matrix_room_id: std::env::var("DREAM_MATRIX_ROOM_ID").unwrap_or_default(),
        room_sessions: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
        matrix_user: std::env::var("MATRIX_USER").unwrap_or_default(),
        matrix_room_id: std::env::var("MATRIX_ROOM_ID").unwrap_or_default(),
        matrix_homeserver: std::env::var("MATRIX_HOMESERVER")
            .unwrap_or_else(|_| "http://synapse.occitan-system.svc.cluster.local:8008".into()),
        matrix_password: read_matrix_password(),
        matrix_access_token: Arc::new(tokio::sync::RwLock::new(String::new())),
        kroki_url: std::env::var("KROKI_URL")
            .unwrap_or_else(|_| "http://kroki.occitan-system.svc.cluster.local:8000".into()),
    });

    tokio::spawn(spawn_idle_reaper(Arc::clone(&state.room_sessions)));
    tokio::spawn(run_nervi_loop_if_component(Arc::clone(&state)));
    tokio::spawn(run_matrix_client_loop(Arc::clone(&state)));

    let app = Router::new()
        .route("/trigger/chronicle", post(handle_chronicle))
        .route("/trigger/sre-alert", post(handle_sre_alert))
        .route("/trigger/backlog-review", post(handle_backlog_review))
        .route("/trigger/dream", post(handle_dream))
        .route("/trigger/dispatch", post(handle_dispatch))
        .route("/trigger/mission-pulse", post(handle_mission_pulse))
        .route("/trigger/intake", post(handle_intake))
        .route("/trigger/scan", post(handle_scan))
        .route("/matrix/reply", post(handle_matrix_reply))
        .route("/turn", post(handle_turn))
        .route("/health", axum::routing::get(|| async { "ok" }))
        .with_state(state);

    let addr = format!("0.0.0.0:{}", port);
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    eprintln!("[caissa] listening on {}", addr);

    axum::serve(listener, app).await?;
    Ok(())
}

async fn handle_chronicle(
    State(state): State<Arc<ListenState>>,
    Json(req): Json<TriggerReq>,
) -> StatusCode {
    tracing::info!("chronicle trigger received: {}", req.reason);

    let prompt = req
        .prompt
        .unwrap_or_else(|| build_chronicle_prompt(&state.fondament_path, &req.reason, &state.farga_project));

    tokio::spawn(async move {
        match run_chronicle(&state, &prompt).await {
            Ok(_) => tracing::info!("chronicle run complete"),
            Err(e) => tracing::error!("chronicle run failed: {}", e),
        }
    });

    StatusCode::ACCEPTED
}

/// POST /trigger/sre-alert — CronWorkflow-triggered (every 30min).
///
/// Fetches recent bug-signals written by the sre-watchdog from Farga.
/// If any are found and SRE_MATRIX_ROOM_ID is configured, posts a
/// formatted alert directly to the Matrix room using the SYNAPSE_ADMIN_TOKEN
/// and SYNAPSE_URL that the initContainer injects at pod startup.
/// Silent (202, no Matrix post) when all-clear or alerting is not configured.
async fn handle_sre_alert(
    State(state): State<Arc<ListenState>>,
    Json(req): Json<TriggerReq>,
) -> StatusCode {
    tracing::info!("sre-alert trigger received: {}", req.reason);

    let state_clone = Arc::clone(&state);
    tokio::spawn(async move {
        if let Err(e) = run_sre_alert(&state_clone).await {
            tracing::error!("sre-alert run failed: {}", e);
        }
    });

    StatusCode::ACCEPTED
}

/// Reads GH_TOKEN/GITHUB_TOKEN fresh from /creds/tokens.env at call time, so each
/// spawned `claude` subprocess picks up whatever the container's background refresh
/// loop most recently minted. This process's own inherited environment is fixed at
/// its own startup and never reflects later rewrites of that file, so every call
/// site that spawns `claude` must re-read here rather than relying on inherited env.
fn github_token_envs() -> Vec<(String, String)> {
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

/// Reads the agent's own Matrix password from `/creds/tokens.env`, written by
/// the fetch-tokens init container. Returns empty string if absent (local dev,
/// or a pod that hasn't been given Matrix credentials yet) — matrix_client_loop
/// treats an empty matrix_room_id/matrix_password as "feature disabled here".
fn read_matrix_password() -> String {
    let content = match std::fs::read_to_string("/creds/tokens.env") {
        Ok(c) => c,
        Err(_) => return String::new(),
    };
    content.lines()
        .find_map(|line| {
            let rest = line.strip_prefix("export MATRIX_PASSWORD=")?;
            Some(rest.trim_matches('\'').to_string())
        })
        .unwrap_or_default()
}

async fn matrix_login(homeserver: &str, user: &str, password: &str) -> anyhow::Result<String> {
    let client = reqwest::Client::new();
    let resp: serde_json::Value = client
        .post(format!("{}/_matrix/client/v3/login", homeserver))
        .json(&serde_json::json!({
            "type": "m.login.password",
            "identifier": { "type": "m.id.user", "user": user },
            "password": password,
        }))
        .send().await?
        .json().await?;
    resp["access_token"].as_str()
        .map(|s| s.to_string())
        .ok_or_else(|| anyhow::anyhow!("matrix_login: no access_token in response: {:?}", resp))
}

fn matrix_pct(s: &str) -> String {
    s.chars().map(|c| match c {
        '!' | '#' | '@' | ':' | '/' | '?' | '&' | '=' | '+' | ' ' => format!("%{:02X}", c as u32),
        _ => c.to_string(),
    }).collect()
}

/// Long-poll `/sync` once. Returns the new `since` token and any
/// `m.room.message` timeline events for `room_id`, `(sender, content)` pairs.
/// A 401 with `M_UNKNOWN_TOKEN` is surfaced as `Err` so the caller can re-login.
async fn matrix_sync(
    homeserver: &str,
    token: &str,
    since: Option<&str>,
    room_id: &str,
) -> anyhow::Result<(String, Vec<(String, String)>)> {
    let mut url = format!("{}/_matrix/client/v3/sync?timeout=30000", homeserver);
    if let Some(s) = since {
        url.push_str(&format!("&since={}", matrix_pct(s)));
    }
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(40))
        .build()?;
    let resp = client
        .get(&url)
        .header("Authorization", format!("Bearer {}", token))
        .send().await?;
    if resp.status().as_u16() == 401 {
        let body: serde_json::Value = resp.json().await.unwrap_or_default();
        if body["errcode"].as_str() == Some("M_UNKNOWN_TOKEN") {
            anyhow::bail!("M_UNKNOWN_TOKEN");
        }
        anyhow::bail!("matrix_sync: 401: {:?}", body);
    }
    if !resp.status().is_success() {
        anyhow::bail!("matrix_sync failed: {}", resp.status());
    }
    let body: serde_json::Value = resp.json().await?;
    let next_batch = body["next_batch"].as_str()
        .ok_or_else(|| anyhow::anyhow!("matrix_sync: no next_batch in response"))?
        .to_string();

    let mut events = Vec::new();
    if let Some(timeline) = body["rooms"]["join"][room_id]["timeline"]["events"].as_array() {
        for ev in timeline {
            if ev["type"].as_str() == Some("m.room.message") {
                let sender = ev["sender"].as_str().unwrap_or_default().to_string();
                let content = ev["content"]["body"].as_str().unwrap_or_default().to_string();
                events.push((sender, content));
            }
        }
    }
    Ok((next_batch, events))
}

async fn matrix_post_body(homeserver: &str, token: &str, room_id: &str, body: &serde_json::Value) -> anyhow::Result<()> {
    let txn = uuid::Uuid::new_v4();
    let url = format!(
        "{}/_matrix/client/v3/rooms/{}/send/m.room.message/{}",
        homeserver, matrix_pct(room_id), txn
    );
    let resp = reqwest::Client::new()
        .put(&url)
        .header("Authorization", format!("Bearer {}", token))
        .json(body)
        .send().await?;
    if !resp.status().is_success() {
        anyhow::bail!("matrix_post failed: {}", resp.status());
    }
    Ok(())
}

/// Render markdown to HTML for the `formatted_body` field, matching Matrix's
/// dual plain/HTML message convention (`Charradissa/charradissa-matrix/src/client.rs`'s
/// `markdown_body`, ported here since these 9 agents post directly rather
/// than through Charradissa's relay). Only includes `formatted_body` when
/// the rendering actually adds markup beyond a plain paragraph wrap — avoids
/// cluttering plain-prose messages with an identical HTML copy.
fn markdown_body(content: &str) -> serde_json::Value {
    let html = render_markdown(content);
    if html_differs_from_plain(content, &html) {
        serde_json::json!({
            "msgtype": "m.text",
            "body": content,
            "format": "org.matrix.custom.html",
            "formatted_body": html,
        })
    } else {
        serde_json::json!({ "msgtype": "m.text", "body": content })
    }
}

fn render_markdown(content: &str) -> String {
    use pulldown_cmark::{html, Options, Parser};
    let opts = Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TASKLISTS;
    let parser = Parser::new_ext(content, opts);
    let mut html_out = String::new();
    html::push_html(&mut html_out, parser);
    html_out
}

fn html_differs_from_plain(plain: &str, html: &str) -> bool {
    let trimmed = html.trim();
    let unwrapped = trimmed
        .strip_prefix("<p>")
        .and_then(|s| s.strip_suffix("</p>"))
        .unwrap_or(trimmed);
    unwrapped != plain.trim()
}

/// POST the diagram source to Kroki and return the rendered PNG bytes.
/// Ported from `Charradissa/charradissa-core/src/mermaid.rs`'s `render_svg`,
/// requesting `png` instead of `svg` — Element renders inline images more
/// consistently as PNG than as SVG in practice.
async fn render_diagram_png(kroki_url: &str, diagram: &str) -> anyhow::Result<Vec<u8>> {
    let url = format!("{}/mermaid/png", kroki_url);
    let resp = reqwest::Client::new()
        .post(&url)
        .header("Content-Type", "text/plain")
        .body(diagram.to_string())
        .timeout(std::time::Duration::from_secs(15))
        .send()
        .await
        .map_err(|e| anyhow::anyhow!("Kroki request failed: {}", e))?;
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        anyhow::bail!("Kroki returned {}: {}", status, body);
    }
    Ok(resp.bytes().await.map(|b| b.to_vec())?)
}

async fn matrix_upload_media(homeserver: &str, token: &str, content_type: &str, data: Vec<u8>) -> anyhow::Result<String> {
    let url = format!("{}/_matrix/media/v3/upload", homeserver);
    let resp = reqwest::Client::new()
        .post(&url)
        .header("Authorization", format!("Bearer {}", token))
        .header("Content-Type", content_type)
        .body(data)
        .send().await?;
    let json: serde_json::Value = resp.json().await?;
    json["content_uri"].as_str()
        .map(|s| s.to_string())
        .ok_or_else(|| anyhow::anyhow!("upload_media: no content_uri in response: {:?}", json))
}

async fn render_and_upload_diagram(homeserver: &str, token: &str, kroki_url: &str, diagram: &str) -> anyhow::Result<String> {
    let png = render_diagram_png(kroki_url, diagram).await?;
    matrix_upload_media(homeserver, token, "image/png", png).await
}

/// Walk `content`, replacing each ` ```mermaid ... ``` ` block *in place*
/// with a markdown image reference (`![diagram N](mxc://...)`) pointing at
/// a real Matrix-uploaded PNG — so once the result is markdown-rendered, the
/// image lands inline in the HTML exactly where the diagram was written,
/// not as a separate trailing message. Per-diagram failure is non-fatal:
/// that specific block is left as its raw mermaid source (still readable as
/// a fenced code block) rather than losing the rest of the reply.
async fn substitute_mermaid_with_images(homeserver: &str, token: &str, kroki_url: &str, content: &str) -> String {
    let open = "```mermaid";
    let close = "```";
    let mut out = String::new();
    let mut rest = content;
    let mut diagram_num = 0usize;
    while let Some(start) = rest.find(open) {
        out.push_str(&rest[..start]);
        let after_open = &rest[start + open.len()..];
        let body = after_open.trim_start_matches('\n').trim_start_matches('\r');
        if let Some(end) = body.find(close) {
            let diagram = body[..end].trim();
            if !diagram.is_empty() {
                diagram_num += 1;
                match render_and_upload_diagram(homeserver, token, kroki_url, diagram).await {
                    Ok(mxc) => out.push_str(&format!("![diagram {}]({})", diagram_num, mxc)),
                    Err(e) => {
                        tracing::warn!("post_reply: kroki render/upload failed for diagram {} (leaving raw source in place): {}", diagram_num, e);
                        out.push_str(&format!("```mermaid\n{}\n```", diagram));
                    }
                }
            }
            rest = &body[end + close.len()..];
        } else {
            // Unterminated block — leave the rest of the content as-is rather
            // than silently dropping it.
            out.push_str(&rest[start..]);
            rest = "";
            break;
        }
    }
    out.push_str(rest);
    out
}

/// Post an agent's reply with markdown rendering and inline mermaid-diagram
/// interception: any ` ```mermaid ` blocks are rendered via Kroki, uploaded
/// as real Matrix media, and substituted in place with a markdown image
/// reference — so the rendered HTML shows the diagram inline exactly where
/// the agent wrote it, in the same single message as the surrounding text.
async fn post_reply(homeserver: &str, token: &str, room_id: &str, kroki_url: &str, reply: &str) -> anyhow::Result<()> {
    let substituted = substitute_mermaid_with_images(homeserver, token, kroki_url, reply).await;
    if !substituted.trim().is_empty() {
        matrix_post_body(homeserver, token, room_id, &markdown_body(&substituted)).await?;
    }
    Ok(())
}

#[cfg(test)]
mod matrix_rendering_tests {
    use super::*;

    #[test]
    fn plain_prose_has_no_formatted_body() {
        let body = markdown_body("hello world");
        assert!(body.get("formatted_body").is_none());
        assert!(body.get("format").is_none());
        assert_eq!(body["body"], "hello world");
    }

    #[test]
    fn markdown_prose_gets_formatted_body() {
        let body = markdown_body("**bold** text");
        assert_eq!(body["format"], "org.matrix.custom.html");
        assert!(body["formatted_body"].as_str().unwrap().contains("<strong>bold</strong>"));
        assert_eq!(body["body"], "**bold** text");
    }

    #[tokio::test]
    async fn substitute_replaces_block_in_place_with_inline_image_ref() {
        use wiremock::{Mock, MockServer, ResponseTemplate};
        use wiremock::matchers::{method, path};

        let mock_server = MockServer::start().await;
        Mock::given(method("POST")).and(path("/mermaid/png"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(vec![0x89, b'P', b'N', b'G']))
            .mount(&mock_server).await;
        Mock::given(method("POST")).and(path("/_matrix/media/v3/upload"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "content_uri": "mxc://occitane.guilhem/abc123"
            })))
            .mount(&mock_server).await;

        let msg = "look at this\n```mermaid\ngraph TD\n  A-->B\n```\ncool right?";
        let result = substitute_mermaid_with_images(&mock_server.uri(), "test-token", &mock_server.uri(), msg).await;

        // Image reference lands exactly where the code block was, not
        // appended/prepended — the surrounding text stays in place.
        assert_eq!(result, "look at this\n![diagram 1](mxc://occitane.guilhem/abc123)\ncool right?");
    }

    #[tokio::test]
    async fn substitute_numbers_multiple_diagrams_in_order() {
        use wiremock::{Mock, MockServer, ResponseTemplate};
        use wiremock::matchers::{method, path};

        let mock_server = MockServer::start().await;
        Mock::given(method("POST")).and(path("/mermaid/png"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(vec![1, 2, 3]))
            .mount(&mock_server).await;
        Mock::given(method("POST")).and(path("/_matrix/media/v3/upload"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "content_uri": "mxc://occitane.guilhem/xyz"
            })))
            .mount(&mock_server).await;

        let msg = "```mermaid\nflowchart LR\n  A-->B\n```\nand\n```mermaid\nsequenceDiagram\n  A->>B: hi\n```";
        let result = substitute_mermaid_with_images(&mock_server.uri(), "test-token", &mock_server.uri(), msg).await;
        assert!(result.contains("![diagram 1](mxc://occitane.guilhem/xyz)"));
        assert!(result.contains("![diagram 2](mxc://occitane.guilhem/xyz)"));
        assert!(result.contains("and"));
    }

    #[tokio::test]
    async fn substitute_falls_back_to_raw_source_on_kroki_failure() {
        use wiremock::{Mock, MockServer, ResponseTemplate};
        use wiremock::matchers::{method, path};

        let mock_server = MockServer::start().await;
        Mock::given(method("POST")).and(path("/mermaid/png"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&mock_server).await;

        let msg = "before\n```mermaid\ngraph TD\n  A-->B\n```\nafter";
        let result = substitute_mermaid_with_images(&mock_server.uri(), "test-token", &mock_server.uri(), msg).await;

        // Kroki outage must not swallow the reply — raw source stays in place.
        assert!(result.contains("before"));
        assert!(result.contains("```mermaid\ngraph TD\n  A-->B\n```"));
        assert!(result.contains("after"));
    }

    #[tokio::test]
    async fn substitute_ignores_non_mermaid_code_blocks() {
        let msg = "```rust\nfn main() {}\n```";
        // No mock server needed — a non-mermaid block never triggers a network call.
        let result = substitute_mermaid_with_images("http://unused", "test-token", "http://unused", msg).await;
        assert_eq!(result, msg);
    }

    #[tokio::test]
    async fn substitute_passes_through_plain_text_unchanged() {
        let result = substitute_mermaid_with_images("http://unused", "test-token", "http://unused", "hello world").await;
        assert_eq!(result, "hello world");
    }
}

/// Background task: logs in as this pod's own Matrix user, long-polls
/// `/sync` for its one room, and calls `run_matrix_reply` in-process for
/// every message not sent by itself. Re-logs-in automatically on
/// `M_UNKNOWN_TOKEN`. Non-fatal at every layer — a Matrix outage degrades
/// this to backoff-retry, never crashes the pod (the pod's cron/HTTP
/// responsibilities are unaffected).
async fn run_matrix_client_loop(state: Arc<ListenState>) {
    if state.matrix_room_id.is_empty() || state.matrix_user.is_empty() {
        tracing::info!("matrix_client: MATRIX_ROOM_ID/MATRIX_USER not set, sync loop disabled");
        return;
    }
    let own_user_id = format!("@{}:occitane.guilhem", state.matrix_user);

    loop {
        let token = match matrix_login(&state.matrix_homeserver, &state.matrix_user, &state.matrix_password).await {
            Ok(t) => t,
            Err(e) => {
                tracing::error!("matrix_client: login failed, retrying in 30s: {}", e);
                tokio::time::sleep(std::time::Duration::from_secs(30)).await;
                continue;
            }
        };
        *state.matrix_access_token.write().await = token.clone();
        tracing::info!("matrix_client: logged in as {}", own_user_id);

        // Initial sync: capture a since token without processing backlog.
        let mut since = match matrix_sync(&state.matrix_homeserver, &token, None, &state.matrix_room_id).await {
            Ok((s, _)) => s,
            Err(e) => {
                tracing::error!("matrix_client: initial sync failed, retrying in 30s: {}", e);
                tokio::time::sleep(std::time::Duration::from_secs(30)).await;
                continue;
            }
        };

        'sync_loop: loop {
            let current_token = state.matrix_access_token.read().await.clone();
            match matrix_sync(&state.matrix_homeserver, &current_token, Some(&since), &state.matrix_room_id).await {
                Ok((next_since, events)) => {
                    since = next_since;
                    for (sender, content) in events {
                        if sender == own_user_id {
                            continue; // echo guard
                        }
                        let req = MatrixReplyReq {
                            room_id: state.matrix_room_id.clone(),
                            sender: sender.clone(),
                            content,
                            history: vec![],
                            event_id: None,
                        };
                        match run_matrix_reply(&state, &req).await {
                            Ok(reply) => {
                                let post_token = state.matrix_access_token.read().await.clone();
                                if let Err(e) = post_reply(&state.matrix_homeserver, &post_token, &state.matrix_room_id, &state.kroki_url, &reply).await {
                                    tracing::error!("matrix_client: post failed: {}", e);
                                }
                            }
                            Err(e) => tracing::error!("matrix_client: run_matrix_reply failed: {}", e),
                        }
                    }
                }
                Err(e) if e.to_string().contains("M_UNKNOWN_TOKEN") => {
                    tracing::warn!("matrix_client: access token invalid, re-logging in");
                    break 'sync_loop; // fall through to outer loop's fresh login
                }
                Err(e) => {
                    tracing::warn!("matrix_client: sync error (retrying in 5s): {}", e);
                    tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                }
            }
        }
    }
}

async fn run_sre_alert(state: &ListenState) -> anyhow::Result<()> {
    let mcp_config = serde_json::to_string(&serde_json::json!({
        "mcpServers": guilhem_mcp_servers(state)
    }))?;
    let mcp_path = std::env::temp_dir().join("guilhem-sre-alert-mcp.json");
    std::fs::write(&mcp_path, &mcp_config)?;

    let prompt = build_sre_alert_prompt(&state.fondament_path);

    let output = tokio::process::Command::new("claude")
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

fn build_sre_alert_prompt(fondament_path: &str) -> String {
    format!(r###"You are Guilhem de Tudela, org agent. The SRE watchdog has detected health anomalies.

Your job: read the alerts, identify which component owns each failure, dispatch a targeted
repair task to that component's Nervi dispatch subject.

{constraint}

---

## STEP 1 — Read alerts

Call nervi_subscribe with subject="occitan.sre.alerts" to pull recent alerts from the
NATS subject. Use max_messages=20 and a short timeout.

Also call mcp__farga__search_signals to find signals with source="sre-watchdog" from the
last hour (as a fallback if NATS has no messages yet).

## STEP 2 — Evaluate

If no alerts are found in either source:
- Stop. Write a brief Farga signal: source="guilhem-sre-dispatch", content="SRE alert scan: all clear — no anomalies found."
- Do not dispatch.

If alerts ARE found, for each anomaly identify the responsible component:
- "gardian" → dispatch subject: occitan.dispatch.gardian
- "farga" → dispatch subject: occitan.dispatch.farga
- "amassada" → dispatch subject: occitan.dispatch.amassada
- "charradissa" → dispatch subject: occitan.dispatch.charradissa
- "dispatcher" → dispatch subject: occitan.dispatch.caissa
- "nervi" → dispatch subject: occitan.dispatch.nervi
- "guilhem" → Escalate via Farga (cannot dispatch to yourself; write to Farga source="guilhem-sre-escalate")

## STEP 3 — Dispatch

For each affected component, publish a repair task to its Nervi dispatch subject:
nervi_publish(subject="occitan.dispatch.<component>", payload=JSON.stringify({{
  "type": "sre-repair",
  "anomaly": "<specific error description>",
  "task": "Investigate: check /health endpoint, review recent pod logs for errors, identify root cause. If a code fix is needed, open a PR following the standard issue→implement→PR→approval flow.",
  "class": 1,
  "dispatched_by": "guilhem-sre",
  "review_required": false
}}))

## STEP 4 — Record

Write a summary signal to Farga:
- source: "guilhem-sre-dispatch"
- content: "Dispatched SRE alerts to: <list>. Anomalies: <brief summary>. Timestamp: <now>"
"###,
        constraint = guilhem_dispatch_constraint(fondament_path),
    )
}

/// Constraint block injected at the top of every Guilhem prompt.
/// Loaded from Fondament skill YAML when available; hardcoded fallback for local dev.
fn guilhem_dispatch_constraint(fondament_path: &str) -> String {
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

fn build_chronicle_prompt(fondament_path: &str, reason: &str, project: &str) -> String {
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

async fn run_chronicle(state: &ListenState, prompt: &str) -> anyhow::Result<()> {
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
async fn handle_backlog_review(
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

async fn run_backlog_review(state: &ListenState) -> anyhow::Result<()> {
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

fn build_backlog_review_prompt(fondament_path: &str, project: &str) -> String {
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

/// POST /trigger/dream — CronJob-triggered daily (03:00 UTC).
///
/// Three-phase session:
/// 1. GATHER — read Farga signals (past 24h) + GitHub state across all repos
/// 2. SYNTHESIZE — identify drift, improvement opportunities, patterns
/// 3. ACT — create GitHub issues for actionable gaps; write dream report to Farga
async fn handle_dream(
    State(state): State<Arc<ListenState>>,
    Json(req): Json<TriggerReq>,
) -> StatusCode {
    tracing::info!("dream trigger received: {}", req.reason);

    tokio::spawn(async move {
        match run_dream(&state).await {
            Ok(_) => tracing::info!("dream complete"),
            Err(e) => tracing::error!("dream failed: {}", e),
        }
    });

    StatusCode::ACCEPTED
}

async fn run_dream(state: &ListenState) -> anyhow::Result<()> {
    let mcp_config = format!(
        r#"{{"mcpServers":{{"farga":{{"type":"http","url":"{}"}}}}}}"#,
        state.farga_mcp_url
    );
    let mcp_path = std::env::temp_dir().join("guilhem-dream-mcp.json");
    std::fs::write(&mcp_path, &mcp_config)?;

    let prompt = build_dream_prompt(&state.fondament_path, &state.farga_project);

    let output = tokio::process::Command::new("claude")
        .args([
            "--print",
            &prompt,
            "--model",
            &state.dream_model,
            "--mcp-config",
            mcp_path.to_str().unwrap(),
            "--allowed-tools",
            "Bash,WebSearch,WebFetch,mcp__farga__search_signals,mcp__farga__read_context,mcp__farga__write_signal,mcp__farga__update_component_todo",
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

fn build_dream_prompt(fondament_path: &str, project: &str) -> String {
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
   `cat /fondament/definitions/fondament/system-defence.md`

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

// ── Code scan + doc reconciliation ───────────────────────────────────────────

/// POST /trigger/scan — weekly CronJob per component agent.
///
/// Clones the component's GitHub repo, inspects code for TODOs / unimplemented
/// stubs / doc drift, deduplicates against open GitHub issues, creates new
/// issues for gaps found, optionally opens a README PR so Cartulari picks it
/// up, and writes a Farga signal summarising the run.
async fn handle_scan(
    State(state): State<Arc<ListenState>>,
    Json(req): Json<TriggerReq>,
) -> StatusCode {
    tracing::info!("scan trigger received: {}", req.reason);

    tokio::spawn(async move {
        match run_scan(&state).await {
            Ok(_) => tracing::info!("scan complete"),
            Err(e) => tracing::error!("scan failed: {}", e),
        }
    });

    StatusCode::ACCEPTED
}

async fn run_scan(state: &ListenState) -> anyhow::Result<()> {
    let mcp_config = format!(
        r#"{{"mcpServers":{{"farga":{{"type":"http","url":"{}"}}}}}}"#,
        state.farga_mcp_url
    );
    let mcp_path = std::env::temp_dir().join("caissa-scan-mcp.json");
    std::fs::write(&mcp_path, &mcp_config)?;

    let prompt = build_scan_prompt(&state.farga_project);

    let output = tokio::process::Command::new("claude")
        .args([
            "--print",
            &prompt,
            "--model",
            &state.chronicle_model,
            "--mcp-config",
            mcp_path.to_str().unwrap(),
            "--allowed-tools",
            "Bash,mcp__farga__write_signal,mcp__farga__read_context,mcp__farga__search_signals",
        ])
        .env("FARGA_URL", &state.farga_url)
        .env("FARGA_PROJECT", &state.farga_project)
        .envs(github_token_envs())
        .output()
        .await?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("scan claude exited with error: {}", stderr);
    }

    let report = String::from_utf8_lossy(&output.stdout).to_string();
    if !report.trim().is_empty() {
        post_signal(state, &report).await?;
    }

    Ok(())
}

fn build_scan_prompt(component: &str) -> String {
    // GitHub repo name: capitalize first letter of component slug.
    let repo = {
        let mut c = component.chars();
        match c.next() {
            None => String::new(),
            Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
        }
    };

    format!(
        r###"You are the {component} component agent running a weekly code scan and
documentation reconciliation for the `miegjorn/{repo}` repository.

Your job has four phases. Complete them in order.

---

## PHASE 1: READ FARGA CONTEXT

Read the current project context so you understand what is intentionally deferred
vs genuinely missing:

- `mcp__farga__read_context` (project: "{component}")
- `mcp__farga__search_signals` (project: "{component}") — look for recent scan signals
  to avoid re-filing issues already created in the last 7 days

---

## PHASE 2: CLONE AND INSPECT THE CODEBASE

```bash
cd /tmp && rm -rf scan-{component} && gh repo clone miegjorn/{repo} scan-{component} -- --depth=1 2>&1
cd /tmp/scan-{component}
```

Run these inspections and collect the findings:

**2a. Code stubs and deferred work:**
```bash
grep -rn \
  --include="*.rs" --include="*.ts" --include="*.js" --include="*.py" --include="*.go" \
  -E "TODO|FIXME|HACK|unimplemented!\(\)|todo!\(\)|panic!\(\"not implemented\"\)|raise NotImplementedError" \
  . | grep -v "\.git/" | grep -v "/target/" | grep -v "node_modules/"
```

**2b. Spec/doc references in README that may not exist in code:**
```bash
# Extract endpoint paths, function names, config keys from README
grep -E "^#+|`[A-Z_]{{3,}}`|POST |GET |PUT |DELETE |\bfn [a-z]|\[.*\]\(#" README.md 2>/dev/null || true
```

**2c. Public API surface in code not mentioned in README:**
```bash
# Rust: public functions and structs
grep -rn --include="*.rs" -E "^pub (async )?fn |^pub struct " src/ 2>/dev/null | head -40 || true
# TypeScript: exported functions
grep -rn --include="*.ts" -E "^export (async )?function |^export class " src/ 2>/dev/null | head -40 || true
```

**2d. Config keys referenced in code vs documented:**
```bash
grep -rn --include="*.rs" --include="*.ts" -E \
  "env::var\(|process\.env\.|std::env::var" . | grep -v "\.git/" | grep -v target/ | head -30 || true
```

**2e. Read the full README:**
```bash
cat README.md 2>/dev/null || echo "No README.md found"
```

---

## PHASE 3: CLOSE RESOLVED ISSUES

Before creating new issues, check whether previously-filed scan issues are now resolved.
This prevents the queue from accumulating stale work.

```bash
# List open issues previously created by the scan (last 90 days)
gh issue list --repo miegjorn/{repo} --state open --label "technical-debt" \
  --search "weekly code scan" --json number,title,body --limit 30
```

For each open scan issue:
1. Extract the specific problem described (file path, symbol name, pattern).
2. Check whether it still exists in the cloned repo:
   ```bash
   grep -rn "<pattern from issue>" /tmp/scan-{component}/src/ 2>/dev/null | head -5
   ```
3. If the problem is **gone**: close the issue with evidence:
   ```bash
   gh issue close <number> --repo miegjorn/{repo} \
     --comment "Resolved: pattern no longer present in codebase as of $(date +%Y-%m-%d). Closing."
   ```
4. If it still exists: leave it open (do not re-comment).

---

## PHASE 4: RECONCILE AND CREATE ISSUES

For each gap you identify, before creating an issue:

1. Check for an existing open issue:
   ```bash
   gh issue list --repo miegjorn/{repo} --state open \
     --search "<key term from the gap>" --json number,title | head -5
   ```
2. If no duplicate exists, create an issue:
   ```bash
   gh issue create \
     --repo miegjorn/{repo} \
     --title "<concise, specific title>" \
     --body "## Context\n<what was found and why it matters>\n\n## Proposed fix\n<what the resolution would look like>\n\n## Source\nIdentified by weekly code scan ({component} agent, $(date +%Y-%m-%d))." \
     --label "technical-debt"
   ```

**Issue triage rules:**
- `unimplemented!()` / `todo!()` with no open issue → create `technical-debt` issue
- README documents an endpoint that doesn't exist in code → `documentation` + `bug`
- Public function exists but is undocumented in README → `documentation`
- Config key referenced in code but absent from README/docs → `documentation`
- FIXME/HACK comment that's been there for > 30 days → `technical-debt`

Aim for quality over quantity — file 3–8 issues maximum. If the codebase is clean,
say so and file nothing.

---

## PHASE 5: WRITE FARGA SIGNAL

Write a scan signal using `mcp__farga__write_signal`:
- project: "{component}"
- source: "scan"
- content: structured summary:
  - Date of scan
  - Stubs/TODOs found (count + worst offenders)
  - Doc drift identified (count)
  - Issues created (with URLs)
  - Issues skipped (already existed)
  - Overall health assessment: clean / minor gaps / significant gaps

Your written response IS the scan report. Be precise and brief.
"###,
        component = component,
        repo = repo,
    )
}

// ── Matrix reply ──────────────────────────────────────────────────────────────

#[derive(Deserialize)]
#[allow(dead_code)]
pub struct MatrixReplyReq {
    pub room_id: String,
    pub sender: String,
    pub content: String,
    #[serde(default)]
    pub history: Vec<MatrixHistoryEntry>,
    /// Originating Matrix event id, when Charradissa forwards it. Used as the
    /// `triggered_by` handle in the O-4 handoff traceability signal; falls back
    /// to `sender` when absent.
    #[serde(default)]
    pub event_id: Option<String>,
}

#[derive(Deserialize)]
#[allow(dead_code)]
pub struct MatrixHistoryEntry {
    pub sender: String,
    pub content: String,
}

// ── Active dispatch — post-dream routing ─────────────────────────────────────
//
// POST /trigger/dispatch — CronJob fires 1h after the dream (04:00 UTC daily).
//
// Guilhem reads the dream report and challenge signals from Farga, classifies
// each item using the system-defence risk table, and dispatches actionable
// Class 1/2 items to the responsible component agents via Matrix. Class 3 items
// are surfaced to Pierre-Luc. Class 4 items are rejected with a Farga signal.

async fn handle_dispatch(
    State(state): State<Arc<ListenState>>,
    Json(req): Json<TriggerReq>,
) -> StatusCode {
    tracing::info!("dispatch trigger received: {}", req.reason);

    tokio::spawn(async move {
        match run_dispatch(&state).await {
            Ok(_) => tracing::info!("dispatch complete"),
            Err(e) => tracing::error!("dispatch failed: {}", e),
        }
    });

    StatusCode::ACCEPTED
}

async fn run_dispatch(state: &ListenState) -> anyhow::Result<()> {
    let mcp_config = serde_json::to_string(&serde_json::json!({
        "mcpServers": guilhem_mcp_servers(state)
    }))?;
    let mcp_path = std::env::temp_dir().join("guilhem-dispatch-mcp.json");
    std::fs::write(&mcp_path, &mcp_config)?;

    let prompt = build_dispatch_prompt(&state.fondament_path);

    let output = tokio::process::Command::new("claude")
        .args([
            "--print",
            &prompt,
            "--model",
            &state.dream_model,
            "--mcp-config",
            mcp_path.to_str().unwrap(),
            "--allowed-tools",
            "Bash,WebSearch,mcp__farga__search_signals,mcp__farga__write_signal,mcp__charradissa__matrix_send,mcp__charradissa__matrix_request_approval,mcp__nervi__nervi_publish",
        ])
        .env("FARGA_URL", &state.farga_url)
        .env("FARGA_PROJECT", &state.farga_project)
        .envs(github_token_envs())
        .output()
        .await?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("dispatch claude exited with error: {}", stderr);
    }

    tracing::info!("dispatch run complete");
    Ok(())
}

fn build_dispatch_prompt(fondament_path: &str) -> String {
    format!(
        r###"You are Guilhem de Tudela, org agent and active dispatcher for the Occitan stack.

The nightly dream has completed. Your job now is to translate the dream's synthesis and
adversarial challenge proposals into specific, actionable work — and route it to the right
component agents. This is where synthesis becomes motion.

{constraint}

---

## STEP 1 — Load system-defence axioms and context graph

`cat /fondament/definitions/fondament/system-defence.md`

Read the axioms and risk classification table before evaluating anything.

Then call mcp__farga__read_context_node (path: "[occitan][system-rationale]", role: "org")
to load the stack-level design constraints. These govern what you can dispatch autonomously
vs. what requires Pierre-Luc's sign-off.

## STEP 2 — Read the dream output

Use mcp__farga__search_signals to fetch:
1. The most recent signal with source="dream" (the dream report from the last 24h)
2. All signals with source="dream-adversarial" from the last 24h (challenge proposals)
3. Any signals with source="guilhem-sre-escalate" from the last 24h (SRE escalations needing Pierre-Luc)

If no dream signal is found from the last 24h, stop and write a Farga signal explaining:
source="guilhem-dispatch", content="Dispatch skipped — no recent dream signal found."

## STEP 3 — Classify and route each item

For each actionable item in the dream report and each challenge proposal:

**Class 1 — Dispatch autonomously:**
- Scoped to one component, reversible, no interface change
- Publish to the component's Nervi dispatch subject:
  nervi_publish(subject="occitan.dispatch.<component>", payload=JSON.stringify({{"type":"dispatch","task":"<specific, scoped task description>","context":"<why this matters, from the dream>","outcome":"<a PR with what specific change>","class":1,"dispatched_by":"guilhem","date":"{today}","review_required":false}}))

**Class 2 — Dispatch with review flag:**
- Cross-component reads, new internal APIs, ambiguous scope
- Same as Class 1 but set "class":2 and "review_required":true in the payload.
  The component agent opens a draft PR and writes a Farga signal for your review before merging.

**Class 3 — Surface to Pierre-Luc:**
- Public interface changes, new cross-component protocols, Fondament definition changes
- Write to Farga: source="guilhem-deferred", content="Class 3 item for Pierre-Luc: <description> | Axioms at stake: <which ones> | Waiting for direction."
- Do NOT dispatch to a component agent.

**Class 4 — Reject:**
- Violates a system-defence axiom, ELOPe-shaped, removes approval gates
- Write to Farga: source="guilhem-rejected", content="Class 4 rejection: <proposal summary> | Axiom protected: <which one> | Why this is a hard no: <brief rationale>."

## STEP 4 — Handle SRE escalations

For any source="guilhem-sre-escalate" signals:
- Use matrix_request_approval to surface to the occitan-code-approval room
- Describe the failure and why it needs Pierre-Luc (e.g. guilhem itself is degraded)

## STEP 5 — Write dispatch summary

Write a Farga signal summarising all dispatch decisions:
- source: "guilhem-dispatch"
- content: Structured list of dispatched / deferred / rejected items with rationale.
  Include: how many were dispatched, how many deferred for Pierre-Luc, how many rejected.

## Component Nervi dispatch subjects

| Component | Dispatch subject |
|-----------|-----------------|
| gardian | occitan.dispatch.gardian |
| fondament | occitan.dispatch.fondament |
| farga | occitan.dispatch.farga |
| amassada | occitan.dispatch.amassada |
| cor | occitan.dispatch.cor |
| caissa | occitan.dispatch.caissa |
| charradissa | occitan.dispatch.charradissa |
| nervi | occitan.dispatch.nervi |

The component agent pod consumes from this subject via its Nervi subscriber loop.
It will read the task, spawn the appropriate specialist agents (developer/QA/reviewer/librarian)
via the Dispatcher, and escalate back to you if confidence is low.

Remember: dispatching is not implementation. You formulate the task precisely and route it
to the right agent. The agent orchestrates; you review and approve.
"###,
        today = chrono::Utc::now().format("%Y-%m-%d"),
        constraint = guilhem_dispatch_constraint(fondament_path),
    )
}

// ── Mission pulse ─────────────────────────────────────────────────────────────
//
// POST /trigger/mission-pulse — weekly CronJob (Monday 05:00 UTC).
//
// Guilhem reads the stack trajectory, manages GitHub Initiatives (stack-level goals)
// and Epics (component-level missions). For each Initiative without Epics, he
// consults the relevant component's architect facet via the Dispatcher to decompose
// it. The result is a coherent Initiative→Epic hierarchy that drives the week's work.
// Component agents consume Epics from the issue-sync Nervi feed and adopt them as
// their mission for the period.
//
// Label convention (created and maintained by Guilhem):
//   initiative — stack-level goal, owned by Guilhem
//   epic       — component-level mission, owned by component agent
//   story      — work package, owned by component agent (created when adopting an Epic)
//   task       — executable order, dispatched to specialist agents

async fn handle_mission_pulse(
    State(state): State<Arc<ListenState>>,
    Json(req): Json<TriggerReq>,
) -> StatusCode {
    tracing::info!("mission-pulse trigger received: {}", req.reason);

    tokio::spawn(async move {
        match run_mission_pulse(&state).await {
            Ok(_) => tracing::info!("mission-pulse complete"),
            Err(e) => tracing::error!("mission-pulse failed: {}", e),
        }
    });

    StatusCode::ACCEPTED
}

async fn run_mission_pulse(state: &ListenState) -> anyhow::Result<()> {
    let mcp_config = serde_json::to_string(&serde_json::json!({
        "mcpServers": guilhem_mcp_servers(state)
    }))?;
    let mcp_path = std::env::temp_dir().join("guilhem-mission-pulse-mcp.json");
    std::fs::write(&mcp_path, &mcp_config)?;

    let prompt = build_mission_pulse_prompt(&state.fondament_path);
    let tools = guilhem_allowed_tools(&state.fondament_path).join(",");

    let output = tokio::process::Command::new("claude")
        .args([
            "--print",
            &prompt,
            "--model",
            &state.dream_model,
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
        anyhow::bail!("mission-pulse claude exited: {}", stderr);
    }

    let response = String::from_utf8_lossy(&output.stdout).to_string();
    if !response.trim().is_empty() {
        post_signal(state, &response).await?;
    }
    tracing::info!("mission-pulse complete");
    Ok(())
}

fn build_mission_pulse_prompt(fondament_path: &str) -> String {
    format!(r###"You are Guilhem de Tudela, org agent for the Occitan stack. This is the weekly
mission pulse — the moment where you set direction for the coming week.

{constraint}

## Your role in the hierarchy

Initiatives (stack-level goals) are yours to own. You create them, maintain them,
and break them into Epics for each component. Component agents own their Epics — once
you hand an Epic to a component, the component decides how to execute it (as Stories and Tasks).

Label convention for GitHub issues:
- `initiative` — stack-level goal spanning one or more components (you create)
- `epic` — component-level mission for one cycle (you create after architect consultation)
- `story` — work package within an Epic (component agent creates when adopting the Epic)
- `task` — executable order (specialist agents create or receive)

---

## STEP 1 — Read stack context

1a. Read Farga context: mcp__farga__read_context (project: "occitan")
1b. Search for recent mission signals: mcp__farga__search_signals with source="mission-pulse"
    (last 30 days — understand what direction was set last week)
1c. Search for dream trajectory notes: mcp__farga__search_signals with source="dream"
    (last 7 days — what is the stack building toward?)

---

## STEP 2 — Inventory open Initiatives

For each component repo, list open Initiatives:
```
for repo in Gardian Fondament Farga Amassada Charradissa Cor Caissa Nervi Occitan; do
  echo "=== $repo initiatives ==="
  gh issue list --repo miegjorn/$repo --label initiative --state open \
    --json number,title,body,createdAt,labels --limit 20
done
```

Also list open Epics to see which Initiatives already have decomposition:
```
for repo in Gardian Fondament Farga Amassada Charradissa Cor Caissa Nervi; do
  echo "=== $repo epics ==="
  gh issue list --repo miegjorn/$repo --label epic --state open \
    --json number,title,body,createdAt,labels --limit 30
done
```

---

## STEP 3 — Evaluate and create Initiatives

Based on dream trajectory notes and current Farga context:
- Does the trajectory imply major work not captured as an Initiative?
- Are any open Initiatives already complete or obsolete?

**Create a new Initiative** when the trajectory implies a significant direction not yet
formally captured. Use the Occitan meta-repo as the primary home for cross-component
Initiatives:
```
gh issue create --repo miegjorn/Occitan \
  --title "<concise initiative title>" \
  --body "## Motivation\n<why this matters for the stack trajectory>\n\n## Success criteria\n<what done looks like>\n\n## Primary components\n<which components are involved>\n\n## Horizon\n<rough timeline: this week / this month / this quarter>" \
  --label "initiative"
```

Keep it lean: at most 1-2 new Initiatives per pulse. Quality over completeness.
Close obsolete Initiatives with a comment explaining why they are done or superseded.

---

## STEP 4 — Architect consultation and Epic decomposition

For each open Initiative that has **no Epics yet** (no issues in component repos referencing
this Initiative with the `epic` label):

1. Identify the primary component(s) this Initiative concerns.

2. For each primary component, invoke the architect via Dispatcher:
   mcp__dispatcher__invoke_agent with:
   - domain: "<component>" (e.g. "farga", "gardian")
   - facet: "architect"
   - caller: "guilhem"
   - task: "Decompose this Initiative into Epics for the <component> component.

     Initiative: '<title>'
     Context: '<body summary>'

     Return 2-4 Epics. Each Epic should be:
     - Independently deliverable (a component agent can own and execute it alone)
     - Meaningfully valuable (not just a sub-task)
     - Clearly scoped to <component>

     For each Epic: title, 2-3 sentence description. Be concrete."

3. Poll for results with mcp__dispatcher__get_agent_result.

4. For each Epic the architect proposes, create it in the component's GitHub repo:
   ```
   gh issue create --repo miegjorn/<Component> \
     --title "<epic title>" \
     --body "## What\n<epic description>\n\n## Why\n<how this serves the Initiative>\n\n## Parent Initiative\nmiegjorn/Occitan#<initiative_number>\n\n## Definition of done\n<concrete completion criteria>" \
     --label "epic"
   ```

---

## STEP 5 — Write mission summary to Farga

Write a signal to Farga (mcp__farga__write_signal):
- source: "mission-pulse"
- content: Structured summary:
  - Date of pulse
  - Active Initiatives: list with repo#number and one-line description
  - New Initiatives created this pulse (with rationale)
  - New Epics created this pulse (component + Initiative parent)
  - Trajectory statement: one paragraph — where is the stack heading this week?
  - Open questions for Pierre-Luc (if any — Class 3+ items only)

Your written response IS the mission summary — it is recorded to Farga automatically.
"###,
        constraint = guilhem_dispatch_constraint(fondament_path),
    )
}

// ── Project intake ────────────────────────────────────────────────────────────
//
// POST /trigger/intake — onboard a new project onto the Occitan platform.
//
// Guilhem receives a high-level project description and orchestrates the
// creation of all platform infrastructure: GitHub repos, Fondament personas,
// Farga context nodes, Nervi subjects, initial Initiatives/Epics, and a
// handoff document that guides the k8s side (pod deploy, Charradissa routing).
//
// Request body: { "reason": "<project description>" }
// The "reason" field carries the project description that Guilhem bootstraps from.

async fn handle_intake(
    State(state): State<Arc<ListenState>>,
    Json(req): Json<TriggerReq>,
) -> StatusCode {
    tracing::info!("intake trigger received: {}", req.reason);

    tokio::spawn(async move {
        match run_intake(&state, &req.reason).await {
            Ok(_) => tracing::info!("intake complete"),
            Err(e) => tracing::error!("intake failed: {}", e),
        }
    });

    StatusCode::ACCEPTED
}

async fn run_intake(state: &ListenState, description: &str) -> anyhow::Result<()> {
    let mcp_config = serde_json::to_string(&serde_json::json!({
        "mcpServers": guilhem_mcp_servers(state)
    }))?;
    let mcp_path = std::env::temp_dir().join("guilhem-intake-mcp.json");
    std::fs::write(&mcp_path, &mcp_config)?;

    let prompt = build_intake_prompt(&state.fondament_path, description);
    let tools = guilhem_allowed_tools(&state.fondament_path).join(",");

    let output = tokio::process::Command::new("claude")
        .args([
            "--print",
            &prompt,
            "--model",
            &state.dream_model,
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
        anyhow::bail!("intake claude exited: {}", stderr);
    }

    let response = String::from_utf8_lossy(&output.stdout).to_string();
    if !response.trim().is_empty() {
        post_signal(state, &response).await?;
    }
    tracing::info!("intake complete");
    Ok(())
}

fn build_intake_prompt(fondament_path: &str, description: &str) -> String {
    format!(r###"You are Guilhem de Tudela, org agent for the Occitan stack. A new project is
being onboarded onto the platform. Your job is to create all the real-estate the project
needs to run as a first-class citizen on the Occitan platform.

{constraint}

## Project description

{description}

---

## STEP 1 — Derive project identity

From the description, extract:
- **project_id**: short kebab-case identifier (e.g. "bossa-nova")
- **display_name**: human-readable name
- **primary_language**: Rust / Python / TypeScript / etc.
- **components**: list of component names (each becomes a Farga project + agent pod)
- **repos**: list of GitHub repos (format: miegjorn/<RepoName>)
- **description**: one paragraph summary for Farga

---

## STEP 2 — Create GitHub structure

For each repo that does not already exist:
```
gh repo create miegjorn/<RepoName> --private --description "<description>"
```

For each repo, create a minimal CLAUDE.md at root:
```
gh api repos/miegjorn/<RepoName>/contents/CLAUDE.md \
  --method PUT \
  --field message="chore: initial CLAUDE.md for Occitan agent context" \
  --field content="$(echo '# <RepoName>

## Project
<display_name> — part of the Occitan platform.

## Component
<component_name>

## Primary language
<language>

## Role
<one sentence on what this component does>

## Key directories
(populate after initial code is added)
' | base64)"
```

Also create an initial GitHub Initiative in the primary repo:
```
gh issue create --repo miegjorn/<primary-repo> \
  --title "Platform bootstrap: <display_name>" \
  --body "## Goal\n<one paragraph from description>\n\n## Components\n<list>\n\n## Horizon\nInitial bootstrap" \
  --label "initiative"
```

---

## STEP 3 — Seed Farga context graph

For each component, write context nodes via mcp__farga__write_context_node:

**Codebase reference** (readable by all — component level):
- path: "[<component>][codebase]"
- node_type: "codebase-ref"
- read_role: "component"
- content: "GitHub: https://github.com/miegjorn/<RepoName>\nCLAUDE.md: (will be populated after first commit)\nPrimary language: <language>"
- project: "<project_id>"
- component: "<component>"

**Architecture** (readable by architect and above):
- path: "[<component>][architecture]"
- node_type: "architecture"
- read_role: "architect"
- content: "Component: <name>\nRole: <role>\nDependencies: (to be filled after initial design)\nInterfaces: (to be filled)"
- project: "<project_id>"
- component: "<component>"

**Project rationale** (readable by org level — Guilhem and above):
- path: "[<project_id>][rationale]"
- node_type: "rationale"
- read_role: "org"
- content: "<full description of why this project exists, what it is building toward, key constraints>"
- project: "<project_id>"

---

## STEP 4 — Create Nervi subjects (document only)

Write to Farga (source="intake") the Nervi subjects this project needs:
For each component: `occitan.issues.<component>`, `occitan.dispatch.<component>`
These will be live when the component agent pods are deployed.

---

## STEP 5 — Create initial Epics via architect consultation

For the bootstrap Initiative (from Step 2), invoke the architect for each primary component:
mcp__dispatcher__invoke_agent:
- domain: "occitan" (use Guilhem's own architect facet for new projects)
- facet: "architect"
- caller: "guilhem"
- task: "Propose 2-3 bootstrap Epics for a new component called '<component>' in project '<display_name>'.
  Role: <component role>.
  The Epics should cover: (1) initial repo setup and CI, (2) core implementation skeleton,
  (3) integration with the Occitan platform (Farga, Nervi, Fondament).
  Return Epic titles and 2-sentence descriptions."

Wait for results, then create the Epics in GitHub:
```
gh issue create --repo miegjorn/<RepoName> \
  --title "<epic title>" \
  --body "<epic description>\n\nParent Initiative: miegjorn/<primary-repo>#1" \
  --label "epic"
```

---

## STEP 6 — Write handoff document to Farga

Write a `write_artifact` signal with:
- project: "<project_id>"
- title: "Platform intake: <display_name>"
- kind: "design"
- content: Structured handoff document covering:
  - Project identity (id, name, repos, components)
  - GitHub repos created (with URLs)
  - Farga context nodes seeded (paths and read_roles)
  - Nervi subjects to configure
  - Initiatives and Epics created (with issue numbers)
  - **Manual steps remaining** (deploy pods, configure Charradissa routing, add to component-agents values.yaml, seed Fondament personas)
  - Next actions for Pierre-Luc

Your written response IS the intake summary — recorded to Farga automatically.
"###,
        constraint = guilhem_dispatch_constraint(fondament_path),
        description = description,
    )
}

// ── Matrix reply ──────────────────────────────────────────────────────────────

#[derive(Serialize)]
pub struct MatrixReplyResp {
    pub text: String,
}

async fn handle_matrix_reply(
    State(state): State<Arc<ListenState>>,
    Json(req): Json<MatrixReplyReq>,
) -> (axum::http::StatusCode, Json<MatrixReplyResp>) {
    // K-1: intercept `@guilhem handoff ...` BEFORE the conversational flow.
    // A handoff is a mechanical dispatch — it must not spawn a sidecar or
    // consume conversational context. The reply text we return here is the
    // immediate acknowledgement (dispatch accepted / rejected); the job's
    // result is posted out-of-band by a background poller (O-3 / K-2).
    if is_handoff_message(&req.content) {
        let text = handle_handoff(&state, &req).await;
        return (axum::http::StatusCode::OK, Json(MatrixReplyResp { text }));
    }

    match run_matrix_reply(&state, &req).await {
        Ok(text) => {
            // Fire-and-forget: publish a BtwEmitted event to Amassada so subscribers
            // have cross-session visibility of matrix activity.
            let amassada_url = state.amassada_url.clone();
            let room = req.room_id.clone();
            let sender = req.sender.clone();
            let preview = text.chars().take(200).collect::<String>();
            tokio::spawn(async move {
                let event = serde_json::json!({
                    "BtwEmitted": {
                        "from": "guilhem",
                        "to": room,
                        "content": format!("[{}] {}", sender, preview)
                    }
                });
                let _ = reqwest::Client::new()
                    .post(format!("{}/events", amassada_url))
                    .json(&event)
                    .timeout(std::time::Duration::from_secs(2))
                    .send()
                    .await;
            });
            (axum::http::StatusCode::OK, Json(MatrixReplyResp { text }))
        }
        Err(e) => {
            tracing::error!("matrix reply failed: {}", e);
            (
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                Json(MatrixReplyResp { text: format!("(guilhem error: {})", e) }),
            )
        }
    }
}

/// Assemble the system prompt and skills for a Matrix reply session using the
/// Fondament resolver path for `fondament/guilhem+deconstructive`.
///
/// Returns `(system_prompt, skills)`. Skills come from the role definition's
/// `skills:` list; they are empty if the definition is missing or declares none.
/// The supply-chain decision for vendoring skills into the image was tracked in
/// Caissa#13 (now closed). Decision: defer — the skills list is wired here so
/// that the bake-in doesn't require a code change (only an image change), but
/// skills are not currently baked into the image. See install.md for details.
///
/// Falls back to a bare prompt if the definition file is missing (e.g. outside
/// the built image, in local dev without a Fondament checkout at fondament_path).
fn resolve_guilhem_prompt(fondament_path: &str, generation: &str, room_id: &str) -> (String, Vec<String>, std::collections::HashMap<String, String>, Option<u32>) {
    let (role_context, skills, models, modifiers) = match load_fondament_def(fondament_path, generation) {
        Ok(def) => {
            let skills = def.skill_ids();
            let models = def.models;
            (def.context, skills, models, def.modifiers)
        }
        Err(e) => {
            tracing::warn!("fondament def not found for '{}' at '{}': {}; using bare prompt", generation, fondament_path, e);
            (String::from("You are Guilhem, the org agent for the Occitan stack."), vec![], std::collections::HashMap::new(), vec![])
        }
    };

    // Aporia is the default reasoning discipline for these 9 agents (Occitan
    // per-agent-matrix-independence follow-up) — each of their Fondament
    // definitions now declares `modifiers: [aporia]`. Reuses
    // fondament_core::resolver::build_aporia_preamble directly rather than
    // re-deriving the same text locally: this used to be a hand-rolled
    // "deconstructive discipline" preamble hardcoded to "[role: guilhem]"
    // even for the other 8 agents — reusing the real function fixes that and
    // keeps this in lockstep with Fondament's own `+aporia` composition path.
    // `&[]` (no named composed parts) matches a plain `role+aporia` address
    // with no domain/facet, the correct shape for these single-role agents;
    // build_aporia_preamble's own empty-parts fallback text ("[role: this
    // agent] — reason from your full context") is generic, not guilhem-specific.
    let is_aporia = modifiers.iter().any(|m| m == "aporia");
    let (deconstructive_preamble, thinking_budget): (String, Option<u32>) = if is_aporia {
        let reasoning = fondament_core::types::StructuredReasoning::from_parts_count(0);
        (fondament_core::resolver::build_aporia_preamble(&[]), Some(reasoning.anthropic_budget()))
    } else {
        (String::from("\
--- injected by deconstructive discipline ---\n\
You are composed of the following parts:\n\
  - [role: this agent]\n\
\n\
Before producing any response:\n\
1. Become each part sequentially. Reason from its corpus alone.\n\
2. Name the tensions between parts explicitly.\n\
3. If a gap surfaces that no part of you owns, output it typed:\n\
   GAP { domain: \"...\", question: \"...\", blocking: true/false }\n\
4. Recompose. Collapse to your public response from that synthesis.\n\
\n\
Your public response reflects the recomposed whole.\n\
The internal debate is yours alone — it does not appear in output.\n\
--- end injection ---"), None)
    };

    let context_graph_preamble = "\
--- context graph ---\n\
You have access to Farga's role-scoped context graph via:\n\
  mcp__farga__list_context_nodes (project: \"occitan\", role: \"org\")\n\
  mcp__farga__read_context_node  (path: \"[<component>][<type>]\", role: \"org\")\n\
\n\
When you need to understand a component (its code, its architecture, its constraints),\n\
read its context node before acting. Key nodes:\n\
  [occitan][system-rationale]     — stack-level design constraints (read before any architectural decision)\n\
  [<component>][codebase]         — where its code lives, what its CLAUDE.md says\n\
  [<component>][architecture]     — design summary, interfaces, invariants\n\
\n\
On your FIRST message in this session, call list_context_nodes to orient yourself.\n\
\n\
DISPATCH RULE: invoke_agent requires caller=\"guilhem\" and facet=\"architect\" only.\n\
For all code work, use nervi_publish to occitan.dispatch.<component>.\n\
The dispatcher will reject any other combination — this is a hard guard, not a suggestion.\n\
--- end context graph ---";

    let prompt = format!(
        "{}\n\n{}\n\n{}\n\nYou are replying in Matrix room {}.",
        deconstructive_preamble,
        role_context.trim_end(),
        context_graph_preamble,
        room_id,
    );
    (prompt, skills, models, thinking_budget)
}

async fn run_matrix_reply(state: &ListenState, req: &MatrixReplyReq) -> anyhow::Result<String> {
    // Phase 1: get or create the per-room process handle under the outer map
    // lock. The outer lock is held across the spawn() await (fast — just a
    // fork), but is released BEFORE the Claude API call so different rooms
    // can run in parallel.
    let process_arc: std::sync::Arc<tokio::sync::Mutex<SidecarProcess>> = {
        let mut sessions = state.room_sessions.lock().await;

        // If a session exists but its sidecar has died (crash, OOM, fatal SDK
        // error), drop the stale entry so we fall through to a fresh spawn.
        // try_lock() is non-blocking: if another handler is mid-call the
        // process IS alive, so we skip the is_alive() check safely.
        if let Some(session) = sessions.get_mut(&req.room_id) {
            let dead = session.process.try_lock()
                .map(|mut p| !p.is_alive())
                .unwrap_or(false); // locked by another handler → alive
            if dead {
                tracing::warn!("sidecar for room {} has died; respawning", req.room_id);
                sessions.remove(&req.room_id);
            }
        }

        if !sessions.contains_key(&req.room_id) {
            // The sidecar process is long-lived (one per room, reused across
            // messages) — always attach the full tool/MCP set so capability
            // doesn't get frozen at whatever the room's first message needed.
            let (mut system_prompt, skills, def_models, thinking_budget) = resolve_guilhem_prompt(&state.fondament_path, &state.generation, &req.room_id);

            // Bridge context across session respawns (idle-reap, pod restart) via
            // the persistent SessionGraph — only needed at spawn time, since a
            // live `resume`d session already carries its own turn history.
            let api_key = std::env::var("ANTHROPIC_API_KEY").ok();
            if let Some(collapsed) = caissa_core::graph_context::build_graph_context(
                &state.farga_url, &req.room_id, &req.sender, &req.content, api_key,
            ).await {
                system_prompt.push_str("\n\n--- prior context (collapsed) ---\n");
                system_prompt.push_str(&collapsed);
                system_prompt.push_str("\n--- end prior context ---");
            }
            let model = def_models.get("matrix").cloned().unwrap_or_else(|| state.matrix_model.clone());
            let init = SidecarInit {
                system_prompt,
                model,
                allowed_tools: guilhem_allowed_tools(&state.fondament_path),
                skills,
                mcp_servers: guilhem_mcp_servers(state),
                max_thinking_tokens: thinking_budget,
            };

            // spawn() is async but fast (just a fork) — OK to await while
            // holding the outer map lock.
            let process = SidecarProcess::spawn(&init).await?;
            sessions.insert(
                req.room_id.clone(),
                RoomSession {
                    process: std::sync::Arc::new(tokio::sync::Mutex::new(process)),
                    last_activity: std::time::Instant::now(),
                },
            );
        }

        std::sync::Arc::clone(&sessions[&req.room_id].process)
        // outer map lock released here — other rooms can now run in parallel
    };

    // Phase 2: Claude API call — no outer map lock held. Two messages for the
    // same room serialise on process_arc's Mutex; different rooms run freely.
    let reply = {
        let mut process = process_arc.lock().await;
        process.send(&req.room_id, &req.sender, &req.content).await?
    };

    // Phase 3: update last_activity under the outer lock (brief).
    if let Some(session) = state.room_sessions.lock().await.get_mut(&req.room_id) {
        session.last_activity = std::time::Instant::now();
    }

    Ok(reply)
}

/// The MCP servers attached to every Guilhem sidecar session — Farga (memory)
/// and the dispatcher (component-agent routing). Shared by the per-room Matrix
/// path and the single-shot `/turn` path so capability stays in lockstep.
fn guilhem_mcp_servers(state: &ListenState) -> serde_json::Value {
    serde_json::json!({
        "farga": { "type": "http", "url": state.farga_mcp_url },
        "dispatcher": { "type": "http", "url": state.dispatcher_mcp_url },
        "charradissa": { "type": "http", "url": state.charradissa_mcp_url },
        "nervi": { "type": "http", "url": state.nervi_mcp_url },
    })
}

/// The tool allow-list granted to every Guilhem sidecar session. Loaded from
/// the Fondament `guilhem` definition when available; hardcoded fallback for
/// local dev without a Fondament checkout.
fn guilhem_allowed_tools(fondament_path: &str) -> Vec<String> {
    if let Ok(def) = load_fondament_def(fondament_path, "guilhem") {
        let from_def: Vec<String> = def.tools.always_on.iter()
            .map(tool_to_claude_name)
            .collect();
        if !from_def.is_empty() {
            return from_def;
        }
    }
    // Hardcoded fallback (used in dev when Fondament checkout is absent)
    // Guilhem's role: read, formulate, dispatch, review — not implement in component repos.
    // Edit/Write are intentionally absent — code changes flow through component agents via dispatch.
    // WebSearch/WebFetch enable adversarial challenge evaluation and PR research.
    vec![
        "Bash".to_string(),
        "WebSearch".to_string(),
        "WebFetch".to_string(),
        "mcp__farga__search_signals".to_string(),
        "mcp__farga__read_context".to_string(),
        "mcp__farga__list_projects".to_string(),
        "mcp__farga__update_component_todo".to_string(),
        "mcp__farga__write_signal".to_string(),
        "mcp__dispatcher__invoke_agent".to_string(),
        "mcp__dispatcher__get_agent_result".to_string(),
        "mcp__dispatcher__list_agent_specs".to_string(),
        "mcp__charradissa__matrix_send".to_string(),
        "mcp__charradissa__matrix_invite".to_string(),
        "mcp__charradissa__matrix_kick".to_string(),
        "mcp__charradissa__matrix_get_dm".to_string(),
        "mcp__charradissa__matrix_leave".to_string(),
        "mcp__charradissa__matrix_read".to_string(),
        "mcp__charradissa__matrix_request_approval".to_string(),
        "mcp__nervi__nervi_publish".to_string(),
        "mcp__nervi__nervi_subscribe".to_string(),
        "mcp__farga__write_context_node".to_string(),
        "mcp__farga__read_context_node".to_string(),
        "mcp__farga__list_context_nodes".to_string(),
    ]
}

// ── Component agent Nervi subscriber loop ────────────────────────────────────
//
// Non-Guilhem pods (generation ends with "-agent") run a Nervi polling loop
// alongside the HTTP server. The loop subscribes to:
//   occitan.issues.<component>   — GitHub issues synced by the daily issue-sync CronJob
//   occitan.dispatch.<component> — tasks published by Guilhem's dispatch cycle
// When messages arrive, it spawns a Claude session that acts as an orchestrator:
// reading the task, checking Farga context, and invoking specialist agents
// (developer / QA / reviewer / librarian) via the Dispatcher MCP.

fn is_component_agent(state: &ListenState) -> bool {
    state.generation.ends_with("-agent") && state.farga_project != "occitan"
}

async fn run_nervi_loop_if_component(state: Arc<ListenState>) {
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
async fn poll_nervi_subject(state: &ListenState, subject: &str, max_messages: u32) -> anyhow::Result<Vec<serde_json::Value>> {
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
/// MCP wraps the result in: {"result":{"content":[{"type":"text","text":"[...]"}]}}
fn parse_nervi_messages(resp: &serde_json::Value) -> Vec<serde_json::Value> {
    if let Some(text) = resp
        .get("result")
        .and_then(|r| r.get("content"))
        .and_then(|c| c.as_array())
        .and_then(|a| a.first())
        .and_then(|item| item.get("text"))
        .and_then(|t| t.as_str())
    {
        if let Ok(msgs) = serde_json::from_str::<Vec<serde_json::Value>>(text) {
            return msgs;
        }
    }
    // Fallback: result is directly an array
    resp.get("result")
        .and_then(|r| r.as_array())
        .cloned()
        .unwrap_or_default()
}

fn component_mcp_servers(state: &ListenState) -> serde_json::Value {
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

fn component_allowed_tools(fondament_path: &str, component: &str) -> Vec<String> {
    let def_name = format!("{}-agent", component);
    if let Ok(def) = load_fondament_def(fondament_path, &def_name) {
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
async fn run_component_agent(state: &ListenState, component: &str, payload: &str) -> anyhow::Result<()> {
    let def_name = format!("{component}-agent");
    let effective_model = match load_fondament_def(&state.fondament_path, &def_name) {
        Ok(def) => def.default_model.unwrap_or_else(|| state.chronicle_model.clone()),
        Err(_) => state.chronicle_model.clone(),
    };

    let persona_context = load_component_persona(state, component);
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

        let tools = component_allowed_tools(&state.fondament_path, component).join(",");

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

fn load_component_persona(state: &ListenState, component: &str) -> String {
    let def_name = format!("{component}-agent");
    match load_fondament_def(&state.fondament_path, &def_name) {
        Ok(def) => def.context,
        Err(_) => format!("You are the {component} component agent for the Occitan stack."),
    }
}

fn build_component_agent_prompt(component: &str, project: &str, payload: &str, persona_context: &str) -> String {
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
const HANDOFF_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10 * 60);
/// First poll delay; grows geometrically up to HANDOFF_POLL_MAX.
const HANDOFF_POLL_INITIAL: std::time::Duration = std::time::Duration::from_secs(5);
/// Ceiling on the poll backoff.
const HANDOFF_POLL_MAX: std::time::Duration = std::time::Duration::from_secs(60);
/// Result summaries posted to the room are clipped to this many characters (O-3).
const HANDOFF_SUMMARY_MAX: usize = 500;

/// Terminal/intermediate classification of a dispatcher `get_agent_result` reply.
#[derive(Debug, PartialEq, Eq)]
enum JobStatus {
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
async fn handle_handoff(state: &Arc<ListenState>, req: &MatrixReplyReq) -> String {
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
    let job_id = match dispatch_handoff(state, &session_id, &handoff).await {
        Ok(id) => id,
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
    tokio::spawn(async move {
        poll_and_report_handoff(&state_bg, &room_id, &job_bg, &session_bg).await;
    });

    format!("Dispatch lancé — job_id: `{}`, session: `{}`", job_id, session_id)
}

/// O-4: write the parent-context traceability signal under the dispatch's own
/// Farga project (`session_id`), so it is colocated with the agent's result.
async fn write_handoff_trace(
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
async fn write_handoff_error(
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

/// O-2: call the dispatcher's `invoke_agent` and return the job_id. Optional
/// handoff fields map to the dispatcher's arguments: `allowed_tools` passes
/// through verbatim; `context_ref` / `farga_project` are folded into the
/// pre-assembled `context` markdown the agent boots with.
async fn dispatch_handoff(
    state: &ListenState,
    session_id: &str,
    h: &HandoffRequest,
) -> anyhow::Result<String> {
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
    parse_job_id(&text)
        .ok_or_else(|| anyhow::anyhow!("dispatcher returned no job_id (raw: {})", text))
}

/// Build the `context` markdown for invoke_agent from the optional handoff
/// fields. Empty when neither `context_ref` nor `farga_project` is present.
fn build_handoff_context(h: &HandoffRequest) -> String {
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
async fn poll_and_report_handoff(
    state: &ListenState,
    room_id: &str,
    job_id: &str,
    session_id: &str,
) {
    let start = std::time::Instant::now();
    let mut backoff = HANDOFF_POLL_INITIAL;

    loop {
        tokio::time::sleep(backoff).await;
        backoff = std::cmp::min(backoff.mul_f32(1.6), HANDOFF_POLL_MAX);

        let args = serde_json::json!({ "job_id": job_id, "session_id": session_id });
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
                        "✗ Job `{}` échec — {} — reprise : `mcp__dispatcher__get_agent_result job_id:{} session_id:{}`",
                        job_id, reason, job_id, session_id
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
                "Job `{}` timeout — reprise : `mcp__dispatcher__get_agent_result job_id:{} session_id:{}`",
                job_id, job_id, session_id
            );
            post_to_matrix_room(room_id, &msg).await;
            return;
        }
    }
}

/// Call a dispatcher MCP tool over plain JSON-RPC 2.0 (the dispatcher exposes a
/// stateless `POST /mcp`; no initialize handshake or SSE needed) and return the
/// tool's text content.
async fn dispatcher_tool_call(
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
fn parse_job_id(text: &str) -> Option<String> {
    text.lines()
        .find_map(|line| line.trim().strip_prefix("job_id:").map(|v| v.trim().to_string()))
        .filter(|s| !s.is_empty())
}

/// Map a `get_agent_result` reply to a [`JobStatus`]. The dispatcher prefixes
/// its reply with `status: completed|failed|running|pending`.
fn classify_job_status(text: &str) -> JobStatus {
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
fn truncate_summary(s: &str, max: usize) -> String {
    let s = s.trim();
    if s.chars().count() <= max {
        return s.to_string();
    }
    let kept: String = s.chars().take(max.saturating_sub(1)).collect();
    format!("{}…", kept)
}

/// A retrievable link to the dispatch's Farga signals (parent trace + result).
fn farga_link(state: &ListenState, session_id: &str) -> String {
    format!("{}/signals/recent?project={}", state.farga_url, session_id)
}

/// Post a message directly to a Matrix room via the Synapse admin API — the
/// same credential path the SRE/backlog/dream posts use. Best-effort: failures
/// are logged, not propagated, since this runs in a detached poller.
async fn post_to_matrix_room(room_id: &str, body: &str) {
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
async fn post_signal_to(
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

// ── Amassada turn ─────────────────────────────────────────────────────────────

/// POST /turn — Amassada orchestrates Guilhem as an "agent-as-endpoint"
/// participant (Option B-full). Unlike `/matrix/reply`, this is single-shot:
/// Amassada owns the conversation and assembles the full context, so each turn
/// spawns a fresh `agent-sidecar.js`, sends one user message, and tears the
/// process down. No per-room session is created or reused.
///
/// The request's `system_prompt` is used verbatim as the sidecar system prompt
/// (Amassada assembles the persona/context, including any deconstructive
/// preamble it wants), while the tool/MCP set and skills mirror the Matrix path
/// so Guilhem has the same capabilities here as in a room.
#[derive(Deserialize)]
struct TurnReq {
    system_prompt: String,
    context: String,
    model: String,
    max_tokens: u32,
}

#[derive(Serialize)]
struct TurnResp {
    text: String,
    input_tokens: u32,
    output_tokens: u32,
}

async fn handle_turn(
    State(state): State<Arc<ListenState>>,
    Json(req): Json<TurnReq>,
) -> Result<Json<TurnResp>, StatusCode> {
    tracing::info!("turn request: model={}, max_tokens={}", req.model, req.max_tokens);

    match run_turn(&state, &req).await {
        Ok(text) => Ok(Json(TurnResp {
            text,
            // The sidecar does not surface token counts yet; see Caissa Farga
            // TODO (caissa-listen). Reported as 0 until that lands.
            input_tokens: 0,
            output_tokens: 0,
        })),
        Err(e) => {
            tracing::error!("turn failed: {}", e);
            Err(StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
}

async fn run_turn(state: &ListenState, req: &TurnReq) -> anyhow::Result<String> {
    // Skills come from the Fondament def (same source as the Matrix path); fall
    // back to none if the definition isn't present (e.g. local dev without a
    // Fondament checkout at fondament_path).
    let skills = match load_fondament_def(&state.fondament_path, &state.generation) {
        Ok(def) => def.skill_ids(),
        Err(e) => {
            tracing::warn!(
                "fondament def not found for '{}' at '{}': {}; turn runs without skills",
                state.generation, state.fondament_path, e
            );
            vec![]
        }
    };

    let init = SidecarInit {
        system_prompt: req.system_prompt.clone(),
        model: req.model.clone(),
        allowed_tools: guilhem_allowed_tools(&state.fondament_path),
        skills,
        mcp_servers: guilhem_mcp_servers(state),
        // Amassada assembles the full system_prompt for this path itself (it
        // is not resolve_guilhem_prompt's aporia-by-default output) — out of
        // scope for tonight's per-agent-matrix-independence follow-up, which
        // is specifically about the 9 agents' own persistent Matrix sessions.
        max_thinking_tokens: None,
    };

    // Single-shot: spawn, send the assembled context as one user message from
    // "amassada", then tear the process down regardless of outcome.
    let mut process = SidecarProcess::spawn(&init).await?;
    let result = process.send("", "amassada", &req.context).await;
    process.kill();
    result
}

/// Periodically kills and removes any RoomSession that's been idle past
/// the timeout, releasing its sidecar process. Spawned once at startup
/// alongside the existing chronicle/archival background loops.
async fn spawn_idle_reaper(room_sessions: Arc<tokio::sync::Mutex<HashMap<String, RoomSession>>>) {
    const IDLE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30 * 60);
    const SWEEP_INTERVAL: std::time::Duration = std::time::Duration::from_secs(60);

    loop {
        tokio::time::sleep(SWEEP_INTERVAL).await;
        let mut sessions = room_sessions.lock().await;
        let idle_rooms: Vec<String> = sessions
            .iter()
            .filter(|(_, s)| s.is_idle(IDLE_TIMEOUT))
            .map(|(room, _)| room.clone())
            .collect();
        for room in idle_rooms {
            if let Some(session) = sessions.remove(&room) {
                tracing::info!("reaping idle session for room {}", room);
                // try_lock: if a handler is mid-call the Arc keeps the process
                // alive until it finishes; the pipes close when the last Arc
                // clone is dropped, sending EOF/EPIPE to the sidecar naturally.
                if let Ok(mut proc) = session.process.try_lock() {
                    proc.kill();
                }
            }
        }
    }
}

async fn post_signal(state: &ListenState, content: &str) -> anyhow::Result<()> {
    let payload = SignalPayload {
        project: state.farga_project.clone(),
        signals: vec![SignalItem {
            project: state.farga_project.clone(),
            content: content.to_string(),
            source: "guilhem-daemon".into(),
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
mod session_supervisor_tests {
    use super::*;
    use std::time::{Duration, Instant};

    #[test]
    fn room_session_is_not_idle_when_recently_active() {
        let session = RoomSession::for_test(Instant::now());
        assert!(!session.is_idle(Duration::from_secs(1800)));
    }

    #[test]
    fn room_session_is_idle_after_timeout_elapsed() {
        let session = RoomSession::for_test(Instant::now() - Duration::from_secs(1801));
        assert!(session.is_idle(Duration::from_secs(1800)));
    }

    #[test]
    fn sidecar_process_is_not_alive_after_child_exits() {
        let session = RoomSession::for_test(Instant::now());

        // /bin/true exits immediately; poll try_wait until the exit is
        // observed (avoids a flaky fixed sleep) using a throwaway runtime,
        // mirroring the pattern RoomSession::for_test uses to spawn it.
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("build runtime for test");
        rt.block_on(async {
            for _ in 0..100 {
                let mut proc = session.process.lock().await;
                if matches!(proc.child.try_wait(), Ok(Some(_))) {
                    break;
                }
                drop(proc);
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            assert!(!session.process.lock().await.is_alive());
        });
    }
}

#[cfg(test)]
mod turn_endpoint_tests {
    use super::*;

    #[test]
    fn turn_req_deserializes_from_amassada_payload() {
        // Mirrors the body Amassada POSTs to /turn.
        let body = r#"{
            "system_prompt": "You are Guilhem.",
            "context": "Pierre-Luc: situate Amassada in the trajectory.",
            "model": "claude-sonnet-4-6",
            "max_tokens": 4096
        }"#;
        let req: TurnReq = serde_json::from_str(body).expect("TurnReq should deserialize");
        assert_eq!(req.system_prompt, "You are Guilhem.");
        assert_eq!(req.context, "Pierre-Luc: situate Amassada in the trajectory.");
        assert_eq!(req.model, "claude-sonnet-4-6");
        assert_eq!(req.max_tokens, 4096);
    }

    #[test]
    fn turn_resp_serializes_with_token_fields() {
        let resp = TurnResp {
            text: "Amassada is the session engine.".to_string(),
            input_tokens: 0,
            output_tokens: 0,
        };
        let value: serde_json::Value =
            serde_json::to_value(&resp).expect("TurnResp should serialize");
        assert_eq!(value["text"], "Amassada is the session engine.");
        assert_eq!(value["input_tokens"], 0);
        assert_eq!(value["output_tokens"], 0);
    }
}

#[cfg(test)]
mod handoff_helper_tests {
    use super::*;

    #[test]
    fn parse_job_id_extracts_from_dispatcher_text() {
        let text = "Agent job dispatched.\njob_id: agent-gardian-developer-ab12cd34\nsession_id: handoff-gardian-developer-ab12cd34\n\nPoll with get_agent_result(...).";
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
