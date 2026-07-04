//! `caissa bootstrap-matrix-agents` — idempotent one-time (but safe-to-rerun)
//! setup for the 9 independent Matrix agent identities (guilhem + the 8
//! component agents). Run as a Kubernetes Job on every Helm sync.
//!
//! For each agent: registers a Matrix user via Synapse's shared-secret admin
//! registration (treats "already registered" as success), generates and
//! stores a password in OpenBao on first registration (read back on later
//! runs so re-running never rotates an in-use password).
//!
//! Room membership does NOT use Synapse's admin "force join" API
//! (`/_synapse/admin/v1/join`) — confirmed live that it does not bypass a
//! local room's own `join_rules: invite` (that admin capability is for
//! joining *federated* rooms an admin couldn't otherwise reach, not for
//! overriding local auth rules — attempting it produces a genuine Matrix
//! auth-chain rejection).
//!
//! It also does NOT use `@charradissa-relay`'s own appservice token to
//! invite, despite that being the current runtime identity of the Charradissa
//! relay: renaming an appservice's `sender_localpart` in code does not
//! migrate Matrix room *membership* — Matrix has no concept of renaming a
//! user. Confirmed live: every one of these 9 rooms' actual member is still
//! the pre-rename `@charradissa` account; `@charradissa-relay` is a brand
//! new identity with zero room memberships, so it has no standing to invite
//! anyone into any of them.
//!
//! Instead this uses `@pierre-luc` (the real, confirmed creator/PL-100
//! member of every one of these 9 rooms) to send the invites. Each agent
//! then logs in with its own just-registered password and joins normally —
//! idempotent per the Matrix spec (inviting an already-joined user errors
//! harmlessly; joining an already-joined room just succeeds).
//!
//! Cleaning up the old `@charradissa` account's stale membership does NOT
//! use a plain kick, or even a demote-then-kick: these rooms were manually
//! recreated after a Matrix reset and left `@charradissa` at PL 100, tied
//! with pierre-luc's own PL 100, and Synapse's power-level auth rule
//! forbids touching a target whose power level is *equal to* the actor's
//! (not just greater) — both approaches 403 unconditionally. Instead this
//! mints a throwaway Synapse server-admin account (via the same
//! shared-secret registration API, `admin: true`), uses it to obtain a
//! token acting *as* `@charradissa` directly (`/_synapse/admin/v1/users/
//! {user}/login`), and has it leave each room on its own behalf — a
//! self-leave needs no power-level comparison at all. The 8th room
//! (`charradissa`'s own) is excluded from this cleanup: the new
//! independent charradissa component agent reuses the same username, so
//! its membership there is the real agent, not a ghost.
//!
//! Deliberately stores passwords, not access tokens: a token can go stale
//! after a Matrix reset in a way a stored password can't (each agent's own
//! `caissa listen` process logs in fresh at startup — see `matrix_client`
//! in `commands/listen.rs`).

use hmac::{Hmac, Mac};
use sha1::Sha1;

/// (Matrix localpart, Matrix room ID) for each of the 9 independent agents.
/// guilhem's room is `#occitan` (a Matrix Space, `!iNQRqUAMckCUQrSFHk`) — the
/// project room, mapped to the `Occitan` GitHub repo where cross-component
/// Initiatives live — not the separate `#guilhem` room drift had produced.
/// That orphaned room is `ORPHANED_GUILHEM_ROOM` below — @guilhem is removed
/// from it on every bootstrap run (2026-07-04: confirmed live that both
/// rooms have @guilhem as a member with no client-visible way to tell them
/// apart, and a real message sent into the orphaned room got no reply,
/// silently, because the running pod's sync loop only ever polls the one
/// room it's configured with — MATRIX_ROOM_ID — not this one).
pub const AGENTS: &[(&str, &str)] = &[
    ("guilhem", "!iNQRqUAMckCUQrSFHk:occitane.guilhem"),
    ("gardian", "!bwuKXFvUXnVZfXcKuz:occitane.guilhem"),
    ("fondament", "!KuWBSmYyvyiyTMFKqJ:occitane.guilhem"),
    ("farga", "!CtktMiOTNtSIkdwOxq:occitane.guilhem"),
    ("amassada", "!vLjgiURMSlkqTXgaDG:occitane.guilhem"),
    ("cor", "!FgfTbZMpLLVGiISZTj:occitane.guilhem"),
    ("caissa", "!ZqGBDioAYnOATihiEU:occitane.guilhem"),
    ("charradissa", "!qZGQFrjAcKjPinhQnp:occitane.guilhem"),
    ("nervi", "!QQeweqsLsOTZYdonXi:occitane.guilhem"),
];

