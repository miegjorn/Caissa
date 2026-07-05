use super::*;


// ── Code scan + doc reconciliation ───────────────────────────────────────────

/// POST /trigger/scan — weekly CronJob per component agent.
///
/// Clones the component's GitHub repo, inspects code for TODOs / unimplemented
/// stubs / doc drift, deduplicates against open GitHub issues, creates new
/// issues for gaps found, optionally opens a README PR so Cartulari picks it
/// up, and writes a Farga signal summarising the run.
pub(crate) async fn handle_scan(
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

pub(crate) async fn run_scan(state: &ListenState) -> anyhow::Result<()> {
    let mcp_config = format!(
        r#"{{"mcpServers":{{"farga":{{"type":"http","url":"{}"}}}}}}"#,
        state.farga_mcp_url
    );
    let mcp_path = std::env::temp_dir().join("caissa-scan-mcp.json");
    std::fs::write(&mcp_path, &mcp_config)?;

    let prompt = build_scan_prompt(&state.farga_project);

    let output = tokio::process::Command::new("claude")
        .prefer_oauth_over_api_key()
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

pub(crate) fn build_scan_prompt(component: &str) -> String {
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

// ── Active dispatch — post-dream routing ─────────────────────────────────────
//
// POST /trigger/dispatch — CronJob fires 1h after the dream (04:00 UTC daily).
//
// Guilhem reads the dream report and challenge signals from Farga, classifies
// each item using the system-defence risk table, and dispatches actionable
// Class 1/2 items to the responsible component agents via Matrix. Class 3 items
// are surfaced to Pierre-Luc. Class 4 items are rejected with a Farga signal.

pub(crate) async fn handle_dispatch(
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

pub(crate) async fn run_dispatch(state: &ListenState) -> anyhow::Result<()> {
    let mcp_config = serde_json::to_string(&serde_json::json!({
        "mcpServers": agent_mcp_servers(state)
    }))?;
    let mcp_path = std::env::temp_dir().join("guilhem-dispatch-mcp.json");
    std::fs::write(&mcp_path, &mcp_config)?;

    let prompt = build_dispatch_prompt(&state.fondament_path);

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

pub(crate) fn build_dispatch_prompt(fondament_path: &str) -> String {
    format!(
        r###"You are Guilhem de Tudela, Steward of the Occitan stack — its triager, dispatcher, reviewer, merger, and SRE.

The nightly dream has completed. Your job now is to translate the dream's synthesis and
adversarial challenge proposals into specific, actionable work — and route it to the right
component agents. This is where synthesis becomes motion.

{constraint}

---

## STEP 1 — Load system-defence axioms and context graph

`curl -s http://fondament.occitan-system.svc.cluster.local:7800/file/fondament/system-defence.md`

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
// Self-paced weekly (Monday 05:00 UTC) via the tick-poller (Task 2/6)
// publishing a `PerceivedMessage::Tick { skill: "mission-pulse" }` on this
// component's tick subject, perceived by chat_loop::run_tick_stream and
// routed here directly -- no HTTP trigger anymore.
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

pub(crate) async fn run_mission_pulse(state: &ListenState) -> anyhow::Result<()> {
    let mcp_config = serde_json::to_string(&serde_json::json!({
        "mcpServers": agent_mcp_servers(state)
    }))?;
    let mcp_path = std::env::temp_dir().join("guilhem-mission-pulse-mcp.json");
    std::fs::write(&mcp_path, &mcp_config)?;

    let prompt = build_mission_pulse_prompt(&state.fondament_path);
    let tools = agent_allowed_tools(&state.fondament_url, state).await.join(",");

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

pub(crate) fn build_mission_pulse_prompt(fondament_path: &str) -> String {
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

---

## STEP 6 — Re-arm your own next tick

Near the end of this run, PUT your next scheduled wake to Farga's KV store:

curl -X PUT {{farga_url}}/kv/schedule/guilhem__mission-pulse \
  -H "Content-Type: application/json" \
  -d '{{"value": {{"next_due": "<ISO8601 UTC, your own judgment -- default roughly
  one week out (matching mission-pulse's original weekly cadence) if nothing
  suggests otherwise>", "note": "<one line on why this interval>"}}, "ttl_seconds": 2592000}}'

Use the actual {{farga_url}} value from your own environment (FARGA_URL), not the
literal string above.
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

pub(crate) async fn handle_intake(
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

pub(crate) async fn run_intake(state: &ListenState, description: &str) -> anyhow::Result<()> {
    let mcp_config = serde_json::to_string(&serde_json::json!({
        "mcpServers": agent_mcp_servers(state)
    }))?;
    let mcp_path = std::env::temp_dir().join("guilhem-intake-mcp.json");
    std::fs::write(&mcp_path, &mcp_config)?;

    let prompt = build_intake_prompt(&state.fondament_path, description);
    let tools = agent_allowed_tools(&state.fondament_url, state).await.join(",");

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

pub(crate) fn build_intake_prompt(fondament_path: &str, description: &str) -> String {
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


#[cfg(test)]
mod self_pace_tests {
    use super::*;

    #[test]
    fn build_mission_pulse_prompt_rearms_its_own_schedule_key() {
        let prompt = build_mission_pulse_prompt("/fondament");
        assert!(prompt.contains("/kv/schedule/guilhem__mission-pulse"));
    }
}
