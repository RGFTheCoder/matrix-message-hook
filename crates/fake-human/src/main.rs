//! `fake-human`: an end-to-end self-test for matrixHook.
//!
//! It registers a throwaway account (via the Synapse `registration_shared_secret`
//! read from the nix-sys sops file), logs in as a real E2EE matrix-sdk client,
//! DMs the hook bot to create a hook, extracts the hook's UUID from the reply,
//! then POSTs and GETs the webhook and asserts the delivered messages arrive
//! back in the DM room. Exits 0 on success, 1 on failure — so it can drive an
//! automated test loop.
//!
//! Env:
//! - `MATRIX_HOMESERVER`        (default `https://matrix.damastacoda.dev`)
//! - `MATRIX_ADMIN_HOMESERVER`  registration/login endpoint (default
//!   `http://10.1.1.30:8008`)
//! - `SERVER_NAME`              MXID server part (default `damastacoda.dev`)
//! - `TARGET_BOT`               the hook bot (default `@matrixhook:damastacoda.dev`)
//! - `SOPS_FILE`                nix-sys sops file with the shared secret
//! - `HOOK_TEST_BASE_URL`       where to send webhook HTTP (default
//!   `https://matrixHook.damastacoda.dev`; set to the local bind, e.g.
//!   `http://127.0.0.1:8480`, when testing a locally-run `hookd`)
//! - `TIMEOUT_SECS`             per-step wait (default 60)

#![recursion_limit = "512"]

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use hook_core::admin;
use hook_core::command::webhook_url;
use matrix_sdk::authentication::matrix::MatrixSession;
use matrix_sdk::config::SyncSettings;
use matrix_sdk::event_handler::Ctx;
use matrix_sdk::ruma::events::room::message::{MessageType, OriginalSyncRoomMessageEvent};
use matrix_sdk::ruma::{OwnedUserId, UserId};
use matrix_sdk::{Client, SessionMeta, SessionTokens};
use tokio::sync::Mutex;

/// Collected message bodies seen from the target bot.
type Seen = Arc<Mutex<Vec<String>>>;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| {
                "warn,fake_human=info,matrix_sdk_crypto::backups=off".into()
            }),
        )
        .init();

    let homeserver = env("MATRIX_HOMESERVER", "https://matrix.damastacoda.dev");
    let admin_hs = env("MATRIX_ADMIN_HOMESERVER", "http://10.1.1.30:8008");
    let server_name = env("SERVER_NAME", "damastacoda.dev");
    let target: OwnedUserId = env("TARGET_BOT", "@matrixhook:damastacoda.dev")
        .parse()
        .context("invalid TARGET_BOT")?;
    let sops_file = env(
        "SOPS_FILE",
        "/home/linx/Documents/Projects/nix/nix-sys/secrets/matrix.yaml",
    );
    let test_base = env("HOOK_TEST_BASE_URL", "https://matrixHook.damastacoda.dev");
    let timeout_secs: u64 = env("TIMEOUT_SECS", "60").parse().unwrap_or(60);

    let nonce = nonce();

    // 1. Register a throwaway account via the shared secret.
    let localpart = format!("hooktest-{nonce}");
    let mxid = format!("@{localpart}:{server_name}");
    let password = admin::random_secret(48);
    tracing::info!("registering throwaway {mxid}");
    let secret = admin::read_registration_secret(&sops_file)
        .context("reading registration_shared_secret")?;
    admin::register_shared_secret(&admin_hs, &secret, &localpart, &password, false)
        .await
        .context("registering test account")?;
    let login = admin::login_password(&admin_hs, &localpart, &password, "fake-human")
        .await
        .context("logging in test account")?;

    // 2. Build an E2EE client and restore the session.
    let store_dir = std::env::temp_dir().join(format!("fake-human-{nonce}"));
    let _ = std::fs::create_dir_all(&store_dir);
    let client = Client::builder()
        .homeserver_url(&homeserver)
        .sqlite_store(&store_dir, None)
        .build()
        .await
        .context("building matrix client")?;
    let user_id: OwnedUserId = mxid.parse()?;
    client
        .restore_session(MatrixSession {
            meta: SessionMeta {
                user_id: user_id.clone(),
                device_id: login.device_id.as_str().into(),
            },
            tokens: SessionTokens {
                access_token: login.access_token.clone(),
                refresh_token: None,
            },
        })
        .await
        .context("restoring session")?;

    // 3. Watch for messages in the room. Collect from ALL senders: the bot's
    //    reply (the hook id) comes from @matrixhook, but webhook deliveries come
    //    from the per-hook virtual @hook_* user. Skip our own messages.
    let me = user_id.clone();
    let seen: Seen = Arc::new(Mutex::new(Vec::new()));
    client.add_event_handler_context(WatchCtx { seen: seen.clone() });
    client.add_event_handler(
        move |ev: OriginalSyncRoomMessageEvent, ctx: Ctx<WatchCtx>| {
            let me = me.clone();
            async move {
                if ev.sender == me {
                    return;
                }
                if let MessageType::Text(t) = ev.content.msgtype {
                    ctx.seen.lock().await.push(t.body);
                }
            }
        },
    );

    // 4. Initial sync, then open a DM with the bot.
    client.sync_once(SyncSettings::default()).await?;
    let sync_client = client.clone();
    let sync_task = tokio::spawn(async move {
        let _ = sync_client.sync(SyncSettings::default()).await;
    });
    let outcome = run_checks(&client, &target, &seen, &test_base, &nonce, timeout_secs).await;
    sync_task.abort();

    // 5. Best-effort cleanup of the throwaway account.
    if let Err(e) = deactivate(&homeserver, &localpart, &password).await {
        tracing::warn!("could not deactivate {mxid}: {e}");
    }

    match outcome {
        Ok(()) => {
            println!("TEST PASS: matrixHook delivered both POST and GET webhooks to the DM");
            Ok(())
        }
        Err(e) => {
            println!("TEST FAIL: {e:#}");
            std::process::exit(1);
        }
    }
}

