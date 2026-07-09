//! Client-prefixed session-name resolution.
//!
//! Every command that takes a user-supplied session reference runs it through
//! [`resolve_session_name`] before constructing wire frames. The rule lets each
//! client carve out its own namespace on a shared server without users having
//! to type the prefix.
//!
//! The slash is the natural separator -- any name containing one is taken as
//! already-namespaced (literal); a bare name gets your client prefix prepended.

use crate::ui;

/// Resolve a user-supplied session name to its wire name.
///
/// - Bare `-` is the "last-attached" marker and passes through unchanged.
/// - A name containing `/` is taken literally (already namespaced, foreign
///   access, or a deliberately-shared session).
/// - A name without `/` is prefixed with `<client_name>/`.
///
/// `client_name` is assumed to have been validated upstream (non-empty, no
/// `/`); callers should not pass empty strings.
pub fn resolve_session_name(name: &str, client_name: &str) -> String {
    if name == "-" {
        return "-".to_string();
    }
    if name.contains('/') {
        return name.to_string();
    }
    format!("{client_name}/{name}")
}

/// Strip the ambient client's prefix from a wire name for display.
///
/// Returns the bare suffix when `wire_name` starts with `<client_name>/`;
/// otherwise returns the input unchanged. Used by `ls` and the session picker
/// so your own sessions read as `work` rather than `mylaptop/work`, while
/// foreign and shared sessions keep their full form for disambiguation.
pub fn display_session_name<'a>(wire_name: &'a str, client_name: &str) -> &'a str {
    if client_name.is_empty() {
        return wire_name;
    }
    let prefix_len = client_name.len() + 1;
    if wire_name.len() > prefix_len
        && wire_name.starts_with(client_name)
        && wire_name.as_bytes()[client_name.len()] == b'/'
    {
        &wire_name[prefix_len..]
    } else {
        wire_name
    }
}

/// Validate a `client_name` for use as a session-name prefix.
///
/// Rejected: empty, contains `/` (would corrupt the separator), contains
/// whitespace or ASCII control characters. Callers that get `Err` should
/// fall back to the literal string `"unknown"` (see [`sanitize_client_name`]).
pub fn validate_client_name(s: &str) -> Result<(), &'static str> {
    if s.is_empty() {
        return Err("client_name must not be empty");
    }
    if s.contains('/') {
        return Err("client_name must not contain '/' (reserved as namespace separator)");
    }
    if s.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err("client_name must not contain whitespace or control characters");
    }
    Ok(())
}

/// Apply [`validate_client_name`] and fall back to `"unknown"` on rejection.
///
/// On fallback, prints a one-line warning to stderr identifying the original
/// value -- a silent fallback would leave users wondering why their sessions
/// landed under `unknown/`.
pub fn sanitize_client_name(candidate: String) -> String {
    match validate_client_name(&candidate) {
        Ok(()) => candidate,
        Err(reason) => {
            ui::warn(&format!("client_name {candidate:?} rejected ({reason}); using \"unknown\""));
            "unknown".to_string()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn last_attached_passes_through() {
        assert_eq!(resolve_session_name("-", "mylaptop"), "-");
    }

    #[test]
    fn bare_name_gets_prefix() {
        assert_eq!(resolve_session_name("work", "mylaptop"), "mylaptop/work");
    }

    #[test]
    fn default_gets_prefix() {
        assert_eq!(resolve_session_name("default", "mylaptop"), "mylaptop/default");
    }

    #[test]
    fn name_with_slash_is_literal() {
        assert_eq!(resolve_session_name("laptop2/work", "mylaptop"), "laptop2/work");
    }

    #[test]
    fn own_prefix_passes_through_when_explicit() {
        // Equivalent to the bare-name form, just spelled out explicitly.
        assert_eq!(resolve_session_name("mylaptop/work", "mylaptop"), "mylaptop/work");
    }

    #[test]
    fn shared_form_preserved() {
        assert_eq!(resolve_session_name("team/shared", "mylaptop"), "team/shared");
    }

    #[test]
    fn multi_slash_treated_as_literal() {
        // The resolver does no slicing; multi-slash names go through as-is.
        // The server's name validator decides whether to accept them.
        assert_eq!(resolve_session_name("a/b/c", "mylaptop"), "a/b/c");
    }

    #[test]
    fn leading_slash_is_literal() {
        assert_eq!(resolve_session_name("/foo", "mylaptop"), "/foo");
    }

    #[test]
    fn trailing_slash_is_literal() {
        assert_eq!(resolve_session_name("foo/", "mylaptop"), "foo/");
    }

    #[test]
    fn display_strips_own_prefix() {
        assert_eq!(display_session_name("mylaptop/work", "mylaptop"), "work");
    }

    #[test]
    fn display_preserves_foreign_prefix() {
        assert_eq!(display_session_name("laptop2/work", "mylaptop"), "laptop2/work");
    }

    #[test]
    fn display_preserves_unnamespaced() {
        // Legacy / bare-name sessions: no prefix to elide.
        assert_eq!(display_session_name("default", "mylaptop"), "default");
    }

    #[test]
    fn display_does_not_strip_prefix_match_with_no_suffix() {
        // A session whose entire wire name is the client prefix would elide
        // to empty -- treat as no match instead so we never display an empty
        // name.
        assert_eq!(display_session_name("mylaptop/", "mylaptop"), "mylaptop/");
        assert_eq!(display_session_name("mylaptop", "mylaptop"), "mylaptop");
    }

    #[test]
    fn display_handles_prefix_that_is_a_substring() {
        // "mylap" is not a prefix of "mylaptop/work" in our sense (must end
        // at the slash) -- the substring check would false-match without the
        // slash guard.
        assert_eq!(display_session_name("mylaptop/work", "mylap"), "mylaptop/work");
    }

    #[test]
    fn display_empty_client_name_is_identity() {
        assert_eq!(display_session_name("work", ""), "work");
    }

    #[test]
    fn validate_rejects_empty() {
        assert!(validate_client_name("").is_err());
    }

    #[test]
    fn validate_rejects_slash() {
        assert!(validate_client_name("my/laptop").is_err());
    }

    #[test]
    fn validate_rejects_whitespace() {
        assert!(validate_client_name("my laptop").is_err());
        assert!(validate_client_name("my\tlaptop").is_err());
        assert!(validate_client_name("my\nlaptop").is_err());
    }

    #[test]
    fn validate_rejects_control_chars() {
        assert!(validate_client_name("my\x00laptop").is_err());
        assert!(validate_client_name("my\x1blaptop").is_err());
    }

    #[test]
    fn validate_accepts_dashes_dots_alnum() {
        assert!(validate_client_name("Chips-MacBook-Pro").is_ok());
        assert!(validate_client_name("host.example.com").is_ok());
        assert!(validate_client_name("laptop_2").is_ok());
        assert!(validate_client_name("a").is_ok());
    }

    #[test]
    fn sanitize_passes_valid_through() {
        assert_eq!(sanitize_client_name("mylaptop".into()), "mylaptop");
    }

    #[test]
    fn sanitize_falls_back_on_invalid() {
        assert_eq!(sanitize_client_name(String::new()), "unknown");
        assert_eq!(sanitize_client_name("bad/name".into()), "unknown");
    }
}
