//! The webhost: turn HTTP requests into Matrix messages.
//!
//! Routes:
//! - `GET /` and `GET /health` — liveness.
//! - `GET /<uuid>/<message>` — deliver a short message carried in the path.
//! - `POST /<uuid>` — deliver a (longer) message carried in the request body.
//!
//! The UUID is the only secret. Delivered messages are sent as plain text,
//! prefixed with the hook's name, so a leaked URL cannot be used to post
//! arbitrary unattributed text as the bot.

use std::sync::Arc;

use axum::{
    Router,
    extract::{Path, State},
    http::StatusCode,
    response::IntoResponse,
    routing::get,
};
use hook_core::{Config, Store};
use tokio::sync::Semaphore;

use crate::clients::HookClients;

/// Cap on a delivered message's length (bytes). Matrix events can be large, but
/// webhook messages should be short; this bounds abuse.
const MAX_MESSAGE_BYTES: usize = 4000;

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
        .route("/{uuid}", get(get_no_message).post(post_body))
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
        "provide a message: GET /<uuid>/<message> or POST /<uuid> with a body\n",
    )
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
