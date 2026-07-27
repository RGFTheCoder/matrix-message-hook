//! The webhost: turn HTTP requests into Matrix messages, and read/search notes.
//!
//! Routes:
//! - `GET /` and `GET /health` — liveness.
//! - `GET /<uuid>/<message>` — deliver a short message carried in the path.
//! - `POST /<uuid>` — deliver a (longer) message carried in the request body.
//! - `GET /onboard/<uuid>` — this hook's usage instructions as plain text, so
//!   the creation reply can link to one URL instead of pasting curl examples
//!   inline (handy for an agent to fetch on demand).
//! - `POST /<uuid>/notes?persistent=<bool>&ttl=<secs>` — create a note.
//! - `GET /<uuid>/notes/<note_id>` — fetch any note back by id.
//! - `GET /<uuid>/notes/search?q=<text>&k=<n>&scope=<public_id>` — semantic
//!   search over notes.
//!
//! ## Authentication and the public/private id split
//!
//! `<uuid>` in every route above is a hook's **private id** — required on
//! every single request as authentication (it must belong to *some* real
//! hook, though not necessarily the one that owns whatever's being read).
//! Once authenticated this way:
//! - delivering a message / creating a note is owned by that hook;
//! - fetching a note by id, or an unscoped search, covers every note
//!   system-wide (the note id — or nothing at all, for search — is the only
//!   further scoping);
//! - a search's optional `scope` parameter is a **public id** (shared more
//!   freely; grants no write access at all) that narrows results to just one
//!   hook's notes.
//!
//! See `hook_core::store`'s module docs for the full reasoning.

use std::sync::Arc;

use axum::{
    Router,
    extract::{Path, Query, State},
    http::StatusCode,
    response::IntoResponse,
    routing::get,
};
use hook_core::{Config, Store};
use serde::Deserialize;
use tokio::sync::Semaphore;

use crate::clients::HookClients;

/// Cap on a delivered message's length (bytes). Matrix events can be large, but
/// webhook messages should be short; this bounds abuse.
const MAX_MESSAGE_BYTES: usize = 4000;

/// Cap on a note's content length (bytes). Notes are meant for storing longer
/// text (e.g. context for an agent) than a chat message, so this is well above
/// `MAX_MESSAGE_BYTES`, but still bounded.
const MAX_NOTE_BYTES: usize = 64 * 1024;

/// Cap on concurrent Matrix sends, so a webhook burst can't starve the sync /
/// E2EE work sharing this process.
const MAX_INFLIGHT_SENDS: usize = 8;

/// Default TTL for a temporary note if the caller doesn't specify one.
const DEFAULT_TTL_SECS: u64 = 24 * 60 * 60;

/// Bounds on a customizable TTL, so "temporary" can't be (ab)used as an
/// unbounded-retention store: at least a minute (else it's barely usable), at
/// most 30 days (else it's just a persistent note with extra steps — use
/// `persistent=true` for that).
const MIN_TTL_SECS: u64 = 60;
const MAX_TTL_SECS: u64 = 30 * 24 * 60 * 60;

/// Cap on live notes (temporary + persistent combined) per hook, so a leaked
/// private id can't be used to grow the DB without bound.
const MAX_NOTES_PER_HOOK: usize = 2000;

/// Default / max number of search results.
const DEFAULT_SEARCH_K: usize = 10;
const MAX_SEARCH_K: usize = 50;

/// Shared webhost state.
#[derive(Clone)]
pub struct WebState {
    store: Store,
    clients: HookClients,
    cfg: Arc<Config>,
    sem: Arc<Semaphore>,
}

impl WebState {
    /// Build web state from the shared store, per-hook client registry, config.
    pub fn new(store: Store, clients: HookClients, cfg: Arc<Config>) -> Self {
        Self {
            store,
            clients,
            cfg,
            sem: Arc::new(Semaphore::new(MAX_INFLIGHT_SENDS)),
        }
    }
}

