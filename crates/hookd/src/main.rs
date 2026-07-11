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
mod clients;
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

    let store = connect_store_with_retry(&cfg)
        .await
        .context("connecting to SurrealDB")?;
    let client = hook_core::client::connect(&cfg)
        .await
        .context("connecting matrix client")?;
    let appservice = AppService::new(&cfg.homeserver, &cfg.as_token, &cfg.server_name);
    let hook_clients =
        clients::HookClients::new(store.clone(), appservice.clone(), cfg.clone(), client.clone());

    // Install handlers, then do one initial sync so room state (and any pending
    // invites) are processed before we start serving webhooks.
    bot::install(
        &client,
        store.clone(),
        appservice.clone(),
        hook_clients.clone(),
        cfg.clone(),
    );
    bot::auto_join_on_invite(&client);
    if let Err(e) = client.sync_once(SyncSettings::default()).await {
        tracing::warn!("initial sync failed: {e}");
    }
    bot::join_pending_invites(&client).await;

    // Continuous sync in the background. RETRY on error — a transient homeserver
    // outage (e.g. Synapse restarting to pick up config) must NOT take hookd
    // down. This loop never returns, so it can't trigger a shutdown.
    let sync_client = client.clone();
    tokio::spawn(async move {
        loop {
            if let Err(e) = sync_client.sync(SyncSettings::default()).await {
                tracing::warn!("matrix sync errored, retrying in 5s: {e}");
                tokio::time::sleep(std::time::Duration::from_secs(5)).await;
            }
        }
    });

    // Bring up the per-hook E2EE clients (each spawned; non-blocking).
    hook_clients.start_all().await;

    // Serve the webhost on the main task. Only a webhost failure ends the
    // process (with an error, so systemd's Restart=on-failure applies).
    let app = web::router(web::WebState::new(store, hook_clients, cfg.clone()));
    let listener = TcpListener::bind(&cfg.bind_addr)
        .await
        .with_context(|| format!("binding {}", cfg.bind_addr))?;
    tracing::info!("webhost listening on {}", cfg.bind_addr);
    axum::serve(listener, app).await.context("webhost error")?;
    Ok(())
}

/// Connect to SurrealDB, retrying with backoff. The service starts alongside the
/// SurrealDB container (systemd ordering can't guarantee the DB *inside* it is
/// listening yet), so a few transient "connection refused" attempts are normal
/// and shouldn't fail the unit.
async fn connect_store_with_retry(cfg: &Config) -> Result<Store> {
    let mut delay = std::time::Duration::from_secs(1);
    let mut last_err = None;
    for attempt in 1..=12 {
        match Store::connect(
            &cfg.surreal_url,
            &cfg.db_namespace,
            &cfg.db_name,
            &cfg.surreal_user,
            &cfg.surreal_pass,
        )
        .await
        {
            Ok(store) => return Ok(store),
            Err(e) => {
                tracing::warn!("SurrealDB connect attempt {attempt}/12 failed: {e}");
                last_err = Some(e);
                tokio::time::sleep(delay).await;
                delay = (delay * 2).min(std::time::Duration::from_secs(10));
            }
        }
    }
    Err(last_err.unwrap()).context("SurrealDB unreachable after retries")
}
