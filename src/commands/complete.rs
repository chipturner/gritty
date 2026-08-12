//! Shell completion of gritty's own vocabulary: host names and
//! `host:session` targets. Flags and subcommands complete from the clap
//! definitions themselves (`COMPLETE=<shell> gritty` emits the shell glue,
//! see `main()`); this module supplies the values that only a live gritty
//! knows -- which tunnels exist and which sessions each daemon has.
//!
//! Everything here runs inside a keystroke, so it is quiet (nothing on
//! stderr), best-effort (a daemon that doesn't answer within
//! [`PROBE_BUDGET`] simply contributes nothing) and never starts anything.

use std::ffi::OsStr;
use std::time::Duration;

use clap_complete::engine::{ArgValueCompleter, CompletionCandidate};

/// How long one daemon gets to list its sessions before the completion
/// gives up on it. Interactive latency, not correctness, sets this.
const PROBE_BUDGET: Duration = Duration::from_millis(300);

/// Completer for arguments that name a host (`ls`, `prune`, `restart`, ...).
pub(crate) fn host() -> ArgValueCompleter {
    ArgValueCompleter::new(|current: &OsStr| candidates(matching(&known_hosts(), text(current))))
}

/// Completer for `tunnel-destroy`: only names with a tunnel behind them.
pub(crate) fn tunnel() -> ArgValueCompleter {
    ArgValueCompleter::new(|current: &OsStr| {
        candidates(matching(&gritty::connect::enumerate_tunnels(), text(current)))
    })
}

/// Completer for `host:session` targets: hosts until a colon is typed, then
/// that host's sessions.
pub(crate) fn target() -> ArgValueCompleter {
    ArgValueCompleter::new(|current: &OsStr| {
        candidates(complete_target(text(current), &known_hosts(), sessions_of))
    })
}

/// Completer for `kill-session`'s targets, whose bare form is a session on
/// `local`: offers local's sessions alongside the hosts until a colon is
/// typed.
pub(crate) fn kill_target() -> ArgValueCompleter {
    ArgValueCompleter::new(|current: &OsStr| {
        let current = text(current);
        if current.contains(':') {
            return candidates(complete_target(current, &known_hosts(), sessions_of));
        }
        let mut bare = sessions_of("local");
        bare.extend(known_hosts());
        candidates(matching(&bare, current))
    })
}

fn text(current: &OsStr) -> &str {
    current.to_str().unwrap_or_default()
}

fn candidates(values: Vec<String>) -> Vec<CompletionCandidate> {
    values.into_iter().map(CompletionCandidate::new).collect()
}

fn matching(items: &[String], prefix: &str) -> Vec<String> {
    items.iter().filter(|item| item.starts_with(prefix)).cloned().collect()
}

/// The pure half of [`target`]: `sessions_of(host)` is only consulted once
/// the user has committed to a host with a colon, so plain host completion
/// never talks to a daemon. The typed host spelling is preserved in the
/// results (`:wo` completes to `:work`, an alias stays as typed) because the
/// shell replaces the whole word with the candidate.
fn complete_target(
    current: &str,
    hosts: &[String],
    sessions_of: impl Fn(&str) -> Vec<String>,
) -> Vec<String> {
    let Some((typed_host, partial)) = current.split_once(':') else {
        return matching(hosts, current);
    };
    let host = if typed_host.is_empty() { "local" } else { typed_host };
    matching(&sessions_of(host), partial)
        .into_iter()
        .map(|session| format!("{typed_host}:{session}"))
        .collect()
}

/// `local`, every tunnel with a lock file, and every configured host --
/// deduplicated, `local` first, the rest sorted. Configured hosts are
/// included even when their tunnel is down: `connect <host>` is exactly how
/// you bring one up.
fn known_hosts() -> Vec<String> {
    let mut hosts: Vec<String> = gritty::connect::enumerate_tunnels();
    if let gritty::config::ConfigStatus::Valid(cfg) =
        gritty::config::config_status(&gritty::config::config_path())
    {
        hosts.extend(cfg.host.keys().cloned());
    }
    hosts.retain(|h| h != "local");
    hosts.sort();
    hosts.dedup();
    hosts.insert(0, "local".to_string());
    hosts
}