/// Build the axum router.
pub fn router(state: WebState) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/health", get(health))
        .route("/onboard/{uuid}", get(onboard))
        .route("/{uuid}", get(get_no_message).post(post_body))
        .route("/{uuid}/notes", get(notes_no_id).post(create_note))
        .route("/{uuid}/notes/search", get(search_notes))
        .route("/{uuid}/notes/{note_id}", get(get_note))
        .route("/{uuid}/{*message}", get(get_with_message))
        .with_state(state)
}

async fn index(State(st): State<WebState>) -> impl IntoResponse {
    (
        StatusCode::OK,
        format!(
            "matrixHook\n\nPOST {base}/<uuid> with a body, or GET {base}/<uuid>/<message>\n",
            base = st.cfg.public_base_url.trim_end_matches('/'),
        ),
    )
}

async fn health() -> impl IntoResponse {
    (StatusCode::OK, "ok\n")
}

/// Look up a hook by its private id, for authentication. Every route in this
/// file (other than `/`/`/health`) must pass this before doing anything else.
async fn authenticate(st: &WebState, private_id: &str) -> Result<hook_core::Hook, (StatusCode, String)> {
    match st.store.get_hook(private_id).await {
        Ok(Some(h)) => Ok(h),
        Ok(None) => Err((StatusCode::NOT_FOUND, "unknown hook\n".to_owned())),
        Err(e) => {
            tracing::warn!("get_hook failed: {e}");
            Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal error\n".to_owned(),
            ))
        }
    }
}

/// `GET /<uuid>` with no message: tell the caller how to send one.
async fn get_no_message(Path(_uuid): Path<String>) -> impl IntoResponse {
    (
        StatusCode::BAD_REQUEST,
        "provide a message: GET /<uuid>/<message> or POST /<uuid> with a body\n\
         (or fetch /onboard/<uuid> for full usage instructions)\n",
    )
}

/// `GET /onboard/<uuid>`: this hook's usage instructions, as plain text. This
/// is what the bot links to after creating a hook, instead of pasting the
/// curl examples inline — one URL an agent or human can fetch on demand.
async fn onboard(State(st): State<WebState>, Path(uuid): Path<String>) -> impl IntoResponse {
    let hook = match authenticate(&st, &uuid).await {
        Ok(h) => h,
        Err(e) => return e,
    };

    let url = hook_core::webhook_url(&st.cfg.public_base_url, &hook.id);
    let body = format!(
        "matrixHook — hook **{name}**\n\n\
         Trigger it (posts appear in the room where it was created, as \
         **{name}**, end-to-end encrypted):\n\
         - POST a body:  curl -X POST {url} -d 'your message here'\n\
         - or GET with a short message in the path:  curl {url}/your%20short%20message\n\n\
         Notes (small bits of text, not posted to the room). `{uuid}` (private) \
         authenticates every request below; a note itself is only gated by its \
         own id — any request with a valid private id can read/search ANY \
         note system-wide, not just ones created under this hook:\n\
         - create:  curl -X POST {url}/notes -d 'note content'\n\
         - create, persistent (default is temporary, 1 day TTL):  \
           curl -X POST '{url}/notes?persistent=true' -d 'note content'\n\
         - create, custom TTL in seconds:  curl -X POST '{url}/notes?ttl=3600' -d 'note content'\n\
         - fetch by id:  curl {url}/notes/<note_id>\n\
         - search (system-wide):  curl '{url}/notes/search?q=your+query'\n\
         - search (scoped to one hook's notes via its PUBLIC id):  \
           curl '{url}/notes/search?q=your+query&scope={public_id}'\n\n\
         Public id: `{public_id}` — share this (not the private id above) to let \
         something search/read only this hook's notes, with no write access at all.\n\n\
         Constraints:\n\
         - message must be non-empty and under {max_msg} bytes\n\
         - note content must be non-empty and under {max_note} bytes\n\
         - anyone with the private id or URL can post here, or read/create/search \
           any note — keep it secret\n",
        name = hook.name,
        public_id = hook.public_id,
        max_msg = MAX_MESSAGE_BYTES,
        max_note = MAX_NOTE_BYTES,
    );
    (StatusCode::OK, body)
}

/// `GET /<uuid>/<message>`: deliver the path-carried message.
async fn get_with_message(
    State(st): State<WebState>,
    Path((uuid, message)): Path<(String, String)>,
) -> impl IntoResponse {
    deliver(&st, &uuid, message).await
}

