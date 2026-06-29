//! Dispatcher — MCP server that creates k8s agent Jobs on behalf of Guilhem.
//!
//! Runs in k8s with a ServiceAccount that has permission to create/watch Jobs
//! in the `agents` namespace. Guilhem calls this via MCP tools from his session.
//!
//! MCP tools:
//!   invoke_agent      — create a k8s Job for a domain/facet agent
//!   get_agent_result  — check Job status and read result from Farga
//!   list_agent_specs  — list known domain/facet combinations

use axum::{extract::State, http::StatusCode, routing::post, Json, Router};
use k8s_openapi::api::batch::v1::{Job, JobSpec};
use k8s_openapi::api::core::v1::{
    Container, EmptyDirVolumeSource, EnvVar, EnvVarSource, LocalObjectReference, PodSpec,
    PodTemplateSpec, SecretKeySelector, Volume, VolumeMount,
};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;
use kube::{api::PostParams, Api, Client};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::sync::Arc;

// ── State ─────────────────────────────────────────────────────────────────────

#[derive(Clone)]
struct DispatchState {
    k8s: Arc<Client>,
    agent_image: String,
    agents_namespace: String,
    farga_url: String,
    farga_mcp_url: String,
}

// ── JSON-RPC 2.0 (shared pattern with Farga MCP) ─────────────────────────────

#[derive(Debug, Deserialize)]
struct JsonRpcRequest {
    id: Option<Value>,
    method: String,
    params: Option<Value>,
}

#[derive(Debug, Serialize)]
struct JsonRpcResponse {
    jsonrpc: &'static str,
    id: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<JsonRpcError>,
}

#[derive(Debug, Serialize)]
struct JsonRpcError {
    code: i32,
    message: String,
}

impl JsonRpcResponse {
    fn ok(id: Option<Value>, result: Value) -> Self {
        Self { jsonrpc: "2.0", id, result: Some(result), error: None }
    }
    fn err(id: Option<Value>, code: i32, message: impl Into<String>) -> Self {
        Self { jsonrpc: "2.0", id, result: None, error: Some(JsonRpcError { code, message: message.into() }) }
    }
}

fn text_result(text: impl Into<String>) -> Value {
    json!({ "content": [{ "type": "text", "text": text.into() }] })
}

// ── Tool definitions ──────────────────────────────────────────────────────────

fn tool_list() -> Value {
    json!({
        "tools": [
            {
                "name": "invoke_agent",
                "description": "Spawn a domain/facet agent as a k8s Job. The agent runs non-interactively, executes the task, writes its result to Farga under session_id, then exits. Returns a job_id for status polling.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "domain": {
                            "type": "string",
                            "description": "Component domain: farga | gardian | amassada | charradissa | cor | caissa | fondament | occitan"
                        },
                        "facet": {
                            "type": "string",
                            "description": "Role facet: architect | developer | qa | infra | db | security"
                        },
                        "task": {
                            "type": "string",
                            "description": "The task for the agent to perform. Be specific — this becomes the claude --print prompt."
                        },
                        "context": {
                            "type": "string",
                            "description": "Pre-assembled domain+facet context markdown. Written to /workspace/CLAUDE.md before the agent runs. Load from /fondament/domains/<domain>.yaml for domain context. For facet context, the filename does NOT match the facet keyword — use this mapping: developer->developer.yaml, infra->infra-engineer.yaml, qa->qa-engineer.yaml, security->security-analyst.yaml, architect->app-architect.yaml, db->data-architect.yaml. Read /fondament/roles/<mapped-filename> in your session."
                        },
                        "allowed_tools": {
                            "type": "string",
                            "description": "Comma-separated Claude tool names for the spawned agent, read from the facet file's tools.always_on list (same file as the context mapping above). Native tools pass through as-is (Bash, Edit, Write); Mcp tools are formatted as mcp__<server>__<tool> (e.g. mcp__farga__search_signals). Defaults to a read-only Farga tool set if omitted."
                        },
                        "session_id": {
                            "type": "string",
                            "description": "Session identifier. The agent writes its result as a Farga Signal under this project name. Use a unique ID per invocation so you can retrieve the result."
                        },
                        "caller": {
                            "type": "string",
                            "description": "Identity of the calling agent. REQUIRED. Use 'guilhem' for the org agent; use the component name (e.g. 'farga', 'gardian') for component agents. Scope rules: guilhem may only invoke facet=architect; a component agent may only invoke its own domain."
                        }
                    },
                    "required": ["domain", "facet", "task", "session_id", "caller"]
                }
            },
            {
                "name": "get_agent_result",
                "description": "Check the status of a dispatched agent Job and retrieve its result from Farga when complete.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "job_id": {
                            "type": "string",
                            "description": "The job_id returned by invoke_agent."
                        },
                        "session_id": {
                            "type": "string",
                            "description": "The session_id used when invoking — used to read the result from Farga."
                        }
                    },
                    "required": ["job_id", "session_id"]
                }
            },
            {
                "name": "list_agent_specs",
                "description": "List all available domain/facet combinations that can be invoked.",
                "inputSchema": {
                    "type": "object",
                    "properties": {}
                }
            }
        ]
    })
}

