use std::collections::HashMap;
use std::path::PathBuf;

use serde::Deserialize;

use crate::protocol::{IDLE_EVICT_SAFETY_MARGIN, IDLE_EVICT_TIMEOUT};
use crate::ui;

/// Embedded default config template (from repo root config.toml).
pub const DEFAULT_CONFIG: &str = include_str!("../config.toml");

/// Upper bound on `heartbeat_interval + heartbeat_timeout` in seconds. Derived
/// from the server's idle-evict window less a safety margin, so tuning the
/// server constant automatically re-tunes the client clamp.
const fn idle_evict_ceiling_secs() -> u64 {
    IDLE_EVICT_TIMEOUT.as_secs() - IDLE_EVICT_SAFETY_MARGIN.as_secs()
}

/// Parse a `linger` config/CLI value into seconds. Accepts the literal
/// `"never"` (mapped to the 0 sentinel = no auto-reap) or any non-zero
/// duration accepted by [`crate::parse_duration`] (`"30m"`, `"1h"`, bare
/// seconds). A literal `0` / `0s` is rejected: it would otherwise alias
/// to `never`, which is the opposite of what someone writing 0 means.
pub fn parse_linger(s: &str) -> anyhow::Result<u64> {
    if s.eq_ignore_ascii_case("never") {
        return Ok(0);
    }
    match crate::parse_duration(s)? {
        0 => anyhow::bail!(
            "linger of 0 is ambiguous -- use \"never\" to disable auto-reap, or \"1s\" for immediate"
        ),
        secs => Ok(secs),
    }
}

/// Clamp the heartbeat interval/timeout to the three invariants the client and
/// server both rely on -- `interval >= 1`, `timeout > interval`, and
/// `interval + timeout <= ceiling` -- and return the clamped pair plus any
/// human-readable warnings.
///
/// The interval is capped first at `(ceiling - 1) / 2`: without the cap a large
/// interval drives the timeout clamp below the interval (or to 0), and the
/// client's `link_is_stale()` check then trips on the first heartbeat tick --
/// an endless reconnect loop.
///
/// Warnings name the config keys in kebab-case (`heartbeat-interval`, not
/// `heartbeat_interval`) because `deny_unknown_fields` + `rename_all` make only
/// the kebab spelling valid -- a user who copies an underscore name back into
/// the config writes an unknown key and the whole file is discarded.
fn clamp_heartbeat(raw_interval: u64, raw_timeout: u64) -> (u64, u64, Vec<String>) {
    let mut warnings = Vec::new();

    let max_interval = (idle_evict_ceiling_secs().saturating_sub(1) / 2).max(1);
    let interval = raw_interval.clamp(1, max_interval);
    if interval != raw_interval {
        warnings.push(format!("heartbeat-interval clamped to {interval}s (was {raw_interval}s)"));
    }

    // timeout must exceed the (clamped) interval.
    let timeout = if raw_timeout <= interval {
        warnings.push(format!(
            "heartbeat-timeout ({raw_timeout}s) must exceed heartbeat-interval ({interval}s); clamped to {}s",
            interval + 1
        ));
        interval + 1
    } else {
        raw_timeout
    };
    // Keep interval + timeout under the server idle-evict ceiling so a healthy
    // client always sends a Ping before the server gives up on it. The interval
    // cap above guarantees max_timeout >= interval + 1, so this never undoes
    // `timeout > interval`.
    let max_timeout = idle_evict_ceiling_secs().saturating_sub(interval);
    let timeout = if timeout > max_timeout {
        let evict = IDLE_EVICT_TIMEOUT.as_secs();
        warnings.push(format!(
            "heartbeat-interval ({interval}s) + heartbeat-timeout ({timeout}s) exceeds server idle-evict ({evict}s); timeout clamped to {max_timeout}s"
        ));
        max_timeout
    } else {
        timeout
    };

    (interval, timeout, warnings)
}

/// Resolved session settings after merging all config layers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionSettings {
    pub forward_agent: bool,
    pub forward_open: bool,
    pub no_escape: bool,
    pub oauth_redirect: bool,
    pub oauth_timeout: u64,
    pub heartbeat_interval: u64,
    pub heartbeat_timeout: u64,
    pub ring_buffer_size: u64,
    pub oauth_tunnel_idle_timeout: u64,
    pub client_name: String,
    /// How long a session survives with zero attached clients before the
    /// daemon reaps it. Seconds; 0 = never. Applies to sessions the user
    /// explicitly named (`host:foo`).
    pub linger: u64,
    /// Same as `linger` but for sessions where the user omitted the session
    /// part (`host` -> auto-numbered slot). Falls back to `linger` if not
    /// set in config.
    pub linger_unnamed: u64,
}

