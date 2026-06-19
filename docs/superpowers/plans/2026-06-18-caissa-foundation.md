# Caissa Foundation Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Scaffold the Caissa container toolbox from scratch — PiiProxy, SidecarReporter, sandbox CLI wrapper, Dockerfile.

**Architecture:** Rust workspace with caissa-core (PiiProxy + SidecarReporter traits/impls) and caissa-cli (sandbox + report commands). No server — caissa runs as a CLI sidecar.

**Tech Stack:** Rust, clap, reqwest, regex, tokio. New project at `/Users/bedardpl/project/Caissa`.

---

## File Map

Files to create:

```
Caissa/
  Cargo.toml                          workspace root
  caissa.toml.example                 example config
  Dockerfile                          multi-stage build
  .gitignore
  caissa-core/
    Cargo.toml
    src/
      lib.rs                          re-exports modules
      config.rs                       CaissaConfig + SandboxConfig
      pii.rs                          PiiProxy trait + RegexPiiProxy impl + tests
      reporter.rs                     ToolInvocation + SidecarReporter + tests
  caissa-cli/
    Cargo.toml
    src/
      main.rs                         clap entrypoint
      commands/
        mod.rs
        sandbox.rs                    docker run wrapper + test
        report.rs                     Farga reachability check
```

---

## Task 1: Scaffold Workspace + caissa-core Skeleton

**Files:**
- Create: `Cargo.toml` (workspace root)
- Create: `caissa-core/Cargo.toml`
- Create: `caissa-core/src/lib.rs`
- Create: `caissa-core/src/config.rs`

---

- [ ] **Step 1.1: Create workspace `Cargo.toml`**

File: `/Users/bedardpl/project/Caissa/Cargo.toml`

```toml
[workspace]
members = ["caissa-core", "caissa-cli"]
resolver = "2"

[workspace.dependencies]
tokio = { version = "1", features = ["full"] }
serde = { version = "1", features = ["derive"] }
serde_json = "1"
anyhow = "1"
thiserror = "1"
reqwest = { version = "0.12", features = ["json"] }
regex = "1"
chrono = { version = "0.4", features = ["serde"] }
clap = { version = "4", features = ["derive"] }
tracing = "0.1"
```

- [ ] **Step 1.2: Create `caissa-core/Cargo.toml`**

File: `/Users/bedardpl/project/Caissa/caissa-core/Cargo.toml`

```toml
[package]
name = "caissa-core"
version = "0.1.0"
edition = "2021"

[dependencies]
tokio = { workspace = true }
serde = { workspace = true }
serde_json = { workspace = true }
anyhow = { workspace = true }
thiserror = { workspace = true }
reqwest = { workspace = true }
regex = { workspace = true }
chrono = { workspace = true }
tracing = { workspace = true }
```

- [ ] **Step 1.3: Create `caissa-core/src/lib.rs`**

File: `/Users/bedardpl/project/Caissa/caissa-core/src/lib.rs`

```rust
pub mod config;
pub mod pii;
pub mod reporter;
```

- [ ] **Step 1.4: Create `caissa-core/src/config.rs`**

File: `/Users/bedardpl/project/Caissa/caissa-core/src/config.rs`

```rust
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct CaissaConfig {
    pub farga_url: String,
    pub project: String,
    /// Named patterns to activate: "email", "phone", "ssn", "credit_card"
    pub pii_patterns: Vec<String>,
}

impl Default for CaissaConfig {
    fn default() -> Self {
        Self {
            farga_url: "http://localhost:7500".into(),
            project: "default".into(),
            pii_patterns: vec!["email".into(), "phone".into()],
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SandboxConfig {
    pub image: String,
    pub network: String,
    pub memory_limit: String,
    pub cpu_limit: Option<String>,
}

impl Default for SandboxConfig {
    fn default() -> Self {
        Self {
            image: "caissa-sandbox:latest".into(),
            network: "none".into(),
            memory_limit: "2g".into(),
            cpu_limit: None,
        }
    }
}
```