// ── HTTP handler ──────────────────────────────────────────────────────────────

async fn handle_mcp(
    State(state): State<DispatchState>,
    Json(req): Json<JsonRpcRequest>,
) -> (StatusCode, Json<JsonRpcResponse>) {
    let id = req.id.clone();
    let result = dispatch(&state, &req.method, req.params).await;
    match result {
        Ok(v) => (StatusCode::OK, Json(JsonRpcResponse::ok(id, v))),
        Err(e) => (StatusCode::OK, Json(JsonRpcResponse::err(id, -32603, e.to_string()))),
    }
}

async fn dispatch(
    state: &DispatchState,
    method: &str,
    params: Option<Value>,
) -> anyhow::Result<Value> {
    match method {
        "initialize" => Ok(json!({
            "protocolVersion": "2024-11-05",
            "capabilities": { "tools": {} },
            "serverInfo": { "name": "dispatcher", "version": "0.1.0" }
        })),
        "tools/list" => Ok(tool_list()),
        "tools/call" => {
            let params = params.unwrap_or(Value::Null);
            let name = params["name"].as_str().unwrap_or("");
            let args = &params["arguments"];
            call_tool(state, name, args).await
        }
        "notifications/initialized" => Ok(json!({})),
        _ => anyhow::bail!("unknown method: {}", method),
    }
}

