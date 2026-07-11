//! A minimal Matrix Application Service (appservice) client.
//!
//! This lets `hookd` post messages *as* per-hook virtual users
//! (`@hook_<name>_<id>:server`) without those being real accounts — the same
//! mechanism Discord/Slack bridges use. It authenticates every call with the
//! appservice `as_token` and "masquerades" as a namespaced user via the
//! `?user_id=` query parameter.
//!
//! Virtual users have no E2EE identity, so their messages are sent as plain,
//! *unencrypted* `m.room.message` events (acceptable for low-sensitivity
//! webhook alerts; the bot warns users at hook-creation time).

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use crate::id;

/// Appservice client bound to a homeserver + `as_token`.
#[derive(Clone)]
pub struct AppService {
    http: reqwest::Client,
    homeserver: String,
    as_token: String,
    server_name: String,
}

#[derive(Deserialize)]
struct MatrixErr {
    errcode: String,
}

impl AppService {
    /// Build an appservice client. `homeserver` is the client-server API base
    /// (e.g. `http://10.1.1.30:8008`), `server_name` the domain part of MXIDs
    /// (e.g. `damastacoda.dev`).
    pub fn new(homeserver: &str, as_token: &str, server_name: &str) -> Self {
        Self {
            http: reqwest::Client::new(),
            homeserver: homeserver.trim_end_matches('/').to_owned(),
            as_token: as_token.to_owned(),
            server_name: server_name.to_owned(),
        }
    }

    /// The full MXID for a virtual user localpart.
    pub fn user_id(&self, localpart: &str) -> String {
        format!("@{localpart}:{}", self.server_name)
    }

    /// Register the virtual user (idempotent: an already-registered user is a
    /// success). Appservice registration bypasses UIAA via the `as_token`.
    pub async fn register(&self, localpart: &str) -> Result<()> {
        let url = format!("{}/_matrix/client/v3/register", self.homeserver);
        let resp = self
            .http
            .post(url)
            .bearer_auth(&self.as_token)
            .json(&serde_json::json!({
                "type": "m.login.application_service",
                "username": localpart,
            }))
            .send()
            .await?;
        if resp.status().is_success() {
            return Ok(());
        }
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        // Already registered -> fine.
        if serde_json::from_str::<MatrixErr>(&body)
            .map(|e| e.errcode == "M_USER_IN_USE")
            .unwrap_or(false)
        {
            return Ok(());
        }
        bail!("register {localpart} failed ({status}): {body}");
    }

    /// Set a virtual user's display name.
    pub async fn set_displayname(&self, localpart: &str, name: &str) -> Result<()> {
        let user = self.user_id(localpart);
        let url = format!(
            "{}/_matrix/client/v3/profile/{}/displayname?user_id={}",
            self.homeserver,
            enc(&user),
            enc(&user),
        );
        let resp = self
            .http
            .put(url)
            .bearer_auth(&self.as_token)
            .json(&serde_json::json!({ "displayname": name }))
            .send()
            .await?;
        check(resp, "set_displayname").await
    }

    /// Make the virtual user join a room (it must already be invited, or the
    /// room must be in the appservice's namespace).
    pub async fn join_room(&self, localpart: &str, room_id: &str) -> Result<()> {
        let user = self.user_id(localpart);
        let url = format!(
            "{}/_matrix/client/v3/rooms/{}/join?user_id={}",
            self.homeserver,
            enc(room_id),
            enc(&user),
        );
        let resp = self
            .http
            .post(url)
            .bearer_auth(&self.as_token)
            .json(&serde_json::json!({}))
            .send()
            .await?;
        check(resp, "join_room").await
    }

    /// Send a plain-text (unencrypted) message into `room_id` as the virtual
    /// user. Returns the event id.
    pub async fn send_text(&self, localpart: &str, room_id: &str, body: &str) -> Result<String> {
        let user = self.user_id(localpart);
        let txn = id::gen(20);
        let url = format!(
            "{}/_matrix/client/v3/rooms/{}/send/m.room.message/{}?user_id={}",
            self.homeserver,
            enc(room_id),
            enc(&txn),
            enc(&user),
        );
        let resp = self
            .http
            .put(url)
            .bearer_auth(&self.as_token)
            .json(&serde_json::json!({ "msgtype": "m.text", "body": body }))
            .send()
            .await?;
        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            bail!("send_text failed ({status}): {text}");
        }
        #[derive(Deserialize)]
        struct Sent {
            event_id: String,
        }
        Ok(resp.json::<Sent>().await?.event_id)
    }
}

/// Percent-encode a path/query component (MXIDs and room ids contain `@`, `:`,
/// `!`, `#`).
fn enc(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 3);
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Turn a non-success response into a typed error.
async fn check(resp: reqwest::Response, what: &str) -> Result<()> {
    if resp.status().is_success() {
        return Ok(());
    }
    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    Err(anyhow::anyhow!("{what} failed ({status}): {body}"))
        .with_context(|| format!("appservice {what}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_id_and_encoding() {
        let a = AppService::new("http://hs:8008/", "tok", "example.dev");
        assert_eq!(a.user_id("hook_alerts_9k3m"), "@hook_alerts_9k3m:example.dev");
        assert_eq!(enc("@hook_x:example.dev"), "%40hook_x%3Aexample.dev");
        assert_eq!(enc("!room:example.dev"), "%21room%3Aexample.dev");
    }
}
