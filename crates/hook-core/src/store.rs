//! The hook store, backed by SurrealDB.
//!
//! In production this connects to the shared SurrealDB server over a secure
//! WebSocket (`wss://surrealdb.damastacoda.dev`) authenticating as root, under
//! matrixHook's own [`NAMESPACE`]. Tests use an embedded in-memory engine
//! (`mem://`) so they need no server.
//!
//! NOTE ON THE TOOLCHAIN: the SurrealDB 3.x client pulls in `diskann-*`, which
//! uses AVX-512 (`vpdpwssd.512`) intrinsics that this environment's default
//! rustc/LLVM cannot lower. The workspace therefore pins an older stable Rust in
//! `flake.nix` (whose bundled LLVM can) — see the comment there. The
//! `live_surreal` integration test exercises the real client↔server path.
//!
//! A [`Hook`] is identified by a v4 UUID stored in the normal `hid` field
//! (uniquely indexed). We deliberately do NOT use the UUID as SurrealDB's record
//! id, which keeps queries free of record-id/`type::record` conversions.

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use surrealdb::Surreal;
use surrealdb::engine::any::{self, Any};
use surrealdb::opt::auth::Root;
use surrealdb::types::SurrealValue;


/// The default SurrealDB namespace matrixHook uses on the shared server.
pub const NAMESPACE: &str = "matrixHook";

/// A webhook: a short id that delivers posted messages into `room_id`, authored
/// by the per-hook virtual (appservice) user `sender`.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Hook {
    /// The hook's short id (the secret in its URL).
    pub id: String,
    /// Human-readable name chosen by the creator.
    pub name: String,
    /// MXID of the user who created the hook.
    pub owner: String,
    /// Room the hook posts into (where it was created).
    pub room_id: String,
    /// Localpart of the per-hook virtual user that authors deliveries, e.g.
    /// `hook_alerts_9k3m…`.
    pub sender: String,
    /// Device id of the per-hook user's E2EE session (empty until provisioned).
    pub device_id: String,
    /// Access token of the per-hook user's session (empty until provisioned).
    pub access_token: String,
}

/// SurrealValue view of a stored `hook` row. `hid` is selected explicitly so it
/// never collides with SurrealDB's reserved record `id`.
#[derive(Debug, SurrealValue)]
struct HookRow {
    hid: String,
    name: String,
    owner: String,
    room_id: String,
    sender: String,
    device_id: String,
    access_token: String,
}

impl From<HookRow> for Hook {
    fn from(r: HookRow) -> Self {
        Hook {
            id: r.hid,
            name: r.name,
            owner: r.owner,
            room_id: r.room_id,
            sender: r.sender,
            device_id: r.device_id,
            access_token: r.access_token,
        }
    }
}

/// Handle to the hook store.
#[derive(Clone)]
pub struct Store {
    db: Surreal<Any>,
    namespace: String,
    database: String,
}

impl Store {
    /// Connect to the shared SurrealDB server at `url`, authenticate as root,
    /// and select `namespace`/`database` (defining them + the schema up front,
    /// all idempotent).
    pub async fn connect(
        url: &str,
        namespace: &str,
        database: &str,
        user: &str,
        pass: &str,
    ) -> Result<Self> {
        Self::open(url, namespace, database, Some((user, pass))).await
    }

    /// Open an in-memory store for tests (no server, no auth).
    pub async fn memory() -> Result<Self> {
        Self::open("mem://", NAMESPACE, "hooks", None).await
    }

    /// Shared connection path for both the remote and in-memory engines.
    async fn open(
        url: &str,
        namespace: &str,
        database: &str,
        auth: Option<(&str, &str)>,
    ) -> Result<Self> {
        install_crypto_provider();

        // Namespace/database names are interpolated into DEFINE statements
        // (which cannot bind identifiers), so reject anything but simple idents.
        valid_ident(namespace).with_context(|| format!("invalid namespace {namespace:?}"))?;
        valid_ident(database).with_context(|| format!("invalid database {database:?}"))?;

        let db = any::connect(url)
            .await
            .with_context(|| format!("connecting to SurrealDB at {url}"))?;

        if let Some((username, password)) = auth {
            db.signin(Root {
                username: username.to_owned(),
                password: password.to_owned(),
            })
            .await
            .context("signing in to SurrealDB as root")?;
        }

        // Ensure the namespace + database exist, then select them. Root auth is
        // required to DEFINE a namespace; the embedded engine runs with full
        // access, so this works there too.
        db.query(format!("DEFINE NAMESPACE IF NOT EXISTS {namespace}"))
            .await?
            .check()
            .context("defining namespace")?;
        db.use_ns(namespace).await?;
        db.query(format!("DEFINE DATABASE IF NOT EXISTS {database}"))
            .await?
            .check()
            .context("defining database")?;
        db.use_db(database).await?;

        let this = Self {
            db,
            namespace: namespace.to_owned(),
            database: database.to_owned(),
        };
        this.migrate().await?;
        Ok(this)
    }