async fn call_tool(state: &DispatchState, name: &str, args: &Value) -> anyhow::Result<Value> {
    match name {
        "invoke_agent" => {
            let domain = args["domain"].as_str().unwrap_or("").to_string();
            let facet = args["facet"].as_str().unwrap_or("").to_string();
            let task = args["task"].as_str().unwrap_or("").to_string();
            let context = args["context"].as_str().unwrap_or("").to_string();
            let allowed_tools = args["allowed_tools"].as_str()
                .unwrap_or("mcp__farga__search_signals,mcp__farga__read_context")
                .to_string();
            let session_id = args["session_id"].as_str().unwrap_or("").to_string();

            anyhow::ensure!(!domain.is_empty(), "domain is required");
            anyhow::ensure!(!facet.is_empty(), "facet is required");
            anyhow::ensure!(!task.is_empty(), "task is required");
            anyhow::ensure!(!session_id.is_empty(), "session_id is required");

            let caller = args["caller"].as_str().unwrap_or("");
            anyhow::ensure!(
                !caller.is_empty(),
                "caller is required — pass caller='guilhem' (org agent) or caller='<component>' (component agent)"
            );
            if caller == "guilhem" {
                anyhow::ensure!(
                    facet == "architect",
                    "scope violation: guilhem may only invoke facet=architect (got '{}'); \
                     route work to component agents via nervi_publish instead",
                    facet
                );
            } else {
                anyhow::ensure!(
                    domain == caller,
                    "scope violation: {} may only invoke agents in its own domain (got domain='{}'); \
                     pass the puck back to Guilhem via nervi_publish if cross-component coordination is needed",
                    caller, domain
                );
            }

            let job_id = create_agent_job(
                &state.k8s,
                &domain,
                &facet,
                &task,
                &context,
                &allowed_tools,
                &session_id,
                &state.agent_image,
                &state.agents_namespace,
                &state.farga_url,
                &state.farga_mcp_url,
            )
            .await?;

            tracing::info!("spawned agent job: {} ({}/{})", job_id, domain, facet);
            Ok(text_result(format!(
                "Agent job dispatched.\njob_id: {}\nsession_id: {}\n\nPoll with get_agent_result(job_id=\"{}\", session_id=\"{}\") to check status.",
                job_id, session_id, job_id, session_id
            )))
        }

        "get_agent_result" => {
            let job_id = args["job_id"].as_str().unwrap_or("").to_string();
            let session_id = args["session_id"].as_str().unwrap_or("").to_string();
            anyhow::ensure!(!job_id.is_empty(), "job_id is required");
            anyhow::ensure!(!session_id.is_empty(), "session_id is required");

            let result = check_job_result(&state.k8s, &state.farga_url, &state.agents_namespace, &job_id, &session_id).await?;
            Ok(text_result(result))
        }

        "list_agent_specs" => {
            let specs = list_specs();
            Ok(text_result(specs))
        }

        _ => anyhow::bail!("unknown tool: {}", name),
    }
}

// ── k8s Job creation ──────────────────────────────────────────────────────────

async fn create_agent_job(
    client: &Client,
    domain: &str,
    facet: &str,
    task: &str,
    context: &str,
    allowed_tools: &str,
    session_id: &str,
    image: &str,
    namespace: &str,
    farga_url: &str,
    farga_mcp_url: &str,
) -> anyhow::Result<String> {
    let short_id = &uuid::Uuid::new_v4().to_string()[..8];
    let job_name = format!("agent-{}-{}-{}", domain, facet, short_id);

    let env = vec![
        env_val("DOMAIN", domain),
        env_val("FACET", facet),
        env_val("TASK", task),
        env_val("AGENT_CONTEXT", context),
        env_val("ALLOWED_TOOLS", allowed_tools),
        env_val("SESSION_ID", session_id),
        env_val("FARGA_URL", farga_url),
        env_val("FARGA_MCP_URL", farga_mcp_url),
        // ANTHROPIC_API_KEY from the cluster secret
        EnvVar {
            name: "ANTHROPIC_API_KEY".into(),
            value_from: Some(EnvVarSource {
                secret_key_ref: Some(SecretKeySelector {
                    name: Some("anthropic".into()),
                    key: "api-key".into(),
                    optional: Some(false),
                }),
                ..Default::default()
            }),
            ..Default::default()
        },
    ];

    let job = build_job(&job_name, domain, facet, session_id, namespace, image, env);

    let api: Api<Job> = Api::namespaced(client.clone(), namespace);
    api.create(&PostParams::default(), &job).await
        .map_err(|e| anyhow::anyhow!("k8s job create failed: {}", e))?;

    Ok(job_name)
}

fn env_val(name: &str, value: &str) -> EnvVar {
    EnvVar { name: name.into(), value: Some(value.into()), ..Default::default() }
}

/// Same OpenBao KV reads + git credential file layout as the `fetch-tokens` init
/// container in `deploy/charts/guilhem/templates/guilhem.yaml` — kept identical so
/// dispatched agents authenticate the same way Guilhem's own pod does.
const FETCH_TOKENS_SCRIPT: &str = r#"set -eu
export BAO_ADDR=http://openbao.occitan-system.svc.cluster.local:8200
GH=$(bao kv get -field=value secret/occitan/github)
GL=$(bao kv get -field=value secret/occitan/gitlab)
umask 077
cat > /creds/tokens.env <<EOF
export GH_TOKEN='$GH'
export GITHUB_TOKEN='$GH'
export GITLAB_TOKEN='$GL'
export GITLAB_PAT_TOKEN='$GL'
EOF
cat > /creds/.git-credentials <<EOF
https://x-access-token:$GH@github.com
https://oauth2:$GL@gitlab.com
EOF
cat > /creds/.gitconfig <<'EOF'
[credential]
    helper = store --file=/creds/.git-credentials
