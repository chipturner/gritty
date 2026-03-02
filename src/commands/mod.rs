mod session;
mod transfer;
mod util;

pub(crate) use session::*;
pub(crate) use transfer::{receive_command, send_command};
pub(crate) use util::*;

/// Attach-specific error type so callers can distinguish "no such session"
/// from other failures (used by `--create` to fall through to session creation).
pub(crate) enum AttachError {
    NoSuchSession,
    Other(anyhow::Error),
}

/// What to auto-start when new-session can't connect.
pub(crate) enum AutoStart {
    /// Explicit --ctl-socket: no auto-start
    None,
    /// Default path, no host: start local server
    Server,
    /// Host-routed: start SSH tunnel via `gritty connect <host>`
    Tunnel(String),
}