/// `POST /<uuid>`: deliver the body-carried message.
async fn post_body(
    State(st): State<WebState>,
    Path(uuid): Path<String>,
    body: String,
) -> impl IntoResponse {
    deliver(&st, &uuid, body).await
}

/// Validate `raw`, look up the hook, and deliver the message into its room.
async fn deliver(st: &WebState, uuid: &str, raw: String) -> (StatusCode, String) {
    let message = raw.trim();
    if message.is_empty() {
        return (StatusCode::BAD_REQUEST, "empty message\n".to_owned());
    }
    if message.len() > MAX_MESSAGE_BYTES {
        return (
            StatusCode::PAYLOAD_TOO_LARGE,
            format!("message too large (max {MAX_MESSAGE_BYTES} bytes)\n"),
        );
    }

    let hook = match authenticate(st, uuid).await {
        Ok(h) => h,
        Err(e) => return e,
    };

    // Deliver as the hook's own E2EE user (display name = the hook name), so
    // each source appears as a distinct, encrypted sender. Bound concurrent
    // sends; the permit is held until the send completes.
    let _permit = st.sem.acquire().await.ok();
    match st.clients.deliver(&hook, message).await {
        Ok(()) => (StatusCode::OK, "delivered\n".to_owned()),
        Err(e) => {
            tracing::warn!("delivery failed for hook {uuid}: {e}");
            (StatusCode::BAD_GATEWAY, "delivery failed\n".to_owned())
        }
    }
}

/// `GET /<uuid>/notes` with no note id: tell the caller how to create/fetch one.
async fn notes_no_id(Path(_uuid): Path<String>) -> impl IntoResponse {
    (
        StatusCode::BAD_REQUEST,
        "POST /<uuid>/notes with a body to create a note, \
         GET /<uuid>/notes/<note_id> to fetch one, or \
         GET /<uuid>/notes/search?q=<text> to search\n",
    )
}

#[derive(Deserialize)]
struct CreateNoteParams {
    /// `?persistent=true` never expires. Default (absent or `false`) is
    /// temporary, TTL'd (see `ttl`).
    #[serde(default)]
    persistent: bool,
    /// TTL in seconds for a temporary note; ignored if `persistent=true`.
    /// Clamped to `[MIN_TTL_SECS, MAX_TTL_SECS]`.
    ttl: Option<u64>,
}

/// `POST /<uuid>/notes?persistent=<bool>&ttl=<secs>`: create a note owned by
/// this hook. Embeds the content via Ollama best-effort — a failed/slow embed
/// never blocks or fails note creation, it just means the note isn't
/// searchable yet (still fetchable by id).
async fn create_note(
    State(st): State<WebState>,
    Path(uuid): Path<String>,
    Query(params): Query<CreateNoteParams>,
    body: String,
) -> impl IntoResponse {
    let content = body.trim();
    if content.is_empty() {
        return (StatusCode::BAD_REQUEST, "empty note content\n".to_owned());
    }
    if content.len() > MAX_NOTE_BYTES {
        return (
            StatusCode::PAYLOAD_TOO_LARGE,
            format!("note too large (max {MAX_NOTE_BYTES} bytes)\n"),
        );
    }

    if let Err(e) = authenticate(&st, &uuid).await {
        return e;
    }

    match st.store.count_live_notes(&uuid).await {
        Ok(n) if n >= MAX_NOTES_PER_HOOK => {
            return (
                StatusCode::TOO_MANY_REQUESTS,
                format!(
                    "too many notes for this hook (max {MAX_NOTES_PER_HOOK}); \
                     delete some or wait for temporary ones to expire\n"
                ),
            );
        }
        Ok(_) => {}
        Err(e) => {
            tracing::warn!("count_live_notes failed: {e}");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal error\n".to_owned(),
            );
        }
    }

    let embedding = match hook_core::embed::embed(&st.cfg.ollama_url, &st.cfg.ollama_embed_model, content).await
    {
        Ok(v) => Some(v),
        Err(e) => {
            tracing::warn!("embedding note failed (note will still be created, unsearchable for now): {e}");
            None
        }
    };

    let ttl_secs = if params.persistent {
        None
    } else {
        Some(
            params
                .ttl
                .unwrap_or(DEFAULT_TTL_SECS)
                .clamp(MIN_TTL_SECS, MAX_TTL_SECS),
        )
    };
    let prefix = if params.persistent { "p_" } else { "t_" };
    let id = format!("{prefix}{}", hook_core::id::gen(16));

    match st.store.create_note(&id, &uuid, content, embedding, ttl_secs).await {
        Ok(note) => {
            let suffix = match ttl_secs {
                Some(secs) => format!(" (temporary, expires in {secs}s)"),
                None => String::new(),
            };
            (StatusCode::CREATED, format!("note created: id={}{suffix}\n", note.id))
        }
        Err(e) => {
            tracing::warn!("create_note failed: {e}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal error\n".to_owned(),
            )
        }
    }
}

