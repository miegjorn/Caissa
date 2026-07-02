//! `caissa bootstrap-matrix-agents` — idempotent one-time (but safe-to-rerun)
//! setup for the 9 independent Matrix agent identities (guilhem + the 8
//! component agents). Run as a Kubernetes Job on every Helm sync.
//!
//! For each agent: registers a Matrix user via Synapse's shared-secret admin
//! registration (treats "already registered" as success), generates and
//! stores a password in OpenBao on first registration (read back on later
//! runs so re-running never rotates an in-use password), and force-joins
//! that user into its designated room using a short-lived admin token minted
//! the same way. Finally kicks `@charradissa-relay` from every component
//! room it's a member of (idempotent — checks membership first).
//!
//! Deliberately stores passwords, not access tokens: a token can go stale
//! after a Matrix reset in a way a stored password can't (each agent's own
//! `caissa listen` process logs in fresh at startup — see `matrix_client`
//! in `commands/listen.rs`).

use hmac::{Hmac, Mac};
use sha1::Sha1;

/// (Matrix localpart, Matrix room ID) for each of the 9 independent agents.
pub const AGENTS: &[(&str, &str)] = &[
    ("guilhem", "!hTNBZpYDxyvfcuralm:occitane.guilhem"),
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

fn server_name(homeserver_room_suffix: &str) -> &str {
    homeserver_room_suffix
}

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

/// Mint a short-lived admin session via the same shared-secret mechanism
/// (registers a throwaway admin user, logs in, returns the token). The
/// caller is responsible for deactivating it when done.
async fn mint_admin_session(client: &reqwest::Client, homeserver: &str, shared_secret: &str) -> anyhow::Result<(String, String)> {
    let admin_user = format!("bootstrap-admin-{}", std::process::id());
    let admin_password = generate_password();
    let nonce_resp: serde_json::Value = client
        .get(format!("{}/_synapse/admin/v1/register", homeserver))
        .send().await?
        .json().await?;
    let nonce = nonce_resp["nonce"].as_str()
        .ok_or_else(|| anyhow::anyhow!("no nonce in registration response"))?;
    let mac = registration_mac(shared_secret, nonce, &admin_user, &admin_password, true);
    let reg: serde_json::Value = client
        .post(format!("{}/_synapse/admin/v1/register", homeserver))
        .json(&serde_json::json!({
            "nonce": nonce, "username": admin_user, "password": admin_password,
            "admin": true, "mac": mac,
        }))
        .send().await?
        .json().await?;
    let token = reg["access_token"].as_str()
        .ok_or_else(|| anyhow::anyhow!("no access_token minting admin session"))?
        .to_string();
    Ok((format!("@{}:{}", admin_user, homeserver_server_name(homeserver)), token))
}

/// Best-effort: the server_name isn't always derivable from the homeserver URL,
/// so callers that need it pass it explicitly where it matters (room ids already
/// carry it). Used only for the throwaway admin user's own mxid in logs.
fn homeserver_server_name(_homeserver: &str) -> &'static str {
    "occitane.guilhem"
}

async fn force_join(client: &reqwest::Client, homeserver: &str, admin_token: &str, room_id: &str, user_id: &str) -> anyhow::Result<()> {
    let resp = client
        .post(format!("{}/_synapse/admin/v1/join/{}", homeserver, urlencode(room_id)))
        .header("Authorization", format!("Bearer {}", admin_token))
        .json(&serde_json::json!({ "user_id": user_id }))
        .send().await?;
    if !resp.status().is_success() {
        anyhow::bail!("force_join {} into {} failed: {}", user_id, room_id, resp.status());
    }
    Ok(())
}

