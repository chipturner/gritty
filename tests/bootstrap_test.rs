//! `gritty bootstrap` end to end with a scripted `ssh` and a scripted `curl`:
//! the real `install.sh` from this repo runs against a fake GitHub release
//! containing the binary under test, on a "remote" that is a temp `$HOME` on
//! this machine. No network, no sshd, no socat.
//!
//! `connect::bootstrap` hands the remote login shell one command string --
//! `GRITTY_INSTALL_DIR=... sh -c "$(curl -sSfL <install.sh>)" sh <release>`
//! -- and then probes `gritty protocol-version` through the same PATH prefix
//! the tunnel uses. Both halves run for real here; only `ssh` (runs the
//! string locally) and `curl` (serves local files) are scripted.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

mod common;
use common::{GRITTY, stderr};

const HOST: &str = "devbox";
const RELEASE: &str = env!("CARGO_PKG_VERSION");

/// The "remote" is this machine: run the command string in `$HOME`, as sshd
/// would. Every argv is appended to `$FAKE_DIR/ssh.calls`.
const FAKE_SSH: &str = r#"#!/bin/sh
echo "$*" >> "$FAKE_DIR/ssh.calls"
for a; do last=$a; done
cd "$HOME" && exec sh -c "$last"
"#;

/// Serves install.sh (from the repo) and release assets (from
/// `$FAKE_DIR/assets/<tag>/`) the way `curl -sSfL URL [-o FILE]` would,
/// 404-ing (exit 22, like `curl -f`) anything else. `$FAKE_DIR/mode` set to
/// `script-404` makes the install.sh fetch itself fail.
const FAKE_CURL: &str = r#"#!/bin/sh
out=; url=; prev=
for a in "$@"; do
    [ "$prev" = "-o" ] && out=$a
    case $a in http*) url=$a ;; esac
    prev=$a
done
echo "$url" >> "$FAKE_DIR/curl.calls"
mode=$(cat "$FAKE_DIR/mode" 2>/dev/null || echo ok)
notfound() { echo "curl: (22) The requested URL returned error: 404" >&2; exit 22; }
case $url in
    */install.sh)
        [ "$mode" = script-404 ] && notfound
        src=$REPO_INSTALL_SH ;;
    */releases/latest/download/*)
        src=$FAKE_DIR/assets/latest/${url##*/} ;;
    */releases/download/*)
        tag=${url%/*}; tag=${tag##*/}
        src=$FAKE_DIR/assets/$tag/${url##*/} ;;
    *) src= ;;
esac
[ -n "$src" ] && [ -f "$src" ] || notfound
if [ -n "$out" ]; then cp "$src" "$out"; else cat "$src"; fi
"#;

struct Fixture {
    dir: tempfile::TempDir,
    fake: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let fake = dir.path().join("fake");
        fs::create_dir_all(fake.join("bin")).unwrap();
        fs::create_dir_all(dir.path().join("home")).unwrap();
        for (name, body) in [("ssh", FAKE_SSH), ("curl", FAKE_CURL)] {
            let path = fake.join("bin").join(name);
            fs::write(&path, body).unwrap();
            set_executable(&path);
        }
        let f = Self { dir, fake };
        f.publish_release(&format!("v{RELEASE}"));
        f
    }

    fn home(&self) -> PathBuf {
        self.dir.path().join("home")
    }

    fn set_mode(&self, mode: &str) {
        fs::write(self.fake.join("mode"), mode).unwrap();
    }

    /// Stage a release under `assets/<tag>/`: the tarball install.sh expects
    /// for this platform, containing the binary under test, plus SHA256SUMS
    /// in the layout release.yml produces (`sha256sum gritty-*.tar.gz`).
    fn publish_release(&self, tag: &str) {
        let assets = self.fake.join("assets").join(tag);
        let stage = assets.join("stage");
        fs::create_dir_all(&stage).unwrap();
        fs::copy(GRITTY, stage.join("gritty")).unwrap();
        let tarball = format!("gritty-{}.tar.gz", host_target());
        let ok = Command::new("tar")
            .args(["czf", &tarball, "-C", "stage", "gritty"])
            .current_dir(&assets)
            .status()
            .unwrap()
            .success();
        assert!(ok, "tar");
        let ok = Command::new("sh")
            .args([
                "-c",
                "{ command -v sha256sum >/dev/null && sha256sum gritty-*.tar.gz \
                 || shasum -a 256 gritty-*.tar.gz; } > SHA256SUMS",
            ])
            .current_dir(&assets)
            .status()
            .unwrap()
            .success();
        assert!(ok, "checksums");
    }

    /// Minimal PATH: the fakes, then system dirs for sh/tar/mktemp/uname.
    /// Deliberately not the test process's PATH, so a `gritty` installed on
    /// this machine can never satisfy the post-install probe.
    fn env(&self) -> Vec<(&'static str, String)> {
        let home = self.home().to_str().unwrap().to_string();
        let bin = self.fake.join("bin").to_str().unwrap().to_string();
        vec![
            ("HOME", home.clone()),
            ("XDG_CONFIG_HOME", home),
            ("GRITTY_SOCKET_DIR", self.dir.path().to_str().unwrap().to_string()),
            ("PATH", format!("{bin}:/usr/bin:/bin")),
            ("FAKE_DIR", self.fake.to_str().unwrap().to_string()),
            ("REPO_INSTALL_SH", concat!(env!("CARGO_MANIFEST_DIR"), "/install.sh").to_string()),
            ("NO_COLOR", "1".to_string()),
        ]
    }

    fn bootstrap(&self, extra: &[&str]) -> Output {
        Command::new(GRITTY)
            .arg("bootstrap")
            .arg(HOST)
            .args(extra)
            .envs(self.env())
            .stdin(Stdio::null())
            .output()
            .expect("run gritty bootstrap")
    }

    fn curl_calls(&self) -> String {
        fs::read_to_string(self.fake.join("curl.calls")).unwrap_or_default()
    }

    /// Every `gritty` file under the remote home -- what the install left.
    fn installed(&self) -> Vec<PathBuf> {
        let mut found = Vec::new();
        walk(&self.home(), &mut found);
        found.sort();
        found
    }
}

