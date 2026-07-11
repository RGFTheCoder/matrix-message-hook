//! Short, URL-friendly, unambiguous IDs.
//!
//! Hook IDs use a Crockford-style alphabet that omits visually ambiguous
//! characters (`0 1 i l o`) so they are easy to read, transcribe, and put in a
//! URL. At 16 characters over a 31-symbol alphabet an ID carries ~79 bits of
//! entropy — ample for a webhook secret (not brute-forceable over HTTP).

use rand::Rng;

/// Alphabet with visually ambiguous characters (`0 1 i l o`) removed.
const ALPHABET: &[u8] = b"23456789abcdefghjkmnpqrstuvwxyz";

/// Length of a generated hook id.
const HOOK_ID_LEN: usize = 16;

/// Generate a random hook id.
pub fn hook_id() -> String {
    gen(HOOK_ID_LEN)
}

/// Generate a random id of length `len` from the unambiguous alphabet.
pub fn gen(len: usize) -> String {
    let mut rng = rand::thread_rng();
    (0..len)
        .map(|_| ALPHABET[rng.gen_range(0..ALPHABET.len())] as char)
        .collect()
}

/// Derive the Matrix localpart for a hook's virtual (appservice) sender from its
/// name + id, e.g. `hook_alerts_9k3m…`. The name is slugified to lowercase
/// alphanumerics (so the result is always a valid localpart matching the
/// appservice namespace `@hook_.*`); if it slugifies to nothing, just
/// `hook_<id>` is used.
pub fn virtual_localpart(name: &str, id: &str) -> String {
    let slug: String = name
        .chars()
        .map(|c| c.to_ascii_lowercase())
        .filter(char::is_ascii_alphanumeric)
        .take(24)
        .collect();
    if slug.is_empty() {
        format!("hook_{id}")
    } else {
        format!("hook_{slug}_{id}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_unambiguous_and_sized() {
        for _ in 0..100 {
            let id = hook_id();
            assert_eq!(id.len(), HOOK_ID_LEN);
            assert!(
                id.bytes().all(|b| ALPHABET.contains(&b)),
                "id {id} has out-of-alphabet chars"
            );
            assert!(
                !id.contains(['0', '1', 'i', 'l', 'o']),
                "id {id} contains an ambiguous char"
            );
        }
    }

    #[test]
    fn ids_are_distinct() {
        let a = hook_id();
        let b = hook_id();
        assert_ne!(a, b);
    }

    #[test]
    fn localpart_slugifies_and_matches_namespace() {
        assert_eq!(virtual_localpart("Alerts!", "9k3m"), "hook_alerts_9k3m");
        assert_eq!(virtual_localpart("  ", "9k3m"), "hook_9k3m");
        assert_eq!(virtual_localpart("My Prod Deploys", "abcd"), "hook_myproddeploys_abcd");
        // Only [a-z0-9_] in the result -> always a valid localpart.
        let lp = virtual_localpart("Wéird / Näme #1", "zzzz");
        assert!(lp.starts_with("hook_"));
        assert!(lp.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_'));
    }
}