- [ ] **Step 1.5: Verify the workspace compiles (lib.rs modules not yet filled — stubs below)**

The `pii` and `reporter` modules are declared but not yet created; rustc will error with "file not found." Create empty placeholder files now so `cargo check` passes before the next task fills them in:

File: `/Users/bedardpl/project/Caissa/caissa-core/src/pii.rs`

```rust
// placeholder — filled in Task 2
```

File: `/Users/bedardpl/project/Caissa/caissa-core/src/reporter.rs`

```rust
// placeholder — filled in Task 3
```

Run:

```bash
cd /Users/bedardpl/project/Caissa && cargo check -p caissa-core
```

Expected output contains: `Finished` with no errors (warnings about unused imports are fine at this stage).

- [ ] **Step 1.6: Commit scaffold**

```bash
cd /Users/bedardpl/project/Caissa
git add Cargo.toml caissa-core/
git commit -m "chore: scaffold Caissa workspace + caissa-core skeleton"
```

---

## Task 2: PiiProxy — Trait + RegexPiiProxy Implementation

**Files:**
- Modify: `caissa-core/src/pii.rs` (replace placeholder)

---

- [ ] **Step 2.1: Write the failing tests first**

Replace `/Users/bedardpl/project/Caissa/caissa-core/src/pii.rs` with:

```rust
use std::collections::HashMap;
use regex::Regex;

// ─── Trait ───────────────────────────────────────────────────────────────────

pub trait PiiProxy: Send + Sync {
    /// Returns (redacted_text, vault) where vault maps placeholder → original.
    fn redact(&self, text: &str) -> (String, HashMap<String, String>);
}

// ─── RegexPiiProxy ───────────────────────────────────────────────────────────

pub struct RegexPiiProxy {
    patterns: Vec<(Regex, String)>, // (compiled pattern, placeholder prefix)
}

impl RegexPiiProxy {
    pub fn new(named_patterns: &[String]) -> anyhow::Result<Self> {
        let mut patterns = Vec::new();
        for name in named_patterns {
            let (re, prefix) = match name.as_str() {
                "email" => (
                    Regex::new(r"[a-zA-Z0-9._%+\-]+@[a-zA-Z0-9.\-]+\.[a-zA-Z]{2,}")?,
                    "EMAIL",
                ),
                "phone" => (
                    Regex::new(r"\b(?:\+?1[-.\s]?)?\(?\d{3}\)?[-.\s]?\d{3}[-.\s]?\d{4}\b")?,
                    "PHONE",
                ),
                "ssn" => (
                    Regex::new(r"\b\d{3}-\d{2}-\d{4}\b")?,
                    "SSN",
                ),
                "credit_card" => (
                    Regex::new(r"\b(?:\d[ -]?){13,16}\b")?,
                    "CREDIT_CARD",
                ),
                _ => continue,
            };
            patterns.push((re, prefix.into()));
        }
        Ok(Self { patterns })
    }
}

impl PiiProxy for RegexPiiProxy {
    fn redact(&self, text: &str) -> (String, HashMap<String, String>) {
        let mut result = text.to_string();
        let mut vault: HashMap<String, String> = HashMap::new();
        let mut counter = 0u32;

        for (re, prefix) in &self.patterns {
            // Collect match spans before mutating the string.
            let matches: Vec<(usize, usize, String)> = re
                .find_iter(&result)
                .map(|m| (m.start(), m.end(), m.as_str().to_string()))
                .collect();

            // Replace in reverse order so earlier indices remain valid.
            for (start, end, original) in matches.into_iter().rev() {
                let placeholder = format!("[{}_{}]", prefix, counter);
                vault.insert(placeholder.clone(), original);
                result.replace_range(start..end, &placeholder);
                counter += 1;
            }
        }

        (result, vault)
    }
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redacts_email_address() {
        let proxy = RegexPiiProxy::new(&["email".to_string()]).unwrap();
        let (redacted, vault) = proxy.redact("contact me at user@example.com please");
        assert!(
            !redacted.contains("user@example.com"),
            "email should be redacted; got: {redacted}"
        );
        assert!(
            vault.values().any(|v| v == "user@example.com"),
            "vault should contain original email"
        );
    }

    #[test]
    fn redacts_phone_number() {
        let proxy = RegexPiiProxy::new(&["phone".to_string()]).unwrap();
        let (redacted, vault) = proxy.redact("call 555-123-4567 now");
        assert!(
            !redacted.contains("555-123-4567"),
            "phone should be redacted; got: {redacted}"
        );
        assert!(
            vault.values().any(|v| v == "555-123-4567"),
            "vault should contain original phone"
        );
    }

    #[test]
    fn redacts_ssn() {
        let proxy = RegexPiiProxy::new(&["ssn".to_string()]).unwrap();
        let (redacted, vault) = proxy.redact("my SSN is 123-45-6789.");
        assert!(!redacted.contains("123-45-6789"), "SSN should be redacted; got: {redacted}");
        assert!(vault.values().any(|v| v == "123-45-6789"));
    }

    #[test]
    fn no_false_positives_on_clean_text() {
        let proxy =
            RegexPiiProxy::new(&["email".to_string(), "phone".to_string()]).unwrap();
        let text = "This is a normal sentence with no PII.";
        let (redacted, vault) = proxy.redact(text);
        assert_eq!(redacted, text, "clean text should be unchanged");
        assert!(vault.is_empty(), "vault should be empty for clean text");
    }

    #[test]
    fn multiple_emails_in_one_string() {
        let proxy = RegexPiiProxy::new(&["email".to_string()]).unwrap();
        let (redacted, vault) =
            proxy.redact("alice@a.com and bob@b.com are contacts");
        assert!(!redacted.contains("alice@a.com"));
        assert!(!redacted.contains("bob@b.com"));
        assert_eq!(vault.len(), 2);
    }

    #[test]
    fn unknown_pattern_name_is_silently_skipped() {
        let proxy = RegexPiiProxy::new(&["nonexistent".to_string()]).unwrap();
        let (redacted, vault) = proxy.redact("hello world");
        assert_eq!(redacted, "hello world");
        assert!(vault.is_empty());
    }
}
```

