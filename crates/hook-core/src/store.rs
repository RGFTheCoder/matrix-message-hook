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
//!
//! ## Notes and the public/private id split
//!
//! Every hook has two ids: the original **private id** (the webhook URL
//! secret — required as the first path segment on every API request, acting
//! as the caller's authentication) and a **public id** (`public_id`, shown
//! alongside the private one), which can be shared more freely to let someone
//! *search* a hook's notes without granting them any write access.
//!
//! Notes themselves are NOT scoped to a hook for reads: any request
//! authenticated with a valid private id can fetch any note system-wide by
//! its note id (the note id itself is the only thing gating a direct read),
//! and unscoped search covers every note system-wide too. A `scope` (a
//! hook's public id) narrows a search to just that one hook's notes.
//! Only note *creation* is owned by the authenticating hook.
//!
//! Notes are a single table regardless of "temporary" vs "persistent" —
//! temporary notes just have a non-`NONE` `expires_at`, cleaned up both
//! lazily (excluded from any read whose expiry has passed) and by a periodic
//! sweep (see `hookd`'s startup wiring). This means temporary notes get
//! embedded and are searchable too, and survive a `hookd` restart (unlike an
//! in-memory-only design).

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
    /// The hook's private id (the secret in its URL; required on every API
    /// request as authentication).
    pub id: String,
    /// The hook's public id: safe to share for read/search access to this
    /// hook's notes (via `scope=`), but grants no write capability at all.
    pub public_id: String,
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

/// A note: small piece of text created under some hook (the authenticating
/// private id at creation time), readable/searchable system-wide by anyone
/// holding any valid private id (see the module docs for the full model).
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Note {
    /// The note's short id — `t_`/`p_` prefixed to signal temporary vs
    /// persistent to the human reading it (cosmetic only; both live in the
    /// same table and are looked up identically).
    pub id: String,
    /// The hook this note was created under.
    pub hook_id: String,
    pub content: String,
    /// Present once a background/creation-time embed call has succeeded;
    /// `None` means the note exists but isn't searchable yet (e.g. Ollama was
    /// briefly unreachable when it was created).
    pub embedding: Option<Vec<f32>>,
}

/// A search hit: a [`Note`] plus its cosine-similarity score (higher = closer).
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct NoteHit {
    pub note: Note,
    pub score: f32,
}

/// SurrealValue view of a stored `hook` row. `hid` is selected explicitly so it
/// never collides with SurrealDB's reserved record `id`. `public_id` is
/// `Option` because rows created before this field existed have it as `NONE`
/// until `Store::backfill_public_ids` fixes them up.
#[derive(Debug, SurrealValue)]
struct HookRow {
    hid: String,
    public_id: Option<String>,
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
            public_id: r.public_id.unwrap_or_default(),
            name: r.name,
            owner: r.owner,
            room_id: r.room_id,
            sender: r.sender,
            device_id: r.device_id,
            access_token: r.access_token,
        }
    }
}

/// SurrealValue view of a stored `note` row. `nid` is selected explicitly so it
/// never collides with SurrealDB's reserved record `id` (same reasoning as
/// `HookRow::hid`).
#[derive(Debug, SurrealValue)]
struct NoteRow {
    nid: String,
    hook_id: String,
    content: String,
    embedding: Option<Vec<f32>>,
}

impl From<NoteRow> for Note {
    fn from(r: NoteRow) -> Self {
        Note {
            id: r.nid,
            hook_id: r.hook_id,
            content: r.content,
            embedding: r.embedding,
        }
    }
}

#[derive(Debug, SurrealValue)]
struct NoteHitRow {
    nid: String,
    hook_id: String,
    content: String,
    embedding: Option<Vec<f32>>,
    score: f32,
}

impl From<NoteHitRow> for NoteHit {
    fn from(r: NoteHitRow) -> Self {
        NoteHit {
            note: Note {
                id: r.nid,
                hook_id: r.hook_id,
                content: r.content,
                embedding: r.embedding,
            },
            score: r.score,
        }
    }
}

/// Handle to the hook store.
#[derive(Clone)]
pub struct Store {
    db: Surreal<Any>,
    namespace: String,
    database: String,
    /// Dimension of `note.embedding`'s HNSW index — must match whatever
    /// embedding model is configured (e.g. 1024 for `qwen3-embedding:0.6b`).
    /// SurrealDB enforces this strictly: writing a vector of any other length
    /// errors. Tests use a small dimension since they write literal test
    /// vectors, not real model output.
    vector_dim: usize,
}

