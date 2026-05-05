//! Records what version of gritty a long-lived process is running.
//!
//! The daemon and tunnel supervisor are both self-daemonized and long-lived,
//! so after a rebuild (or `bootstrap` on the remote) they keep running old
//! code until explicitly restarted. Nothing in the wire protocol can detect a
//! *same-protocol-version* stale process, and a stale supervisor is completely
//! invisible (it's a pure byte proxy -- handshake checks sail right through
//! it). The `.info` sidecar fixes that: each long-lived process writes its
//! compile-time identity at startup, and `gritty doctor` diffs it against the
//! on-disk binary to say exactly which process is stale and how to fix it.
//!
//! Format is human-readable `key=value` lines. Unknown keys are ignored so
//! the format is forward-extensible without a version bump.

use std::io;
use std::path::{Path, PathBuf};

use crate::protocol::PROTOCOL_VERSION;

/// Build-time git short hash (+ `-dirty` if the working tree had changes).
/// Baked in by `build.rs` via `cargo:rustc-env`. Every process (daemon,
/// supervisor, client, doctor) built from the same source tree carries the
/// same value -- so if a running process's recorded hash differs from ours,
/// the on-disk binary has been replaced since that process started.
pub const GIT_HASH: &str = env!("GRITTY_GIT_HASH");

/// Identity of a gritty process, written at startup so peers can detect
/// staleness without a live handshake.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunInfo {
    /// Wire protocol version this process speaks.
    pub protocol: u16,
    /// Git short hash the process was built from (may be `-dirty`).
    pub git_hash: String,
    /// Resolved path of the executable at process start.
    pub exe: PathBuf,
    /// OS PID.
    pub pid: u32,
    /// Wall-clock start time (Unix seconds). Informational only.
    pub started_unix: u64,
}

impl RunInfo {
    /// Snapshot the current process's identity.
    pub fn current() -> Self {
        Self {
            protocol: PROTOCOL_VERSION,
            git_hash: GIT_HASH.to_string(),
            exe: std::env::current_exe().unwrap_or_default(),
            pid: std::process::id(),
            started_unix: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0),
        }
    }

    /// Serialize to `key=value` lines.
    pub fn to_string_repr(&self) -> String {
        format!(
            "protocol={}\ngit={}\nexe={}\npid={}\nstarted={}\n",
            self.protocol,
            self.git_hash,
            self.exe.display(),
            self.pid,
            self.started_unix,
        )
    }

    /// Write to a file with 0600 perms (matching the socket dir policy).
    pub fn write(&self, path: &Path) -> io::Result<()> {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(path)?;
        f.write_all(self.to_string_repr().as_bytes())
    }

    /// Parse from a file. Unknown keys are ignored; missing keys default to
    /// zero/empty so an older or truncated file degrades gracefully.
    pub fn read(path: &Path) -> io::Result<Self> {
        let text = std::fs::read_to_string(path)?;
        Ok(Self::parse(&text))
    }

    fn parse(text: &str) -> Self {
        let mut info = Self {
            protocol: 0,
            git_hash: String::new(),
            exe: PathBuf::new(),
            pid: 0,
            started_unix: 0,
        };
        for line in text.lines() {
            let Some((k, v)) = line.split_once('=') else { continue };
            match k.trim() {
                "protocol" => info.protocol = v.trim().parse().unwrap_or(0),
                "git" => info.git_hash = v.trim().to_string(),
                "exe" => info.exe = PathBuf::from(v.trim()),
                "pid" => info.pid = v.trim().parse().unwrap_or(0),
                "started" => info.started_unix = v.trim().parse().unwrap_or(0),
                _ => {}
            }
        }
        info
    }

    /// Compare a running process's recorded identity against the current
    /// process (== the on-disk binary). Returns a human-readable staleness
    /// description, or `None` if they match.
    ///
    /// Protocol mismatch is the hard problem (handshake will fail); git-hash
    /// mismatch with the same protocol is the soft problem (everything works
    /// but bug fixes and behavior changes aren't picked up).
    pub fn staleness_vs_current(&self) -> Option<Staleness> {
        let cur = Self::current();
        if self.protocol != cur.protocol {
            return Some(Staleness::Protocol { running: self.protocol, on_disk: cur.protocol });
        }
        if !self.git_hash.is_empty() && self.git_hash != cur.git_hash {
            return Some(Staleness::Build {
                running: self.git_hash.clone(),
                on_disk: cur.git_hash,
            });
        }
        None
    }
}