    /// Re-assert the namespace + database on the session before a query.
    ///
    /// SurrealDB's namespace/database selection is per-session state on the
    /// shared connection; it can be lost (e.g. across a ws reconnect) leading to
    /// "Specify a database to use" errors on a later query. Re-selecting before
    /// each operation is cheap and keeps the store robust. (Same approach as the
    /// sibling matrix-db crate.)
    async fn select(&self) -> Result<()> {
        self.db
            .use_ns(self.namespace.clone())
            .use_db(self.database.clone())
            .await?;
        Ok(())
    }

    /// Define the `hook` table (schemaful) + indexes (idempotent). The schema is
    /// now stable, so we type every field and index the lookup columns:
    /// `hid` (the webhook secret, unique) and `owner` (for `list_by_owner`).
    async fn migrate(&self) -> Result<()> {
        self.db
            .query(
                "DEFINE TABLE IF NOT EXISTS hook SCHEMAFULL;
                 DEFINE FIELD IF NOT EXISTS hid ON hook TYPE string;
                 DEFINE FIELD IF NOT EXISTS name ON hook TYPE string;
                 DEFINE FIELD IF NOT EXISTS owner ON hook TYPE string;
                 DEFINE FIELD IF NOT EXISTS room_id ON hook TYPE string;
                 DEFINE FIELD IF NOT EXISTS sender ON hook TYPE string;
                 DEFINE FIELD IF NOT EXISTS device_id ON hook TYPE string;
                 DEFINE FIELD IF NOT EXISTS access_token ON hook TYPE string;
                 DEFINE FIELD IF NOT EXISTS created_at ON hook TYPE datetime;
                 DEFINE INDEX IF NOT EXISTS hook_hid ON hook COLUMNS hid UNIQUE;
                 DEFINE INDEX IF NOT EXISTS hook_owner ON hook COLUMNS owner;",
            )
            .await?
            .check()?;
        Ok(())
    }

    /// Create a new hook owned by `owner`, bound to `room_id`, with an already
    /// provisioned per-hook session (`sender` localpart, `device_id`, `token`).
    #[allow(clippy::too_many_arguments)]
    pub async fn create_hook(
        &self,
        id: &str,
        name: &str,
        owner: &str,
        room_id: &str,
        sender: &str,
        device_id: &str,
        access_token: &str,
    ) -> Result<Hook> {
        self.select().await?;
        self.db
            .query(
                "CREATE hook SET hid = $hid, name = $name, owner = $owner,
                     room_id = $room_id, sender = $sender, device_id = $device_id,
                     access_token = $access_token, created_at = time::now()",
            )
            .bind(("hid", id.to_owned()))
            .bind(("name", name.to_owned()))
            .bind(("owner", owner.to_owned()))
            .bind(("room_id", room_id.to_owned()))
            .bind(("sender", sender.to_owned()))
            .bind(("device_id", device_id.to_owned()))
            .bind(("access_token", access_token.to_owned()))
            .await?
            .check()?;
        Ok(Hook {
            id: id.to_owned(),
            name: name.to_owned(),
            owner: owner.to_owned(),
            room_id: room_id.to_owned(),
            sender: sender.to_owned(),
            device_id: device_id.to_owned(),
            access_token: access_token.to_owned(),
        })
    }

    /// Replace a hook's session (device id + access token). Used when a fresh
    /// device must be minted because the persistent crypto store was lost.
    pub async fn set_session(&self, id: &str, device_id: &str, access_token: &str) -> Result<()> {
        self.select().await?;
        self.db
            .query("UPDATE hook SET device_id = $device_id, access_token = $access_token WHERE hid = $hid")
            .bind(("hid", id.to_owned()))
            .bind(("device_id", device_id.to_owned()))
            .bind(("access_token", access_token.to_owned()))
            .await?
            .check()?;
        Ok(())
    }

    /// List every hook (across all owners) — used at startup to bring up the
    /// per-hook clients.
    pub async fn all_hooks(&self) -> Result<Vec<Hook>> {
        self.select().await?;
        let mut res = self
            .db
            .query(
                "SELECT hid, name, owner, room_id, sender, device_id, access_token, created_at
                     FROM hook ORDER BY created_at",
            )
            .await?;
        let rows: Vec<HookRow> = res.take(0)?;
        Ok(rows.into_iter().map(Hook::from).collect())
    }