/// The actual assertions, factored out so cleanup always runs.
async fn run_checks(
    client: &Client,
    target: &UserId,
    seen: &Seen,
    test_base: &str,
    nonce: &str,
    timeout_secs: u64,
) -> Result<()> {
    let room = match client.get_dm_room(target) {
        Some(room) => room,
        None => client.create_dm(target).await.context("creating DM")?,
    };
    tracing::info!("DM room {} with {target}", room.room_id());

    // Give the bot a moment to auto-join and exchange keys.
    tokio::time::sleep(Duration::from_secs(3)).await;

    // Ask the bot to create a hook.
    let hook_name = format!("selftest-{nonce}");
    room.send(matrix_sdk::ruma::events::room::message::RoomMessageEventContent::text_plain(
        format!("new {hook_name}"),
    ))
    .await
    .context("sending create command")?;

    // Wait for the reply and extract the UUID.
    let uuid = wait_for_map(seen, timeout_secs, extract_uuid)
        .await
        .context("bot did not reply with a hook UUID")?;
    tracing::info!("hook uuid = {uuid}");
    let url = webhook_url(test_base, &uuid);

    let http = reqwest::Client::new();

    // POST check.
    let post_probe = format!("post-probe-{nonce}");
    let resp = http
        .post(&url)
        .body(post_probe.clone())
        .send()
        .await
        .context("POST webhook")?;
    if !resp.status().is_success() {
        bail!("POST {url} returned {}", resp.status());
    }
    wait_for(seen, timeout_secs, |b| b.contains(&post_probe))
        .await
        .with_context(|| format!("POST probe {post_probe:?} never arrived in the room"))?;

    // GET check (message carried in the path).
    let get_probe = format!("get-probe-{nonce}");
    let get_url = format!("{url}/{get_probe}");
    let resp = http.get(&get_url).send().await.context("GET webhook")?;
    if !resp.status().is_success() {
        bail!("GET {get_url} returned {}", resp.status());
    }
    wait_for(seen, timeout_secs, |b| b.contains(&get_probe))
        .await
        .with_context(|| format!("GET probe {get_probe:?} never arrived in the room"))?;

    Ok(())
}

/// Context for the message watcher.
#[derive(Clone)]
struct WatchCtx {
    seen: Seen,
}

/// Poll the collected bot messages until `pred` matches one, or time out.
async fn wait_for(seen: &Seen, timeout_secs: u64, pred: impl Fn(&str) -> bool) -> Result<()> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(timeout_secs);
    loop {
        if seen.lock().await.iter().any(|b| pred(b)) {
            return Ok(());
        }
        if tokio::time::Instant::now() >= deadline {
            bail!("timed out after {timeout_secs}s");
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

/// Poll the collected bot messages until `f` extracts a value from one, or time
/// out.
async fn wait_for_map<T>(
    seen: &Seen,
    timeout_secs: u64,
    f: impl Fn(&str) -> Option<T>,
) -> Result<T> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(timeout_secs);
    loop {
        if let Some(v) = seen.lock().await.iter().find_map(|b| f(b)) {
            return Ok(v);
        }
        if tokio::time::Instant::now() >= deadline {
            bail!("timed out after {timeout_secs}s");
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

/// Extract the hook id from a bot reply of the form ``… id: `<id>` …``.
fn extract_uuid(body: &str) -> Option<String> {
    let after = body.split("id: `").nth(1)?;
    let uuid = after.split('`').next()?.trim();
    if uuid.is_empty() {
        None
    } else {
        Some(uuid.to_owned())
    }
}

/// Deactivate the throwaway account via a UIAA password flow (cleanup).
async fn deactivate(homeserver: &str, localpart: &str, password: &str) -> Result<()> {
    // Re-login to get a fresh token, then deactivate with password auth.
    let login = admin::login_password(homeserver, localpart, password, "fake-human-cleanup").await?;
    let http = reqwest::Client::new();
    let url = format!(
        "{}/_matrix/client/v3/account/deactivate",
        homeserver.trim_end_matches('/')
    );
    let resp = http
        .post(url)
        .bearer_auth(&login.access_token)
        .json(&serde_json::json!({
            "auth": {
                "type": "m.login.password",
                "identifier": { "type": "m.id.user", "user": localpart },
                "password": password,
            },
            "erase": true,
        }))
        .send()
        .await?;
    if !resp.status().is_success() {
        return Err(anyhow!("deactivate returned {}", resp.status()));
    }
    Ok(())
}

/// Read an env var or fall back to `default`.
fn env(key: &str, default: &str) -> String {
    std::env::var(key)
        .ok()
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| default.to_owned())
}

/// A short unique token for this run.
fn nonce() -> String {
    let n = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{n}")
}