- [ ] **Step 2.2: Run the tests — expect them to pass (implementation is written above)**

```bash
cd /Users/bedardpl/project/Caissa && cargo test -p caissa-core --lib pii
```

Expected: all 6 tests pass. If any fail, debug the regex or replace_range logic before moving on.

- [ ] **Step 2.3: Commit**

```bash
cd /Users/bedardpl/project/Caissa
git add caissa-core/src/pii.rs
git commit -m "feat: PiiProxy trait + RegexPiiProxy with email/phone/ssn/credit_card patterns"
```

---

## Task 3: SidecarReporter — Buffer + Farga Flush

**Files:**
- Modify: `caissa-core/src/reporter.rs` (replace placeholder)

---

- [ ] **Step 3.1: Replace reporter.rs with full implementation + tests**

File: `/Users/bedardpl/project/Caissa/caissa-core/src/reporter.rs`

```rust
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

// ─── ToolInvocation ──────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolInvocation {
    pub tool: String,
    pub input_tokens: u32,
    pub output_tokens: u32,
    pub cost_usd: f64,
    pub timestamp: DateTime<Utc>,
}

// ─── SidecarReporter ─────────────────────────────────────────────────────────

pub struct SidecarReporter {
    farga_url: String,
    project: String,
    buffer: Mutex<Vec<ToolInvocation>>,
    client: reqwest::Client,
}

impl SidecarReporter {
    pub fn new(farga_url: String, project: String) -> Self {
        Self {
            farga_url,
            project,
            buffer: Mutex::new(Vec::new()),
            client: reqwest::Client::new(),
        }
    }

    /// Append one invocation to the in-memory buffer.
    pub async fn record(&self, inv: ToolInvocation) {
        self.buffer.lock().await.push(inv);
    }

    /// Drain the buffer and POST to Farga's /signals endpoint.
    /// No-ops silently when the buffer is empty.
    pub async fn flush(&self) -> anyhow::Result<()> {
        let invocations: Vec<ToolInvocation> = {
            let mut buf = self.buffer.lock().await;
            std::mem::take(&mut *buf)
        };

        if invocations.is_empty() {
            return Ok(());
        }

        let total_cost: f64 = invocations.iter().map(|i| i.cost_usd).sum();
        let total_tokens: u32 =
            invocations.iter().map(|i| i.input_tokens + i.output_tokens).sum();
        let total_invocations = invocations.len();

        let content = serde_json::to_string(&serde_json::json!({
            "invocations": invocations,
            "summary": {
                "total_invocations": total_invocations,
                "total_tokens": total_tokens,
                "total_cost_usd": total_cost,
            }
        }))?;

        let url = format!("{}/signals", self.farga_url);
        self.client
            .post(&url)
            .json(&serde_json::json!({
                "project": self.project,
                "signals": [{
                    "project": self.project,
                    "content": content,
                    "source": "caissa-sidecar"
                }]
            }))
            .send()
            .await?
            .error_for_status()?;

        Ok(())
    }

    /// Returns the number of buffered invocations (for testing).
    pub async fn buffered_count(&self) -> usize {
        self.buffer.lock().await.len()
    }
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn make_invocation(tool: &str) -> ToolInvocation {
        ToolInvocation {
            tool: tool.into(),
            input_tokens: 100,
            output_tokens: 50,
            cost_usd: 0.001,
            timestamp: Utc::now(),
        }
    }

    #[tokio::test]
    async fn record_appends_to_buffer() {
        let reporter =
            SidecarReporter::new("http://localhost:7500".into(), "test".into());
        reporter.record(make_invocation("bash")).await;
        assert_eq!(reporter.buffered_count().await, 1);
        reporter.record(make_invocation("read")).await;
        assert_eq!(reporter.buffered_count().await, 2);
    }

    #[tokio::test]
    async fn flush_on_empty_buffer_is_noop() {
        // flush() must not panic or error when buffer is empty.
        // We can't call the real endpoint, so we only test the empty-buffer
        // early-return path (no network required).
        let reporter =
            SidecarReporter::new("http://localhost:7500".into(), "test".into());
        // Empty buffer → should return Ok(()) without any network call.
        let result = reporter.flush().await;
        assert!(result.is_ok(), "flush on empty buffer should succeed");
    }

    #[tokio::test]
    async fn flush_drains_buffer() {
        // We cannot reach a real Farga in unit tests.
        // Verify that a non-empty buffer is drained even when the HTTP call
        // would fail. We do this by checking that the buffer is empty after
        // the (expected-to-fail) flush attempt.
        let reporter =
            SidecarReporter::new("http://127.0.0.1:1".into(), "test".into()); // port 1 = unreachable
        reporter.record(make_invocation("bash")).await;
        assert_eq!(reporter.buffered_count().await, 1);

        // flush() drains the buffer before the network call, so even on
        // network failure the buffer is empty afterward.
        let _ = reporter.flush().await; // error expected — ignore it
        assert_eq!(
            reporter.buffered_count().await,
            0,
            "buffer must be drained regardless of network outcome"
        );
    }

    #[test]
    fn tool_invocation_round_trips_json() {
        let inv = ToolInvocation {
            tool: "write".into(),
            input_tokens: 200,
            output_tokens: 80,
            cost_usd: 0.002,
            timestamp: Utc::now(),
        };
        let json = serde_json::to_string(&inv).unwrap();
        let back: ToolInvocation = serde_json::from_str(&json).unwrap();
        assert_eq!(back.tool, "write");
        assert_eq!(back.input_tokens, 200);
    }
}
```

