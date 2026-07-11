//! Per-hook end-to-end-encrypted matrix-sdk clients.
//!
//! Each hook delivers as its own `@hook_<name>_<id>` user with REAL E2EE. Since
//! matrix-rust-sdk has no MSC3202 support, we run a full client per hook: a real
//! device session is minted via the standard `m.login.application_service` login
//! (the appservice `as_token`), and each client keeps a persistent crypto store
//! + a sync loop so it can hold room keys and encrypt to the recipient.
//!
//! Safeguards (per design review):
//! - device_id + access_token + crypto store are one atomic unit: if the store
//!   is missing, we mint a FRESH device rather than reuse a stale device id;
//! - a client is only "ready" to deliver after its own initial sync + it is
//!   joined to an ENCRYPTED room (never send plaintext);
//! - singleflight per hook id (one live client / store owner);
//! - on delete the hook user leaves the room and logs out its device.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use hook_core::{AppService, Config, Hook, Store};
use matrix_sdk::config::SyncSettings;
use matrix_sdk::ruma::events::room::message::RoomMessageEventContent;
use matrix_sdk::ruma::{OwnedRoomId, OwnedUserId};
use matrix_sdk::{Client, RoomState};
use tokio::sync::Mutex;
use tokio::task::JoinHandle;

/// A live per-hook client + its background sync task.
struct HookClient {
    client: Client,
    ready: AtomicBool,
    sync_task: Mutex<Option<JoinHandle<()>>>,
}

impl HookClient {
    async fn abort(&self) {
        if let Some(task) = self.sync_task.lock().await.take() {
            task.abort();
        }
    }
}

/// Registry of per-hook E2EE clients, keyed by hook id.
#[derive(Clone)]
pub struct HookClients {
    inner: Arc<Inner>,
}

struct Inner {
    clients: Mutex<HashMap<String, Arc<HookClient>>>,
    /// Per-hook-id build locks (singleflight): only one task builds a given
    /// hook's client at a time.
    locks: Mutex<HashMap<String, Arc<Mutex<()>>>>,
    store: Store,
    appservice: AppService,
    cfg: Arc<Config>,
    /// The command bot's client — used to invite hook users into rooms.
    command: Client,
}

impl HookClients {
    /// Build the registry.
    pub fn new(store: Store, appservice: AppService, cfg: Arc<Config>, command: Client) -> Self {
        Self {
            inner: Arc::new(Inner {
                clients: Mutex::new(HashMap::new()),
                locks: Mutex::new(HashMap::new()),
                store,
                appservice,
                cfg,
                command,
            }),
        }
    }

    /// Directory for a hook's persistent crypto/state store.
    fn store_dir(&self, hook_id: &str) -> PathBuf {
        self.inner
            .cfg
            .store_path
            .join("hooks")
            .join(hook_id)
    }

    /// Get the per-id build lock.
    async fn id_lock(&self, hook_id: &str) -> Arc<Mutex<()>> {
        let mut locks = self.inner.locks.lock().await;
        locks
            .entry(hook_id.to_owned())
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone()
    }

    /// Bring up all existing hooks' clients (called at startup). Each is spawned
    /// so a slow/failed one doesn't block the others.
    pub async fn start_all(&self) {
        let hooks = match self.inner.store.all_hooks().await {
            Ok(h) => h,
            Err(e) => {
                tracing::error!("could not load hooks at startup: {e}");
                return;
            }
        };
        tracing::info!(count = hooks.len(), "starting per-hook E2EE clients");
        for hook in hooks {
            let this = self.clone();
            tokio::spawn(async move {
                if let Err(e) = this.ensure(&hook).await {
                    tracing::warn!("hook {} client failed to start: {e}", hook.id);
                }
            });
        }
    }

    /// Ensure a live, ready client exists for `hook`, building it if needed
    /// (singleflight). Returns the client.
    async fn ensure(&self, hook: &Hook) -> Result<Arc<HookClient>> {
        // Fast path.
        if let Some(hc) = self.inner.clients.lock().await.get(&hook.id).cloned() {
            return Ok(hc);
        }
        let lock = self.id_lock(&hook.id).await;
        let _guard = lock.lock().await;
        // Re-check under the per-id lock.
        if let Some(hc) = self.inner.clients.lock().await.get(&hook.id).cloned() {
            return Ok(hc);
        }
        let hc = self.build(hook).await?;
        self.inner
            .clients
            .lock()
            .await
            .insert(hook.id.clone(), hc.clone());
        Ok(hc)
    }

