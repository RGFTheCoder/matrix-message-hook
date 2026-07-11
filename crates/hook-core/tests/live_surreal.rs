//! Live integration test: the SurrealDB client in `hook-core` against a real
//! SurrealDB **server** (the same 3.x version deployed on the shared host).
//!
//! It spawns the local `surreal` binary and exercises the real store end to end,
//! proving the client↔server path works (not just the embedded `mem://` engine
//! the unit tests use).
//!
//! It is skipped automatically when no `surreal` binary is on `PATH` (e.g. the
//! nix build sandbox), so it never fails a hermetic check.

use std::process::{Child, Command};
use std::time::Duration;

use hook_core::Store;

/// A spawned `surreal start` server, killed on drop.
struct Server {
    child: Child,
    port: u16,
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Pick a free TCP port by binding to :0 and releasing it.
fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// Start an in-memory 3.x `surreal` server with root creds, or return `None` if
/// the binary is not installed.
fn start_server() -> Option<Server> {
    let port = free_port();
    let child = Command::new("surreal")
        .args([
            "start",
            "--username",
            "root",
            "--password",
            "root",
            "--bind",
            &format!("127.0.0.1:{port}"),
            "--no-banner",
            "memory",
        ])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .ok()?;
    Some(Server { child, port })
}

/// Wait until a `Store` can connect to the server (or give up after ~15s).
async fn connect_with_retry(port: u16) -> Store {
    let url = format!("ws://127.0.0.1:{port}");
    let mut last_err = None;
    for _ in 0..60 {
        match Store::connect(&url, "matrixHook_test", "hooks", "root", "root").await {
            Ok(store) => return store,
            Err(e) => last_err = Some(e),
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    panic!("could not connect to spawned surreal server: {last_err:?}");
}

#[tokio::test]
async fn client_against_server_roundtrip() {
    let Some(server) = start_server() else {
        eprintln!("SKIP: `surreal` binary not found on PATH");
        return;
    };

    let store = connect_with_retry(server.port).await;

    // Full CRUD roundtrip against the real 3.x server.
    let h = store
        .create_hook("liveid1", "live-alerts", "@alice:s", "!room:s", "hook_livealerts_liveid1", "D", "t")
        .await
        .expect("create_hook");
    assert_eq!(h.name, "live-alerts");
    assert!(!h.id.is_empty());

    let got = store.get_hook(&h.id).await.expect("get_hook").expect("some");
    assert_eq!(got, h);

    store
        .create_hook("liveid2", "live-deploys", "@alice:s", "!other:s", "hook_livedeploys_liveid2", "D", "t")
        .await
        .expect("second create");
    let listed = store.list_by_owner("@alice:s").await.expect("list");
    assert_eq!(listed.len(), 2);

    assert!(
        !store.delete_hook(&h.id, "@bob:s").await.expect("del non-owner"),
        "non-owner must not delete"
    );
    assert!(
        store.delete_hook(&h.id, "@alice:s").await.expect("del owner"),
        "owner must delete"
    );
    assert!(store.get_hook(&h.id).await.expect("get after del").is_none());

    drop(server);
}