/// The session names a host's daemon reports, in the same typeable form
/// `ls` prints; empty if the daemon is down or slow.
fn sessions_of(host: &str) -> Vec<String> {
    use gritty::protocol::Frame;

    let config = match gritty::config::config_status(&gritty::config::config_path()) {
        gritty::config::ConfigStatus::Valid(cfg) => *cfg,
        _ => gritty::config::ConfigFile::default(),
    };
    let host = config.canonical_host_quiet(host);
    let Ok(ctl_path) = super::util::resolve_ctl_path(None, Some(&host)) else {
        return Vec::new();
    };
    let client_name = config.resolve_session(Some(&host)).client_name;

    let Ok(runtime) = tokio::runtime::Builder::new_current_thread().enable_all().build() else {
        return Vec::new();
    };
    let listed = runtime.block_on(async {
        tokio::time::timeout(
            PROBE_BUDGET,
            super::util::server_request(&ctl_path, Frame::ListSessions),
        )
        .await
    });
    match listed {
        Ok(Ok(Frame::SessionInfo { sessions })) => sessions
            .iter()
            .filter(|s| !s.name.is_empty())
            .map(|s| gritty::naming::display_session_name(&s.name, &client_name).to_string())
            .collect(),
        _ => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hosts() -> Vec<String> {
        ["local", "coder", "fate.x.pattern.net"].map(String::from).to_vec()
    }

    fn sessions(host: &str) -> Vec<String> {
        match host {
            "local" => vec!["0".into(), "scratch".into()],
            "fate.x.pattern.net" => vec!["work".into(), "wiki".into(), "laptop2/x".into()],
            _ => Vec::new(),
        }
    }

    #[test]
    fn hosts_complete_until_a_colon_without_probing() {
        let probed = std::cell::Cell::new(false);
        let spy = |_: &str| {
            probed.set(true);
            Vec::new()
        };
        assert_eq!(complete_target("", &hosts(), spy), hosts());
        assert_eq!(complete_target("fa", &hosts(), spy), ["fate.x.pattern.net"]);
        assert!(complete_target("zzz", &hosts(), spy).is_empty());
        assert!(!probed.get(), "host completion must not talk to daemons");
    }

    #[test]
    fn sessions_complete_after_the_colon_keeping_the_typed_host() {
        assert_eq!(
            complete_target("fate.x.pattern.net:", &hosts(), sessions),
            ["fate.x.pattern.net:work", "fate.x.pattern.net:wiki", "fate.x.pattern.net:laptop2/x"]
        );
        assert_eq!(
            complete_target("fate.x.pattern.net:wo", &hosts(), sessions),
            ["fate.x.pattern.net:work"]
        );
        assert_eq!(
            complete_target("fate.x.pattern.net:laptop2/", &hosts(), sessions),
            ["fate.x.pattern.net:laptop2/x"]
        );
        assert!(complete_target("coder:", &hosts(), sessions).is_empty(), "down daemon = nothing");
    }

    #[test]
    fn empty_host_means_local_and_stays_empty_in_the_result() {
        assert_eq!(complete_target(":", &hosts(), sessions), [":0", ":scratch"]);
        assert_eq!(complete_target(":s", &hosts(), sessions), [":scratch"]);
    }

    #[test]
    fn unknown_typed_host_is_still_probed_as_spelled() {
        // An alias or a host without a tunnel yet: the daemon lookup decides,
        // not the host list -- so aliases resolve and typos just yield nothing.
        let asked = std::cell::RefCell::new(Vec::new());
        let _ = complete_target("f:", &hosts(), |h| {
            asked.borrow_mut().push(h.to_string());
            Vec::new()
        });
        assert_eq!(*asked.borrow(), ["f"]);
    }
}