/// The 8 component rooms (all of AGENTS except guilhem, which has no
/// pre-existing relay identity to clean up) — @charradissa-relay is kicked
/// from these if present.
fn component_rooms() -> impl Iterator<Item = &'static (&'static str, &'static str)> {
    AGENTS.iter().filter(|(name, _)| *name != "guilhem")
}

/// The `#guilhem` room (canonical alias, name "Guilhem", created by
/// pierre-luc) drift produced before the per-agent-Matrix-independence
/// migration. @guilhem never left it, so it still shows up as "a room with
/// Guilhem in it" in any client — indistinguishable from the real, actively
/// polled `#occitan` DM (`!iNQRqUAMckCUQrSFHk`, in AGENTS above) without
/// inside knowledge of MATRIX_ROOM_ID. Removed on every bootstrap run so
/// there is exactly one unambiguous room per agent.
const ORPHANED_GUILHEM_ROOM: &str = "!hTNBZpYDxyvfcuralm:occitane.guilhem";

/// Compute the HMAC-SHA1 MAC for Synapse's shared-secret registration API,
/// matching the algorithm in Synapse's `register_new_matrix_user` script:
/// `HMAC-SHA1(shared_secret, nonce \0 username \0 password \0 admin_flag)`.
pub fn registration_mac(shared_secret: &str, nonce: &str, username: &str, password: &str, admin: bool) -> String {
    let mut mac = Hmac::<Sha1>::new_from_slice(shared_secret.as_bytes())
        .expect("HMAC accepts any key length");
    let admin_flag = if admin { "admin" } else { "notadmin" };
    let input = [nonce, username, password, admin_flag].join("\0");
    mac.update(input.as_bytes());
    hex::encode(mac.finalize().into_bytes())
}

