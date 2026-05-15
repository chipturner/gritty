use std::process::Command;

fn main() {
    // Identify this build. In a git checkout: the short HEAD hash plus a
    // `-dirty` flag. Outside a checkout (e.g. a crates.io source tarball, which
    // ships no `.git`): fall back to the crate version so that `cargo install`
    // builds still carry a distinguishable identifier -- otherwise every
    // version bakes in the same "unknown" string and `gritty refresh`/`doctor`
    // can never detect a stale same-protocol daemon.
    let git_hash = Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());

    let build_id = match git_hash {
        Some(hash) => {
            let dirty = Command::new("git")
                .args(["diff", "--quiet", "HEAD"])
                .status()
                .map(|s| !s.success())
                .unwrap_or(false);
            if dirty { format!("{hash}-dirty") } else { hash }
        }
        None => {
            let version = std::env::var("CARGO_PKG_VERSION").unwrap_or_default();
            format!("v{version}")
        }
    };

    println!("cargo:rustc-env=GRITTY_GIT_HASH={build_id}");

    // Rebuild if git HEAD changes
    println!("cargo:rerun-if-changed=.git/HEAD");
    println!("cargo:rerun-if-changed=.git/index");
}