    /// Build (login/restore), join the room, initial-sync, and start the sync
    /// loop for `hook`.
    async fn build(&self, hook: &Hook) -> Result<Arc<HookClient>> {
        let store_dir = self.store_dir(&hook.id);
        // Coupling rule: reuse the stored session ONLY if its crypto store is
        // present; otherwise mint a fresh device (a reused device id without its
        // keys would publish mismatched keys and break decryption).
        let store_present = store_dir.join("matrix-sdk-state.sqlite3").exists();
        let (device_id, token) = if store_present && !hook.device_id.is_empty() {
            (hook.device_id.clone(), hook.access_token.clone())
        } else {
            tracing::info!("minting fresh session for hook {} ({})", hook.id, hook.sender);
            let _ = std::fs::remove_dir_all(&store_dir);
            let s = self
                .inner
                .appservice
                .login(&hook.sender)
                .await
                .context("appservice login for hook user")?;
            self.inner
                .store
                .set_session(&hook.id, &s.device_id, &s.access_token)
                .await?;
            (s.device_id, s.access_token)
        };

        let user_id = self.inner.appservice.user_id(&hook.sender);
        let client = hook_core::client::restore(
            &self.inner.cfg.homeserver,
            &user_id,
            &device_id,
            &token,
            &store_dir,
        )
        .await
        .context("restoring hook client session")?;

        // Set the display name to the hook name (best effort).
        if let Err(e) = client
            .account()
            .set_display_name(Some(&hook.name))
            .await
        {
            tracing::debug!("set_display_name for {}: {e}", hook.sender);
        }

        // Join the hook's room: the command bot invites it, then this client
        // joins. Sync once first so the client learns about the invite.
        self.ensure_joined(hook, &client).await;

        // Self-sign this device via cross-signing so recipients don't flag the
        // hook's messages as "encrypted by a device not verified by its owner".
        ensure_cross_signing(&client, &hook.sender).await;

        let ready = AtomicBool::new(self.is_ready(hook, &client).await);
        if !ready.load(Ordering::Relaxed) {
            tracing::warn!("hook {} not yet in an encrypted room; will retry on delivery", hook.id);
        }

        // Continuous sync in the background (drives key sharing + delivery).
        // Retry on error so a transient homeserver outage doesn't kill the loop.
        let sync_client = client.clone();
        let hid = hook.id.clone();
        let sync_task = tokio::spawn(async move {
            loop {
                if let Err(e) = sync_client.sync(SyncSettings::default()).await {
                    tracing::warn!("hook {hid} sync errored, retrying in 5s: {e}");
                    tokio::time::sleep(Duration::from_secs(5)).await;
                }
            }
        });

        Ok(Arc::new(HookClient {
            client,
            ready,
            sync_task: Mutex::new(Some(sync_task)),
        }))
    }

    /// Invite (via the bot) + join the hook's room, then sync so room state is
    /// known.
    async fn ensure_joined(&self, hook: &Hook, client: &Client) {
        let Ok(room_id) = hook.room_id.parse::<OwnedRoomId>() else {
            return;
        };
        let user_id: Option<OwnedUserId> = self.inner.appservice.user_id(&hook.sender).parse().ok();

        // Bot invites the hook user (ignored if already a member).
        if let (Some(uid), Some(room)) = (&user_id, self.inner.command.get_room(&room_id)) {
            if let Err(e) = room.invite_user_by_id(uid).await {
                tracing::debug!("invite {} (maybe already a member): {e}", hook.sender);
            }
        }
        // Let the hook client see the invite/room.
        let _ = client.sync_once(SyncSettings::default()).await;
        // Join if not already joined.
        match client.get_room(&room_id) {
            Some(room) if room.state() == RoomState::Joined => {}
            Some(room) => {
                if let Err(e) = room.join().await {
                    tracing::debug!("hook {} join: {e}", hook.id);
                }
            }
            None => {
                if let Err(e) = client.join_room_by_id(&room_id).await {
                    tracing::debug!("hook {} join_by_id: {e}", hook.id);
                }
            }
        }
        // Sync again to pick up room state (encryption, members) post-join.
        let _ = client.sync_once(SyncSettings::default()).await;
    }

