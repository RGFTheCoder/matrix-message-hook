//! The matrix bot: auto-join invites and handle hook commands in DMs.
//!
//! There is no botmaster-style trust gate here — any user may create a hook.
//! Hooks are low-risk capabilities (a UUID that posts into the creator's own DM
//! room), so authorization is simply "you can only see/delete your own hooks."
//! Commands are only honored in one-to-one rooms; group rooms are ignored.

use std::sync::Arc;

use hook_core::command::{Command, parse_command, webhook_url};
use hook_core::{Config, Store};
use matrix_sdk::event_handler::Ctx;
use matrix_sdk::ruma::events::room::member::StrippedRoomMemberEvent;
use matrix_sdk::ruma::events::room::message::{
    MessageType, OriginalSyncRoomMessageEvent, RoomMessageEventContent,
};
use matrix_sdk::{Client, Room};

/// Max accepted hook name length (defensive; names are user input).
const NAME_MAX: usize = 100;

/// Max hooks a single user may own (defensive bound on the shared store).
const MAX_HOOKS_PER_USER: usize = 50;

/// Shared bot state handed to the message handler.
#[derive(Clone)]
struct BotState {
    store: Store,
    cfg: Arc<Config>,
}

/// Install the message handler on `client`.
pub fn install(client: &Client, store: Store, cfg: Arc<Config>) {
    client.add_event_handler_context(BotState { store, cfg });
    client.add_event_handler(on_message);
}

/// Handle one inbound message: parse a command and reply.
async fn on_message(
    ev: OriginalSyncRoomMessageEvent,
    room: Room,
    client: Client,
    state: Ctx<BotState>,
) {
    // Ignore our own messages.
    if Some(ev.sender.as_ref()) == client.user_id() {
        return;
    }
    // Only handle plain text.
    let MessageType::Text(text) = ev.content.msgtype else {
        return;
    };

    // Only operate in one-to-one rooms (a DM: the bot + exactly one user).
    // Ignore group rooms entirely so we never bind a hook to a shared room
    // (which would expose its UUID to every member) nor spam a busy room with
    // command replies. The bot may still auto-join a group invite, but it stays
    // silent there.
    if !is_direct_chat(&room).await {
        tracing::debug!(room = %room.room_id(), "ignoring message in non-DM room");
        return;
    }

    let sender = ev.sender.to_string();
    let room_id = room.room_id().to_string();
    let cmd = parse_command(&text.body);
    tracing::info!(%sender, %room_id, ?cmd, "handling command");

    let reply = handle_command(cmd, &state.store, &state.cfg, &sender, &room_id).await;
    if let Err(e) = room
        .send(RoomMessageEventContent::text_markdown(reply))
        .await
    {
        tracing::warn!("failed to send reply: {e}");
    }
}

/// Whether `room` is a one-to-one conversation (the bot plus at most one other
/// member). Fetches the joined-member list so the count is accurate rather than
/// relying on a possibly-stale summary. Fails closed (treats an error as "not a
/// DM") so we never act on ambiguous rooms.
async fn is_direct_chat(room: &Room) -> bool {
    match room.members(matrix_sdk::RoomMemberships::JOIN).await {
        Ok(members) => members.len() <= 2,
        Err(e) => {
            tracing::warn!("could not fetch members for {}: {e}", room.room_id());
            false
        }
    }
}

