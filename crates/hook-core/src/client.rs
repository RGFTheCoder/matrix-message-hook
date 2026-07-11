//! Building the matrix-sdk [`Client`] and sending messages.
//!
//! The bot restores an existing E2EE session from its
//! `(user_id, device_id, access_token)` — it never logs in with a password at
//! runtime. The crypto/state store is persisted on disk so the device identity
//! and room keys survive restarts (essential for reading encrypted DMs).

use anyhow::{Context, Result};
use matrix_sdk::authentication::matrix::MatrixSession;
use matrix_sdk::ruma::OwnedRoomId;
use matrix_sdk::ruma::events::room::message::RoomMessageEventContent;
use matrix_sdk::{Client, SessionMeta, SessionTokens};

use crate::config::Config;

/// Build a matrix-sdk client for `config` and restore its session, using a
/// persistent SQLite crypto/state store at `config.store_path`.
pub async fn connect(config: &Config) -> Result<Client> {
    // The SQLite store needs its directory to exist.
    let _ = std::fs::create_dir_all(&config.store_path);

    let client = Client::builder()
        .homeserver_url(&config.homeserver)
        .sqlite_store(&config.store_path, None)
        .build()
        .await
        .context("building matrix client")?;

    let user_id = config
        .user_id
        .parse()
        .with_context(|| format!("invalid MATRIX_USER {:?}", config.user_id))?;
    let session = MatrixSession {
        meta: SessionMeta {
            user_id,
            device_id: config.device_id.as_str().into(),
        },
        tokens: SessionTokens {
            access_token: config.access_token.clone(),
            refresh_token: None,
        },
    };
    client
        .restore_session(session)
        .await
        .context("restoring matrix session")?;
    Ok(client)
}

/// Restore an arbitrary session into a fresh matrix-sdk client with a persistent
/// SQLite crypto/state store at `store_path`. Used for per-hook E2EE clients
/// (whose sessions come from `m.login.application_service`).
pub async fn restore(
    homeserver: &str,
    user_id: &str,
    device_id: &str,
    access_token: &str,
    store_path: &std::path::Path,
) -> Result<Client> {
    let _ = std::fs::create_dir_all(store_path);
    let client = Client::builder()
        .homeserver_url(homeserver)
        .sqlite_store(store_path, None)
        .build()
        .await
        .context("building matrix client")?;
    let uid = user_id
        .parse()
        .with_context(|| format!("invalid user id {user_id:?}"))?;
    client
        .restore_session(MatrixSession {
            meta: SessionMeta {
                user_id: uid,
                device_id: device_id.into(),
            },
            tokens: SessionTokens {
                access_token: access_token.to_owned(),
                refresh_token: None,
            },
        })
        .await
        .context("restoring session")?;
    Ok(client)
}

/// Give this client's account a cross-signing identity (if it lacks one) and
/// self-sign the current device, so recipients see messages as coming from a
/// device the account owner has verified (clearing Element's "Encrypted by a
/// device not verified by its owner" shield).
///
/// Works for both password-holding accounts and passwordless appservice ghosts,
/// as long as the homeserver allows the first cross-signing key upload without
/// User-Interactive Auth (Synapse's MSC3967). Best-effort: on failure this logs
/// and returns — the client still works, the shield just remains.
pub async fn ensure_cross_signing(client: &Client, who: &str) {
    if let Err(e) = client
        .encryption()
        .bootstrap_cross_signing_if_needed(None)
        .await
    {
        tracing::warn!("cross-signing bootstrap for {who} skipped: {e}");
        return;
    }
    match client.encryption().get_own_device().await {
        Ok(Some(device)) if !device.is_verified_with_cross_signing() => {
            if let Err(e) = device.verify().await {
                tracing::warn!("self-signing device for {who} failed: {e}");
            }
        }
        Ok(_) => {}
        Err(e) => tracing::warn!("loading own device for {who} failed: {e}"),
    }
}

/// Send a plain-text message into a joined room identified by `room_id`.
///
/// Plain text (not Markdown/HTML) is deliberate: webhook content is untrusted
/// and the message is authored by the bot account, so we avoid rendering
/// attacker-controlled formatting.
pub async fn send_room_text(client: &Client, room_id: &str, body: &str) -> Result<()> {
    let rid: OwnedRoomId = room_id
        .parse()
        .with_context(|| format!("invalid room id {room_id:?}"))?;
    let room = client
        .get_room(&rid)
        .with_context(|| format!("bot is not a member of room {room_id}"))?;
    room.send(RoomMessageEventContent::text_plain(body))
        .await
        .context("sending room message")?;
    Ok(())
}