/// `GET /<uuid>/notes/<note_id>`: fetch a note's content. `<uuid>` only
/// authenticates the caller (any valid private id) — it need not be the hook
/// that created the note.
async fn get_note(
    State(st): State<WebState>,
    Path((uuid, note_id)): Path<(String, String)>,
) -> impl IntoResponse {
    if let Err(e) = authenticate(&st, &uuid).await {
        return e;
    }
    match st.store.get_note(&note_id).await {
        Ok(Some(note)) => (StatusCode::OK, note.content),
        Ok(None) => (StatusCode::NOT_FOUND, "unknown or expired note\n".to_owned()),
        Err(e) => {
            tracing::warn!("get_note failed: {e}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal error\n".to_owned(),
            )
        }
    }
}

#[derive(Deserialize)]
struct SearchParams {
    q: String,
    k: Option<usize>,
    /// A hook's PUBLIC id — narrows the search to just that hook's notes.
    /// Omitted = search every note system-wide.
    scope: Option<String>,
}

/// `GET /<uuid>/notes/search?q=<text>&k=<n>&scope=<public_id>`: semantic
/// search over notes. `<uuid>` authenticates the caller; `scope`, if given,
/// is a hook's PUBLIC id (never its private one) and narrows results to that
/// hook's notes only.
async fn search_notes(State(st): State<WebState>, Path(uuid): Path<String>, Query(params): Query<SearchParams>) -> impl IntoResponse {
    if let Err(e) = authenticate(&st, &uuid).await {
        return e;
    }
    let q = params.q.trim();
    if q.is_empty() {
        return (StatusCode::BAD_REQUEST, "empty query (?q=...)\n".to_owned());
    }
    let k = params.k.unwrap_or(DEFAULT_SEARCH_K).clamp(1, MAX_SEARCH_K);

    let scope_hook_id = match &params.scope {
        Some(public_id) => match st.store.get_hook_by_public_id(public_id).await {
            Ok(Some(h)) => Some(h.id),
            Ok(None) => return (StatusCode::BAD_REQUEST, "unknown scope (public id)\n".to_owned()),
            Err(e) => {
                tracing::warn!("get_hook_by_public_id failed: {e}");
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "internal error\n".to_owned(),
                );
            }
        },
        None => None,
    };

    let query_embedding =
        match hook_core::embed::embed(&st.cfg.ollama_url, &st.cfg.ollama_embed_model, q).await {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!("embedding search query failed: {e}");
                return (
                    StatusCode::SERVICE_UNAVAILABLE,
                    "couldn't embed the search query (embedding service unavailable); try again\n"
                        .to_owned(),
                );
            }
        };

    match st
        .store
        .search_notes(&query_embedding, k, scope_hook_id.as_deref())
        .await
    {
        Ok(hits) if hits.is_empty() => (StatusCode::OK, "no matching notes\n".to_owned()),
        Ok(hits) => {
            let mut out = String::new();
            for hit in hits {
                out.push_str(&format!(
                    "{:.4}  {}  {}\n",
                    hit.score,
                    hit.note.id,
                    hit.note.content.lines().next().unwrap_or("")
                ));
            }
            (StatusCode::OK, out)
        }
        Err(e) => {
            tracing::warn!("search_notes failed: {e}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal error\n".to_owned(),
            )
        }
    }
}