impl Store {
    /// Connect to the shared SurrealDB server at `url`, authenticate as root,
    /// and select `namespace`/`database` (defining them + the schema up front,
    /// all idempotent). `vector_dim` must match the configured embedding
    /// model's output dimension.
    pub async fn connect(
        url: &str,
        namespace: &str,
        database: &str,
        user: &str,
        pass: &str,
        vector_dim: usize,
    ) -> Result<Self> {
        Self::open(url, namespace, database, Some((user, pass)), vector_dim).await
    }

    /// Open an in-memory store for tests (no server, no auth). Uses a small
    /// fixed vector dimension (8) since tests write literal test vectors, not
    /// real embedding-model output.
    pub async fn memory() -> Result<Self> {
        Self::open("mem://", NAMESPACE, "hooks", None, 8).await
    }

    /// Shared connection path for both the remote and in-memory engines.
    async fn open(
        url: &str,
        namespace: &str,
        database: &str,
        auth: Option<(&str, &str)>,
        vector_dim: usize,
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
            vector_dim,
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

    /// Define the `hook` and `note` tables (schemaful) + indexes (idempotent).
    /// `hook.public_id` is `option<string>` (not required) so existing rows
    /// from before this field existed remain valid until
    /// [`Store::backfill_public_ids`] fills them in. `note.embedding` gets an
    /// HNSW index for semantic search; `note.expires_at` is
    /// `option<datetime>` (`NONE` = persistent, never expires).
    async fn migrate(&self) -> Result<()> {
        // DIMENSION can't be a bound parameter in a DEFINE statement (same
        // restriction as the namespace/database names above), so it's
        // interpolated — safe since it's a validated Rust usize, never
        // user input.
        let vector_dim = self.vector_dim;
        self.db
            .query(format!(
                "DEFINE TABLE IF NOT EXISTS hook SCHEMAFULL;
                 DEFINE FIELD IF NOT EXISTS hid ON hook TYPE string;
                 DEFINE FIELD IF NOT EXISTS public_id ON hook TYPE option<string>;
                 DEFINE FIELD IF NOT EXISTS name ON hook TYPE string;
                 DEFINE FIELD IF NOT EXISTS owner ON hook TYPE string;
                 DEFINE FIELD IF NOT EXISTS room_id ON hook TYPE string;
                 DEFINE FIELD IF NOT EXISTS sender ON hook TYPE string;
                 DEFINE FIELD IF NOT EXISTS device_id ON hook TYPE string;
                 DEFINE FIELD IF NOT EXISTS access_token ON hook TYPE string;
                 DEFINE FIELD IF NOT EXISTS created_at ON hook TYPE datetime;
                 DEFINE INDEX IF NOT EXISTS hook_hid ON hook COLUMNS hid UNIQUE;
                 DEFINE INDEX IF NOT EXISTS hook_public_id ON hook COLUMNS public_id UNIQUE;
                 DEFINE INDEX IF NOT EXISTS hook_owner ON hook COLUMNS owner;
                 DEFINE TABLE IF NOT EXISTS note SCHEMAFULL;
                 DEFINE FIELD IF NOT EXISTS nid ON note TYPE string;
                 DEFINE FIELD IF NOT EXISTS hook_id ON note TYPE string;
                 DEFINE FIELD IF NOT EXISTS content ON note TYPE string;
                 DEFINE FIELD IF NOT EXISTS embedding ON note TYPE option<array<float>>;
                 DEFINE FIELD IF NOT EXISTS expires_at ON note TYPE option<datetime>;
                 DEFINE FIELD IF NOT EXISTS created_at ON note TYPE datetime;
                 DEFINE INDEX IF NOT EXISTS note_nid ON note COLUMNS nid UNIQUE;
                 DEFINE INDEX IF NOT EXISTS note_hook ON note COLUMNS hook_id;
                 DEFINE INDEX IF NOT EXISTS note_vec ON note FIELDS embedding
                     HNSW DIMENSION {vector_dim} DIST COSINE;"
            ))
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
        public_id: &str,
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
                "CREATE hook SET hid = $hid, public_id = $public_id, name = $name,
                     owner = $owner, room_id = $room_id, sender = $sender,
                     device_id = $device_id, access_token = $access_token,
                     created_at = time::now()",
            )
            .bind(("hid", id.to_owned()))
            .bind(("public_id", public_id.to_owned()))
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
            public_id: public_id.to_owned(),
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
                "SELECT hid, public_id, name, owner, room_id, sender, device_id, access_token, created_at
                     FROM hook ORDER BY created_at",
            )
            .await?;
        let rows: Vec<HookRow> = res.take(0)?;
        Ok(rows.into_iter().map(Hook::from).collect())
    }

    /// Look up a hook by its private id.
    pub async fn get_hook(&self, id: &str) -> Result<Option<Hook>> {
        self.select().await?;
        let mut res = self
            .db
            .query(
                "SELECT hid, public_id, name, owner, room_id, sender, device_id, access_token
                     FROM hook WHERE hid = $hid LIMIT 1",
            )
            .bind(("hid", id.to_owned()))
            .await?;
        let rows: Vec<HookRow> = res.take(0)?;
        Ok(rows.into_iter().next().map(Hook::from))
    }

    /// Look up a hook by its public id — used to resolve a search `scope`.
    pub async fn get_hook_by_public_id(&self, public_id: &str) -> Result<Option<Hook>> {
        self.select().await?;
        let mut res = self
            .db
            .query(
                "SELECT hid, public_id, name, owner, room_id, sender, device_id, access_token
                     FROM hook WHERE public_id = $public_id LIMIT 1",
            )
            .bind(("public_id", public_id.to_owned()))
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
                "SELECT hid, public_id, name, owner, room_id, sender, device_id, access_token, created_at
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

    /// Assign a fresh public id to every hook that doesn't have one yet
    /// (rows created before this field existed). Returns how many were
    /// fixed up. Safe to call on every startup — a no-op once everything is
    /// backfilled.
    pub async fn backfill_public_ids(&self) -> Result<usize> {
        self.select().await?;
        let mut res = self
            .db
            .query("SELECT hid FROM hook WHERE public_id = NONE")
            .await?;
        #[derive(Debug, SurrealValue)]
        struct HidOnly {
            hid: String,
        }
        let rows: Vec<HidOnly> = res.take(0)?;
        let n = rows.len();
        for row in rows {
            let public_id = crate::id::gen(16);
            self.db
                .query("UPDATE hook SET public_id = $public_id WHERE hid = $hid")
                .bind(("hid", row.hid))
                .bind(("public_id", public_id))
                .await?
                .check()?;
        }
        Ok(n)
    }

    /// Create a note under `hook_id` with the given `id` (already generated
    /// by the caller, `p_`/`t_` prefixed). `embedding` is best-effort — pass
    /// `None` if the Ollama call failed or was skipped; the note still
    /// exists and is readable by id, just not searchable until backfilled.
    /// `ttl_secs` is `None` for a persistent note (never expires), or
    /// `Some(secs)` for a temporary one.
    pub async fn create_note(
        &self,
        id: &str,
        hook_id: &str,
        content: &str,
        embedding: Option<Vec<f32>>,
        ttl_secs: Option<u64>,
    ) -> Result<Note> {
        self.select().await?;
        // Two branches (rather than one query with a conditional SET) because
        // binding `NONE` as `$embedding`/computing `expires_at` inline reads
        // more clearly this way, and avoids surrealdb query-macro edge cases
        // with optional bound values.
        let query = match ttl_secs {
            Some(_) => {
                "CREATE note SET nid = $nid, hook_id = $hook_id, content = $content,
                     embedding = $embedding, expires_at = time::now() + duration::from_secs($ttl_secs),
                     created_at = time::now()"
            }
            None => {
                "CREATE note SET nid = $nid, hook_id = $hook_id, content = $content,
                     embedding = $embedding, expires_at = NONE, created_at = time::now()"
            }
        };
        let mut q = self
            .db
            .query(query)
            .bind(("nid", id.to_owned()))
            .bind(("hook_id", hook_id.to_owned()))
            .bind(("content", content.to_owned()))
            .bind(("embedding", embedding.clone()));
        if let Some(ttl) = ttl_secs {
            q = q.bind(("ttl_secs", ttl));
        }
        q.await?.check()?;
        Ok(Note {
            id: id.to_owned(),
            hook_id: hook_id.to_owned(),
            content: content.to_owned(),
            embedding,
        })
    }

    /// Look up a note by id alone — no hook-ownership check. Any caller
    /// already authenticated with a valid private id can read any note (see
    /// module docs). Excludes expired temporary notes.
    pub async fn get_note(&self, id: &str) -> Result<Option<Note>> {
        self.select().await?;
        let mut res = self
            .db
            .query(
                "SELECT nid, hook_id, content, embedding FROM note
                     WHERE nid = $nid AND (expires_at IS NONE OR expires_at > time::now())
                     LIMIT 1",
            )
            .bind(("nid", id.to_owned()))
            .await?;
        let rows: Vec<NoteRow> = res.take(0)?;
        Ok(rows.into_iter().next().map(Note::from))
    }

    /// Semantic search over notes. If `scope_hook_id` is `Some`, results are
    /// restricted to that one hook's notes; if `None`, searches every note
    /// system-wide. Excludes expired temporary notes and notes with no
    /// embedding yet.
    pub async fn search_notes(
        &self,
        query_embedding: &[f32],
        k: usize,
        scope_hook_id: Option<&str>,
    ) -> Result<Vec<NoteHit>> {
        self.select().await?;
        // EF (candidate-list size) at 4x k, floor 64, is a reasonable default
        // recall/speed trade-off for a corpus this size; not user-tunable to
        // keep the API surface small.
        let ef = (k * 4).max(64);
        let base = "SELECT nid, hook_id, content, embedding,
                        vector::similarity::cosine(embedding, $q) AS score
                     FROM note
                     WHERE (expires_at IS NONE OR expires_at > time::now())
                       AND embedding IS NOT NONE";
        let query = if scope_hook_id.is_some() {
            format!(
                "{base} AND hook_id = $scope AND embedding <|{k},{ef}|> $q
                 ORDER BY score DESC"
            )
        } else {
            format!(
                "{base} AND embedding <|{k},{ef}|> $q
                 ORDER BY score DESC"
            )
        };
        let mut q = self.db.query(query).bind(("q", query_embedding.to_vec()));
        if let Some(scope) = scope_hook_id {
            q = q.bind(("scope", scope.to_owned()));
        }
        let mut res = q.await?.check()?;
        let rows: Vec<NoteHitRow> = res.take(0)?;
        Ok(rows.into_iter().map(NoteHit::from).collect())
    }

    /// Delete every temporary note whose TTL has elapsed. Intended to run
    /// periodically (see `hookd`'s startup wiring) so expired notes don't
    /// linger forever if nothing ever reads them (which would otherwise be
    /// the only place expiry is enforced).
    pub async fn sweep_expired_notes(&self) -> Result<usize> {
        self.select().await?;
        let mut res = self
            .db
            .query("DELETE note WHERE expires_at IS NOT NONE AND expires_at < time::now() RETURN BEFORE")
            .await?
            .check()?;
        let rows: Vec<serde_json::Value> = res.take(0)?;
        Ok(rows.len())
    }

    /// Count live (non-expired) notes under `hook_id` — used to enforce a
    /// per-hook cap against unbounded note creation.
    pub async fn count_live_notes(&self, hook_id: &str) -> Result<usize> {
        self.select().await?;
        #[derive(Debug, SurrealValue)]
        struct CountRow {
            count: usize,
        }
        let mut res = self
            .db
            .query(
                "SELECT count() FROM note
                     WHERE hook_id = $hook_id AND (expires_at IS NONE OR expires_at > time::now())
                     GROUP ALL",
            )
            .bind(("hook_id", hook_id.to_owned()))
            .await?;
        let rows: Vec<CountRow> = res.take(0)?;
        Ok(rows.first().map(|r| r.count).unwrap_or(0))
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
            .create_hook("id1", "pub1", "alerts", "@alice:s", "!room:s", "hook_alerts_id1", "DEV1", "tok1")
            .await
            .unwrap();
        assert_eq!(h.name, "alerts");
        assert_eq!(h.owner, "@alice:s");
        assert_eq!(h.room_id, "!room:s");
        assert_eq!(h.id, "id1");
        assert_eq!(h.public_id, "pub1");
        assert_eq!(h.sender, "hook_alerts_id1");
        assert_eq!(h.device_id, "DEV1");
        assert_eq!(h.access_token, "tok1");

        // Round-trips by id.
        let got = store.get_hook(&h.id).await.unwrap().unwrap();
        assert_eq!(got, h);

        // Round-trips by public id too.
        let got = store.get_hook_by_public_id("pub1").await.unwrap().unwrap();
        assert_eq!(got, h);
        assert!(store.get_hook_by_public_id("nope").await.unwrap().is_none());

        // set_session replaces device + token.
        store.set_session("id1", "DEV2", "tok2").await.unwrap();
        let got = store.get_hook("id1").await.unwrap().unwrap();
        assert_eq!(got.device_id, "DEV2");
        assert_eq!(got.access_token, "tok2");

        // A second hook for the same owner + one for another owner.
        let h2 = store
            .create_hook("id2", "pub2", "deploys", "@alice:s", "!other:s", "hook_deploys_id2", "D", "t")
            .await
            .unwrap();
        store
            .create_hook("id3", "pub3", "bob-hook", "@bob:s", "!bobroom:s", "hook_bobhook_id3", "D", "t")
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

    #[tokio::test]
    async fn backfill_public_ids_fixes_up_legacy_rows() {
        let store = Store::memory().await.unwrap();
        // Simulate a pre-migration row (no public_id) by writing directly.
        store
            .db
            .query(
                "CREATE hook SET hid = 'legacy1', name='n', owner='@a:s', room_id='!r:s',
                     sender='s', device_id='d', access_token='t', created_at=time::now()",
            )
            .await
            .unwrap()
            .check()
            .unwrap();
        assert_eq!(store.get_hook("legacy1").await.unwrap().unwrap().public_id, "");

        let fixed = store.backfill_public_ids().await.unwrap();
        assert_eq!(fixed, 1);
        let got = store.get_hook("legacy1").await.unwrap().unwrap();
        assert!(!got.public_id.is_empty());

        // Idempotent: nothing left to backfill.
        assert_eq!(store.backfill_public_ids().await.unwrap(), 0);
    }

    #[tokio::test]
    async fn notes_are_readable_by_any_caller_but_owned_at_creation() {
        let store = Store::memory().await.unwrap();

        // Unknown id -> None.
        assert!(store.get_note("nope").await.unwrap().is_none());

        let n = store
            .create_note("p_note1", "hookA", "hello world", None, None)
            .await
            .unwrap();
        assert_eq!(n.id, "p_note1");
        assert_eq!(n.hook_id, "hookA");
        assert_eq!(n.content, "hello world");
        assert_eq!(n.embedding, None);

        // Readable without needing to know/present the owning hook id at all
        // (the auth check that a *private* id is valid happens one layer up,
        // in hookd — the store layer only knows about the note id).
        let got = store.get_note("p_note1").await.unwrap().unwrap();
        assert_eq!(got, n);

        // A second note under a different hook is equally readable.
        store
            .create_note("p_note2", "hookB", "other content", None, None)
            .await
            .unwrap();
        assert_eq!(
            store.get_note("p_note2").await.unwrap().unwrap().content,
            "other content"
        );
    }

    #[tokio::test]
    async fn temporary_notes_expire_and_get_swept() {
        let store = Store::memory().await.unwrap();

        // A note that already "expired" (ttl in the past isn't expressible
        // via ttl_secs, so use 0s and just wait past it).
        store
            .create_note("t_soon", "hookA", "short-lived", None, Some(0))
            .await
            .unwrap();
        store
            .create_note("p_forever", "hookA", "long-lived", None, None)
            .await
            .unwrap();

        tokio::time::sleep(std::time::Duration::from_millis(1100)).await;

        // Lazily excluded from get_note once expired.
        assert!(store.get_note("t_soon").await.unwrap().is_none());
        assert!(store.get_note("p_forever").await.unwrap().is_some());

        // Sweep physically removes it.
        let removed = store.sweep_expired_notes().await.unwrap();
        assert_eq!(removed, 1);
        assert!(store.get_note("p_forever").await.unwrap().is_some());
    }

    #[tokio::test]
    async fn search_scopes_by_hook_and_excludes_unembedded_or_expired() {
        let store = Store::memory().await.unwrap();

        // Orthogonal unit vectors (padded to the test store's 8-dim index)
        // so cosine similarity is unambiguous.
        store
            .create_note(
                "p_a",
                "hookA",
                "about cats",
                Some(vec![1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0]),
                None,
            )
            .await
            .unwrap();
        store
            .create_note(
                "p_b",
                "hookA",
                "about dogs",
                Some(vec![0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0]),
                None,
            )
            .await
            .unwrap();
        store
            .create_note(
                "p_c",
                "hookB",
                "also about cats",
                Some(vec![0.9, 0.1, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0]),
                None,
            )
            .await
            .unwrap();
        // No embedding yet -> never a search hit.
        store
            .create_note("p_d", "hookA", "not yet embedded", None, None)
            .await
            .unwrap();

        let q = [1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0];

        // Unscoped: sees notes from both hooks.
        let hits = store.search_notes(&q, 10, None).await.unwrap();
        let ids: Vec<_> = hits.iter().map(|h| h.note.id.clone()).collect();
        assert!(ids.contains(&"p_a".to_owned()));
        assert!(ids.contains(&"p_c".to_owned()));
        assert!(!ids.contains(&"p_d".to_owned()));
        // Closest match first.
        assert_eq!(hits[0].note.id, "p_a");

        // Scoped to hookA: hookB's note is excluded even though it'd rank well.
        let hits = store.search_notes(&q, 10, Some("hookA")).await.unwrap();
        let ids: Vec<_> = hits.iter().map(|h| h.note.id.clone()).collect();
        assert!(ids.contains(&"p_a".to_owned()));
        assert!(!ids.contains(&"p_c".to_owned()));
    }
}
