use super::*;

pub(crate) struct SidecarProcess {
    pub(crate) child: tokio::process::Child,
    pub(crate) stdin: tokio::process::ChildStdin,
    pub(crate) stdout: tokio::io::BufReader<tokio::process::ChildStdout>,
}

impl SidecarProcess {
    pub(crate) async fn spawn(init: &SidecarInit) -> anyhow::Result<Self> {
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

    pub(crate) async fn send(&mut self, room_id: &str, sender: &str, content: &str) -> anyhow::Result<String> {
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

    pub(crate) fn kill(&mut self) {
        let _ = self.child.start_kill();
    }

    /// Returns true if the child process is still running. `try_wait()`
    /// returns `Ok(None)` while alive, `Ok(Some(_))` once it has exited, and
    /// `Err` if the OS-level status check itself fails — in that case we
    /// treat the process as dead (safer to respawn than keep using
    /// something we can't verify).
    pub(crate) fn is_alive(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }
}

#[derive(serde::Serialize)]
pub(crate) struct SidecarInit {
    #[serde(rename = "systemPrompt")]
    pub(crate) system_prompt: String,
    pub(crate) model: String,
    #[serde(rename = "allowedTools")]
    pub(crate) allowed_tools: Vec<String>,
    pub(crate) skills: Vec<String>,
    #[serde(rename = "mcpServers")]
    pub(crate) mcp_servers: serde_json::Value,
    /// Extended-thinking token budget for the aporia discipline (Occitan
    /// per-agent-matrix-independence follow-up: aporia is now the default
    /// reasoning mode for these 9 agents, not opt-in). `None` when the
    /// agent's Fondament definition doesn't declare the `aporia` modifier —
    /// omitted entirely from the JSON in that case so agent-sidecar.js's
    /// default (no extended thinking) is unchanged for those.
    #[serde(rename = "maxThinkingTokens", skip_serializing_if = "Option::is_none")]
    pub(crate) max_thinking_tokens: Option<u32>,
}

/// One room's live session: the running sidecar child process, and when it
/// last handled a message.
///
/// `process` is behind its own `Arc<Mutex>` so the outer map lock can be
/// released before the Claude API call. Different rooms run in parallel;
/// two messages for the same room serialise on the per-room Mutex.
/// `last_activity` is updated under the outer map lock so the idle reaper
/// can inspect it without touching the inner Mutex.
pub(crate) struct RoomSession {
    pub(crate) process: std::sync::Arc<tokio::sync::Mutex<SidecarProcess>>,
    pub(crate) last_activity: std::time::Instant,
    /// Whether this room's agent runs under the aporia discipline — fixed
    /// for the life of the session (a property of the agent's Fondament
    /// definition, resolved once at spawn), reused on every turn to decide
    /// whether to build a composed-parts preamble in build_turn_context_block.
    pub(crate) is_aporia: bool,
}

impl RoomSession {
    #[cfg(test)]
    pub(crate) fn for_test(last_activity: std::time::Instant) -> Self {
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
            is_aporia: false,
        }
    }

    pub(crate) fn is_idle(&self, timeout: std::time::Duration) -> bool {
        self.last_activity.elapsed() >= timeout
    }
}

pub(crate) async fn spawn_idle_reaper(room_sessions: Arc<tokio::sync::Mutex<HashMap<String, RoomSession>>>) {
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

#[cfg(test)]
mod session_supervisor_tests {
    use super::*;
    use crate::commands::listen::*;
    use std::time::{Duration, Instant};

    #[test]
    pub(crate) fn room_session_is_not_idle_when_recently_active() {
        let session = RoomSession::for_test(Instant::now());
        assert!(!session.is_idle(Duration::from_secs(1800)));
    }

    #[test]
    pub(crate) fn room_session_is_idle_after_timeout_elapsed() {
        let session = RoomSession::for_test(Instant::now() - Duration::from_secs(1801));
        assert!(session.is_idle(Duration::from_secs(1800)));
    }

    #[test]
    pub(crate) fn sidecar_process_is_not_alive_after_child_exits() {
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