/// Resolve a default `client_name` from the system hostname, sanitized through
/// [`crate::naming::sanitize_client_name`] so an empty hostname or one with a
/// `/` in it falls back to `"unknown"` instead of producing a broken prefix.
fn default_client_name() -> String {
    let raw =
        nix::unistd::gethostname().ok().and_then(|s| s.into_string().ok()).unwrap_or_default();
    crate::naming::sanitize_client_name(raw)
}

impl Default for SessionSettings {
    fn default() -> Self {
        Self {
            forward_agent: false,
            forward_open: true,
            no_escape: false,
            oauth_redirect: true,
            oauth_timeout: 180,
            heartbeat_interval: 10,
            heartbeat_timeout: 60,
            ring_buffer_size: 1 << 20, // 1 MB
            oauth_tunnel_idle_timeout: 5,
            client_name: default_client_name(),
            linger: 0,
            linger_unnamed: 0,
        }
    }
}

/// Resolved tunnel settings after merging all config layers.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TunnelSettings {
    pub session: SessionSettings,
    pub ssh_options: Vec<String>,
    pub no_server_start: bool,
    pub isolate_control_path: bool,
    pub connect_timeout: u64,
}

/// Top-level config file structure.
///
/// `deny_unknown_fields` rejects a misspelled top-level section (`[default]`,
/// `[hosts.x]`, `[Host.x]`) instead of silently dropping it. The `host` field
/// is a map, so arbitrary `[host.<name>]` tables are still accepted.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ConfigFile {
    pub defaults: Defaults,
    pub host: HashMap<String, HostConfig>,
}

/// Session/connection overrides. Used both for the global `[defaults]` section
/// and for each `[host.<name>]` section (see [`HostConfig`]) -- a per-host
/// entry can override exactly the same keys as the defaults, so both share one
/// schema to keep them from drifting.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields, rename_all = "kebab-case")]
pub struct Defaults {
    pub forward_agent: Option<bool>,
    pub forward_open: Option<bool>,
    pub no_escape: Option<bool>,
    pub oauth_redirect: Option<bool>,
    pub oauth_timeout: Option<u64>,
    pub heartbeat_interval: Option<u64>,
    pub heartbeat_timeout: Option<u64>,
    pub ring_buffer_size: Option<u64>,
    pub oauth_tunnel_idle_timeout: Option<u64>,
    pub client_name: Option<String>,
    /// Linger for explicitly-named sessions (e.g. `"30m"`, `"1h"`,
    /// `"never"`). See [`SessionSettings::linger`].
    pub linger: Option<String>,
    /// Linger for sessions where the user omitted the name. Falls back to
    /// `linger` if unset. See [`SessionSettings::linger_unnamed`].
    pub linger_unnamed: Option<String>,
    /// Alternate names for this connection (`[host.<name>]` only; meaningless
    /// under `[defaults]`). Typing an alias as the host part of a target
    /// resolves to the canonical connection name, and the *first* alias is
    /// the SSH destination used when no tunnel exists yet.
    pub aliases: Option<Vec<String>>,
    pub tunnel: Option<TunnelDefaults>,
}

/// Tunnel-specific defaults nested under [defaults.tunnel].
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields, rename_all = "kebab-case")]
pub struct TunnelDefaults {
    pub ssh_options: Option<Vec<String>>,
    pub no_server_start: Option<bool>,
    pub isolate_control_path: Option<bool>,
    pub connect_timeout: Option<u64>,
}

/// Per-host override section (`[host.<name>]`). A host entry overrides the same
/// keys as `[defaults]`, so it is exactly the [`Defaults`] schema -- aliased
/// rather than duplicated so the two cannot drift.
pub type HostConfig = Defaults;

/// Return the config file path: $XDG_CONFIG_HOME/gritty/config.toml
pub fn config_path() -> PathBuf {
    if let Some(proj) = directories::ProjectDirs::from("", "", "gritty") {
        return proj.config_dir().join("config.toml");
    }
    PathBuf::from(".config").join("gritty").join("config.toml")
}

/// Outcome of strictly parsing the config file. Unlike `load`/`load_from`
/// (which swallow any error and silently fall back to defaults), this reports
/// a rejected file -- so `info` and `doctor` agree and never call a discarded
/// config "loaded".
pub enum ConfigStatus {
    NotFound,
    Valid(Box<ConfigFile>),
    Invalid(String),
}