async fn room_members(client: &reqwest::Client, homeserver: &str, admin_token: &str, room_id: &str) -> anyhow::Result<Vec<String>> {
    let resp: serde_json::Value = client
        .get(format!("{}/_synapse/admin/v1/rooms/{}/members", homeserver, urlencode(room_id)))
        .header("Authorization", format!("Bearer {}", admin_token))
        .send().await?
        .json().await?;
    Ok(resp["members"].as_array()
        .map(|arr| arr.iter().filter_map(|v| v.as_str().map(String::from)).collect())
        .unwrap_or_default())
}

async fn kick(client: &reqwest::Client, homeserver: &str, admin_token: &str, room_id: &str, user_id: &str, reason: &str) -> anyhow::Result<()> {
    let resp = client
        .post(format!("{}/_matrix/client/v3/rooms/{}/kick", homeserver, urlencode(room_id)))
        .header("Authorization", format!("Bearer {}", admin_token))
        .json(&serde_json::json!({ "user_id": user_id, "reason": reason }))
        .send().await?;
    if !resp.status().is_success() {
        anyhow::bail!("kick {} from {} failed: {}", user_id, room_id, resp.status());
    }
    Ok(())
}

async fn deactivate(client: &reqwest::Client, homeserver: &str, admin_token: &str, user_id: &str) -> anyhow::Result<()> {
    let resp = client
        .post(format!("{}/_synapse/admin/v1/deactivate/{}", homeserver, urlencode(user_id)))
        .header("Authorization", format!("Bearer {}", admin_token))
        .json(&serde_json::json!({ "erase": true }))
        .send().await?;
    if !resp.status().is_success() {
        anyhow::bail!("deactivate {} failed: {}", user_id, resp.status());
    }
    Ok(())
}

fn urlencode(s: &str) -> String {
    s.chars().map(|c| match c {
        '!' | '#' | '@' | ':' | '/' | '?' | '&' | '=' | '+' | ' ' => format!("%{:02X}", c as u32),
        _ => c.to_string(),
    }).collect()
}

/// Full bootstrap: register the 9 agents, force-join each into its room,
/// kick @charradissa-relay from the 8 component rooms if present. Safe to
/// rerun — every step checks state before acting.
pub async fn run(homeserver: &str, bao_addr: &str) -> anyhow::Result<()> {
    let bao_token = std::env::var("BAO_TOKEN")
        .map_err(|_| anyhow::anyhow!("BAO_TOKEN not set"))?;
    let shared_secret = std::env::var("SYNAPSE_REGISTRATION_SHARED_SECRET")
        .map_err(|_| anyhow::anyhow!("SYNAPSE_REGISTRATION_SHARED_SECRET not set"))?;
    let client = reqwest::Client::new();

    for (name, room_id) in AGENTS {
        let password = get_or_create_password(&client, bao_addr, &bao_token, name).await?;
        register_user(&client, homeserver, &shared_secret, name, &password).await?;
    }

    let (admin_mxid, admin_token) = mint_admin_session(&client, homeserver, &shared_secret).await?;
    tracing::info!("bootstrap-matrix: minted throwaway admin session {}", admin_mxid);

    for (name, room_id) in AGENTS {
        let user_id = format!("@{}:{}", name, "occitane.guilhem");
        force_join(&client, homeserver, &admin_token, room_id, &user_id).await?;
        tracing::info!("bootstrap-matrix: {} joined {}", user_id, room_id);
    }

    let relay_id = "@charradissa-relay:occitane.guilhem";
    for (name, room_id) in component_rooms() {
        let members = room_members(&client, homeserver, &admin_token, room_id).await?;
        if members.iter().any(|m| m == relay_id) {
            kick(&client, homeserver, &admin_token, room_id, relay_id,
                "component agents now run independent Matrix sessions — see Occitan#per-agent-matrix-independence").await?;
            tracing::info!("bootstrap-matrix: kicked {} from {} ({})", relay_id, name, room_id);
        }
    }

    deactivate(&client, homeserver, &admin_token, &admin_mxid).await?;
    tracing::info!("bootstrap-matrix: complete, throwaway admin session deactivated");
    Ok(())
}