/// Execute a parsed command against the store, returning a Markdown reply.
async fn handle_command(
    cmd: Command,
    store: &Store,
    cfg: &Config,
    sender: &str,
    room_id: &str,
) -> String {
    match cmd {
        Command::New(name) => {
            let name = name.trim();
            if name.len() > NAME_MAX {
                return format!("Name is too long (max {NAME_MAX} characters).");
            }
            match store.list_by_owner(sender).await {
                Ok(existing) if existing.len() >= MAX_HOOKS_PER_USER => {
                    return format!(
                        "You already have {MAX_HOOKS_PER_USER} hooks (the maximum). \
                         Delete one with `delete <uuid>` first."
                    );
                }
                Ok(_) => {}
                Err(e) => {
                    tracing::warn!("list_by_owner (cap check) failed: {e}");
                    return "Sorry, I couldn't create that hook (internal error).".to_owned();
                }
            }
            match store.create_hook(name, sender, room_id).await {
                Ok(hook) => {
                    let url = webhook_url(&cfg.public_base_url, &hook.id);
                    format!(
                        "✅ Created hook **{name}**.\n\n\
                         - UUID: `{id}`\n\
                         - URL: `{url}`\n\n\
                         Trigger it (messages appear in *this* room):\n\
                         - `curl -X POST {url} -d 'your message here'`\n\
                         - or GET `{url}/your%20short%20message`\n\n\
                         Anyone with this URL can post here, so keep it secret.",
                        id = hook.id,
                    )
                }
                Err(e) => {
                    tracing::warn!("create_hook failed: {e}");
                    "Sorry, I couldn't create that hook (internal error).".to_owned()
                }
            }
        }
        Command::List => match store.list_by_owner(sender).await {
            Ok(hooks) if hooks.is_empty() => {
                "You have no hooks yet. Create one with `new <name>`.".to_owned()
            }
            Ok(hooks) => {
                let mut out = String::from("Your hooks:\n");
                for h in hooks {
                    let url = webhook_url(&cfg.public_base_url, &h.id);
                    out.push_str(&format!("- **{}** — `{}`\n  {}\n", h.name, h.id, url));
                }
                out
            }
            Err(e) => {
                tracing::warn!("list_by_owner failed: {e}");
                "Sorry, I couldn't list your hooks (internal error).".to_owned()
            }
        },
        Command::Delete(id) => match store.delete_hook(&id, sender).await {
            Ok(true) => format!("🗑️ Deleted hook `{id}`."),
            Ok(false) => format!("No hook `{id}` owned by you."),
            Err(e) => {
                tracing::warn!("delete_hook failed: {e}");
                "Sorry, I couldn't delete that hook (internal error).".to_owned()
            }
        },
        Command::Help => help_text(),
        Command::Unknown(_) => format!("I didn't understand that.\n\n{}", help_text()),
    }
}

/// Usage help shown for `help` / unknown input.
fn help_text() -> String {
    "**matrixHook** — turn messages into Matrix posts.\n\n\
     Commands:\n\
     - `new <name>` — create a hook and get its URL\n\
     - `list` — list your hooks\n\
     - `delete <uuid>` — delete a hook\n\
     - `help` — show this help\n\n\
     Once you have a hook URL, POST a body or GET `<url>/<message>` and it \
     appears in the room where you created it."
        .to_owned()
}

/// Register a handler that auto-joins any room this account is invited to
/// (retrying to ride out federation races). Adapted from matrix-common.
pub fn auto_join_on_invite(client: &Client) {
    client.add_event_handler(
        |ev: StrippedRoomMemberEvent, room: Room, client: Client| async move {
            let Some(me) = client.user_id() else { return };
            if ev.state_key.as_str() != me.as_str() {
                return;
            }
            tokio::spawn(async move {
                let mut delay = 1u64;
                for _ in 0..5 {
                    match room.join().await {
                        Ok(()) => {
                            tracing::info!("auto-joined room {}", room.room_id());
                            return;
                        }
                        Err(e) => {
                            tracing::warn!(
                                "failed to join {}: {e}; retrying in {delay}s",
                                room.room_id()
                            );
                            tokio::time::sleep(std::time::Duration::from_secs(delay)).await;
                            delay *= 2;
                        }
                    }
                }
            });
        },
    );
}

/// Join every room this account is currently invited to (invites that were
/// pending before the handler was live). Adapted from matrix-common.
pub async fn join_pending_invites(client: &Client) {
    for room in client.invited_rooms() {
        tracing::info!(room = %room.room_id(), "joining pending invite");
        if let Err(e) = room.join().await {
            tracing::warn!("failed to join pending invite {}: {e}", room.room_id());
        }
    }
}