[user]
    name = Guilhem de Tudela
    email = guilhem@occitane.guilhem
[safe]
    directory = *
EOF
echo "tokens + git creds written to /creds"
"#;

fn fetch_tokens_init_container() -> Container {
    Container {
        name: "fetch-tokens".into(),
        image: Some("openbao/openbao:latest".into()),
        image_pull_policy: Some("IfNotPresent".into()),
        command: Some(vec!["/bin/sh".into(), "-c".into()]),
        args: Some(vec![FETCH_TOKENS_SCRIPT.into()]),
        env: Some(vec![EnvVar {
            name: "BAO_TOKEN".into(),
            value_from: Some(EnvVarSource {
                secret_key_ref: Some(SecretKeySelector {
                    name: Some("openbao".into()),
                    key: "token".into(),
                    optional: None,
                }),
                ..Default::default()
            }),
            ..Default::default()
        }]),
        volume_mounts: Some(vec![VolumeMount {
            name: "creds".into(),
            mount_path: "/creds".into(),
            ..Default::default()
        }]),
        ..Default::default()
    }
}

/// Builds the k8s Job spec for a dispatched domain/facet agent. Pulled out as a
/// pure function so the pod spec (in particular `image_pull_secrets`) can be
/// unit-tested without a real k8s client.
fn build_job(
    job_name: &str,
    domain: &str,
    facet: &str,
    session_id: &str,
    namespace: &str,
    image: &str,
    env: Vec<EnvVar>,
) -> Job {
    Job {
        metadata: ObjectMeta {
            name: Some(job_name.into()),
            namespace: Some(namespace.into()),
            labels: Some([
                ("app.kubernetes.io/managed-by".into(), "caissa-dispatcher".into()),
                ("caissa.io/domain".into(), domain.into()),
                ("caissa.io/facet".into(), facet.into()),
                ("caissa.io/session".into(), session_id.into()),
            ].into()),
            ..Default::default()
        },
        spec: Some(JobSpec {
            ttl_seconds_after_finished: Some(600),
            backoff_limit: Some(1),
            template: PodTemplateSpec {
                metadata: None,
                spec: Some(PodSpec {
                    restart_policy: Some("Never".into()),
                    image_pull_secrets: Some(vec![LocalObjectReference {
                        name: Some("ghcr-creds".into()),
                    }]),
                    init_containers: Some(vec![fetch_tokens_init_container()]),
                    containers: vec![Container {
                        name: "agent".into(),
                        image: Some(image.into()),
                        // AGENT_IMAGE is a floating tag (e.g. ghcr.io/miegjorn/caissa-sandbox:guilhem),
                        // not a digest. Kubernetes defaults non-":latest" tags to IfNotPresent, which
                        // would silently keep using whatever this node cached the first time any job
                        // ever pulled the tag — never picking up newer pushes under the same name.
                        image_pull_policy: Some("Always".into()),
                        env: Some(env),
                        volume_mounts: Some(vec![VolumeMount {
                            name: "creds".into(),
                            mount_path: "/creds".into(),
                            ..Default::default()
                        }]),
                        ..Default::default()
                    }],
                    volumes: Some(vec![Volume {
                        name: "creds".into(),
                        empty_dir: Some(EmptyDirVolumeSource {
                            medium: Some("Memory".into()),
                            ..Default::default()
                        }),
                        ..Default::default()
                    }]),
                    ..Default::default()
                }),
            },
            ..Default::default()
        }),
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_job_sets_ghcr_pull_secret() {
        let job = build_job(
            "agent-charradissa-infra-abc123",
            "charradissa",
            "infra",
            "session-1",
            "agents",
            "ghcr.io/miegjorn/caissa-sandbox:guilhem",
            vec![],
        );

        let pod_spec = job.spec.unwrap().template.spec.unwrap();
        let secrets = pod_spec.image_pull_secrets.expect("image_pull_secrets must be set");
        assert_eq!(secrets.len(), 1);
        assert_eq!(secrets[0].name.as_deref(), Some("ghcr-creds"));
    }

    #[test]
    fn build_job_uses_the_given_image() {
        let job = build_job(
            "agent-gardian-developer-abc123",
            "gardian",
            "developer",
            "session-2",
            "agents",
            "ghcr.io/miegjorn/caissa-sandbox:guilhem",
            vec![],
        );

        let container = &job.spec.unwrap().template.spec.unwrap().containers[0];
        assert_eq!(container.image.as_deref(), Some("ghcr.io/miegjorn/caissa-sandbox:guilhem"));
    }

    #[test]
    fn build_job_always_pulls_the_agent_image() {
        let job = build_job(
            "agent-amassada-developer-xyz789",
            "amassada",
            "developer",
            "session-5",
            "agents",
            "ghcr.io/miegjorn/caissa-sandbox:guilhem",
            vec![],
        );

        let container = &job.spec.unwrap().template.spec.unwrap().containers[0];
        assert_eq!(
            container.image_pull_policy.as_deref(),
            Some("Always"),
            "AGENT_IMAGE is a floating tag — IfNotPresent (the k8s default for non-':latest' tags) would cache stale content forever"
        );
    }

    #[test]
    fn build_job_includes_credential_init_container() {
        let job = build_job(
            "agent-amassada-developer-abc123",
            "amassada",
            "developer",
            "session-3",
            "agents",
            "ghcr.io/miegjorn/caissa-sandbox:guilhem",
            vec![],
        );

        let pod_spec = job.spec.unwrap().template.spec.unwrap();
        let init_containers = pod_spec.init_containers.expect("init_containers must be set");
        assert_eq!(init_containers.len(), 1);
        assert_eq!(init_containers[0].name, "fetch-tokens");
        assert_eq!(init_containers[0].image.as_deref(), Some("openbao/openbao:latest"));

        let volumes = pod_spec.volumes.expect("volumes must be set");
        assert!(volumes.iter().any(|v| v.name == "creds"), "expected a 'creds' volume");

        let agent_container = &pod_spec.containers[0];
        let mounts = agent_container.volume_mounts.as_ref().expect("agent container must mount creds");
        assert!(mounts.iter().any(|m| m.name == "creds" && m.mount_path == "/creds"));
    }

    #[test]
    fn build_job_init_container_reads_openbao_token_secret() {
        let job = build_job(
            "agent-farga-developer-def456",
            "farga",
            "developer",
            "session-4",
            "agents",
            "ghcr.io/miegjorn/caissa-sandbox:guilhem",
            vec![],
        );

        let pod_spec = job.spec.unwrap().template.spec.unwrap();
        let init_container = &pod_spec.init_containers.unwrap()[0];
        let env = init_container.env.as_ref().expect("init container must have env");
        let bao_token = env.iter().find(|e| e.name == "BAO_TOKEN").expect("BAO_TOKEN env var must be set");
        let secret_ref = bao_token.value_from.as_ref().unwrap().secret_key_ref.as_ref().unwrap();
        assert_eq!(secret_ref.name.as_deref(), Some("openbao"));
        assert_eq!(secret_ref.key, "token");
    }
}

// ── Job result polling ────────────────────────────────────────────────────────

async fn check_job_result(
    client: &Client,
    farga_url: &str,
    namespace: &str,
    job_id: &str,
    session_id: &str,
) -> anyhow::Result<String> {
    let api: Api<Job> = Api::namespaced(client.clone(), namespace);

    let job = api.get(job_id).await
        .map_err(|e| anyhow::anyhow!("k8s job get failed ({}): {}", job_id, e))?;

    let status = job.status.unwrap_or_default();
    let succeeded = status.succeeded.unwrap_or(0);
    let failed = status.failed.unwrap_or(0);
    let active = status.active.unwrap_or(0);

    if succeeded > 0 {
        // Job done — read result from Farga
        let signals = fetch_signals(farga_url, session_id).await?;
        Ok(format!("status: completed\n\n{}", signals))
    } else if failed > 0 {
        Ok(format!("status: failed (check pod logs: kubectl logs -n {} -l job-name={})", namespace, job_id))
    } else if active > 0 {
        Ok("status: running".into())
    } else {
        Ok("status: pending".into())
    }
}

async fn fetch_signals(farga_url: &str, project: &str) -> anyhow::Result<String> {
    let url = format!("{}/signals/recent?project={}", farga_url, project);
    let resp = reqwest::get(&url).await?;
    if !resp.status().is_success() {
        return Ok(format!("(no signals found for session {})", project));
    }
    let signals: Vec<serde_json::Value> = resp.json().await.unwrap_or_default();
    if signals.is_empty() {
        return Ok(format!("(no signals yet for session {})", project));
    }
    Ok(signals.iter()
        .filter_map(|s| s["content"].as_str())
        .collect::<Vec<_>>()
        .join("\n\n---\n\n"))
}

// ── Agent spec catalog ────────────────────────────────────────────────────────

fn list_specs() -> String {
    let domains = ["occitan", "farga", "gardian", "amassada", "charradissa", "cor", "caissa", "fondament"];
    let facets = ["architect", "developer", "qa", "infra", "db", "security"];

    let mut lines = vec!["Available domain/facet combinations:\n".to_string()];
    for domain in domains {
        for facet in facets {
            lines.push(format!("  {}/{}", domain, facet));
        }
    }
    lines.push("\nLoad domain context from /fondament/domains/<domain>.yaml.\nFacet filenames under /fondament/roles/ do not match the facet keyword above —\nuse this mapping: developer->developer.yaml, infra->infra-engineer.yaml,\nqa->qa-engineer.yaml, security->security-analyst.yaml, architect->app-architect.yaml,\ndb->data-architect.yaml. Read the facet file's tools.always_on list and pass it\nas invoke_agent's allowed_tools (comma-separated tool names).".into());
    lines.join("\n")
}

// ── Entry point ───────────────────────────────────────────────────────────────

pub async fn run(port: u16) -> anyhow::Result<()> {
    let agent_image = std::env::var("AGENT_IMAGE")
        .unwrap_or_else(|_| "caissa-sandbox:guilhem".into());
    let agents_namespace = std::env::var("AGENTS_NAMESPACE")
        .unwrap_or_else(|_| "agents".into());
    let farga_url = std::env::var("FARGA_URL")
        .unwrap_or_else(|_| "http://farga.occitan-system.svc.cluster.local:7500".into());
    let farga_mcp_url = std::env::var("FARGA_MCP_URL")
        .unwrap_or_else(|_| "http://farga.occitan-system.svc.cluster.local:7500/mcp".into());

    let k8s = Client::try_default().await
        .map_err(|e| anyhow::anyhow!("k8s client init failed: {}", e))?;

    tracing::info!("dispatcher starting on :{}", port);
    tracing::info!("agent image: {}", agent_image);
    tracing::info!("agents namespace: {}", agents_namespace);
    tracing::info!("farga: {}", farga_url);

    let state = DispatchState {
        k8s: Arc::new(k8s),
        agent_image,
        agents_namespace,
        farga_url,
        farga_mcp_url,
    };

    let app = Router::new()
        .route("/mcp", post(handle_mcp))
        .route("/health", axum::routing::get(|| async { "ok" }))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(format!("0.0.0.0:{}", port)).await?;
    axum::serve(listener, app).await?;
    Ok(())
}
