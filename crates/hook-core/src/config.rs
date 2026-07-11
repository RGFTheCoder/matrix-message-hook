//! Configuration loaded from the process environment (and an optional `.env`).
//!
//! Matrix credentials follow the same contract as the sibling bots workspace:
//! the E2EE session is restored from `(user_id, device_id, access_token)`, so no
//! password is needed at runtime. The remaining vars configure the embedded hook
//! store and the webhost.

use std::path::PathBuf;

use anyhow::{Context, Result};

/// Everything `hookd` needs to run: matrix credentials, the hook store location,
/// and the webhost bind address + public base URL.
#[derive(Clone, Debug)]
pub struct Config {
    /// Homeserver base URL (e.g. `https://matrix.damastacoda.dev`).
    pub homeserver: String,
    /// Full bot user id (e.g. `@matrixhook:damastacoda.dev`).
    pub user_id: String,
    /// Access token bound to [`Config::device_id`].
    pub access_token: String,
    /// Device id the token belongs to (required to restore an E2EE session).
    pub device_id: String,
    /// Directory for the persistent matrix-sdk crypto/state store. Persisting
    /// this keeps the bot's device identity + room keys stable across restarts,
    /// which E2EE relies on.
    pub store_path: PathBuf,
    /// SurrealDB connection URL for the shared control-plane server (e.g.
    /// `wss://surrealdb.damastacoda.dev`, or `ws://127.0.0.1:8000` for a local
    /// test server).
    pub surreal_url: String,
    /// SurrealDB root username (empty for an unauthenticated embedded engine).
    pub surreal_user: String,
    /// SurrealDB root password.
    pub surreal_pass: String,
    /// SurrealDB namespace to use (isolates matrixHook from other tenants of the
    /// shared server).
    pub db_namespace: String,
    /// SurrealDB database name to use.
    pub db_name: String,
    /// Address the webhost binds to (e.g. `127.0.0.1:8480`).
    pub bind_addr: String,
    /// Public base URL the webhost is reached at, used to build hook URLs handed
    /// back to users (e.g. `https://matrixHook.damastacoda.dev`).
    pub public_base_url: String,
    /// Appservice `as_token`: authorizes posting as the per-hook virtual
    /// `@hook_*` users (must match the Synapse appservice registration).
    pub as_token: String,
    /// Server name (the domain part of MXIDs), e.g. `damastacoda.dev`.
    pub server_name: String,
}

impl Config {
    /// Load config from the environment, first sourcing a `.env` if present.
    ///
    /// Required: `MATRIX_HOMESERVER`, `MATRIX_USER`, `MATRIX_ACCESS_TOKEN`,
    /// `MATRIX_DEVICE_ID`, `SURREAL_USER`, `SURREAL_PASS`, `AS_TOKEN`. Optional
    /// (with defaults): `MATRIX_STORE_PATH`, `SURREAL_URL`, `SURREAL_NS`,
    /// `SURREAL_DB`, `HOOK_BIND_ADDR`, `HOOK_PUBLIC_BASE_URL`, `SERVER_NAME`.
    pub fn from_env() -> Result<Self> {
        let _ = dotenvy::dotenv();
        Ok(Self {
            homeserver: req("MATRIX_HOMESERVER")?,
            user_id: req("MATRIX_USER")?,
            access_token: req("MATRIX_ACCESS_TOKEN")?,
            device_id: req("MATRIX_DEVICE_ID")?,
            store_path: opt("MATRIX_STORE_PATH")
                .unwrap_or_else(|| "./store".to_owned())
                .into(),
            surreal_url: opt("SURREAL_URL")
                .unwrap_or_else(|| "wss://surrealdb.damastacoda.dev".to_owned()),
            surreal_user: req("SURREAL_USER")?,
            surreal_pass: req("SURREAL_PASS")?,
            db_namespace: opt("SURREAL_NS").unwrap_or_else(|| "matrixHook".to_owned()),
            db_name: opt("SURREAL_DB").unwrap_or_else(|| "hooks".to_owned()),
            bind_addr: opt("HOOK_BIND_ADDR").unwrap_or_else(|| "127.0.0.1:8480".to_owned()),
            public_base_url: opt("HOOK_PUBLIC_BASE_URL")
                .unwrap_or_else(|| "https://matrixHook.damastacoda.dev".to_owned()),
            as_token: req("AS_TOKEN")?,
            server_name: opt("SERVER_NAME").unwrap_or_else(|| "damastacoda.dev".to_owned()),
        })
    }
}

/// Read a required environment variable, erroring (with the key name) if absent
/// or empty.
fn req(key: &str) -> Result<String> {
    std::env::var(key)
        .ok()
        .filter(|v| !v.is_empty())
        .with_context(|| format!("required environment variable {key} is not set"))
}

/// Read an optional environment variable, treating empty as absent.
fn opt(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.is_empty())
}
