//! `hook-admin`: one-time bootstrap for the matrixHook bot account.
//!
//! `setup` registers the bot's (non-admin) Matrix account using the Synapse
//! `registration_shared_secret` (read from the nix-sys sops file), logs in to
//! mint a device id + access token, and prints a ready-to-use `.env` for
//! `hookd`. `login` re-mints a session from an existing password.
//!
//! It deliberately never touches the hook store — that file is owned
//! exclusively by the running `hookd` process.

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use hook_core::admin;

/// Default nix-sys sops file holding the registration shared secret.
const DEFAULT_SOPS_FILE: &str = "/home/linx/Documents/Projects/nix/nix-sys/secrets/matrix.yaml";
/// Default internal admin/registration homeserver address.
const DEFAULT_ADMIN_HS: &str = "http://10.1.1.30:8008";
/// Default public homeserver URL written into the printed `.env`.
const DEFAULT_PUBLIC_HS: &str = "https://matrix.damastacoda.dev";
/// Default server name (the `:server` part of the MXID).
const DEFAULT_SERVER_NAME: &str = "damastacoda.dev";

#[derive(Parser)]
#[command(name = "hook-admin", about = "Bootstrap the matrixHook bot account")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Register the bot account via the shared secret, log in, and print `.env`.
    Setup {
        /// Bot localpart (the part before `:server`).
        #[arg(long, default_value = "matrixhook")]
        localpart: String,
        /// nix-sys sops file with the `registration_shared_secret`.
        #[arg(long, default_value = DEFAULT_SOPS_FILE)]
        sops_file: String,
        /// Internal admin/registration homeserver address.
        #[arg(long, default_value = DEFAULT_ADMIN_HS)]
        admin_hs: String,
        /// Public homeserver URL to write into the `.env`.
        #[arg(long, default_value = DEFAULT_PUBLIC_HS)]
        public_hs: String,
        /// Server name (the `:server` part of the MXID).
        #[arg(long, default_value = DEFAULT_SERVER_NAME)]
        server_name: String,
    },
    /// Re-mint a session from an existing password and print an updated `.env`.
    Login {
        /// Account password.
        #[arg(long, env = "MATRIX_PASSWORD")]
        password: String,
        /// Bot localpart.
        #[arg(long, default_value = "matrixhook")]
        localpart: String,
        /// Public homeserver URL (used for login and written into the `.env`).
        #[arg(long, default_value = DEFAULT_PUBLIC_HS)]
        public_hs: String,
        /// Server name (the `:server` part of the MXID).
        #[arg(long, default_value = DEFAULT_SERVER_NAME)]
        server_name: String,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "warn,hook_admin=info".into()),
        )
        .init();

    match Cli::parse().command {
        Command::Setup {
            localpart,
            sops_file,
            admin_hs,
            public_hs,
            server_name,
        } => {
            let secret = admin::read_registration_secret(&sops_file)
                .context("reading registration_shared_secret")?;
            let password = admin::random_secret(48);

            admin::register_shared_secret(&admin_hs, &secret, &localpart, &password, false)
                .await
                .context("registering the bot account")?;
            // Log in against the admin (internal) homeserver to mint the session;
            // the public URL is what hookd uses at runtime.
            let login = admin::login_password(&admin_hs, &localpart, &password, "matrixhook")
                .await
                .context("logging in the new account")?;

            print_env(&public_hs, &login.user_id, &login.access_token, &login.device_id);
            eprintln!(
                "\n# Account @{localpart}:{server_name} created. Password (store safely):\n# {password}"
            );
        }
        Command::Login {
            password,
            localpart,
            public_hs,
            server_name,
        } => {
            let login = admin::login_password(&public_hs, &localpart, &password, "matrixhook")
                .await
                .context("logging in")?;
            print_env(&public_hs, &login.user_id, &login.access_token, &login.device_id);
            let _ = server_name;
        }
    }
    Ok(())
}

/// Print a `.env` fragment for `hookd`.
fn print_env(homeserver: &str, user_id: &str, token: &str, device_id: &str) {
    println!("MATRIX_HOMESERVER={homeserver}");
    println!("MATRIX_USER={user_id}");
    println!("MATRIX_ACCESS_TOKEN={token}");
    println!("MATRIX_DEVICE_ID={device_id}");
}
