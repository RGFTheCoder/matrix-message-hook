//! The webhost: turn HTTP requests into Matrix messages.
//!
//! Routes:
//! - `GET /` and `GET /health` — liveness.
//! - `GET /<uuid>/<message>` — deliver a short message carried in the path.
//! - `POST /<uuid>` — deliver a (longer) message carried in the request body.
//! - `GET /onboard/<uuid>` — this hook's usage instructions as plain text, so
//!   the creation reply can link to one URL instead of pasting curl examples
//!   inline (handy for an agent to fetch on demand).
//! - `POST /<uuid>/notes` — create a note (temporary, in-memory + TTL'd by
//!   default, or persistent/SurrealDB-backed with `?persistent=true`).
//! - `GET /<uuid>/notes/<note_id>` — fetch a note back by id.
//!
//! The UUID is the only secret. Delivered messages are sent as plain text,
//! prefixed with the hook's name, so a leaked URL cannot be used to post
//! arbitrary unattributed text as the bot.

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
use crate::notes::TempNotes;

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

/// Shared webhost state.
#[derive(Clone)]
pub struct WebState {
    store: Store,
    clients: HookClients,
    cfg: Arc<Config>,
    sem: Arc<Semaphore>,
    temp_notes: TempNotes,
}

impl WebState {
    /// Build web state from the shared store, per-hook client registry,
    /// config, and the temporary-notes registry.
    pub fn new(store: Store, clients: HookClients, cfg: Arc<Config>, temp_notes: TempNotes) -> Self {
        Self {
            store,
            clients,
            cfg,
            sem: Arc::new(Semaphore::new(MAX_INFLIGHT_SENDS)),
            temp_notes,
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
    let hook = match st.store.get_hook(&uuid).await {
        Ok(Some(h)) => h,
        Ok(None) => return (StatusCode::NOT_FOUND, "unknown hook\n".to_owned()),
        Err(e) => {
            tracing::warn!("get_hook failed: {e}");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal error\n".to_owned(),
            );
        }
    };

    let url = hook_core::webhook_url(&st.cfg.public_base_url, &hook.id);
    let body = format!(
        "matrixHook — hook **{name}**\n\n\
         Trigger it (posts appear in the room where it was created, as \
         **{name}**, end-to-end encrypted):\n\
         - POST a body:  curl -X POST {url} -d 'your message here'\n\
         - or GET with a short message in the path:  curl {url}/your%20short%20message\n\n\
         Notes (small bits of text stored under this hook, not posted to the room):\n\
         - create:  curl -X POST {url}/notes -d 'note content'\n\
         - create, persistent (default is temporary, 1 day TTL):  \
           curl -X POST '{url}/notes?persistent=true' -d 'note content'\n\
         - create, custom TTL in seconds:  curl -X POST '{url}/notes?ttl=3600' -d 'note content'\n\
         - fetch:  curl {url}/notes/<note_id>\n\n\
         Constraints:\n\
         - message must be non-empty and under {max_msg} bytes\n\
         - note content must be non-empty and under {max_note} bytes\n\
         - anyone with this URL can post here or read/create its notes — keep it secret\n",
        name = hook.name,
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

    let hook = match st.store.get_hook(uuid).await {
        Ok(Some(h)) => h,
        Ok(None) => return (StatusCode::NOT_FOUND, "unknown hook\n".to_owned()),
        Err(e) => {
            tracing::warn!("get_hook failed: {e}");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal error\n".to_owned(),
            );
        }
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
        "POST /<uuid>/notes with a body to create a note, or \
         GET /<uuid>/notes/<note_id> to fetch one\n",
    )
}

#[derive(Deserialize)]
struct CreateNoteParams {
    /// `?persistent=true` stores the note in SurrealDB (durable, no expiry).
    /// Default (absent or `false`) is temporary: in-memory only, TTL'd.
    #[serde(default)]
    persistent: bool,
    /// TTL in seconds for a temporary note; ignored if `persistent=true`.
    /// Clamped to `[TempNotes::MIN_TTL, TempNotes::MAX_TTL]`.
    ttl: Option<u64>,
}

/// `POST /<uuid>/notes?persistent=<bool>&ttl=<secs>`: create a note under this
/// hook. The hook must exist (same trust boundary as delivering a message).
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

    // A note can only be created under a hook that actually exists — same
    // check as delivery, and it's what scopes note ownership.
    match st.store.get_hook(&uuid).await {
        Ok(Some(_)) => {}
        Ok(None) => return (StatusCode::NOT_FOUND, "unknown hook\n".to_owned()),
        Err(e) => {
            tracing::warn!("get_hook failed: {e}");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal error\n".to_owned(),
            );
        }
    }

    if params.persistent {
        let id = format!("p_{}", hook_core::id::gen(16));
        match st.store.create_note(&id, &uuid, content).await {
            Ok(note) => (
                StatusCode::CREATED,
                format!("note created: id={}\n", note.id),
            ),
            Err(e) => {
                tracing::warn!("create_note failed: {e}");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "internal error\n".to_owned(),
                )
            }
        }
    } else {
        let ttl = TempNotes::clamp_ttl(params.ttl.unwrap_or(crate::notes::DEFAULT_TTL.as_secs()));
        match st.temp_notes.create(&uuid, content.to_owned(), ttl).await {
            Some(id) => (
                StatusCode::CREATED,
                format!(
                    "note created: id={id} (temporary, expires in {}s)\n",
                    ttl.as_secs()
                ),
            ),
            None => (
                StatusCode::TOO_MANY_REQUESTS,
                "too many temporary notes for this hook; delete some or wait for them \
                 to expire\n"
                    .to_owned(),
            ),
        }
    }
}

/// `GET /<uuid>/notes/<note_id>`: fetch a note's content. Dispatches to the
/// temporary or persistent backend based on the id's prefix.
async fn get_note(
    State(st): State<WebState>,
    Path((uuid, note_id)): Path<(String, String)>,
) -> impl IntoResponse {
    if note_id.starts_with(crate::notes::TEMP_PREFIX) {
        return match st.temp_notes.get(&note_id, &uuid).await {
            Some(content) => (StatusCode::OK, content),
            None => (StatusCode::NOT_FOUND, "unknown or expired note\n".to_owned()),
        };
    }
    match st.store.get_note(&note_id, &uuid).await {
        Ok(Some(note)) => (StatusCode::OK, note.content),
        Ok(None) => (StatusCode::NOT_FOUND, "unknown note\n".to_owned()),
        Err(e) => {
            tracing::warn!("get_note failed: {e}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal error\n".to_owned(),
            )
        }
    }
}


