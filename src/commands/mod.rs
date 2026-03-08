mod session;
mod transfer;
mod util;

pub(crate) use session::*;
pub(crate) use transfer::{receive_command, send_command};
pub(crate) use util::*;

/// What to auto-start when connect can't reach the server.
pub(crate) enum AutoStart {
    /// Explicit --ctl-socket: no auto-start
    None,
    /// Default path, no host: start local server
    Server,
    /// Host-routed: start SSH tunnel via `gritty tunnel-create <host>`
    Tunnel(String),
}