- [ ] **Step 3.2: Run caissa-core tests**

```bash
cd /Users/bedardpl/project/Caissa && cargo test -p caissa-core
```

Expected: all tests pass. The `flush_drains_buffer` test makes a connection attempt to `127.0.0.1:1` which will fail at the network layer — that is intentional and handled by `let _ = reporter.flush().await`.

- [ ] **Step 3.3: Commit**

```bash
cd /Users/bedardpl/project/Caissa
git add caissa-core/src/reporter.rs
git commit -m "feat: SidecarReporter — buffered ToolInvocation + Farga flush"
```

---

## Task 4: caissa-cli — Clap Entrypoint + Commands

**Files:**
- Create: `caissa-cli/Cargo.toml`
- Create: `caissa-cli/src/main.rs`
- Create: `caissa-cli/src/commands/mod.rs`
- Create: `caissa-cli/src/commands/sandbox.rs`
- Create: `caissa-cli/src/commands/report.rs`

---

- [ ] **Step 4.1: Create `caissa-cli/Cargo.toml`**

File: `/Users/bedardpl/project/Caissa/caissa-cli/Cargo.toml`

```toml
[package]
name = "caissa-cli"
version = "0.1.0"
edition = "2021"

[[bin]]
name = "caissa"
path = "src/main.rs"

[dependencies]
caissa-core = { path = "../caissa-core" }
tokio = { workspace = true }
clap = { workspace = true }
anyhow = { workspace = true }
serde_json = { workspace = true }
reqwest = { workspace = true }
tracing = { workspace = true }
tracing-subscriber = "0.3"
```

