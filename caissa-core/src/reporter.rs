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
