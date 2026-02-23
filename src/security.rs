use std::io;
use std::os::fd::{FromRawFd, OwnedFd, RawFd};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::Path;

const MAX_WINSIZE: u16 = 10_000;

/// Create a directory hierarchy with mode 0700, validating ownership of existing components.
/// Trusted system roots (`/`, `/tmp`, `/run`, `$XDG_RUNTIME_DIR`) are accepted without
/// ownership checks. All other existing directories must be owned by the current user
/// and must not be symlinks.
pub fn secure_create_dir_all(path: &Path) -> io::Result<()> {
    if path.exists() {
        if is_trusted_root(path) {
            return Ok(());
        }
        return validate_dir(path);
    }

    if let Some(parent) = path.parent() {
        secure_create_dir_all(parent)?;
    }

    std::fs::create_dir(path)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
    Ok(())
}

/// Bind a `UnixListener` with TOCTOU-safe stale socket handling and 0600 permissions.
///
/// On `AddrInUse`, probes the existing socket: if it responds to a connect, returns
/// an error (socket is alive). Otherwise, removes the stale socket and retries.
pub fn bind_unix_listener(path: &Path) -> io::Result<tokio::net::UnixListener> {
    match tokio::net::UnixListener::bind(path) {
        Ok(listener) => {
            set_socket_permissions(path)?;
            Ok(listener)
        }
        Err(e) if e.kind() == io::ErrorKind::AddrInUse => {
            match std::os::unix::net::UnixStream::connect(path) {
                Ok(_) => Err(io::Error::new(
                    io::ErrorKind::AddrInUse,
                    format!("{} is already in use by a running process", path.display()),
                )),
                Err(_) => {
                    std::fs::remove_file(path)?;
                    let listener = tokio::net::UnixListener::bind(path)?;
                    set_socket_permissions(path)?;
                    Ok(listener)
                }
            }
        }
        Err(e) => Err(e),
    }
}

/// Verify that the peer on a Unix stream has the same UID as the current process.
pub fn verify_peer_uid(stream: &tokio::net::UnixStream) -> io::Result<()> {
    let cred = stream.peer_cred()?;
    let my_uid = unsafe { libc::getuid() };
    if cred.uid() != my_uid {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("rejecting connection from uid {} (expected {my_uid})", cred.uid()),
        ));
    }
    Ok(())
}

/// `dup(2)` that returns an `OwnedFd` or an error (instead of silently returning -1).
pub fn checked_dup(fd: RawFd) -> io::Result<OwnedFd> {
    let new_fd = unsafe { libc::dup(fd) };
    if new_fd == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { OwnedFd::from_raw_fd(new_fd) })
}

/// Clamp window-size values to a sane range, preventing zero-sized or absurdly large values.
pub fn clamp_winsize(cols: u16, rows: u16) -> (u16, u16) {
    (cols.clamp(1, MAX_WINSIZE), rows.clamp(1, MAX_WINSIZE))
}

fn set_socket_permissions(path: &Path) -> io::Result<()> {
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
}

fn is_trusted_root(path: &Path) -> bool {
    if matches!(path.to_str(), Some("/" | "/tmp" | "/run")) {
        return true;
    }
    std::env::var("XDG_RUNTIME_DIR").ok().is_some_and(|xdg| path == Path::new(&xdg))
}

fn validate_dir(path: &Path) -> io::Result<()> {
    let meta = std::fs::symlink_metadata(path)?;

    if meta.file_type().is_symlink() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("refusing to use symlink at {}", path.display()),
        ));
    }
    if !meta.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{} is not a directory", path.display()),
        ));
    }

    let uid = unsafe { libc::getuid() };
    if meta.uid() != uid {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "{} is owned by uid {}, expected uid {uid}; \
                 set $XDG_RUNTIME_DIR or use --ctl-socket",
                path.display(),
                meta.uid()
            ),
        ));
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clamp_winsize_zeros_to_minimum() {
        assert_eq!(clamp_winsize(0, 0), (1, 1));
    }

    #[test]
    fn clamp_winsize_normal_passthrough() {
        assert_eq!(clamp_winsize(80, 24), (80, 24));
    }

    #[test]
    fn clamp_winsize_max_boundary() {
        assert_eq!(clamp_winsize(10_000, 10_000), (10_000, 10_000));
    }

    #[test]
    fn clamp_winsize_over_max_clamped() {
        assert_eq!(clamp_winsize(10_001, 10_001), (10_000, 10_000));
    }

    #[test]
    fn clamp_winsize_extreme_values() {
        assert_eq!(clamp_winsize(u16::MAX, u16::MAX), (10_000, 10_000));
    }

    #[test]
    fn clamp_winsize_asymmetric() {
        assert_eq!(clamp_winsize(0, 80), (1, 80));
        assert_eq!(clamp_winsize(20_000, 5), (10_000, 5));
    }
}
