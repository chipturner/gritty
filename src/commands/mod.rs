mod doctor;
mod refresh;
mod report;
mod session;
mod transfer;
mod util;

pub(crate) use doctor::doctor;
pub(crate) use refresh::refresh;
pub(crate) use report::{DEFAULT_TAIL_LINES, llm_report};
pub(crate) use session::*;
pub(crate) use transfer::{receive_command, resolve_receive_output, send_command};
pub(crate) use util::*;

/// What to auto-start when connect can't reach the server.
pub(crate) enum AutoStart {
    /// Explicit --ctl-socket: no auto-start
    None,
    /// Default path, no host: start local server
    Server,
    /// Host-routed: start SSH tunnel via `gritty tunnel-create <host>`.
    /// `config_dest` is the destination implied by `[host.<name>] aliases`
    /// (first entry), used when no `.dest` sidecar exists yet.
    Tunnel { name: String, config_dest: Option<String> },
}
