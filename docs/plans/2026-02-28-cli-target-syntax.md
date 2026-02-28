# CLI Target Syntax Redesign -- Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Replace the separate `host` positional + `-t` flag with a unified `host[:session]` positional arg across all CLI commands.

**Architecture:** Add a `parse_target()` helper that splits on first `:`. Refactor each `Command` variant to use a single `target: String` positional. Update `resolve_ctl_path()` to accept the parsed host. Update all command handlers to use the parsed session. Add friendly error for missing session on commands that require one.

**Tech Stack:** Rust, clap (derive), existing `main.rs` structure.

---

### Task 1: Add `parse_target()` and update unit tests

**Files:**
- Modify: `src/main.rs` (add `parse_target()` near `resolve_ctl_path()`, update tests)

**Step 1: Write failing tests for `parse_target()`**

Add these tests in `mod tests` at the bottom of `src/main.rs` (around line 1306):

```rust
#[test]
fn parse_target_host_only() {
    let (host, session) = parse_target("local");
    assert_eq!(host, "local");
    assert_eq!(session, None);
}

#[test]
fn parse_target_host_and_session() {
    let (host, session) = parse_target("local:work");
    assert_eq!(host, "local");
    assert_eq!(session, Some("work".to_string()));
}

#[test]
fn parse_target_remote_and_id() {
    let (host, session) = parse_target("devbox:0");
    assert_eq!(host, "devbox");
    assert_eq!(session, Some("0".to_string()));
}

#[test]
fn parse_target_colon_in_session_name() {
    let (host, session) = parse_target("local:my:weird:name");
    assert_eq!(host, "local");
    assert_eq!(session, Some("my:weird:name".to_string()));
}

#[test]
fn parse_target_empty_session() {
    let (host, session) = parse_target("local:");
    assert_eq!(host, "local");
    assert_eq!(session, None);
}
```

**Step 2: Run tests to verify they fail**

Run: `cargo test --bin gritty parse_target`
Expected: compilation error -- `parse_target` not found.

**Step 3: Implement `parse_target()`**

Add this function near `resolve_ctl_path()` (around line 498):

```rust
/// Parse a `host[:session]` target string. Splits on the first `:`.
fn parse_target(s: &str) -> (String, Option<String>) {
    match s.split_once(':') {
        Some((host, session)) if !session.is_empty() => {
            (host.to_string(), Some(session.to_string()))
        }
        Some((host, _)) => (host.to_string(), None),
        None => (s.to_string(), None),
    }
}
```

**Step 4: Run tests to verify they pass**

Run: `cargo test --bin gritty parse_target`
Expected: all 5 tests PASS.

**Step 5: Update `resolve_ctl_path()` signature**

Change `resolve_ctl_path` to take `host: &str` (not `Option<&str>`) since host is now always present after `parse_target()`. Remove the `(None, None)` arm. Update existing tests to match.

Old:
```rust
fn resolve_ctl_path(ctl_socket: Option<PathBuf>, host: Option<&str>) -> anyhow::Result<PathBuf> {
    match (ctl_socket, host) {
        (Some(p), _) => Ok(p),
        (None, Some("local")) => Ok(gritty::daemon::control_socket_path()),
        (None, Some(h)) => Ok(gritty::daemon::socket_dir().join(format!("connect-{h}.sock"))),
        (None, None) => anyhow::bail!("specify a host (use 'local' for the local server)"),
    }
}
```

New:
```rust
fn resolve_ctl_path(ctl_socket: Option<PathBuf>, host: &str) -> PathBuf {
    match ctl_socket {
        Some(p) => p,
        None if host == "local" => gritty::daemon::control_socket_path(),
        None => gritty::daemon::socket_dir().join(format!("connect-{host}.sock")),
    }
}
```

Note: this now returns `PathBuf` directly (no `Result`) since all arms are infallible. Every call site currently does `resolve_ctl_path(...)?` -- remove the `?`.

Update tests:

```rust
#[test]
fn resolve_ctl_path_ctl_socket_wins() {
    let p = PathBuf::from("/tmp/x.sock");
    let result = resolve_ctl_path(Some(p.clone()), "myhost");
    assert_eq!(result, p);
}

#[test]
fn resolve_ctl_path_ctl_socket_only() {
    let p = PathBuf::from("/tmp/custom.sock");
    let result = resolve_ctl_path(Some(p.clone()), "ignored");
    assert_eq!(result, p);
}

#[test]
fn resolve_ctl_path_host_only() {
    let result = resolve_ctl_path(None, "devbox");
    let s = result.to_string_lossy();
    assert!(s.contains("connect-devbox.sock"), "got: {s}");
}

#[test]
fn resolve_ctl_path_local() {
    let result = resolve_ctl_path(None, "local");
    assert_eq!(result, gritty::daemon::control_socket_path());
}
```

