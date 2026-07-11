//! `hookd`: the matrixHook service.
//!
//! A single process that runs BOTH faces of the integration, sharing one matrix
//! client and one embedded hook store:
//! - the **matrix bot** — any user DMs it to create/list/delete hooks;
//! - the **webhost** — `GET /<uuid>/<message>` and `POST /<uuid>` deliver a
//!   message into the room the hook was created in.
//!
//! One process is deliberate: the embedded SurrealDB store holds an exclusive
//! file lock, and both faces need the same hook→room mapping.

// matrix-sdk's sync future is deeply nested; proving it `Send` for `tokio::spawn`
// needs a higher auto-trait recursion limit than the default.
#![recursion_limit = "512"]

mod bot;
mod sender;
mod web;

use std::sync::Arc;

use anyhow::{Context, Result};
use hook_core::{AppService, Config, Store};
use matrix_sdk::config::SyncSettings;
use tokio::net::TcpListener;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| {
                "warn,hookd=info,hook_core=info,matrix_sdk_crypto::backups=off".into()
            }),
        )
        .init();

    let cfg = Arc::new(Config::from_env()?);
    tracing::info!(user = %cfg.user_id, bind = %cfg.bind_addr, "starting hookd");

    let store = Store::connect(
        &cfg.surreal_url,
        &cfg.db_namespace,
        &cfg.db_name,
        &cfg.surreal_user,
        &cfg.surreal_pass,
    )
    .await
    .context("connecting to SurrealDB")?;
    let client = hook_core::client::connect(&cfg)
        .await
        .context("connecting matrix client")?;
    let appservice = AppService::new(&cfg.homeserver, &cfg.as_token, &cfg.server_name);

    // Install handlers, then do one initial sync so room state (and any pending
    // invites) are processed before we start serving webhooks.
    bot::install(&client, store.clone(), appservice.clone(), cfg.clone());
    bot::auto_join_on_invite(&client);
    if let Err(e) = client.sync_once(SyncSettings::default()).await {
        tracing::warn!("initial sync failed: {e}");
    }
    bot::join_pending_invites(&client).await;

    // Continuous sync in the background (drives E2EE + inbound messages).
    let sync_client = client.clone();
    let sync_task = tokio::spawn(async move {
        if let Err(e) = sync_client.sync(SyncSettings::default()).await {
            tracing::error!("matrix sync loop exited: {e}");
        }
    });

    // Serve the webhost on the main task.
    let app = web::router(web::WebState::new(store, client, appservice, cfg.clone()));
    let listener = TcpListener::bind(&cfg.bind_addr)
        .await
        .with_context(|| format!("binding {}", cfg.bind_addr))?;
    tracing::info!("webhost listening on {}", cfg.bind_addr);
    let serve = axum::serve(listener, app);

    tokio::select! {
        r = serve => r.context("webhost error")?,
        _ = sync_task => tracing::error!("sync task ended; shutting down"),
    }
    Ok(())
}
