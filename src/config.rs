use std::collections::HashMap;
use std::path::PathBuf;

use serde::Deserialize;

/// Embedded default config template (from repo root config.toml).
pub const DEFAULT_CONFIG: &str = include_str!("../config.toml");

/// Resolved session settings after merging all config layers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionSettings {
    pub forward_agent: bool,
    pub forward_open: bool,
    pub no_escape: bool,
    pub no_redraw: bool,
    pub oauth_redirect: bool,
    pub oauth_timeout: u64,
    pub heartbeat_interval: u64,
    pub heartbeat_timeout: u64,
    pub ring_buffer_size: u64,
    pub oauth_tunnel_idle_timeout: u64,
}

impl Default for SessionSettings {
    fn default() -> Self {
        Self {
            forward_agent: true,
            forward_open: true,
            no_escape: false,
            no_redraw: false,
            oauth_redirect: true,
            oauth_timeout: 180,
            heartbeat_interval: 5,
            heartbeat_timeout: 15,
            ring_buffer_size: 1 << 20, // 1 MB
            oauth_tunnel_idle_timeout: 5,
        }
    }
}

/// Resolved connect settings after merging all config layers.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ConnectSettings {
    pub session: SessionSettings,
    pub ssh_options: Vec<String>,
    pub no_server_start: bool,
}

/// Top-level config file structure.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct ConfigFile {
    pub defaults: Defaults,
    pub host: HashMap<String, HostConfig>,
}

/// Global defaults section.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, rename_all = "kebab-case")]
pub struct Defaults {
    pub forward_agent: Option<bool>,
    pub forward_open: Option<bool>,
    pub no_escape: Option<bool>,
    pub no_redraw: Option<bool>,
    pub oauth_redirect: Option<bool>,
    pub oauth_timeout: Option<u64>,
    pub heartbeat_interval: Option<u64>,
    pub heartbeat_timeout: Option<u64>,
    pub ring_buffer_size: Option<u64>,
    pub oauth_tunnel_idle_timeout: Option<u64>,
    pub connect: Option<ConnectDefaults>,
}

/// Connect-specific defaults nested under [defaults.connect].
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, rename_all = "kebab-case")]
pub struct ConnectDefaults {
    pub ssh_options: Option<Vec<String>>,
    pub no_server_start: Option<bool>,
}

/// Per-host override section.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, rename_all = "kebab-case")]
pub struct HostConfig {
    pub forward_agent: Option<bool>,
    pub forward_open: Option<bool>,
    pub no_escape: Option<bool>,
    pub no_redraw: Option<bool>,
    pub oauth_redirect: Option<bool>,
    pub oauth_timeout: Option<u64>,
    pub heartbeat_interval: Option<u64>,
    pub heartbeat_timeout: Option<u64>,
    pub ring_buffer_size: Option<u64>,
    pub oauth_tunnel_idle_timeout: Option<u64>,
    pub connect: Option<ConnectDefaults>,
}