/// Strictly parse the config file for diagnostics.
pub fn config_status(path: &std::path::Path) -> ConfigStatus {
    let content = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return ConfigStatus::NotFound,
        Err(e) => return ConfigStatus::Invalid(format!("cannot read config: {e}")),
    };
    match toml::from_str(&content) {
        Ok(cfg) => ConfigStatus::Valid(Box::new(cfg)),
        Err(e) => ConfigStatus::Invalid(e.to_string()),
    }
}

impl ConfigFile {
    /// Load config from the default path. Returns default on missing or malformed file.
    pub fn load() -> Self {
        Self::load_from(&config_path())
    }

    /// Load config from a specific path. Returns default on missing or malformed file.
    pub fn load_from(path: &std::path::Path) -> Self {
        let content = match std::fs::read_to_string(path) {
            Ok(c) => c,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Self::default(),
            Err(e) => {
                ui::warn(&format!("cannot read config {}: {e}", path.display()));
                return Self::default();
            }
        };
        match toml::from_str(&content) {
            Ok(c) => c,
            Err(e) => {
                ui::warn(&format!("malformed config at {}: {e}", path.display()));
                Self::default()
            }
        }
    }

    /// Resolve session settings for a given host (or None for local),
    /// printing any heartbeat-clamp warnings to stderr. Used by the connect
    /// path and the daemon -- both want the user (or the daemon log) to see a
    /// misconfiguration.
    pub fn resolve_session(&self, host: Option<&str>) -> SessionSettings {
        self.resolve_session_inner(host, true)
    }

    fn resolve_session_inner(&self, host: Option<&str>, warn: bool) -> SessionSettings {
        let d = &self.defaults;
        let h = host.and_then(|name| self.host.get(name));

        let raw_interval =
            h.and_then(|h| h.heartbeat_interval).or(d.heartbeat_interval).unwrap_or(10);
        let raw_timeout = h.and_then(|h| h.heartbeat_timeout).or(d.heartbeat_timeout).unwrap_or(60);
        let (heartbeat_interval, heartbeat_timeout, mut warnings) =
            clamp_heartbeat(raw_interval, raw_timeout);

        // Linger durations are stored as strings in config (so users can
        // write `"30m"` / `"never"`); a malformed value warns and is skipped
        // so the next layer in the precedence chain applies.
        let mut resolve_linger = |key: &str, raw: Option<&str>| -> Option<u64> {
            let raw = raw?;
            match parse_linger(raw) {
                Ok(secs) => Some(secs),
                Err(e) => {
                    warnings.push(format!("ignoring {key} = {raw:?}: {e}"));
                    None
                }
            }
        };
        let h_linger = resolve_linger("linger", h.and_then(|h| h.linger.as_deref()));
        let h_unnamed =
            resolve_linger("linger-unnamed", h.and_then(|h| h.linger_unnamed.as_deref()));
        let d_linger = resolve_linger("linger", d.linger.as_deref());
        let d_unnamed = resolve_linger("linger-unnamed", d.linger_unnamed.as_deref());
        let linger = h_linger.or(d_linger).unwrap_or(0);
        // `linger_unnamed` precedence: host.linger_unnamed > host.linger >
        // defaults.linger_unnamed > defaults.linger. A host that sets only
        // `linger = "never"` shields its unnamed sessions too, even if
        // `[defaults] linger-unnamed` is set.
        let linger_unnamed = h_unnamed.or(h_linger).or(d_unnamed).or(d_linger).unwrap_or(0);

        if warn {
            for w in &warnings {
                ui::warn(w);
            }
        }

        SessionSettings {
            forward_agent: h.and_then(|h| h.forward_agent).or(d.forward_agent).unwrap_or(false),
            forward_open: h.and_then(|h| h.forward_open).or(d.forward_open).unwrap_or(true),
            no_escape: pick(h.and_then(|h| h.no_escape), d.no_escape),
            oauth_redirect: h.and_then(|h| h.oauth_redirect).or(d.oauth_redirect).unwrap_or(true),
            oauth_timeout: h.and_then(|h| h.oauth_timeout).or(d.oauth_timeout).unwrap_or(180),
            heartbeat_interval,
            heartbeat_timeout,
            ring_buffer_size: h
                .and_then(|h| h.ring_buffer_size)
                .or(d.ring_buffer_size)
                .unwrap_or(1 << 20),
            oauth_tunnel_idle_timeout: h
                .and_then(|h| h.oauth_tunnel_idle_timeout)
                .or(d.oauth_tunnel_idle_timeout)
                .unwrap_or(5),
            client_name: h
                .and_then(|h| h.client_name.clone())
                .or_else(|| d.client_name.clone())
                .map(crate::naming::sanitize_client_name)
                .unwrap_or_else(default_client_name),
            linger,
            linger_unnamed,
        }
    }