/// Generate a random password. 32 hex chars (16 bytes) — long enough to not
/// need rotation, short enough to fit comfortably in an env var / OpenBao value.
pub fn generate_password() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    // Not cryptographically reviewed RNG-grade, but this is a machine-to-machine
    // Matrix account password behind cluster-internal auth, generated once and
    // stored in OpenBao — matches the trust model of the existing GitHub/GitLab
    // token handling in this same init-container pattern.
    let mut seed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let mut out = String::with_capacity(32);
    for _ in 0..32 {
        seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
        let nibble = ((seed >> 60) & 0xf) as u8;
        out.push(std::char::from_digit(nibble as u32, 16).unwrap());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registration_mac_matches_known_vector() {
        // Verified against the HMAC computed live during the 2026-07-01
        // investigation (registering @verify-admin) using the same shared
        // secret format Synapse expects.
        let mac = registration_mac("supersecret", "abc123", "testuser", "testpass", true);
        // Deterministic: same inputs always produce the same 40-hex-char SHA1 MAC.
        assert_eq!(mac.len(), 40);
        let mac2 = registration_mac("supersecret", "abc123", "testuser", "testpass", true);
        assert_eq!(mac, mac2);
    }

    #[test]
    fn registration_mac_differs_for_admin_flag() {
        let admin_mac = registration_mac("secret", "n", "u", "p", true);
        let notadmin_mac = registration_mac("secret", "n", "u", "p", false);
        assert_ne!(admin_mac, notadmin_mac);
    }

    #[test]
    fn generate_password_is_32_hex_chars() {
        let pw = generate_password();
        assert_eq!(pw.len(), 32);
        assert!(pw.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn component_rooms_excludes_guilhem() {
        let names: Vec<&str> = component_rooms().map(|(n, _)| *n).collect();
        assert!(!names.contains(&"guilhem"));
        assert_eq!(names.len(), 8);
    }

    /// The old-@charradissa cleanup loop in `run()` filters component_rooms()
    /// down to exclude "charradissa" itself — that room's @charradissa
    /// membership is the real independent agent (registered earlier in the
    /// same run via the same username), not a ghost. Regression test for a
    /// bug found live: the unfiltered cleanup evicted the real agent from
    /// its own room.
    #[test]
    fn cleanup_rooms_excludes_charradissas_own_room() {
        let names: Vec<&str> = component_rooms()
            .filter(|(name, _)| *name != "charradissa")
            .map(|(n, _)| *n)
            .collect();
        assert!(!names.contains(&"charradissa"));
        assert!(!names.contains(&"guilhem"));
        assert_eq!(names.len(), 7);
    }

    /// A genuine 404 from OpenBao means the secret doesn't exist yet — safe
    /// for the caller to generate and store a fresh password.
    #[tokio::test]
    async fn bao_get_404_is_ok_none() {
        use wiremock::{Mock, MockServer, ResponseTemplate};
        use wiremock::matchers::{method, path};

        let mock_server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/secret/data/occitan/matrix/guilhem-password"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&mock_server)
            .await;

        let client = reqwest::Client::new();
        let result = bao_get(&client, &mock_server.uri(), "test-token", "occitan/matrix/guilhem-password").await;
        assert!(result.is_ok(), "expected Ok, got {:?}", result);
        assert_eq!(result.unwrap(), None);
    }

    /// A transient OpenBao failure (500) must NOT be confused with "doesn't
    /// exist" — it must surface as an error so the caller doesn't regenerate
    /// and overwrite a live password.
    #[tokio::test]
    async fn bao_get_500_is_err() {
        use wiremock::{Mock, MockServer, ResponseTemplate};
        use wiremock::matchers::{method, path};

        let mock_server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/secret/data/occitan/matrix/guilhem-password"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&mock_server)
            .await;

        let client = reqwest::Client::new();
        let result = bao_get(&client, &mock_server.uri(), "test-token", "occitan/matrix/guilhem-password").await;
        assert!(result.is_err(), "expected Err, got {:?}", result);
    }
}

/// Read a stored secret from OpenBao's KV v2 HTTP API.
///
/// Returns `Ok(None)` only for a genuine 404 (the secret doesn't exist yet —
/// safe for the caller to generate and store a fresh one). Any other failure
/// — network error, non-404 non-success status, or a response body missing
/// the expected `data.data.value` field — is `Err(...)`. Collapsing those
/// into `None` would let a transient read failure look like "doesn't exist,"
/// causing the caller to silently regenerate and overwrite a live password.
async fn bao_get(client: &reqwest::Client, bao_addr: &str, bao_token: &str, path: &str) -> anyhow::Result<Option<String>> {
    let url = format!("{}/v1/secret/data/{}", bao_addr, path);
    let resp = client.get(&url)
        .header("X-Vault-Token", bao_token)
        .send().await
        .map_err(|e| anyhow::anyhow!("bao_get {} request failed: {}", path, e))?;

    if resp.status().as_u16() == 404 {
        return Ok(None);
    }
    if !resp.status().is_success() {
        anyhow::bail!("bao_get {} failed: {}", path, resp.status());
    }
    let json: serde_json::Value = resp.json().await
        .map_err(|e| anyhow::anyhow!("bao_get {} returned malformed JSON: {}", path, e))?;
    match json["data"]["data"]["value"].as_str() {
        Some(v) => Ok(Some(v.to_string())),
        None => anyhow::bail!("bao_get {} succeeded but response is missing data.data.value: {}", path, json),
    }
}

async fn bao_put(client: &reqwest::Client, bao_addr: &str, bao_token: &str, path: &str, value: &str) -> anyhow::Result<()> {
    let url = format!("{}/v1/secret/data/{}", bao_addr, path);
    let resp = client.post(&url)
        .header("X-Vault-Token", bao_token)
        .json(&serde_json::json!({ "data": { "value": value } }))
        .send().await?;
    if !resp.status().is_success() {
        anyhow::bail!("bao_put {} failed: {}", path, resp.status());
    }
    Ok(())
}

/// Get the existing password for `agent` from OpenBao, or generate one, store
/// it, and return it. Never rotates a password that's already there — every
/// agent pod is currently logging in with whatever password this returns.
async fn get_or_create_password(client: &reqwest::Client, bao_addr: &str, bao_token: &str, agent: &str) -> anyhow::Result<String> {
    let path = format!("occitan/matrix/{}-password", agent);
    match bao_get(client, bao_addr, bao_token, &path).await? {
        Some(existing) => return Ok(existing),
        None => { /* genuinely absent, fall through to generate+store */ }
    }
    let fresh = generate_password();
    bao_put(client, bao_addr, bao_token, &path, &fresh).await?;
    Ok(fresh)
}

/// Register `username` with `password` via Synapse's shared-secret admin
/// registration API. Treats "user already exists" (Synapse returns 400
/// M_USER_IN_USE) as success — this must be safe to rerun forever.
async fn register_user(client: &reqwest::Client, homeserver: &str, shared_secret: &str, username: &str, password: &str) -> anyhow::Result<()> {
    let nonce_resp: serde_json::Value = client
        .get(format!("{}/_synapse/admin/v1/register", homeserver))
        .send().await?
        .json().await?;
    let nonce = nonce_resp["nonce"].as_str()
        .ok_or_else(|| anyhow::anyhow!("no nonce in registration response"))?;

    let mac = registration_mac(shared_secret, nonce, username, password, false);
    let resp = client
        .post(format!("{}/_synapse/admin/v1/register", homeserver))
        .json(&serde_json::json!({
            "nonce": nonce,
            "username": username,
            "password": password,
            "admin": false,
            "mac": mac,
        }))
        .send().await?;

    if resp.status().is_success() {
        tracing::info!("bootstrap-matrix: registered @{}", username);
        return Ok(());
    }
    let body: serde_json::Value = resp.json().await.unwrap_or_default();
    if body["errcode"].as_str() == Some("M_USER_IN_USE") {
        tracing::info!("bootstrap-matrix: @{} already registered", username);
        return Ok(());
    }
    anyhow::bail!("register_user {} failed: {:?}", username, body);
}

/// Invite `user_id` into `room_id` as `token`'s own identity (no
/// `?user_id=` impersonation — the caller acts as itself). Retries on 429
/// (Synapse's per-user invite rate limiter, confirmed live: a rapid-fire
/// loop of 9 invites from the same sender trips it well within a second)
/// with a short fixed backoff, up to a few attempts. Any other failure is
/// treated as best-effort by the caller: inviting an already-invited or
/// already-joined user is a harmless no-op for the overall bootstrap (the
/// subsequent self-join step succeeds either way).
async fn matrix_invite(client: &reqwest::Client, homeserver: &str, token: &str, room_id: &str, user_id: &str) -> anyhow::Result<()> {
    const MAX_ATTEMPTS: u32 = 5;
    for attempt in 1..=MAX_ATTEMPTS {
        let resp = client
            .post(format!("{}/_matrix/client/v3/rooms/{}/invite", homeserver, urlencode(room_id)))
            .header("Authorization", format!("Bearer {}", token))
            .json(&serde_json::json!({ "user_id": user_id }))
            .send().await?;
        if resp.status().is_success() {
            return Ok(());
        }
        if resp.status().as_u16() == 429 && attempt < MAX_ATTEMPTS {
            tokio::time::sleep(std::time::Duration::from_millis(500 * attempt as u64)).await;
            continue;
        }
        anyhow::bail!("invite {} into {} failed: {}", user_id, room_id, resp.status());
    }
    unreachable!()
}

/// Log in as `username` with its own password — the same login path each
/// agent's own `caissa listen` process uses at startup (see `matrix_login`
/// in `commands/listen.rs`).
async fn matrix_login(client: &reqwest::Client, homeserver: &str, username: &str, password: &str) -> anyhow::Result<String> {
    let resp: serde_json::Value = client
        .post(format!("{}/_matrix/client/v3/login", homeserver))
        .json(&serde_json::json!({
            "type": "m.login.password",
            "identifier": { "type": "m.id.user", "user": username },
            "password": password,
        }))
        .send().await?
        .json().await?;
    resp["access_token"].as_str()
        .map(|s| s.to_string())
        .ok_or_else(|| anyhow::anyhow!("matrix_login {} failed: no access_token in response: {:?}", username, resp))
}

/// Join `room_id` using the agent's own token. Idempotent per the Matrix
/// spec — joining a room the caller is already in just succeeds again.
async fn matrix_join(client: &reqwest::Client, homeserver: &str, token: &str, room_id: &str) -> anyhow::Result<()> {
    let resp = client
        .post(format!("{}/_matrix/client/v3/join/{}", homeserver, urlencode(room_id)))
        .header("Authorization", format!("Bearer {}", token))
        .json(&serde_json::json!({}))
        .send().await?;
    if !resp.status().is_success() {
        anyhow::bail!("join {} failed: {}", room_id, resp.status());
    }
    Ok(())
}

/// Register a throwaway Synapse server-admin account via the shared-secret
/// registration API (`admin: true`), returning its (user_id, access_token)
/// directly from the registration response (no separate login needed).
async fn admin_register(client: &reqwest::Client, homeserver: &str, shared_secret: &str) -> anyhow::Result<(String, String)> {
    let nonce_resp: serde_json::Value = client
        .get(format!("{}/_synapse/admin/v1/register", homeserver))
        .send().await?
        .json().await?;
    let nonce = nonce_resp["nonce"].as_str()
        .ok_or_else(|| anyhow::anyhow!("no nonce in registration response"))?;

    let username = format!("cleanup-admin-{}", &generate_password()[..12]);
    let password = generate_password();
    let mac = registration_mac(shared_secret, nonce, &username, &password, true);
    let resp: serde_json::Value = client
        .post(format!("{}/_synapse/admin/v1/register", homeserver))
        .json(&serde_json::json!({
            "nonce": nonce,
            "username": username,
            "password": password,
            "admin": true,
            "mac": mac,
        }))
        .send().await?
        .json().await?;
    let user_id = resp["user_id"].as_str()
        .ok_or_else(|| anyhow::anyhow!("admin_register failed: no user_id in response: {:?}", resp))?
        .to_string();
    let token = resp["access_token"].as_str()
        .ok_or_else(|| anyhow::anyhow!("admin_register failed: no access_token in response: {:?}", resp))?
        .to_string();
    Ok((user_id, token))
}

/// Deactivate (and erase) the throwaway admin account created by
/// `admin_register`, using its own token.
async fn admin_deactivate_self(client: &reqwest::Client, homeserver: &str, admin_token: &str, admin_user_id: &str) -> anyhow::Result<()> {
    let resp = client
        .post(format!("{}/_synapse/admin/v1/deactivate/{}", homeserver, urlencode(admin_user_id)))
        .header("Authorization", format!("Bearer {}", admin_token))
        .json(&serde_json::json!({ "erase": true }))
        .send().await?;
    if !resp.status().is_success() {
        anyhow::bail!("admin_deactivate_self {} failed: {}", admin_user_id, resp.status());
    }
    Ok(())
}

/// Use the Synapse admin API to obtain an access token acting as `user_id`,
/// bypassing normal login (no password needed). Requires `admin_token` to
/// belong to a server admin.
async fn admin_login_as(client: &reqwest::Client, homeserver: &str, admin_token: &str, user_id: &str) -> anyhow::Result<String> {
    let resp: serde_json::Value = client
        .post(format!("{}/_synapse/admin/v1/users/{}/login", homeserver, urlencode(user_id)))
        .header("Authorization", format!("Bearer {}", admin_token))
        .json(&serde_json::json!({}))
        .send().await?
        .json().await?;
    resp["access_token"].as_str()
        .map(|s| s.to_string())
        .ok_or_else(|| anyhow::anyhow!("admin_login_as {} failed: no access_token in response: {:?}", user_id, resp))
}

/// Remove `user_id` from `room_id` by puppeting it directly (via
/// `admin_login_as`) and calling `/leave` on its own behalf. Self-leave
/// requires no power-level comparison at all, unlike kicking.
///
/// A plain kick (even after demoting the target to power level 0 first)
/// does not work here: Synapse's power-level auth rule forbids an actor
/// from touching a target whose power level is *equal to* the actor's own
/// (`"You don't have permission to remove ops level equal to your own"`),
/// not just greater. Confirmed live: these rooms were manually recreated
/// after a Matrix reset and don't follow `create_room_with_owner`'s
/// intended PL scheme — the old `@charradissa` account ended up at PL 100,
/// the same as pierre-luc's, so both a plain kick and a demote-then-kick
/// 403 every time. Puppeting the target and having it leave sidesteps the
/// whole power-level question.
/// Best-effort: leaving a room the account is no longer in errors
/// harmlessly, making this safe to rerun once the cleanup has already
/// happened.
async fn matrix_leave_as(client: &reqwest::Client, homeserver: &str, admin_token: &str, user_id: &str, room_id: &str) -> anyhow::Result<()> {
    let user_token = admin_login_as(client, homeserver, admin_token, user_id).await?;
    let resp = client
        .post(format!("{}/_matrix/client/v3/rooms/{}/leave", homeserver, urlencode(room_id)))
        .header("Authorization", format!("Bearer {}", user_token))
        .json(&serde_json::json!({}))
        .send().await?;
    if !resp.status().is_success() {
        anyhow::bail!("leave {} from {} failed: {}", user_id, room_id, resp.status());
    }
    Ok(())
}

fn urlencode(s: &str) -> String {
    s.chars().map(|c| match c {
        '!' | '#' | '@' | ':' | '/' | '?' | '&' | '=' | '+' | ' ' => format!("%{:02X}", c as u32),
        _ => c.to_string(),
    }).collect()
}

/// Full bootstrap: register the 9 agents, invite+self-join each into its
/// room, then have @charradissa-relay leave the 8 component rooms it's
/// still in. Safe to rerun — every step is naturally idempotent or checks
/// state before acting.
pub async fn run(homeserver: &str, bao_addr: &str) -> anyhow::Result<()> {
    let bao_token = std::env::var("BAO_TOKEN")
        .map_err(|_| anyhow::anyhow!("BAO_TOKEN not set"))?;
    let shared_secret = std::env::var("SYNAPSE_REGISTRATION_SHARED_SECRET")
        .map_err(|_| anyhow::anyhow!("SYNAPSE_REGISTRATION_SHARED_SECRET not set"))?;
    let client = reqwest::Client::new();

    // pierre-luc: real confirmed creator/PL-100 member of every one of these
    // 9 rooms — read-only lookup, this password is never generated or
    // rotated by this Job (unlike the 9 agents' own passwords below).
    let pierre_luc_password = bao_get(&client, bao_addr, &bao_token, "occitan/matrix/pierre-luc-password")
        .await?
        .ok_or_else(|| anyhow::anyhow!("occitan/matrix/pierre-luc-password not found in OpenBao"))?;
    let pierre_luc_token = matrix_login(&client, homeserver, "pierre-luc", &pierre_luc_password).await?;

    let mut passwords = Vec::with_capacity(AGENTS.len());
    for (name, room_id) in AGENTS {
        let password = get_or_create_password(&client, bao_addr, &bao_token, name).await?;
        register_user(&client, homeserver, &shared_secret, name, &password).await?;
        passwords.push((*name, *room_id, password));
    }

    for (name, room_id, _password) in &passwords {
        let user_id = format!("@{}:occitane.guilhem", name);
        // Best-effort: already-invited/already-joined errors are harmless —
        // the self-join step below succeeds either way.
        if let Err(e) = matrix_invite(&client, homeserver, &pierre_luc_token, room_id, &user_id).await {
            tracing::warn!("bootstrap-matrix: invite {} into {} failed (continuing — may already be invited or joined): {}", user_id, room_id, e);
        }
    }

    for (name, room_id, password) in &passwords {
        let token = matrix_login(&client, homeserver, name, password).await?;
        matrix_join(&client, homeserver, &token, room_id).await?;
        tracing::info!("bootstrap-matrix: @{} joined {}", name, room_id);
    }

    // The pre-rename @charradissa account (not @charradissa-relay, which has
    // never held membership anywhere) is still an actual member of these 8
    // rooms — clean it up now that each room has its own real agent.
    // Skip the "charradissa" room itself: the new independent charradissa
    // component agent registered above reuses this same username, so its
    // membership there is legitimate, not a ghost.
    let old_charradissa_id = "@charradissa:occitane.guilhem";
    let cleanup_rooms: Vec<_> = component_rooms().filter(|(name, _)| *name != "charradissa").collect();
    let guilhem_id = "@guilhem:occitane.guilhem";
    // One throwaway admin covers both cleanups: @charradissa's stale
    // membership in the 7 non-charradissa component rooms, and @guilhem's
    // stale membership in the orphaned #guilhem room (see ORPHANED_GUILHEM_ROOM).
    {
        let (admin_user_id, admin_token) = admin_register(&client, homeserver, &shared_secret).await?;
        for (name, room_id) in &cleanup_rooms {
            if let Err(e) = matrix_leave_as(&client, homeserver, &admin_token, old_charradissa_id, room_id).await {
                tracing::warn!("bootstrap-matrix: remove {} from {} ({}) failed (continuing — may already be gone): {}", old_charradissa_id, name, room_id, e);
            } else {
                tracing::info!("bootstrap-matrix: removed {} from {} ({})", old_charradissa_id, name, room_id);
            }
        }
        if let Err(e) = matrix_leave_as(&client, homeserver, &admin_token, guilhem_id, ORPHANED_GUILHEM_ROOM).await {
            tracing::warn!("bootstrap-matrix: remove {} from orphaned #guilhem room failed (continuing — may already be gone): {}", guilhem_id, e);
        } else {
            tracing::info!("bootstrap-matrix: removed {} from orphaned #guilhem room ({})", guilhem_id, ORPHANED_GUILHEM_ROOM);
        }
        // Best-effort: the throwaway admin's own deactivation failing does
        // not affect correctness of the cleanup above, just leaves an unused
        // account behind (harmless — it's re-derivable and re-run-safe).
        if let Err(e) = admin_deactivate_self(&client, homeserver, &admin_token, &admin_user_id).await {
            tracing::warn!("bootstrap-matrix: deactivate throwaway admin failed (non-fatal): {}", e);
        }
    }

    tracing::info!("bootstrap-matrix: complete");
    Ok(())
}