/// How a running process differs from the binary on disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Staleness {
    /// Protocol version differs -- handshakes will fail until the process is
    /// restarted.
    Protocol { running: u16, on_disk: u16 },
    /// Same protocol, different build -- everything works but the process is
    /// running stale code (missed bug fixes / behavior changes).
    Build { running: String, on_disk: String },
}

impl std::fmt::Display for Staleness {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Staleness::Protocol { running, on_disk } => {
                write!(f, "running protocol v{running} but binary on disk is v{on_disk}")
            }
            Staleness::Build { running, on_disk } => {
                write!(f, "running build {running} but binary on disk is {on_disk}")
            }
        }
    }
}

/// Path of the daemon's `.info` sidecar, next to `daemon.pid`.
pub fn daemon_info_path(ctl_path: &Path) -> PathBuf {
    ctl_path.with_file_name("daemon.info")
}

/// Path of a tunnel supervisor's `.info` sidecar, next to
/// `connect-{name}.pid`.
pub fn connect_info_path(connection_name: &str) -> PathBuf {
    crate::daemon::socket_dir().join(format!("connect-{connection_name}.info"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let info = RunInfo {
            protocol: 42,
            git_hash: "abc1234-dirty".to_string(),
            exe: PathBuf::from("/usr/local/bin/gritty"),
            pid: 1234,
            started_unix: 1_700_000_000,
        };
        let parsed = RunInfo::parse(&info.to_string_repr());
        assert_eq!(info, parsed);
    }

    #[test]
    fn parse_tolerates_unknown_keys_and_missing_fields() {
        let parsed = RunInfo::parse("protocol=5\nfuture_field=xyz\ngit=deadbeef\n");
        assert_eq!(parsed.protocol, 5);
        assert_eq!(parsed.git_hash, "deadbeef");
        assert_eq!(parsed.pid, 0);
        assert_eq!(parsed.exe, PathBuf::new());
    }

    #[test]
    fn parse_tolerates_garbage_lines() {
        let parsed = RunInfo::parse("not a kv line\nprotocol=not_a_number\ngit=x\n");
        assert_eq!(parsed.protocol, 0);
        assert_eq!(parsed.git_hash, "x");
    }

    #[test]
    fn current_reflects_build() {
        let cur = RunInfo::current();
        assert_eq!(cur.protocol, PROTOCOL_VERSION);
        assert_eq!(cur.git_hash, GIT_HASH);
        assert_eq!(cur.pid, std::process::id());
        assert!(!cur.exe.as_os_str().is_empty());
    }

    #[test]
    fn staleness_none_when_matching() {
        let cur = RunInfo::current();
        assert_eq!(cur.staleness_vs_current(), None);
    }

    #[test]
    fn staleness_protocol_mismatch() {
        let mut info = RunInfo::current();
        info.protocol = PROTOCOL_VERSION.wrapping_sub(1);
        match info.staleness_vs_current() {
            Some(Staleness::Protocol { running, on_disk }) => {
                assert_eq!(running, PROTOCOL_VERSION.wrapping_sub(1));
                assert_eq!(on_disk, PROTOCOL_VERSION);
            }
            other => panic!("expected Protocol staleness, got {other:?}"),
        }
    }

    #[test]
    fn staleness_build_mismatch_same_protocol() {
        let mut info = RunInfo::current();
        info.git_hash = "different-hash".to_string();
        match info.staleness_vs_current() {
            Some(Staleness::Build { running, on_disk }) => {
                assert_eq!(running, "different-hash");
                assert_eq!(on_disk, GIT_HASH);
            }
            other => panic!("expected Build staleness, got {other:?}"),
        }
    }

    #[test]
    fn staleness_protocol_takes_priority_over_build() {
        let mut info = RunInfo::current();
        info.protocol = PROTOCOL_VERSION.wrapping_sub(1);
        info.git_hash = "different-hash".to_string();
        assert!(matches!(info.staleness_vs_current(), Some(Staleness::Protocol { .. })));
    }

    #[test]
    fn staleness_ignores_empty_git_hash() {
        // An older `.info` file (or corrupted one) with no git hash should
        // not be flagged as stale on build alone -- protocol is the signal.
        let mut info = RunInfo::current();
        info.git_hash = String::new();
        assert_eq!(info.staleness_vs_current(), None);
    }

    #[test]
    fn write_read_roundtrip() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("test.info");
        let info = RunInfo::current();
        info.write(&path).unwrap();
        let read = RunInfo::read(&path).unwrap();
        assert_eq!(info, read);
        // File should be 0600.
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }
}
