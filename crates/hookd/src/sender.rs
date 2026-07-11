//! Provisioning and delivery for per-hook virtual (appservice) users.
//!
//! Each hook posts as its own `@hook_<name>_<id>` user. That user has no real
//! login, so `hookd` acts on its behalf via the appservice `as_token`. To post
//! into the (private) room where the hook was created, the virtual user must be
//! a member — so the command bot (which is in the room) invites it, then the
//! appservice makes it join.

use anyhow::Result;
use hook_core::{AppService, Hook};
use matrix_sdk::Client;
use matrix_sdk::ruma::{OwnedRoomId, OwnedUserId};

/// Register the virtual user, set its display name to the hook name, and get it
/// into the hook's room (invite via the bot + appservice join). Best-effort on
/// the individual steps; returns an error only if registration fails.
pub async fn provision(client: &Client, appservice: &AppService, hook: &Hook) -> Result<()> {
    appservice.register(&hook.sender).await?;
    if let Err(e) = appservice.set_displayname(&hook.sender, &hook.name).await {
        tracing::warn!("set_displayname for {} failed: {e}", hook.sender);
    }
    ensure_in_room(client, appservice, hook).await;
    Ok(())
}

/// Ensure the virtual user is a member of the hook's room: the bot invites it
/// (ignored if already a member), then the appservice joins (idempotent).
async fn ensure_in_room(client: &Client, appservice: &AppService, hook: &Hook) {
    if let (Ok(user_id), Ok(room_id)) = (
        appservice.user_id(&hook.sender).parse::<OwnedUserId>(),
        hook.room_id.parse::<OwnedRoomId>(),
    ) {
        if let Some(room) = client.get_room(&room_id) {
            // Ignore errors (e.g. the user is already in the room).
            if let Err(e) = room.invite_user_by_id(&user_id).await {
                tracing::debug!("invite {user_id} (maybe already a member): {e}");
            }
        }
    }
    if let Err(e) = appservice.join_room(&hook.sender, &hook.room_id).await {
        tracing::debug!("join_room for {} (maybe already joined): {e}", hook.sender);
    }
}

/// Deliver `body` into the hook's room as its virtual user. If the first send
/// fails (e.g. the user isn't in the room yet), re-ensure membership and retry
/// once.
pub async fn deliver(client: &Client, appservice: &AppService, hook: &Hook, body: &str) -> Result<()> {
    if appservice.send_text(&hook.sender, &hook.room_id, body).await.is_ok() {
        return Ok(());
    }
    tracing::warn!("first send as {} failed; re-ensuring room membership", hook.sender);
    ensure_in_room(client, appservice, hook).await;
    appservice
        .send_text(&hook.sender, &hook.room_id, body)
        .await
        .map(|_| ())
}