- [ ] **Step 4.2: Create `caissa-cli/src/main.rs`**

File: `/Users/bedardpl/project/Caissa/caissa-cli/src/main.rs`

```rust
mod commands;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "caissa", about = "Caissa container toolbox — PII proxy + sandbox runner")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Run a command inside the Caissa sandbox container.
    Sandbox {
        /// Arguments forwarded verbatim to `docker run <image> <args...>`.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Check reachability of the Farga signals endpoint.
    Report {
        #[arg(long, default_value = "http://localhost:7500")]
        farga_url: String,
        #[arg(long, default_value = "default")]
        project: String,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();
    let cli = Cli::parse();
    match cli.command {
        Commands::Sandbox { args } => commands::sandbox::run(&args).await,
        Commands::Report { farga_url, project } => {
            commands::report::run(&farga_url, &project).await
        }
    }
}
```

- [ ] **Step 4.3: Create `caissa-cli/src/commands/mod.rs`**

File: `/Users/bedardpl/project/Caissa/caissa-cli/src/commands/mod.rs`

```rust
pub mod report;
pub mod sandbox;
```

- [ ] **Step 4.4: Create `caissa-cli/src/commands/sandbox.rs`**

File: `/Users/bedardpl/project/Caissa/caissa-cli/src/commands/sandbox.rs`

