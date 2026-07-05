//! Shared tick-poller: one small persistent process replacing the former
//! per-agent-per-skill k8s CronJobs for chronicle/dream/mission-pulse.
//! Polls Farga's `/kv/schedule` namespace (every live entry across all
//! agents and skills, one HTTP call -- Farga's `GET /kv/*path` falls back
//! to namespace listing when the path isn't an exact key match) and, for
//! each entry whose `next_due` has passed, publishes a Tick onto that
//! component's tick subject (corrier_core::tick_subject) and deletes the
//! entry so it doesn't re-fire until the skill's own next `schedule_tick`
//! write re-arms it.
//!
//! A periodic skill (Task 6's occitan/amassada, and the existing
//! chronicle/dream/mission-pulse prompts, Task 3) re-arms its own next
//! wake near the end of its run by writing (via a plain `curl`, no new
//! tool -- see Task 6):
//!   PUT {farga_url}/kv/schedule/<component>__<skill>
//!   { "value": { "next_due": "<iso8601 UTC>", "note": "<why>" },
//!     "ttl_seconds": 2592000 }
//! The 30-day TTL is a dead-pod safety net (Farga eventually forgets a
//! schedule nobody ever re-arms), not the scheduling mechanism itself --
//! `next_due` comparison is what actually decides "is this due now".

use corrier_core::{tick_subject, PerceivedMessage};
use serde::Deserialize;
use std::time::Duration;

/// `component__skill` -- the flat key stored under Farga's `schedule`
/// namespace (matches `routing.rs`'s convention of one flat namespace per
/// concern, e.g. `/kv/routing/<room_id>`).
fn schedule_kv_key(component: &str, skill: &str) -> String {
    format!("{}__{}", component, skill)
}

fn parse_schedule_key(key: &str) -> Option<(String, String)> {
    let (component, skill) = key.split_once("__")?;
    Some((component.to_string(), skill.to_string()))
}

fn is_due(next_due_iso8601: &str) -> bool {
    match chrono::DateTime::parse_from_rfc3339(next_due_iso8601) {
        Ok(due) => due.with_timezone(&chrono::Utc) <= chrono::Utc::now(),
        Err(_) => false,
    }
}

#[derive(Deserialize)]
struct ScheduleEntryValue {
    next_due: String,
}

#[derive(Deserialize)]
struct KvListEntry {
    key: String,
    value: ScheduleEntryValue,
}

pub async fn run(nats_url: &str, farga_url: &str, poll_interval_secs: u64) -> anyhow::Result<()> {
    let nervi = nervi_core::NerviClient::connect(nats_url).await?;
    let client = reqwest::Client::new();

    tracing::info!("tick-poller starting -- polling {} every {}s", farga_url, poll_interval_secs);

    loop {
        if let Err(e) = poll_once(&client, &nervi, farga_url).await {
            tracing::warn!("tick-poller: poll cycle failed (non-fatal, retrying next interval): {}", e);
        }
        tokio::time::sleep(Duration::from_secs(poll_interval_secs)).await;
    }
}

async fn poll_once(
    client: &reqwest::Client,
    nervi: &nervi_core::NerviClient,
    farga_url: &str,
) -> anyhow::Result<()> {
    let url = format!("{}/kv/schedule", farga_url.trim_end_matches('/'));
    let resp = client.get(&url).send().await?;

    if resp.status() == reqwest::StatusCode::NOT_FOUND {
        return Ok(()); // no schedule entries exist yet -- nothing due
    }
    if !resp.status().is_success() {
        anyhow::bail!("farga /kv/schedule returned {}", resp.status());
    }

    let entries: Vec<KvListEntry> = resp.json().await?;

    for entry in entries {
        let Some((component, skill)) = parse_schedule_key(&entry.key) else {
            tracing::warn!("tick-poller: malformed schedule key '{}', skipping", entry.key);
            continue;
        };

        if !is_due(&entry.value.next_due) {
            continue;
        }

        let subject = tick_subject(&component, &skill);
        let payload = serde_json::to_string(&PerceivedMessage::Tick { skill: skill.clone() })?;

        if let Err(e) = nervi
            .publish(nervi_core::client::PublishOptions {
                subject: subject.clone(),
                payload,
                qualifier: Some("info".to_string()),
                timestamp: Some(chrono::Utc::now().to_rfc3339()),
            })
            .await
        {
            tracing::error!("tick-poller: publish to {} failed: {}", subject, e);
            continue; // leave the KV entry in place -- retry next interval
        }

        let delete_url = format!("{}/kv/schedule/{}", farga_url.trim_end_matches('/'), entry.key);
        if let Err(e) = client.delete(&delete_url).send().await {
            tracing::warn!("tick-poller: delete of fired entry {} failed (non-fatal): {}", entry.key, e);
        }

        tracing::info!("tick-poller: fired {}/{} tick", component, skill);
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schedule_kv_key_is_double_underscore_joined() {
        assert_eq!(schedule_kv_key("guilhem", "dream"), "guilhem__dream");
    }

    #[test]
    fn parse_schedule_key_splits_component_and_skill() {
        assert_eq!(
            parse_schedule_key("guilhem__dream"),
            Some(("guilhem".to_string(), "dream".to_string()))
        );
    }

    #[test]
    fn parse_schedule_key_rejects_malformed_input() {
        assert_eq!(parse_schedule_key("no-separator-here"), None);
    }

    #[test]
    fn is_due_compares_against_now() {
        let past = chrono::Utc::now() - chrono::Duration::hours(1);
        let future = chrono::Utc::now() + chrono::Duration::hours(1);
        assert!(is_due(&past.to_rfc3339()));
        assert!(!is_due(&future.to_rfc3339()));
    }

    #[test]
    fn is_due_treats_unparseable_timestamp_as_not_due() {
        // A malformed next_due must never crash the poller loop -- skip it,
        // don't panic, and don't treat garbage as "always fire".
        assert!(!is_due("not-a-timestamp"));
    }
}