    /// Resolve a typed host name through `[host.*]` aliases to its canonical
    /// connection name, printing a warning for ambiguous aliases.
    ///
    /// Real names always win: `local` and exact `[host.<name>]` keys are
    /// returned unchanged even if some host claims them as aliases. An alias
    /// claimed by multiple hosts is ambiguous and resolves to itself.
    pub fn canonical_host(&self, typed: &str) -> String {
        self.canonical_host_inner(typed, true)
    }

    /// Like [`canonical_host`](Self::canonical_host) but silent -- for
    /// secondary resolutions (e.g. log-path routing) that would otherwise
    /// duplicate the ambiguity warning within one invocation.
    pub fn canonical_host_quiet(&self, typed: &str) -> String {
        self.canonical_host_inner(typed, false)
    }

    fn canonical_host_inner(&self, typed: &str, warn: bool) -> String {
        if typed == "local" || self.host.contains_key(typed) {
            return typed.to_string();
        }
        let mut owners: Vec<&str> = self
            .host
            .iter()
            .filter(|(_, h)| h.aliases.iter().flatten().any(|a| a == typed))
            .map(|(name, _)| name.as_str())
            .collect();
        owners.sort_unstable();
        match owners.as_slice() {
            [name] => name.to_string(),
            [] => typed.to_string(),
            _ => {
                if warn {
                    eprintln!(
                        "warning: alias '{typed}' is claimed by multiple hosts ({}); using '{typed}' literally",
                        owners.join(", ")
                    );
                }
                typed.to_string()
            }
        }
    }

    /// The SSH destination implied by config for a canonical connection name:
    /// the first entry of `[host.<name>] aliases`. Used as the fallback when
    /// no `.dest` sidecar exists yet (first-ever connect, or the socket dir
    /// was wiped), so `gritty connect FOO:x` can cold-start a tunnel to
    /// `FOO.BAR.COM` without a prior `tunnel-create`.
    pub fn alias_destination(&self, host: &str) -> Option<String> {
        self.host.get(host)?.aliases.as_ref()?.first().cloned()
    }

    /// Resolve tunnel settings for a given host.
    pub fn resolve_tunnel(&self, host: &str) -> TunnelSettings {
        let d = &self.defaults;
        let dc = d.tunnel.as_ref();
        let h = self.host.get(host);
        let hc = h.and_then(|h| h.tunnel.as_ref());

        // ssh-options: host-specific first, then defaults (SSH uses first-match)
        let mut ssh_options = Vec::new();
        if let Some(opts) = hc.and_then(|c| c.ssh_options.as_ref()) {
            ssh_options.extend(opts.iter().cloned());
        }
        if let Some(opts) = dc.and_then(|c| c.ssh_options.as_ref()) {
            ssh_options.extend(opts.iter().cloned());
        }

        TunnelSettings {
            // Quiet: tunnel-create / bootstrap / refresh never establish a
            // heartbeat, so a heartbeat-clamp warning here is pure noise.
            session: self.resolve_session_inner(Some(host), false),
            ssh_options,
            no_server_start: pick(
                hc.and_then(|c| c.no_server_start),
                dc.and_then(|c| c.no_server_start),
            ),
            // Default true: a long-lived -L forward riding a ControlMaster
            // mux ignores our ServerAliveInterval / ExitOnForwardFailure /
            // StreamLocalBindUnlink, and the master can end up holding a
            // listener on a deleted inode after we unlink the local socket.
            // remote_exec() still rides the mux, so pre-checks stay fast.
            isolate_control_path: hc
                .and_then(|c| c.isolate_control_path)
                .or(dc.and_then(|c| c.isolate_control_path))
                .unwrap_or(true),
            connect_timeout: hc
                .and_then(|c| c.connect_timeout)
                .or(dc.and_then(|c| c.connect_timeout))
                .unwrap_or(30),
        }
    }
}