```rust
use caissa_core::config::SandboxConfig;

/// Build the `docker run` argument list from a SandboxConfig + user args.
/// Extracted so it can be tested without spawning a real process.
pub fn build_docker_args(config: &SandboxConfig, user_args: &[String]) -> Vec<String> {
    let mut args = vec![
        "run".into(),
        "--rm".into(),
        "--network".into(),
        config.network.clone(),
        "-m".into(),
        config.memory_limit.clone(),
    ];
    if let Some(ref cpus) = config.cpu_limit {
        args.push("--cpus".into());
        args.push(cpus.clone());
    }
    args.push(config.image.clone());
    args.extend_from_slice(user_args);
    args
}

pub async fn run(user_args: &[String]) -> anyhow::Result<()> {
    let config = SandboxConfig::default();
    let docker_args = build_docker_args(&config, user_args);

    let status = tokio::process::Command::new("docker")
        .args(&docker_args)
        .status()
        .await?;

    if !status.success() {
        anyhow::bail!("sandbox exited with status: {}", status);
    }
    Ok(())
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_docker_args_includes_network_and_memory() {
        let config = SandboxConfig::default();
        let args = build_docker_args(&config, &[]);
        assert!(args.contains(&"--network".to_string()));
        assert!(args.contains(&"none".to_string()), "default network should be none");
        assert!(args.contains(&"-m".to_string()));
        assert!(args.contains(&"2g".to_string()), "default memory should be 2g");
    }

    #[test]
    fn build_docker_args_includes_image() {
        let config = SandboxConfig::default();
        let args = build_docker_args(&config, &[]);
        assert!(
            args.contains(&"caissa-sandbox:latest".to_string()),
            "image must appear in args"
        );
    }

    #[test]
    fn build_docker_args_appends_user_args() {
        let config = SandboxConfig::default();
        let user_args = vec!["echo".to_string(), "hello".to_string()];
        let args = build_docker_args(&config, &user_args);
        let last_two: Vec<_> = args.iter().rev().take(2).rev().cloned().collect();
        assert_eq!(last_two, vec!["echo", "hello"]);
    }

    #[test]
    fn build_docker_args_includes_cpu_limit_when_set() {
        let config = SandboxConfig {
            cpu_limit: Some("1.5".into()),
            ..SandboxConfig::default()
        };
        let args = build_docker_args(&config, &[]);
        assert!(args.contains(&"--cpus".to_string()));
        assert!(args.contains(&"1.5".to_string()));
    }

    #[test]
    fn build_docker_args_omits_cpu_flag_when_none() {
        let config = SandboxConfig::default(); // cpu_limit = None
        let args = build_docker_args(&config, &[]);
        assert!(!args.contains(&"--cpus".to_string()));
    }
}
```

- [ ] **Step 4.5: Create `caissa-cli/src/commands/report.rs`**

File: `/Users/bedardpl/project/Caissa/caissa-cli/src/commands/report.rs`

```rust
pub async fn run(farga_url: &str, project: &str) -> anyhow::Result<()> {
    let client = reqwest::Client::new();
    let url = format!("{}/signals/recent?project={}&since=1h", farga_url, project);
    let resp = client.get(&url).send().await?;
    println!(
        "Farga at {} — status: {} (project: {})",
        farga_url,
        resp.status(),
        project
    );
    Ok(())
}
```

- [ ] **Step 4.6: Run all tests**

```bash
cd /Users/bedardpl/project/Caissa && cargo test
```

Expected: all tests pass. Check for:
- `caissa_core::pii::tests` — 6 tests
- `caissa_core::reporter::tests` — 4 tests
- `caissa_cli::commands::sandbox::tests` — 5 tests

- [ ] **Step 4.7: Verify the binary compiles and help flag works**

```bash
cd /Users/bedardpl/project/Caissa && cargo build -p caissa-cli && ./target/debug/caissa --help
```

Expected output contains: `caissa container toolbox` and subcommands `sandbox` and `report`.

- [ ] **Step 4.8: Commit**

```bash
cd /Users/bedardpl/project/Caissa
git add caissa-cli/
git commit -m "feat: caissa-cli — sandbox + report commands"
```

---

## Task 5: Dockerfile + caissa.toml.example + .gitignore + Git Init

**Files:**
- Create: `Dockerfile`
- Create: `caissa.toml.example`
- Create: `.gitignore`

---

- [ ] **Step 5.1: Create `Dockerfile`**

File: `/Users/bedardpl/project/Caissa/Dockerfile`