fn walk(dir: &Path, found: &mut Vec<PathBuf>) {
    for entry in fs::read_dir(dir).unwrap().flatten() {
        let p = entry.path();
        if p.is_dir() {
            walk(&p, found);
        } else if p.file_name().is_some_and(|n| n == "gritty") {
            found.push(p);
        }
    }
}

/// The target triple install.sh derives from `uname` on this machine.
fn host_target() -> &'static str {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("linux", "x86_64") => "x86_64-unknown-linux-musl",
        ("linux", "aarch64") => "aarch64-unknown-linux-musl",
        ("macos", "x86_64") => "x86_64-apple-darwin",
        ("macos", "aarch64") => "aarch64-apple-darwin",
        other => panic!("install.sh has no target for {other:?}"),
    }
}

fn set_executable(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

#[test]
fn bootstrap_installs_this_release_and_confirms_its_protocol() {
    let f = Fixture::new();
    let out = f.bootstrap(&[]);
    let err = stderr(&out);
    assert!(out.status.success(), "{err}");
    assert_eq!(f.installed(), vec![f.home().join(".local/bin/gritty")]);
    // The binary is pinned to this release, not `latest`: both sides must
    // speak the same protocol without the user thinking about it.
    assert!(
        f.curl_calls().contains(&format!("/releases/download/v{RELEASE}/gritty-")),
        "{}",
        f.curl_calls()
    );
    assert!(err.contains(&format!("installed gritty {RELEASE} on {HOST}")), "{err}");
    assert!(err.contains("matches this machine"), "{err}");
    assert!(err.contains(&format!("next: gritty connect {HOST}:<name>")), "{err}");
}

#[test]
fn bootstrap_fails_when_the_release_has_no_published_build() {
    let f = Fixture::new();
    let out = f.bootstrap(&["--release", "9.9.9"]);
    let err = stderr(&out);
    assert!(!out.status.success(), "{err}");
    assert!(err.contains("remote install failed"), "{err}");
    assert!(err.contains("retry with --release latest"), "{err}");
    assert!(f.installed().is_empty());
}

#[test]
fn bootstrap_fails_when_the_install_script_cannot_be_fetched() {
    // `sh -c "$(curl ...)"` alone runs an empty command -- exit 0 -- when
    // the fetch fails, and bootstrap would then report an install that never
    // happened (a remote that can reach github.com but not
    // raw.githubusercontent.com, say).
    let f = Fixture::new();
    f.set_mode("script-404");
    let out = f.bootstrap(&[]);
    let err = stderr(&out);
    assert!(!out.status.success(), "{err}");
    assert!(err.contains("remote install failed"), "{err}");
    assert!(!err.contains("installed gritty"), "{err}");
    assert!(f.installed().is_empty());
}

#[test]
fn bootstrap_relative_install_dir_is_under_the_remote_home() {
    // ssh runs the command in the remote $HOME, so `bin` means `~/bin` --
    // not a directory inside install.sh's scratch dir that its cleanup then
    // removes.
    let f = Fixture::new();
    let out = f.bootstrap(&["--install-dir", "bin"]);
    let err = stderr(&out);
    assert!(out.status.success(), "{err}");
    assert_eq!(f.installed(), vec![f.home().join("bin/gritty")]);
    assert!(err.contains("matches this machine"), "{err}");
}

#[test]
fn bootstrap_warns_when_the_install_dir_is_off_the_ssh_path() {
    // The install succeeds, but the tunnel resolves `gritty` through a fixed
    // PATH prefix; a binary outside it is exactly as useful as no binary.
    let f = Fixture::new();
    let dir = f.home().join("elsewhere");
    let out = f.bootstrap(&["--install-dir", dir.to_str().unwrap()]);
    let err = stderr(&out);
    assert!(out.status.success(), "{err}");
    assert_eq!(f.installed(), vec![dir.join("gritty")]);
    assert!(err.contains("is not on the PATH gritty uses over ssh"), "{err}");
    assert!(!err.contains("try `gritty refresh`"), "{err}");
}
