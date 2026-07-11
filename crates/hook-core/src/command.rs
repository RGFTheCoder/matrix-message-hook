//! The chat command grammar and webhook-URL construction.
//!
//! These are pure functions so they can be unit-tested without a live Matrix
//! connection. The command surface is intentionally tiny — any user should be
//! able to figure it out from `help`.

/// A parsed command from a DM to the bot.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Command {
    /// Create a new hook with the given name.
    New(String),
    /// List the sender's hooks.
    List,
    /// Delete the hook with the given UUID.
    Delete(String),
    /// Show usage help (also the response to an empty/unknown-but-blank input).
    Help,
    /// Anything unrecognized; carries the original text for an error reply.
    Unknown(String),
}

/// Parse a message body into a [`Command`].
///
/// Grammar (case-insensitive verb):
/// - `new|create|add <name>` → [`Command::New`]
/// - `list|ls|hooks`         → [`Command::List`]
/// - `delete|del|rm|remove <uuid>` → [`Command::Delete`]
/// - `help|?|commands` or empty → [`Command::Help`]
/// - otherwise → [`Command::Unknown`]
pub fn parse_command(body: &str) -> Command {
    let trimmed = body.trim();
    let mut parts = trimmed.splitn(2, char::is_whitespace);
    let verb = parts.next().unwrap_or("").to_lowercase();
    let rest = parts.next().unwrap_or("").trim();

    match verb.as_str() {
        "" | "help" | "?" | "commands" => Command::Help,
        "new" | "create" | "add" => {
            if rest.is_empty() {
                Command::Unknown(trimmed.to_owned())
            } else {
                Command::New(rest.to_owned())
            }
        }
        "list" | "ls" | "hooks" => Command::List,
        "delete" | "del" | "rm" | "remove" => {
            match rest.split_whitespace().next() {
                Some(id) => Command::Delete(id.to_owned()),
                None => Command::Unknown(trimmed.to_owned()),
            }
        }
        _ => Command::Unknown(trimmed.to_owned()),
    }
}

/// Build the public webhook URL for `uuid` under `base` (e.g.
/// `https://matrixHook.damastacoda.dev/<uuid>`).
pub fn webhook_url(base: &str, uuid: &str) -> String {
    format!("{}/{}", base.trim_end_matches('/'), uuid)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_verbs() {
        assert_eq!(parse_command("new alerts"), Command::New("alerts".into()));
        assert_eq!(
            parse_command("  CREATE   my hook  "),
            Command::New("my hook".into())
        );
        assert_eq!(parse_command("list"), Command::List);
        assert_eq!(parse_command("LS"), Command::List);
        assert_eq!(
            parse_command("delete abc-123"),
            Command::Delete("abc-123".into())
        );
        assert_eq!(
            parse_command("rm  abc-123 extra"),
            Command::Delete("abc-123".into())
        );
        assert_eq!(parse_command(""), Command::Help);
        assert_eq!(parse_command("help"), Command::Help);
    }

    #[test]
    fn incomplete_and_unknown() {
        assert_eq!(parse_command("new"), Command::Unknown("new".into()));
        assert_eq!(parse_command("delete"), Command::Unknown("delete".into()));
        assert_eq!(parse_command("frobnicate x"), Command::Unknown("frobnicate x".into()));
    }

    #[test]
    fn builds_url_trimming_trailing_slash() {
        assert_eq!(
            webhook_url("https://h.example.dev/", "uuid-1"),
            "https://h.example.dev/uuid-1"
        );
        assert_eq!(
            webhook_url("https://h.example.dev", "uuid-1"),
            "https://h.example.dev/uuid-1"
        );
    }
}