    /// Look up a hook by its id.
    pub async fn get_hook(&self, id: &str) -> Result<Option<Hook>> {
        self.select().await?;
        let mut res = self
            .db
            .query(
                "SELECT hid, name, owner, room_id, sender, device_id, access_token FROM hook
                     WHERE hid = $hid LIMIT 1",
            )
            .bind(("hid", id.to_owned()))
            .await?;
        let rows: Vec<HookRow> = res.take(0)?;
        Ok(rows.into_iter().next().map(Hook::from))
    }

    /// List every hook owned by `owner`, oldest first.
    pub async fn list_by_owner(&self, owner: &str) -> Result<Vec<Hook>> {
        self.select().await?;
        let mut res = self
            .db
            .query(
                "SELECT hid, name, owner, room_id, sender, device_id, access_token, created_at
                     FROM hook WHERE owner = $owner ORDER BY created_at",
            )
            .bind(("owner", owner.to_owned()))
            .await?;
        let rows: Vec<HookRow> = res.take(0)?;
        Ok(rows.into_iter().map(Hook::from).collect())
    }

    /// Delete a hook by UUID, but only if `owner` owns it. Returns whether a hook
    /// was deleted (false if it did not exist or belonged to someone else).
    pub async fn delete_hook(&self, id: &str, owner: &str) -> Result<bool> {
        let existing = self.get_hook(id).await?;
        match existing {
            Some(h) if h.owner == owner => {
                self.select().await?;
                self.db
                    .query("DELETE hook WHERE hid = $hid AND owner = $owner")
                    .bind(("hid", id.to_owned()))
                    .bind(("owner", owner.to_owned()))
                    .await?
                    .check()?;
                Ok(true)
            }
            _ => Ok(false),
        }
    }
}

/// Validate that `s` is a simple SurrealDB identifier (safe to interpolate into
/// a `DEFINE` statement, which cannot bind identifiers).
fn valid_ident(s: &str) -> Result<()> {
    if !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_')
        && s.chars().next().is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
    {
        Ok(())
    } else {
        bail!("must match [A-Za-z_][A-Za-z0-9_]*")
    }
}

/// Install the aws-lc-rs rustls crypto provider as the process default (once).
///
/// SurrealDB's `wss://` transport uses aws-lc-rs; binaries that also link
/// matrix-sdk pull a second provider (ring), and with both present rustls cannot
/// pick a default and panics on connect. Installing one explicitly fixes it; a
/// no-op if a provider is already installed. (Adapted from matrix-db.)
fn install_crypto_provider() {
    use std::sync::Once;
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn create_get_list_delete_roundtrip() {
        let store = Store::memory().await.unwrap();

        // Unknown id -> None.
        assert!(store.get_hook("nope").await.unwrap().is_none());

        let h = store
            .create_hook("id1", "alerts", "@alice:s", "!room:s", "hook_alerts_id1", "DEV1", "tok1")
            .await
            .unwrap();
        assert_eq!(h.name, "alerts");
        assert_eq!(h.owner, "@alice:s");
        assert_eq!(h.room_id, "!room:s");
        assert_eq!(h.id, "id1");
        assert_eq!(h.sender, "hook_alerts_id1");
        assert_eq!(h.device_id, "DEV1");
        assert_eq!(h.access_token, "tok1");

        // Round-trips by id.
        let got = store.get_hook(&h.id).await.unwrap().unwrap();
        assert_eq!(got, h);

        // set_session replaces device + token.
        store.set_session("id1", "DEV2", "tok2").await.unwrap();
        let got = store.get_hook("id1").await.unwrap().unwrap();
        assert_eq!(got.device_id, "DEV2");
        assert_eq!(got.access_token, "tok2");

        // A second hook for the same owner + one for another owner.
        let h2 = store
            .create_hook("id2", "deploys", "@alice:s", "!other:s", "hook_deploys_id2", "D", "t")
            .await
            .unwrap();
        store
            .create_hook("id3", "bob-hook", "@bob:s", "!bobroom:s", "hook_bobhook_id3", "D", "t")
            .await
            .unwrap();

        let alice = store.list_by_owner("@alice:s").await.unwrap();
        assert_eq!(alice.len(), 2);
        assert_eq!(alice[0].id, h.id);
        assert_eq!(alice[1].id, h2.id);
        assert_eq!(store.all_hooks().await.unwrap().len(), 3);

        // Non-owner cannot delete.
        assert!(!store.delete_hook(&h.id, "@bob:s").await.unwrap());
        assert!(store.get_hook(&h.id).await.unwrap().is_some());

        // Owner can delete.
        assert!(store.delete_hook(&h.id, "@alice:s").await.unwrap());
        assert!(store.get_hook(&h.id).await.unwrap().is_none());
        assert_eq!(store.list_by_owner("@alice:s").await.unwrap().len(), 1);

        // Deleting a gone hook is false.
        assert!(!store.delete_hook(&h.id, "@alice:s").await.unwrap());
    }
}