/// Pick the most specific value: host override > default > false.
fn pick(host_val: Option<bool>, default_val: Option<bool>) -> bool {
    host_val.or(default_val).unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_config_returns_defaults() {
        let cfg: ConfigFile = toml::from_str("").unwrap();
        let s = cfg.resolve_session(None);
        assert_eq!(s, SessionSettings::default());
    }

    #[test]
    fn defaults_apply_when_no_host() {
        let cfg: ConfigFile = toml::from_str(
            r#"
            [defaults]
            forward-agent = true
            forward-open = true
            "#,
        )
        .unwrap();
        let s = cfg.resolve_session(None);
        assert!(s.forward_agent);
        assert!(s.forward_open);
        assert!(!s.no_escape);
    }

    #[test]
    fn host_overrides_defaults() {
        let cfg: ConfigFile = toml::from_str(
            r#"
            [defaults]
            forward-agent = true
            forward-open = false

            [host.devbox]
            forward-agent = false
            forward-open = true
            "#,
        )
        .unwrap();
        let s = cfg.resolve_session(Some("devbox"));
        assert!(!s.forward_agent);
        assert!(s.forward_open);
    }

    #[test]
    fn unknown_host_uses_defaults() {
        let cfg: ConfigFile = toml::from_str(
            r#"
            [defaults]
            forward-agent = true

            [host.devbox]
            forward-open = true
            "#,
        )
        .unwrap();
        let s = cfg.resolve_session(Some("unknown"));
        assert!(s.forward_agent);
        assert!(s.forward_open);
    }

    #[test]
    fn host_partial_override_inherits_defaults() {
        let cfg: ConfigFile = toml::from_str(
            r#"
            [defaults]
            forward-agent = true
            no-escape = true

            [host.devbox]
            forward-open = true
            "#,
        )
        .unwrap();
        let s = cfg.resolve_session(Some("devbox"));
        assert!(s.forward_agent); // from defaults
        assert!(s.forward_open); // from host
        assert!(s.no_escape); // from defaults
    }

    #[test]
    fn tunnel_settings_merge_ssh_options() {
        let cfg: ConfigFile = toml::from_str(
            r#"
            [defaults.tunnel]
            ssh-options = ["Compression=yes"]

            [host.devbox.tunnel]
            ssh-options = ["IdentityFile=~/.ssh/key"]
            "#,
        )
        .unwrap();
        let c = cfg.resolve_tunnel("devbox");
        // Host-specific first, then defaults
        assert_eq!(c.ssh_options, vec!["IdentityFile=~/.ssh/key", "Compression=yes"]);
    }

    #[test]
    fn tunnel_settings_no_host_ssh_options() {
        let cfg: ConfigFile = toml::from_str(
            r#"
            [defaults.tunnel]
            ssh-options = ["Compression=yes"]
            "#,
        )
        .unwrap();
        let c = cfg.resolve_tunnel("unknown");
        assert_eq!(c.ssh_options, vec!["Compression=yes"]);
    }

    #[test]
    fn tunnel_no_server_start() {
        let cfg: ConfigFile = toml::from_str(
            r#"
            [host.prod.tunnel]
            no-server-start = true
            "#,
        )
        .unwrap();
        let c = cfg.resolve_tunnel("prod");
        assert!(c.no_server_start);
        assert!(!cfg.resolve_tunnel("devbox").no_server_start);
    }

    #[test]
    fn tunnel_isolate_control_path() {
        let cfg: ConfigFile = toml::from_str(
            r#"
            [host.mux.tunnel]
            isolate-control-path = false
            "#,
        )
        .unwrap();
        // Default is true; explicit false opts back into ControlMaster mux.
        assert!(cfg.resolve_tunnel("devbox").isolate_control_path);
        assert!(!cfg.resolve_tunnel("mux").isolate_control_path);
    }

    #[test]
    fn tunnel_connect_timeout() {
        let cfg: ConfigFile = toml::from_str(
            r#"
            [host.prod.tunnel]
            connect-timeout = 10
            "#,
        )
        .unwrap();
        assert_eq!(cfg.resolve_tunnel("prod").connect_timeout, 10);
        assert_eq!(cfg.resolve_tunnel("devbox").connect_timeout, 30);
    }

    #[test]
    fn missing_file_returns_default() {
        let cfg = ConfigFile::load_from(std::path::Path::new("/nonexistent/config.toml"));
        assert_eq!(cfg.resolve_session(None), SessionSettings::default());
    }

    #[test]
    fn config_path_ends_with_expected_suffix() {
        // Can't safely set env vars in tests (Rust 2024), but we can verify the
        // function returns a path ending in gritty/config.toml
        let p = config_path();
        assert!(p.ends_with("gritty/config.toml"), "got: {}", p.display());
    }

    #[test]
    fn oauth_settings_defaults() {
        let cfg: ConfigFile = toml::from_str("").unwrap();
        let s = cfg.resolve_session(None);
        assert!(s.oauth_redirect);
        assert_eq!(s.oauth_timeout, 180);
    }

    #[test]
    fn oauth_settings_configurable() {
        let cfg: ConfigFile = toml::from_str(
            r#"
            [defaults]
            oauth-redirect = false
            oauth-timeout = 60

            [host.devbox]
            oauth-redirect = true
            oauth-timeout = 300
            "#,
        )
        .unwrap();
        let s = cfg.resolve_session(None);
        assert!(!s.oauth_redirect);
        assert_eq!(s.oauth_timeout, 60);

        let s = cfg.resolve_session(Some("devbox"));
        assert!(s.oauth_redirect);
        assert_eq!(s.oauth_timeout, 300);
    }

    #[test]
    fn oauth_settings_host_partial_override() {
        let cfg: ConfigFile = toml::from_str(
            r#"
            [defaults]
            oauth-timeout = 90

            [host.devbox]
            oauth-redirect = false
            "#,
        )
        .unwrap();
        let s = cfg.resolve_session(Some("devbox"));
        assert!(!s.oauth_redirect); // from host
        assert_eq!(s.oauth_timeout, 90); // from defaults
    }

    #[test]
    fn oauth_tunnel_idle_timeout_configurable() {
        let cfg: ConfigFile = toml::from_str(
            r#"
            [defaults]
            oauth-tunnel-idle-timeout = 10

            [host.devbox]
            oauth-tunnel-idle-timeout = 30
            "#,
        )
        .unwrap();
        assert_eq!(cfg.resolve_session(None).oauth_tunnel_idle_timeout, 10);
        assert_eq!(cfg.resolve_session(Some("devbox")).oauth_tunnel_idle_timeout, 30);
        assert_eq!(cfg.resolve_session(Some("unknown")).oauth_tunnel_idle_timeout, 10);
    }

    #[test]
    fn canonical_host_resolves_alias() {
        let cfg: ConfigFile = toml::from_str(
            r#"
            [host.foo]
            aliases = ["foo.bar.com", "f"]
            "#,
        )
        .unwrap();
        assert_eq!(cfg.canonical_host("foo.bar.com"), "foo");
        assert_eq!(cfg.canonical_host("f"), "foo");
        assert_eq!(cfg.canonical_host("foo"), "foo");
        assert_eq!(cfg.canonical_host("unrelated"), "unrelated");
    }

    // A real `[host.*]` key and the literal `local` must never be remapped,
    // even if some host claims them as aliases -- otherwise a config typo
    // could hijack every command targeting the local daemon.
    #[test]
    fn canonical_host_exact_names_win_over_aliases() {
        let cfg: ConfigFile = toml::from_str(
            r#"
            [host.thief]
            aliases = ["local", "victim"]

            [host.victim]
            forward-agent = true
            "#,
        )
        .unwrap();
        assert_eq!(cfg.canonical_host("local"), "local");
        assert_eq!(cfg.canonical_host("victim"), "victim");
    }

    // An alias claimed by multiple hosts is ambiguous: resolve to neither and
    // use the typed name literally (a warning is printed on the warn path).
    #[test]
    fn canonical_host_ambiguous_alias_unresolved() {
        let cfg: ConfigFile = toml::from_str(
            r#"
            [host.a]
            aliases = ["dup"]

            [host.b]
            aliases = ["dup"]
            "#,
        )
        .unwrap();
        assert_eq!(cfg.canonical_host("dup"), "dup");
    }

    #[test]
    fn alias_destination_is_first_alias() {
        let cfg: ConfigFile = toml::from_str(
            r#"
            [host.foo]
            aliases = ["user@foo.bar.com:2222", "f"]

            [host.bare]
            forward-agent = true
            "#,
        )
        .unwrap();
        assert_eq!(cfg.alias_destination("foo"), Some("user@foo.bar.com:2222".to_string()));
        assert_eq!(cfg.alias_destination("bare"), None);
        assert_eq!(cfg.alias_destination("unknown"), None);
    }

    #[test]
    fn unknown_keys_rejected() {
        let result: Result<ConfigFile, _> = toml::from_str(
            r#"
            [defaults]
            forward-agent = true
            some-future-setting = "ignored"
            "#,
        );
        assert!(result.is_err());
    }

    // A misspelled top-level section ([hosts.x], [default], [Host.x]) must be
    // rejected, not silently dropped -- otherwise the override is lost and every
    // diagnostic still reports the config as valid.
    #[test]
    fn unknown_top_level_section_rejected() {
        let result: Result<ConfigFile, _> = toml::from_str(
            r#"
            [hosts.devbox]
            forward-agent = true
            "#,
        );
        assert!(result.is_err(), "misspelled [hosts.x] section must be rejected");
    }

    #[test]
    fn config_status_reports_invalid_for_bad_key() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "[defaults]\nforward_agent = true\n").unwrap();
        assert!(
            matches!(config_status(&path), ConfigStatus::Invalid(_)),
            "underscore key must parse as Invalid, not Valid"
        );
    }

    #[test]
    fn config_status_reports_valid_and_not_found() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("nope.toml");
        assert!(matches!(config_status(&missing), ConfigStatus::NotFound));

        let path = dir.path().join("config.toml");
        std::fs::write(&path, "[host.devbox]\nforward-agent = true\n").unwrap();
        match config_status(&path) {
            ConfigStatus::Valid(cfg) => assert_eq!(cfg.host.len(), 1),
            other => panic!("expected Valid, got {}", status_name(&other)),
        }
    }

    fn status_name(s: &ConfigStatus) -> &'static str {
        match s {
            ConfigStatus::NotFound => "NotFound",
            ConfigStatus::Valid(_) => "Valid",
            ConfigStatus::Invalid(_) => "Invalid",
        }
    }

    #[test]
    fn tunnel_session_settings_resolved() {
        let cfg: ConfigFile = toml::from_str(
            r#"
            [defaults]
            forward-agent = true

            [host.devbox]
            forward-open = true
            "#,
        )
        .unwrap();
        let c = cfg.resolve_tunnel("devbox");
        assert!(c.session.forward_agent);
        assert!(c.session.forward_open);
    }

    #[test]
    fn heartbeat_timeout_clamped_to_avoid_idle_evict() {
        let cfg: ConfigFile = toml::from_str(
            r#"
            [defaults]
            heartbeat-interval = 30
            heartbeat-timeout = 100
            "#,
        )
        .unwrap();
        let s = cfg.resolve_session(None);
        assert!(
            s.heartbeat_interval + s.heartbeat_timeout <= idle_evict_ceiling_secs(),
            "interval={} timeout={} ceiling={}",
            s.heartbeat_interval,
            s.heartbeat_timeout,
            idle_evict_ceiling_secs()
        );
        assert_eq!(s.heartbeat_timeout, idle_evict_ceiling_secs() - 30);
    }

    // Guard: the built-in defaults must always fit under the idle-evict
    // ceiling without clamping. If IDLE_EVICT_TIMEOUT is ever lowered, this
    // test forces the defaults to be reconsidered in the same change.
    #[test]
    fn heartbeat_defaults_fit_under_idle_evict() {
        let cfg: ConfigFile = toml::from_str("").unwrap();
        let s = cfg.resolve_session(None);
        assert!(s.heartbeat_interval + s.heartbeat_timeout <= idle_evict_ceiling_secs());
        assert_eq!(s.heartbeat_interval, 10);
        assert_eq!(s.heartbeat_timeout, 60);
    }

    #[test]
    fn parse_linger_accepts_never_and_durations() {
        assert_eq!(parse_linger("never").unwrap(), 0);
        assert_eq!(parse_linger("Never").unwrap(), 0);
        assert_eq!(parse_linger("30m").unwrap(), 30 * 60);
        assert_eq!(parse_linger("1h").unwrap(), 3600);
        assert_eq!(parse_linger("90").unwrap(), 90);
        assert!(parse_linger("nope").is_err());
        assert!(parse_linger("").is_err());
    }

    #[test]
    fn parse_linger_rejects_zero() {
        // 0 would alias to the `never` sentinel -- the opposite of what
        // someone writing 0 means -- so reject it explicitly.
        for z in ["0", "0s", "0m", "0h", "0d"] {
            let e = parse_linger(z).unwrap_err().to_string();
            assert!(e.contains("never"), "wrong error for {z}: {e}");
        }
    }

    #[test]
    fn linger_defaults_to_never() {
        let cfg: ConfigFile = toml::from_str("").unwrap();
        let s = cfg.resolve_session(None);
        assert_eq!(s.linger, 0);
        assert_eq!(s.linger_unnamed, 0);
    }

    #[test]
    fn linger_unnamed_falls_back_to_linger() {
        let cfg: ConfigFile = toml::from_str(
            r#"
            [defaults]
            linger = "1h"
        "#,
        )
        .unwrap();
        let s = cfg.resolve_session(None);
        assert_eq!(s.linger, 3600);
        assert_eq!(s.linger_unnamed, 3600);
    }

    #[test]
    fn linger_unnamed_overrides_linger() {
        let cfg: ConfigFile = toml::from_str(
            r#"
            [defaults]
            linger = "never"
            linger-unnamed = "30m"
        "#,
        )
        .unwrap();
        let s = cfg.resolve_session(None);
        assert_eq!(s.linger, 0);
        assert_eq!(s.linger_unnamed, 1800);
    }

    #[test]
    fn linger_per_host_overrides_default() {
        let cfg: ConfigFile = toml::from_str(
            r#"
            [defaults]
            linger = "1h"
            [host.work]
            linger = "never"
        "#,
        )
        .unwrap();
        assert_eq!(cfg.resolve_session(None).linger, 3600);
        assert_eq!(cfg.resolve_session(Some("work")).linger, 0);
    }

    #[test]
    fn host_linger_shields_unnamed_from_defaults() {
        // Precedence: host.linger_unnamed > host.linger >
        // defaults.linger_unnamed > defaults.linger. Setting
        // `[host.prod] linger = "never"` must protect prod's unnamed
        // sessions even when a global linger-unnamed fuse is set.
        let cfg: ConfigFile = toml::from_str(
            r#"
            [defaults]
            linger-unnamed = "30m"
            [host.prod]
            linger = "never"
        "#,
        )
        .unwrap();
        let s = cfg.resolve_session(Some("prod"));
        assert_eq!(s.linger, 0);
        assert_eq!(s.linger_unnamed, 0);
        // Other hosts still get the global unnamed fuse.
        assert_eq!(cfg.resolve_session(None).linger_unnamed, 1800);
    }

    #[test]
    fn linger_malformed_host_value_falls_through_to_default() {
        // A typo at the host layer should warn and inherit the valid
        // [defaults] value, not silently disable the feature.
        let cfg: ConfigFile = toml::from_str(
            r#"
            [defaults]
            linger = "1h"
            [host.work]
            linger = "1 h"
        "#,
        )
        .unwrap();
        let s = cfg.resolve_session_inner(Some("work"), false);
        assert_eq!(s.linger, 3600);
    }

    #[test]
    fn linger_malformed_default_falls_back_to_never() {
        let cfg: ConfigFile = toml::from_str(
            r#"
            [defaults]
            linger = "30x"
        "#,
        )
        .unwrap();
        let s = cfg.resolve_session_inner(None, false);
        assert_eq!(s.linger, 0);
    }

    // A large heartbeat_interval must not drive the resolved timeout below
    // the interval (or to 0) via the idle-evict ceiling clamp. Exercise a
    // range that includes values above the old un-capped behavior's break
    // point (~40s) and assert all three invariants hold for every one.
    #[test]
    fn heartbeat_interval_capped_so_timeout_stays_valid() {
        let ceiling = idle_evict_ceiling_secs();
        for interval in [1, 10, 40, 41, 60, 80, 1000] {
            let cfg: ConfigFile =
                toml::from_str(&format!("[defaults]\nheartbeat-interval = {interval}\n")).unwrap();
            let s = cfg.resolve_session(None);
            assert!(s.heartbeat_interval >= 1, "interval={interval}");
            assert!(
                s.heartbeat_timeout > s.heartbeat_interval,
                "interval={interval}: timeout {} must exceed interval {}",
                s.heartbeat_timeout,
                s.heartbeat_interval,
            );
            assert!(
                s.heartbeat_interval + s.heartbeat_timeout <= ceiling,
                "interval={interval}: {} + {} > ceiling {ceiling}",
                s.heartbeat_interval,
                s.heartbeat_timeout,
            );
        }
    }

    #[test]
    fn clamp_heartbeat_in_range_is_silent() {
        let (iv, to, warnings) = clamp_heartbeat(10, 60);
        assert_eq!((iv, to), (10, 60));
        assert!(warnings.is_empty(), "in-range values must not warn: {warnings:?}");
    }

    // The warning text must name the config keys in kebab-case: the structs use
    // rename_all = "kebab-case" + deny_unknown_fields, so an underscore spelling
    // copied back into the config is an unknown key that discards the whole file.
    #[test]
    fn clamp_heartbeat_warnings_use_kebab_case_keys() {
        let (.., warnings) = clamp_heartbeat(10_000, 1);
        assert!(!warnings.is_empty(), "out-of-range values must warn");
        for w in &warnings {
            assert!(
                !w.contains("heartbeat_interval") && !w.contains("heartbeat_timeout"),
                "warning must not use underscore key names: {w}"
            );
            assert!(
                w.contains("heartbeat-interval") || w.contains("heartbeat-timeout"),
                "warning must name a kebab-case config key: {w}"
            );
        }
    }
}
