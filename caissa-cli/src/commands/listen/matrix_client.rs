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

/// Reads the agent's own Matrix password from `/creds/tokens.env`, written by
/// the fetch-tokens init container. Returns empty string if absent (local dev,
/// or a pod that hasn't been given Matrix credentials yet) — matrix_client_loop
/// treats an empty matrix_room_id/matrix_password as "feature disabled here".
pub(crate) fn read_matrix_password() -> String {
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

pub(crate) async fn matrix_login(homeserver: &str, user: &str, password: &str) -> anyhow::Result<String> {
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

pub(crate) fn matrix_pct(s: &str) -> String {
    s.chars().map(|c| match c {
        '!' | '#' | '@' | ':' | '/' | '?' | '&' | '=' | '+' | ' ' => format!("%{:02X}", c as u32),
        _ => c.to_string(),
    }).collect()
}

/// Long-poll `/sync` once. Returns the new `since` token and any
/// `m.room.message` timeline events for `room_id`, `(sender, content)` pairs.
/// A 401 with `M_UNKNOWN_TOKEN` is surfaced as `Err` so the caller can re-login.
pub(crate) async fn matrix_sync(
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

pub(crate) async fn matrix_post_body(homeserver: &str, token: &str, room_id: &str, body: &serde_json::Value) -> anyhow::Result<()> {
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
pub(crate) fn markdown_body(content: &str) -> serde_json::Value {
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

pub(crate) fn render_markdown(content: &str) -> String {
    use pulldown_cmark::{html, Options, Parser};
    let opts = Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TASKLISTS;
    let parser = Parser::new_ext(content, opts);
    let mut html_out = String::new();
    html::push_html(&mut html_out, parser);
    html_out
}

pub(crate) fn html_differs_from_plain(plain: &str, html: &str) -> bool {
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
pub(crate) async fn render_diagram_png(kroki_url: &str, diagram: &str) -> anyhow::Result<Vec<u8>> {
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

pub(crate) async fn matrix_upload_media(homeserver: &str, token: &str, content_type: &str, data: Vec<u8>) -> anyhow::Result<String> {
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

pub(crate) async fn render_and_upload_diagram(homeserver: &str, token: &str, kroki_url: &str, diagram: &str) -> anyhow::Result<String> {
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
pub(crate) async fn substitute_mermaid_with_images(homeserver: &str, token: &str, kroki_url: &str, content: &str) -> String {
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
pub(crate) async fn post_reply(homeserver: &str, token: &str, room_id: &str, kroki_url: &str, reply: &str) -> anyhow::Result<()> {
    let substituted = substitute_mermaid_with_images(homeserver, token, kroki_url, reply).await;
    if !substituted.trim().is_empty() {
        matrix_post_body(homeserver, token, room_id, &markdown_body(&substituted)).await?;
    }
    Ok(())
}

#[cfg(test)]
mod matrix_rendering_tests {
    use super::*;
    use crate::commands::listen::*;

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
pub(crate) async fn run_matrix_client_loop(state: Arc<ListenState>) {
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