/// Return the config file path: $XDG_CONFIG_HOME/gritty/config.toml
pub fn config_path() -> PathBuf {
    if let Some(proj) = directories::ProjectDirs::from("", "", "gritty") {
        return proj.config_dir().join("config.toml");
    }
    PathBuf::from(".config").join("gritty").join("config.toml")
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
                eprintln!("warning: cannot read config {}: {e}", path.display());
                return Self::default();
            }
        };
        match toml::from_str(&content) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("warning: malformed config at {}: {e}", path.display());
                Self::default()
            }
        }
    }

    /// Resolve session settings for a given host (or None for local).
    pub fn resolve_session(&self, host: Option<&str>) -> SessionSettings {
        let d = &self.defaults;
        let h = host.and_then(|name| self.host.get(name));

        SessionSettings {
            forward_agent: h.and_then(|h| h.forward_agent).or(d.forward_agent).unwrap_or(true),
            forward_open: h.and_then(|h| h.forward_open).or(d.forward_open).unwrap_or(true),
            no_escape: pick(h.and_then(|h| h.no_escape), d.no_escape),
            no_redraw: pick(h.and_then(|h| h.no_redraw), d.no_redraw),
            oauth_redirect: h.and_then(|h| h.oauth_redirect).or(d.oauth_redirect).unwrap_or(true),
            oauth_timeout: h.and_then(|h| h.oauth_timeout).or(d.oauth_timeout).unwrap_or(180),
            heartbeat_interval: h
                .and_then(|h| h.heartbeat_interval)
                .or(d.heartbeat_interval)
                .unwrap_or(5),
            heartbeat_timeout: h
                .and_then(|h| h.heartbeat_timeout)
                .or(d.heartbeat_timeout)
                .unwrap_or(15),
            ring_buffer_size: h
                .and_then(|h| h.ring_buffer_size)
                .or(d.ring_buffer_size)
                .unwrap_or(1 << 20),
            oauth_tunnel_idle_timeout: h
                .and_then(|h| h.oauth_tunnel_idle_timeout)
                .or(d.oauth_tunnel_idle_timeout)
                .unwrap_or(5),
        }
    }

    /// Resolve connect settings for a given host.
    pub fn resolve_connect(&self, host: &str) -> ConnectSettings {
        let d = &self.defaults;
        let dc = d.connect.as_ref();
        let h = self.host.get(host);
        let hc = h.and_then(|h| h.connect.as_ref());

        // ssh-options: host-specific first, then defaults (SSH uses first-match)
        let mut ssh_options = Vec::new();
        if let Some(opts) = hc.and_then(|c| c.ssh_options.as_ref()) {
            ssh_options.extend(opts.iter().cloned());
        }
        if let Some(opts) = dc.and_then(|c| c.ssh_options.as_ref()) {
            ssh_options.extend(opts.iter().cloned());
        }

        ConnectSettings {
            session: self.resolve_session(Some(host)),
            ssh_options,
            no_server_start: pick(
                hc.and_then(|c| c.no_server_start),
                dc.and_then(|c| c.no_server_start),
            ),
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
    fn connect_settings_merge_ssh_options() {
        let cfg: ConfigFile = toml::from_str(
            r#"
            [defaults.connect]
            ssh-options = ["Compression=yes"]

            [host.devbox.connect]
            ssh-options = ["IdentityFile=~/.ssh/key"]
            "#,
        )
        .unwrap();
        let c = cfg.resolve_connect("devbox");
        // Host-specific first, then defaults
        assert_eq!(c.ssh_options, vec!["IdentityFile=~/.ssh/key", "Compression=yes"]);
    }

    #[test]
    fn connect_settings_no_host_ssh_options() {
        let cfg: ConfigFile = toml::from_str(
            r#"
            [defaults.connect]
            ssh-options = ["Compression=yes"]
            "#,
        )
        .unwrap();
        let c = cfg.resolve_connect("unknown");
        assert_eq!(c.ssh_options, vec!["Compression=yes"]);
    }

    #[test]
    fn connect_no_server_start() {
        let cfg: ConfigFile = toml::from_str(
            r#"
            [host.prod.connect]
            no-server-start = true
            "#,
        )
        .unwrap();
        let c = cfg.resolve_connect("prod");
        assert!(c.no_server_start);
        assert!(!cfg.resolve_connect("devbox").no_server_start);
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
    fn no_redraw_configurable() {
        let cfg: ConfigFile = toml::from_str(
            r#"
            [defaults]
            no-redraw = true

            [host.devbox]
            no-redraw = false
            "#,
        )
        .unwrap();
        assert!(cfg.resolve_session(None).no_redraw);
        assert!(cfg.resolve_session(Some("unknown")).no_redraw);
        assert!(!cfg.resolve_session(Some("devbox")).no_redraw);
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
    fn unknown_keys_ignored() {
        let cfg: ConfigFile = toml::from_str(
            r#"
            [defaults]
            forward-agent = true
            some-future-setting = "ignored"
            "#,
        )
        .unwrap();
        assert!(cfg.resolve_session(None).forward_agent);
    }

    #[test]
    fn connect_session_settings_resolved() {
        let cfg: ConfigFile = toml::from_str(
            r#"
            [defaults]
            forward-agent = true

            [host.devbox]
            forward-open = true
            "#,
        )
        .unwrap();
        let c = cfg.resolve_connect("devbox");
        assert!(c.session.forward_agent);
        assert!(c.session.forward_open);
    }
}