    /// Whether the client is joined to the hook's room AND the room is encrypted.
    async fn is_ready(&self, hook: &Hook, client: &Client) -> bool {
        let Ok(room_id) = hook.room_id.parse::<OwnedRoomId>() else {
            return false;
        };
        match client.get_room(&room_id) {
            Some(room) if room.state() == RoomState::Joined => {
                room.encryption_state().is_encrypted()
            }
            _ => false,
        }
    }

    /// Deliver `body` into the hook's room, end-to-end encrypted. Ensures the
    /// client is up + ready first; refuses to send into an unencrypted room.
    pub async fn deliver(&self, hook: &Hook, body: &str) -> Result<()> {
        let hc = self.ensure(hook).await?;

        // If not ready yet (fresh start), re-join + wait briefly.
        if !hc.ready.load(Ordering::Relaxed) {
            self.ensure_joined(hook, &hc.client).await;
            for _ in 0..20 {
                if self.is_ready(hook, &hc.client).await {
                    hc.ready.store(true, Ordering::Relaxed);
                    break;
                }
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
        }

        let room_id: OwnedRoomId = hook.room_id.parse().context("invalid room id")?;
        let room = hc
            .client
            .get_room(&room_id)
            .context("hook client is not in its room")?;
        if room.state() != RoomState::Joined {
            bail!("hook client is not joined to its room");
        }
        // Enforce encryption: never fall back to plaintext.
        if !room.encryption_state().is_encrypted() {
            bail!("hook room is not encrypted; refusing to send plaintext");
        }
        // matrix-sdk auto-encrypts (Megolm) because the room is encrypted.
        room.send(RoomMessageEventContent::text_plain(body))
            .await
            .context("sending encrypted message")?;
        Ok(())
    }

    /// Provision a brand-new hook's client at creation time (bot is known to be
    /// in the room). Fails if provisioning can't reach a ready (joined +
    /// encrypted) state.
    pub async fn provision(&self, hook: &Hook) -> Result<()> {
        let hc = self.ensure(hook).await?;
        if !hc.ready.load(Ordering::Relaxed) {
            for _ in 0..20 {
                if self.is_ready(hook, &hc.client).await {
                    hc.ready.store(true, Ordering::Relaxed);
                    break;
                }
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
        }
        if !hc.ready.load(Ordering::Relaxed) {
            bail!("hook client could not join an encrypted room");
        }
        Ok(())
    }

    /// Tear down a hook's client: stop syncing, leave the room, log out the
    /// device, and delete its crypto store.
    pub async fn remove(&self, hook: &Hook) {
        let hc = self.inner.clients.lock().await.remove(&hook.id);
        if let Some(hc) = hc {
            // Leave the room so it stops receiving future room keys.
            if let Ok(room_id) = hook.room_id.parse::<OwnedRoomId>() {
                if let Some(room) = hc.client.get_room(&room_id) {
                    let _ = room.leave().await;
                }
            }
            // Log out this device.
            let _ = hc.client.matrix_auth().logout().await;
            hc.abort().await;
        }
        let _ = std::fs::remove_dir_all(self.store_dir(&hook.id));
    }
}

/// Give the hook user a cross-signing identity (if it lacks one) and self-sign
/// this device, so recipients see the messages as coming from a device the
/// owner has verified (clearing the "not verified by its owner" shield).
///
/// Appservice users have no password, so the initial cross-signing key upload
/// relies on the homeserver allowing it without User-Interactive Auth
/// (Synapse's MSC3967 support). Best-effort: on failure we log and carry on —
/// delivery still works, the shield just remains.
async fn ensure_cross_signing(client: &Client, sender: &str) {
    if let Err(e) = client
        .encryption()
        .bootstrap_cross_signing_if_needed(None)
        .await
    {
        tracing::warn!("cross-signing bootstrap for {sender} skipped: {e}");
        return;
    }
    match client.encryption().get_own_device().await {
        Ok(Some(device)) if !device.is_verified_with_cross_signing() => {
            if let Err(e) = device.verify().await {
                tracing::warn!("self-signing device for {sender} failed: {e}");
            }
        }
        Ok(_) => {}
        Err(e) => tracing::warn!("loading own device for {sender} failed: {e}"),
    }
}