Delete `resolve_ctl_path_neither_errors` test (no longer applicable).

**Step 6: Run all tests**

Run: `cargo test --bin gritty`
Expected: PASS (compilation may fail at call sites -- that's fine, fixed in next task).

**Step 7: Commit**

```bash
git add src/main.rs
git commit -m "refactor: add parse_target() and simplify resolve_ctl_path()"
```

---

### Task 2: Refactor `Command` enum -- drop `-t`, use `target` positional

**Files:**
- Modify: `src/main.rs` (Command enum, lines 22-210)

**Step 1: Update the `Command` enum**

Replace all `host: Option<String>` + `target` combos with a single `target: String` (or `Option<String>` for send/receive). Remove all `-t`/`--target` `#[arg]` attributes.

Changes per variant:

`NewSession`: Remove `host: Option<String>` and `#[arg(short = 't', long = "target")] target: Option<String>`. Add `/// Target host, with optional session name (host:session)` and `target: String`.

`Attach`: Remove `host: Option<String>` and `#[arg(short = 't', long = "target")] target: String`. Add `/// Target host and session (host:session)` and `target: String`.

`Tail`: Same pattern as Attach.

`ListSessions`: Remove `host: Option<String>`. Add `/// Target host` and `target: String`.

`KillSession`: Remove `host: Option<String>` and `#[arg(short = 't', long = "target")] target: String`. Add `target: String`.

`KillServer`: Remove `host: Option<String>`. Add `target: String`.

`Send`: Remove `host: Option<String>` and `#[arg(short = 't', long = "target")] target: Option<String>`. Add `/// Target host[:session]; omit when inside a session` and `target: Option<String>`. Keep `#[arg(required = true)] files: Vec<PathBuf>`.

`Receive`: Remove `host: Option<String>` and `#[arg(short = 't', long = "target")] target: Option<String>`. Add `target: Option<String>`. Keep `dir: Option<PathBuf>`.

**Step 2: Verify it compiles (fix errors incrementally)**

Run: `cargo check`
Expected: errors at every match arm in `run()` that destructures the old fields. These are fixed in Task 3.

---

### Task 3: Update all command handler call sites

**Files:**
- Modify: `src/main.rs` (the `run()` async function + `connect_send_socket()`)

**Step 1: Update `NewSession` handler (around line 510)**

Old:
```rust
Command::NewSession { host, target, no_redraw, ... } => {
    let auto_start_mode = match (&cli.ctl_socket, &host) { ... };
    let ctl_path = resolve_ctl_path(cli.ctl_socket, host.as_deref())?;
    let resolved = config.resolve_session(host.as_deref());
    new_session(target, settings, ctl_path, auto_start_mode, wait).await
}
```

New:
```rust
Command::NewSession { target, no_redraw, no_escape, forward_agent, forward_open, no_oauth_redirect, oauth_timeout, wait } => {
    let (host, session) = parse_target(&target);
    let auto_start_mode = match &cli.ctl_socket {
        Some(_) => AutoStart::None,
        None if host == "local" => AutoStart::Server,
        None => AutoStart::Tunnel(host.clone()),
    };
    let ctl_path = resolve_ctl_path(cli.ctl_socket, &host);
    let resolved = config.resolve_session(Some(&host));
    let settings = gritty::config::SessionSettings {
        no_redraw: no_redraw || resolved.no_redraw,
        no_escape: no_escape || resolved.no_escape,
        forward_agent: forward_agent || resolved.forward_agent,
        forward_open: forward_open || resolved.forward_open,
        oauth_redirect: if no_oauth_redirect { false } else { resolved.oauth_redirect },
        oauth_timeout: oauth_timeout.unwrap_or(resolved.oauth_timeout),
    };
    new_session(session, settings, ctl_path, auto_start_mode, wait).await
}
```

Note: `new_session` now receives `session: Option<String>` (the parsed session part) instead of `target: Option<String>`.

**Step 2: Update `Attach` handler (around line 544)**

Old:
```rust
Command::Attach { host, target, ... } => {
    let ctl_path = resolve_ctl_path(cli.ctl_socket, host.as_deref())?;
    let resolved = config.resolve_session(host.as_deref());
    ...
    let code = attach(target, settings, ctl_path).await?;
}
```

New:
```rust
Command::Attach { target, no_redraw, no_escape, forward_agent, forward_open, no_oauth_redirect, oauth_timeout } => {
    let (host, session) = parse_target(&target);
    let ctl_path = resolve_ctl_path(cli.ctl_socket, &host);
    let resolved = config.resolve_session(Some(&host));
    let settings = gritty::config::SessionSettings {
        no_redraw: no_redraw || resolved.no_redraw,
        no_escape: no_escape || resolved.no_escape,
        forward_agent: forward_agent || resolved.forward_agent,
        forward_open: forward_open || resolved.forward_open,
        oauth_redirect: if no_oauth_redirect { false } else { resolved.oauth_redirect },
        oauth_timeout: oauth_timeout.unwrap_or(resolved.oauth_timeout),
    };
    let session = match session {
        Some(s) => s,
        None => {
            // Friendly error: list sessions and tell user to specify
            suggest_session("attach", &host, &ctl_path).await?;
            unreachable!()
        }
    };
    let code = attach(session, settings, ctl_path).await?;
    std::process::exit(code);
}
```

**Step 3: Update `Tail` handler (around line 539)**

```rust
Command::Tail { target } => {
    let (host, session) = parse_target(&target);
    let ctl_path = resolve_ctl_path(cli.ctl_socket, &host);
    let session = match session {
        Some(s) => s,
        None => {
            suggest_session("tail", &host, &ctl_path).await?;
            unreachable!()
        }
    };
    let code = tail_session(session, ctl_path).await?;
    std::process::exit(code);
}
```

**Step 4: Update `ListSessions` handler**

```rust
Command::ListSessions { target } => {
    let (host, _session) = parse_target(&target);
    let ctl_path = resolve_ctl_path(cli.ctl_socket, &host);
    list_sessions(ctl_path).await
}
```

**Step 5: Update `KillSession` handler**

```rust
Command::KillSession { target } => {
    let (host, session) = parse_target(&target);
    let ctl_path = resolve_ctl_path(cli.ctl_socket, &host);
    let session = match session {
        Some(s) => s,
        None => {
            suggest_session("kill-session", &host, &ctl_path).await?;
            unreachable!()
        }
    };
    kill_session(session, ctl_path).await
}
```

**Step 6: Update `KillServer` handler**

```rust
Command::KillServer { target } => {
    let (host, _session) = parse_target(&target);
    let ctl_path = resolve_ctl_path(cli.ctl_socket, &host);
    kill_server(ctl_path).await?;
    if host != "local" {
        gritty::connect::disconnect(&host).await?;
    }
    Ok(())
}
```

**Step 7: Update `Send` handler**

```rust
Command::Send { target, files } => {
    let (ctl_socket, host, session) = match target {
        Some(t) => {
            let (host, session) = parse_target(&t);
            (cli.ctl_socket, Some(host), session)
        }
        None => (cli.ctl_socket, None, None),
    };
    send_command(ctl_socket, host, session, files).await
}
```

**Step 8: Update `Receive` handler**

```rust
Command::Receive { target, dir } => {
    let (ctl_socket, host, session) = match target {
        Some(t) => {
            let (host, session) = parse_target(&t);
            (cli.ctl_socket, Some(host), session)
        }
        None => (cli.ctl_socket, None, None),
    };
    receive_command(ctl_socket, host, session, dir).await
}
```

**Step 9: Update `connect_send_socket()` ambiguous session error message**

In `connect_send_socket()`, around line 1112, change the error from:
```rust
let mut msg = format!("{n} sessions active, specify one with -t:\n");
```
to:
```rust
let mut msg = format!("{n} sessions active, specify one with host:session:\n");
```

**Step 10: Update `resolve_ctl_path` call in `connect_send_socket()`**

Around line 1096, change:
```rust
let ctl_path = resolve_ctl_path(ctl_socket, host.as_deref())?;
```
to:
```rust
let host = host.ok_or_else(|| anyhow::anyhow!("specify a host (use 'local' for the local server)"))?;
let ctl_path = resolve_ctl_path(ctl_socket, &host);
```

**Step 11: Verify it compiles**

Run: `cargo check`
Expected: error -- `suggest_session` not found. Implemented in next task.

---

### Task 4: Implement `suggest_session()` for friendly errors

**Files:**
- Modify: `src/main.rs`

**Step 1: Write a test for `suggest_session` formatting**

This is an async function that connects to a server, so a unit test isn't practical. Instead, we'll test it via the e2e tests in Task 6. For now, implement directly.

**Step 2: Implement `suggest_session()`**

Add near the other helper functions (around `list_sessions`):

```rust
/// Print available sessions and exit with an error when a session-requiring
/// command is invoked without the session part (e.g. `gritty attach local`
/// instead of `gritty attach local:session`).
async fn suggest_session(cmd: &str, host: &str, ctl_path: &Path) -> anyhow::Result<()> {
    use gritty::protocol::Frame;

    let ctl_path_buf = ctl_path.to_path_buf();
    let resp = match server_request(&ctl_path_buf, Frame::ListSessions).await {
        Ok(resp) => resp,
        Err(_) => {
            anyhow::bail!("specify a session: gritty {cmd} {host}:<session>");
        }
    };

    match resp {
        Frame::SessionInfo { sessions } if sessions.is_empty() => {
            anyhow::bail!("no active sessions on {host}");
        }
        Frame::SessionInfo { sessions } => {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();

            let mut msg = format!("specify a session: gritty {cmd} {host}:<session>\n\n");
            msg.push_str("  ID  Name     Age\n");
            for s in &sessions {
                let name = if s.name.is_empty() { "-".to_string() } else { s.name.clone() };
                let age = format_age(now, s.created_at);
                msg.push_str(&format!("  {}   {:<8} {}\n", s.id, name, age));
            }
            anyhow::bail!("{msg}");
        }
        _ => anyhow::bail!("specify a session: gritty {cmd} {host}:<session>"),
    }
}
```

**Step 3: Add `format_age()` helper**

```rust
fn format_age(now: u64, created_at: u64) -> String {
    let secs = now.saturating_sub(created_at);
    if secs < 60 {
        format!("{secs}s ago")
    } else if secs < 3600 {
        format!("{}m ago", secs / 60)
    } else if secs < 86400 {
        format!("{}h ago", secs / 3600)
    } else {
        format!("{}d ago", secs / 86400)
    }
}
```

**Step 4: Add test for `format_age()`**

```rust
#[test]
fn format_age_seconds() {
    assert_eq!(format_age(100, 70), "30s ago");
}

#[test]
fn format_age_minutes() {
    assert_eq!(format_age(1000, 700), "5m ago");
}

#[test]
fn format_age_hours() {
    assert_eq!(format_age(10000, 0), "2h ago");
}

#[test]
fn format_age_days() {
    assert_eq!(format_age(200000, 0), "2d ago");
}
```

**Step 5: Verify it compiles and tests pass**

Run: `cargo test --bin gritty`
Expected: PASS.

**Step 6: Commit**

```bash
git add src/main.rs
git commit -m "feat: unified host:session target syntax, drop -t flag"
```

---

### Task 5: Update help text and documentation

**Files:**
- Modify: `src/main.rs` (command doc comments for clap help)
- Modify: `CLAUDE.md` (CLI reference)
- Modify: `README.md` (if it has usage examples)

**Step 1: Update clap doc comments**

Update the `///` doc comments on each `Command` variant to reflect the new syntax:

- `NewSession`: `/// Create a new persistent session (auto-attaches). Target: host or host:name`
- `Attach`: `/// Attach to an existing session (detaches other clients). Target: host:session`
- `Tail`: `/// Tail a session's output (read-only, like tail -f). Target: host:session`
- `ListSessions`: `/// List active sessions. Target: host`
- `KillSession`: `/// Kill a specific session. Target: host:session`
- `KillServer`: `/// Kill the server and all sessions. Target: host`
- `Send`: `/// Send files to a paired receiver. Target: host[:session] (omit inside a session)`
- `Receive`: `/// Receive files from a paired sender. Target: host[:session] (omit inside a session)`

Also update the `///` comment on the `target` field in each variant to describe the format.

**Step 2: Update CLAUDE.md**

In the CLI reference section at the top of `CLAUDE.md`, update every command description:
- `gritty new-session <host[:name]>` instead of `gritty new-session <host> -t <name>`
- `gritty attach <host:session>` instead of `gritty attach <host> -t <id|name>`
- etc.

Remove all references to `-t`/`--target` flag.

**Step 3: Update README.md**

Check for usage examples and update them.

**Step 4: Verify help output**

Run: `cargo run -- --help` and `cargo run -- new --help`
Expected: new syntax shown, no `-t` flag.

**Step 5: Commit**

```bash
git add src/main.rs CLAUDE.md README.md
git commit -m "docs: update CLI help and docs for host:session syntax"
```

---

### Task 6: Update integration tests

**Files:**
- Modify: `tests/e2e_test.rs` (update any `-t` usage)
- Modify: `tests/daemon_test.rs` (update any `-t` usage)

**Step 1: Search for `-t` usage in test files**

Run: `grep -n 'target\|"-t"' tests/e2e_test.rs tests/daemon_test.rs`

These tests mostly test the server/protocol layer directly (not the CLI), so changes should be minimal. The daemon tests use `Frame::Attach { session: ... }` directly. But check for any CLI invocations.

**Step 2: Update any affected tests**

Fix any tests that invoke CLI commands with the old `-t` syntax.

**Step 3: Run full test suite**

Run: `cargo test`
Expected: all tests PASS.

**Step 4: Run clippy and format**

Run: `just fmt && just check`
Expected: clean.

**Step 5: Commit**

```bash
git add tests/
git commit -m "test: update tests for host:session syntax"
```
