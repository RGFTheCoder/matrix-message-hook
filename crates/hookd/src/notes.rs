//! In-memory registry for **temporary** notes.
//!
//! Temporary notes never touch SurrealDB — they live only in this process's
//! memory and are gone on restart or expiry, whichever comes first. Persistent
//! notes (durable, SurrealDB-backed) are handled directly by
//! [`hook_core::Store`]; this module only covers the ephemeral half.
//!
//! Note ids are prefixed to make the two backends unambiguous from the id
//! alone: temporary notes are `t_<id>`, persistent notes are `p_<id>` (see
//! `hook_core::store::Note`) — a `GET` handler can dispatch straight to the
//! right backend without guessing or double-querying.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use hook_core::id;
use tokio::sync::Mutex;

/// Prefix marking a note id as temporary (in-memory only).
pub const TEMP_PREFIX: &str = "t_";

/// Default time-to-live for a temporary note if the caller doesn't specify one.
pub const DEFAULT_TTL: Duration = Duration::from_secs(24 * 60 * 60);

/// Bounds on the customizable TTL, so "temporary" can't be (ab)used as an
/// unbounded-retention store: at least a minute (else it's barely usable), at
/// most 30 days (else it's just a persistent note with extra steps — use
/// `persistent=true` for that).
pub const MIN_TTL: Duration = Duration::from_secs(60);
pub const MAX_TTL: Duration = Duration::from_secs(30 * 24 * 60 * 60);

/// Cap on live temporary notes per hook, so a leaked hook URL can't be used to
/// grow this process's memory without bound (same spirit as the webhost's
/// message-size and concurrency caps).
const MAX_TEMP_NOTES_PER_HOOK: usize = 500;

struct TempNote {
    hook_id: String,
    content: String,
    expires_at: Instant,
}

/// Shared, cloneable handle to the temporary-notes registry.
#[derive(Clone)]
pub struct TempNotes {
    inner: Arc<Mutex<HashMap<String, TempNote>>>,
}

impl TempNotes {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Clamp a caller-provided TTL (seconds) into `[MIN_TTL, MAX_TTL]`.
    pub fn clamp_ttl(requested_secs: u64) -> Duration {
        Duration::from_secs(requested_secs).clamp(MIN_TTL, MAX_TTL)
    }

    /// Create a temporary note under `hook_id`, expiring after `ttl`. Returns
    /// the new note's id (already `t_`-prefixed), or `None` if `hook_id`
    /// already has `MAX_TEMP_NOTES_PER_HOOK` live (non-expired) notes.
    pub async fn create(&self, hook_id: &str, content: String, ttl: Duration) -> Option<String> {
        let mut map = self.inner.lock().await;
        let now = Instant::now();
        let live_for_hook = map
            .values()
            .filter(|n| n.hook_id == hook_id && n.expires_at > now)
            .count();
        if live_for_hook >= MAX_TEMP_NOTES_PER_HOOK {
            return None;
        }
        let id = format!("{TEMP_PREFIX}{}", id::gen(16));
        map.insert(
            id.clone(),
            TempNote {
                hook_id: hook_id.to_owned(),
                content,
                expires_at: now + ttl,
            },
        );
        Some(id)
    }

    /// Fetch a temporary note, scoped to `hook_id` (same rule as persistent
    /// notes: the owning hook id must match). Lazily evicts if expired.
    pub async fn get(&self, id: &str, hook_id: &str) -> Option<String> {
        let mut map = self.inner.lock().await;
        match map.get(id) {
            Some(n) if n.hook_id == hook_id && n.expires_at > Instant::now() => {
                Some(n.content.clone())
            }
            Some(n) if n.hook_id == hook_id => {
                // Expired: clean it up now rather than waiting for the sweep.
                map.remove(id);
                None
            }
            _ => None,
        }
    }

    /// Remove every expired note. Intended to run periodically so memory
    /// doesn't grow unbounded from notes nobody ever fetches again.
    async fn sweep(&self) {
        let mut map = self.inner.lock().await;
        let now = Instant::now();
        let before = map.len();
        map.retain(|_, n| n.expires_at > now);
        let removed = before - map.len();
        if removed > 0 {
            tracing::debug!(removed, "swept expired temporary notes");
        }
    }

    /// Spawn a background task that sweeps expired notes every `period`.
    /// Runs forever (until the process exits) — matches the resilience
    /// pattern used elsewhere in this service (retry-forever loops, not
    /// one-shot tasks that can silently stop).
    pub fn spawn_sweeper(&self, period: Duration) {
        let this = self.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(period);
            loop {
                interval.tick().await;
                this.sweep().await;
            }
        });
    }
}

impl Default for TempNotes {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn create_get_and_hook_scoping() {
        let notes = TempNotes::new();

        let id = notes
            .create("hookA", "hello".to_owned(), Duration::from_secs(60))
            .await
            .unwrap();
        assert!(id.starts_with(TEMP_PREFIX));

        assert_eq!(notes.get(&id, "hookA").await, Some("hello".to_owned()));
        // Wrong hook id -> not found, even though the note id is right.
        assert_eq!(notes.get(&id, "hookB").await, None);
        // Unknown id -> not found.
        assert_eq!(notes.get("t_doesnotexist", "hookA").await, None);
    }

    #[tokio::test]
    async fn expiry_is_enforced_lazily() {
        let notes = TempNotes::new();
        let id = notes
            .create("hookA", "bye".to_owned(), Duration::from_millis(10))
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(notes.get(&id, "hookA").await, None);
    }

    #[tokio::test]
    async fn sweep_removes_expired_entries() {
        let notes = TempNotes::new();
        notes
            .create("hookA", "short-lived".to_owned(), Duration::from_millis(10))
            .await
            .unwrap();
        notes
            .create("hookA", "long-lived".to_owned(), Duration::from_secs(60))
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        notes.sweep().await;
        assert_eq!(notes.inner.lock().await.len(), 1);
    }

    #[tokio::test]
    async fn per_hook_cap_is_enforced() {
        let notes = TempNotes::new();
        for _ in 0..MAX_TEMP_NOTES_PER_HOOK {
            notes
                .create("hookA", "x".to_owned(), Duration::from_secs(60))
                .await
                .unwrap();
        }
        // One more over the cap is rejected...
        assert!(
            notes
                .create("hookA", "over the cap".to_owned(), Duration::from_secs(60))
                .await
                .is_none()
        );
        // ...but a different hook is unaffected.
        assert!(
            notes
                .create("hookB", "fine".to_owned(), Duration::from_secs(60))
                .await
                .is_some()
        );
    }

    #[test]
    fn ttl_clamping() {
        assert_eq!(TempNotes::clamp_ttl(1), MIN_TTL);
        assert_eq!(TempNotes::clamp_ttl(u64::MAX), MAX_TTL);
        assert_eq!(TempNotes::clamp_ttl(3600), Duration::from_secs(3600));
    }
}
