//! Synapse account bootstrap: shared-secret registration + password login.
//!
//! Used by `hook-admin` (to create the bot's own `@matrixhook` account) and by
//! `fake-human` (to register a throwaway test account). The
//! `registration_shared_secret` is read from the nix-sys sops file, mirroring
//! how the sibling botmaster bootstraps itself.

use anyhow::{Context, Result, anyhow, bail};
use serde::Deserialize;
use std::process::Command as ProcCommand;

/// Subset of a `/login` response we care about.
#[derive(Clone, Debug, Deserialize)]
pub struct LoginResponse {
    /// Full user id of the logged-in account.
    pub user_id: String,
    /// Device id created by this login.
    pub device_id: String,
    /// Access token bound to `device_id`.
    pub access_token: String,
}

/// Generate a random alphanumeric secret of length `len` (for passwords).
pub fn random_secret(len: usize) -> String {
    use rand::Rng;
    const CHARSET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";
    let mut rng = rand::thread_rng();
    (0..len)
        .map(|_| CHARSET[rng.gen_range(0..CHARSET.len())] as char)
        .collect()
}

/// Read the Synapse `registration_shared_secret` from a nix-sys sops file by
/// shelling out to `sops` (matching the botmaster bootstrap).
pub fn read_registration_secret(sops_file: &str) -> Result<String> {
    let output = ProcCommand::new("sops")
        .args(["-d", "--extract", "[\"synapse_extra_config\"]", sops_file])
        .output()
        .context("running `sops` (is it installed and are you authorized to decrypt?)")?;
    if !output.status.success() {
        bail!(
            "sops failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let text = String::from_utf8_lossy(&output.stdout);
    for line in text.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("registration_shared_secret:") {
            return Ok(rest.trim().trim_matches('"').to_owned());
        }
    }
    bail!("registration_shared_secret not found in {sops_file}")
}

/// Register a Synapse account via the shared-secret endpoint
/// (`/_synapse/admin/v1/register`). `admin = false` creates an ordinary
/// (non-admin) user, which is what both the hook bot and the test human are.
pub async fn register_shared_secret(
    admin_hs: &str,
    shared_secret: &str,
    localpart: &str,
    password: &str,
    admin: bool,
) -> Result<()> {
    use hmac::{Hmac, Mac};
    use sha1::Sha1;

    let http = reqwest::Client::new();
    let url = format!(
        "{}/_synapse/admin/v1/register",
        admin_hs.trim_end_matches('/')
    );

    // Step 1: get a one-time nonce.
    let nonce: String = http
        .get(&url)
        .send()
        .await?
        .json::<serde_json::Value>()
        .await?
        .get("nonce")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("no nonce in register response"))?
        .to_owned();

    // Step 2: HMAC-SHA1 (key = shared secret) over
    // `nonce\0user\0password\0(admin|notadmin)`.
    let mut mac = Hmac::<Sha1>::new_from_slice(shared_secret.as_bytes())
        .map_err(|e| anyhow!("hmac key error: {e}"))?;
    mac.update(nonce.as_bytes());
    mac.update(b"\0");
    mac.update(localpart.as_bytes());
    mac.update(b"\0");
    mac.update(password.as_bytes());
    mac.update(b"\0");
    mac.update(if admin { b"admin" } else { b"notadmin" });
    let mac_hex = hex::encode(mac.finalize().into_bytes());

    // Step 3: submit registration.
    let resp = http
        .post(&url)
        .json(&serde_json::json!({
            "nonce": nonce,
            "username": localpart,
            "password": password,
            "admin": admin,
            "mac": mac_hex,
        }))
        .send()
        .await?;
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        bail!("shared-secret register failed ({status}): {body}");
    }
    Ok(())
}

/// Log in with a password to mint an access token + device id via
/// `POST /_matrix/client/v3/login`.
pub async fn login_password(
    homeserver: &str,
    localpart: &str,
    password: &str,
    device_display_name: &str,
) -> Result<LoginResponse> {
    let http = reqwest::Client::new();
    let url = format!("{}/_matrix/client/v3/login", homeserver.trim_end_matches('/'));
    let resp = http
        .post(url)
        .json(&serde_json::json!({
            "type": "m.login.password",
            "identifier": { "type": "m.id.user", "user": localpart },
            "password": password,
            "initial_device_display_name": device_display_name,
        }))
        .send()
        .await?;
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        bail!("login failed ({status}): {body}");
    }
    Ok(resp.json::<LoginResponse>().await?)
}
