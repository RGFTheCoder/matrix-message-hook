//! Shared library for the `matrixHook` webhook integration.
//!
//! `matrixHook` lets any user on the homeserver DM the bot, create a named
//! **hook**, and receive a UUID. Posting to `…/<uuid>` (or `…/<uuid>/<message>`)
//! then delivers that message into the Matrix room where the hook was created.
//!
//! This crate holds the pieces the service (`hookd`), the bootstrap CLI
//! (`hook-admin`), and the end-to-end test (`fake-human`) all share:
//! - [`config`]: process/`.env` configuration.
//! - [`store`]: the SurrealDB hook store (embedded, `surrealkv://` in prod /
//!   `mem://` in tests).
//! - [`client`]: building the matrix-sdk client and sending messages.
//! - [`admin`]: Synapse account bootstrap (shared-secret registration + login).
//! - [`command`]: parsing the chat command grammar and building webhook URLs.
//!
//! Design patterns here are adapted from the sibling `../../matrix` workspace
//! (matrix-common, matrix-db, botmaster) but this crate is standalone — it has
//! no botmaster trust gate, since any user may create a hook.

pub mod admin;
pub mod client;
pub mod command;
pub mod config;
pub mod store;

pub use command::{Command, webhook_url};
pub use config::Config;
pub use store::{Hook, Store};