```dockerfile
FROM rust:1.80-slim AS builder
WORKDIR /app
COPY . .
RUN cargo build --release -p caissa-cli

FROM debian:bookworm-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*
COPY --from=builder /app/target/release/caissa /usr/local/bin/caissa
ENTRYPOINT ["caissa"]
```

- [ ] **Step 5.2: Create `caissa.toml.example`**

File: `/Users/bedardpl/project/Caissa/caissa.toml.example`

```toml
[caissa]
farga_url = "http://localhost:7500"
project = "my-project"
pii_patterns = ["email", "phone", "ssn"]

[sandbox]
image = "caissa-sandbox:latest"
network = "none"
memory_limit = "2g"
# cpu_limit = "1.5"
```

- [ ] **Step 5.3: Create `.gitignore`**

File: `/Users/bedardpl/project/Caissa/.gitignore`

```
/target
Cargo.lock
.env
caissa.toml
```

- [ ] **Step 5.4: Initialize git repo and make initial commit**

```bash
cd /Users/bedardpl/project/Caissa
git init
git add -A
git commit -m "feat: initial Caissa project — PiiProxy, SidecarReporter, sandbox CLI"
```

Note: if the repo is already initialized (there is already a README.md), skip `git init` and just stage + commit:

```bash
cd /Users/bedardpl/project/Caissa
git add -A
git commit -m "feat: initial Caissa project — PiiProxy, SidecarReporter, sandbox CLI"
```

- [ ] **Step 5.5: Create remotes and push**

```bash
gh repo create bedardpl/Caissa --private --source=. --remote=github
git remote add origin git@gitlab.com:cor912026/caissa.git
git push -u origin main
git push -u github main
```

If the GitLab remote already exists or the `gh` call errors, check with `git remote -v` and adjust accordingly.

---

## Self-Review

**Spec coverage check:**

| Requirement | Task |
|---|---|
| Workspace `Cargo.toml` with two members | Task 1, Step 1.1 |
| `caissa-core/Cargo.toml` | Task 1, Step 1.2 |
| `caissa-core/src/lib.rs` | Task 1, Step 1.3 |
| `caissa-core/src/config.rs` — `CaissaConfig` + `SandboxConfig` | Task 1, Step 1.4 |
| `caissa-core/src/pii.rs` — `PiiProxy` trait + `RegexPiiProxy` | Task 2, Step 2.1 |
| Unit tests for `RegexPiiProxy` | Task 2, Step 2.1 |
| `caissa-core/src/reporter.rs` — `ToolInvocation` + `SidecarReporter` | Task 3, Step 3.1 |
| Unit tests for `SidecarReporter` | Task 3, Step 3.1 |
| `caissa-cli/Cargo.toml` | Task 4, Step 4.1 |
| `caissa-cli/src/main.rs` — clap entrypoint | Task 4, Step 4.2 |
| `caissa-cli/src/commands/mod.rs` | Task 4, Step 4.3 |
| `caissa-cli/src/commands/sandbox.rs` — docker run wrapper | Task 4, Step 4.4 |
| `caissa-cli/src/commands/report.rs` — Farga reachability | Task 4, Step 4.5 |
| `Dockerfile` | Task 5, Step 5.1 |
| `caissa.toml.example` | Task 5, Step 5.2 |
| `.gitignore` | Task 5, Step 5.3 |
| git init + push to GitLab + GitHub | Task 5, Step 5.4–5.5 |

**Placeholder scan:** No TBD, TODO, or "similar to Task N" references found.

**Type consistency:**
- `SandboxConfig` defined in `config.rs` (Task 1) and imported via `caissa_core::config::SandboxConfig` in `sandbox.rs` (Task 4). Field names `network`, `memory_limit`, `cpu_limit`, `image` are consistent across all references.
- `ToolInvocation` defined in `reporter.rs` (Task 3) and used only in `reporter.rs` tests — no cross-task type drift.
- `build_docker_args` defined and tested in the same file — no cross-file reference.
