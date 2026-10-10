//! Binary-level end-to-end tests: boot the real `zigzag` daemon as a
//! subprocess and drive its HTTP API over loopback TCP.
//!
//! Hermetic by construction:
//! - a fresh temp dir per test (secret file, state file, HOME)
//! - `ZIGZAG_UPDATE_POLICY=paused`, so the update checker never touches the
//!   network
//! - a fake `tailscale` on PATH answering `127.0.0.2`, so the daemon binds
//!   loopback only
//! - no `--watch-repo` flags and no `~/.zigzag/config.yaml`, so neither the
//!   GitHub watch loop nor the review loop starts
//! - the OS assigns free ports, parsed from the daemon's
//!   `listening on http://127.0.0.1:PORT` log line
//!
//! `/v1/exec` and `/v1/spawn` are expected to fail closed (500) here: the
//! exec policy lives in the macOS keychain behind the GUI login session,
//! which CI runners do not have. The in-crate `e2e` module covers those
//! paths with an injected policy instead.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

static RELAY_COUNTER: AtomicU64 = AtomicU64::new(0);

/// A running daemon plus everything needed to talk to it. Kills the child
/// and removes the temp dir on drop.
struct TestRelay {
    dir: PathBuf,
    port: u16,
    secret: String,
    child: Child,
}

impl TestRelay {
    fn start() -> Self {
        let n = RELAY_COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!("zigzag-e2e-bin-{}-{n}", std::process::id()));
        if dir.exists() {
            std::fs::remove_dir_all(&dir).unwrap();
        }
        std::fs::create_dir_all(&dir).unwrap();

        // Secret file: >= 32 bytes, not group/world accessible.
        let secret = "e2e-binary-test-secret-0123456789abcdef".to_owned();
        let secret_file = dir.join("daemon.token");
        std::fs::write(&secret_file, &secret).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&secret_file, std::fs::Permissions::from_mode(0o600)).unwrap();
        }

        // Fake `tailscale ip -4` with a bindable address: the daemon binds
        // it alongside 127.0.0.1, so it must be a local address distinct
        // from loopback (the 127/8 aliases Linux has are not bindable on
        // macOS).
        let tailnet_ip = bindable_tailnet_ip();
        let bin_dir = dir.join("bin");
        std::fs::create_dir_all(&bin_dir).unwrap();
        let tailscale = bin_dir.join("tailscale");
        std::fs::write(&tailscale, format!("#!/bin/sh\necho {tailnet_ip}\n")).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&tailscale, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        // The agent-restart E2E drives the real agent API without requiring
        // an installed CLI. Keep the fake harness alive after its daemon dies.
        let codex = bin_dir.join("codex");
        std::fs::write(
            &codex,
            "#!/bin/sh\necho started >> \"$HOME/fake-agent.started\"\nexec sleep 300\n",
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&codex, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        let path = format!(
            "{}:{}",
            bin_dir.display(),
            std::env::var("PATH").unwrap_or_default()
        );

        let state_file = dir.join("events.json");
        let mut child = Command::new(env!("CARGO_BIN_EXE_zigzag"))
            .arg("--secret-file")
            .arg(&secret_file)
            .arg("--state-file")
            .arg(&state_file)
            .arg("--port")
            .arg("0")
            .arg("--socket-port")
            .arg("0")
            .env("PATH", path)
            .env("HOME", &dir)
            .env("ZIGZAG_WORKTREE_ROOTS", &dir)
            .env("ZIGZAG_WORKTREE_REPO_ROOT", &dir)
            .env("ZIGZAG_UPDATE_POLICY", "paused")
            .stderr(Stdio::piped())
            .stdout(Stdio::null())
            .stdin(Stdio::null())
            .spawn()
            .expect("failed to spawn zigzag daemon");

        let port = wait_for_listening(&mut child);
        assert!(
            child.try_wait().expect("could not poll daemon").is_none(),
            "daemon exited during startup"
        );
        Self {
            dir,
            port,
            secret,
            child,
        }
    }

    /// Raw HTTP request. Returns (status code, body).
    fn http(
        &self,
        method: &str,
        target: &str,
        body: Option<&str>,
        token: Option<&str>,
    ) -> (u16, String) {
        let body = body.unwrap_or("");
        let auth = token
            .map(|t| format!("Authorization: Bearer {t}\r\n"))
            .unwrap_or_default();
        let request = format!(
            "{method} {target} HTTP/1.1\r\nHost: 127.0.0.1\r\n{auth}Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        let mut stream =
            TcpStream::connect(("127.0.0.1", self.port)).expect("could not connect to daemon");
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        stream.write_all(request.as_bytes()).unwrap();
        // The daemon always replies with `Connection: close`.
        let mut raw = Vec::new();
        stream.read_to_end(&mut raw).unwrap();
        let text = String::from_utf8_lossy(&raw).into_owned();
        let (head, body) = text
            .split_once("\r\n\r\n")
            .unwrap_or_else(|| panic!("malformed HTTP response: {text:?}"));
        let status: u16 = head
            .lines()
            .next()
            .unwrap()
            .split_whitespace()
            .nth(1)
            .unwrap()
            .parse()
            .unwrap();
        (status, body.to_owned())
    }

    fn authed(&self, method: &str, target: &str, body: Option<&str>) -> (u16, String) {
        self.http(method, target, body, Some(&self.secret))
    }

    fn restart(&mut self) {
        self.child
            .kill()
            .expect("could not stop daemon for restart");
        self.child.wait().expect("could not reap old daemon");

        let bin_dir = self.dir.join("bin");
        let path = format!(
            "{}:{}",
            bin_dir.display(),
            std::env::var("PATH").unwrap_or_default()
        );
        let secret_file = self.dir.join("daemon.token");
        let state_file = self.dir.join("events.json");
        let mut child = Command::new(env!("CARGO_BIN_EXE_zigzag"))
            .arg("--secret-file")
            .arg(&secret_file)
            .arg("--state-file")
            .arg(&state_file)
            .arg("--port")
            .arg("0")
            .arg("--socket-port")
            .arg("0")
            .env("PATH", path)
            .env("HOME", &self.dir)
            .env("ZIGZAG_WORKTREE_ROOTS", &self.dir)
            .env("ZIGZAG_WORKTREE_REPO_ROOT", &self.dir)
            .env("ZIGZAG_UPDATE_POLICY", "paused")
            .stderr(Stdio::piped())
            .stdout(Stdio::null())
            .stdin(Stdio::null())
            .spawn()
            .expect("failed to restart zigzag daemon");
        self.port = wait_for_listening(&mut child);
        assert!(
            child
                .try_wait()
                .expect("could not poll restarted daemon")
                .is_none(),
            "daemon exited during restart"
        );
        self.child = child;
    }
}

impl Drop for TestRelay {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// Find an IP address that this machine can bind for the fake `tailscale ip
/// -4`. Prefers loopback aliases (Linux CI), then the default route's address
/// (macOS), and finally loopback for restricted test sandboxes. Tests use
/// ephemeral ports, so two loopback listeners still bind independently.
fn bindable_tailnet_ip() -> String {
    for candidate in ["127.0.0.2", "127.0.0.3"] {
        if std::net::TcpListener::bind((candidate, 0)).is_ok() {
            return candidate.to_owned();
        }
    }
    // UDP "connect" sends no packets; it just asks the OS which source
    // address it would use for outbound traffic.
    if let Ok(socket) = std::net::UdpSocket::bind("0.0.0.0:0")
        && socket.connect("8.8.8.8:80").is_ok()
        && let Ok(addr) = socket.local_addr()
    {
        let ip = addr.ip().to_string();
        if !ip.starts_with("127.") {
            return ip;
        }
    }
    "127.0.0.1".to_owned()
}

/// Drain the daemon's stderr until the `listening on http://127.0.0.1:PORT`
/// line appears; return the port. Panics if the daemon dies first.
fn wait_for_listening(child: &mut Child) -> u16 {
    let stderr = child.stderr.take().expect("daemon stderr not piped");
    let (tx, rx) = std::sync::mpsc::channel::<String>();
    std::thread::spawn(move || {
        for line in BufReader::new(stderr).lines() {
            match line {
                Ok(line) => {
                    if tx.send(line).is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });
    let marker = "listening on http://127.0.0.1:";
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut recent: Vec<String> = Vec::new();
    loop {
        match rx.recv_timeout(Duration::from_millis(200)) {
            Ok(line) => {
                if line.contains("invalid agent registry") {
                    panic!("daemon rejected the agent registry: {line}");
                }
                recent.push(line);
                if recent.len() > 20 {
                    recent.remove(0);
                }
                if let Some(port) = recent
                    .last()
                    .and_then(|line| line.split_once(marker).map(|(_, rest)| rest))
                    .and_then(|rest| rest.split_whitespace().next())
                    .and_then(|port| port.parse::<u16>().ok())
                {
                    return port;
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                if Instant::now() > deadline {
                    let alive = child
                        .try_wait()
                        .map(|status| status.is_none())
                        .unwrap_or(false);
                    panic!(
                        "daemon did not listen within 20s (alive={alive}); recent stderr:\n{}",
                        recent.join("\n")
                    );
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                panic!(
                    "daemon stderr closed before it started listening; recent stderr:\n{}",
                    recent.join("\n")
                );
            }
        }
    }
}

#[test]
fn binary_health_and_auth() {
    let relay = TestRelay::start();

    // No token.
    let (status, body) = relay.http("GET", "/v1/health", None, None);
    assert_eq!(status, 401, "missing token not rejected: {body}");

    // Wrong token.
    let (status, _) = relay.http("GET", "/v1/health", None, Some("wrong"));
    assert_eq!(status, 401, "wrong token not rejected");

    // Right token.
    let (status, body) = relay.authed("GET", "/v1/health", None);
    assert_eq!(status, 200, "health failed: {body}");
    assert!(
        body.contains(r#""status":"ok""#),
        "unexpected health body: {body}"
    );
}

#[test]
fn binary_events_post_get_roundtrip() {
    let relay = TestRelay::start();

    // POST without an id is a 400.
    let (status, _) = relay.authed("POST", "/v1/events", Some(r#"{"kind":"x"}"#));
    assert_eq!(status, 400);

    // Malformed JSON is a 400.
    let (status, _) = relay.authed("POST", "/v1/events", Some("not json"));
    assert_eq!(status, 400);

    // First post: 201, not a duplicate.
    let (status, body) = relay.authed(
        "POST",
        "/v1/events",
        Some(r#"{"id":"e2e-evt-1","kind":"e2e","task_id":"t1"}"#),
    );
    assert_eq!(status, 201, "event post failed: {body}");
    assert!(
        body.contains(r#""duplicate":false"#),
        "expected duplicate:false: {body}"
    );

    // Same id again: 200, duplicate.
    let (status, body) = relay.authed(
        "POST",
        "/v1/events",
        Some(r#"{"id":"e2e-evt-1","kind":"e2e","task_id":"t1"}"#),
    );
    assert_eq!(status, 200, "duplicate post failed: {body}");
    assert!(
        body.contains(r#""duplicate":true"#),
        "expected duplicate:true: {body}"
    );

    // GET returns the event (timeout=0: no long-poll).
    let (status, body) = relay.authed("GET", "/v1/events?after=0&timeout=0", None);
    assert_eq!(status, 200, "events get failed: {body}");
    assert!(
        body.contains("e2e-evt-1"),
        "posted event missing from stream: {body}"
    );

    // Bad query params are a 400.
    let (status, _) = relay.authed("GET", "/v1/events?after=bogus&timeout=0", None);
    assert_eq!(status, 400);
}

#[test]
fn binary_agents_endpoints() {
    let relay = TestRelay::start();

    let (status, body) = relay.authed("GET", "/v1/agents", None);
    assert_eq!(status, 200, "agents list failed: {body}");
    assert!(
        body.contains(r#""agents":[]"#),
        "expected empty agents list: {body}"
    );

    let (status, body) = relay.authed("GET", "/v1/agents/no-such-agent", None);
    assert_eq!(status, 404, "unknown agent did not 404: {body}");
    assert!(body.contains("unknown_agent"), "unexpected body: {body}");

    let (status, _) = relay.authed("GET", "/v1/agents/no-such-agent/logs", None);
    assert_eq!(status, 404, "unknown agent logs did not 404");

    let (status, _) = relay.authed("GET", "/v1/agents?bogus=1", None);
    assert_eq!(status, 400, "bad agent query not rejected");
}

#[test]
fn binary_agent_remains_tracked_across_real_daemon_restart() {
    let mut relay = TestRelay::start();
    // Use the test sandbox as both allowed project root and repository root.
    // no_branch avoids creating a worktree; the fake Codex executable is a
    // real detached child process that survives killing the daemon.
    let project_dir = relay.dir.display().to_string();
    let body = format!(
        r#"{{"prompt":"keep running for restart test","project_dir":"{project_dir}","no_branch":true,"no_auto_pr":true}}"#
    );
    let (status, created) = relay.authed("POST", "/v1/agents", Some(&body));
    assert_eq!(status, 200, "agent create failed: {created}");
    let agent_id = created
        .split("\"id\":\"")
        .nth(1)
        .and_then(|value| value.split('"').next())
        .expect("agent create response omitted id")
        .to_owned();

    let old_daemon_pid = relay.child.id();
    relay.restart();
    assert_ne!(
        relay.child.id(),
        old_daemon_pid,
        "daemon process was not replaced"
    );

    let (status, agent) = relay.authed("GET", &format!("/v1/agents/{agent_id}"), None);
    assert_eq!(
        status, 200,
        "agent disappeared after daemon restart: {agent}"
    );
    assert!(
        agent.contains(r#""state":"running""#),
        "live agent was not recovered as tracked: {agent}"
    );
    let (status, listing) = relay.authed("GET", "/v1/agents?state=running", None);
    assert_eq!(status, 200, "agent listing failed after restart: {listing}");
    assert!(
        listing.contains(&agent_id),
        "running agent missing from list: {listing}"
    );

    // Stop the detached child so the E2E leaves no process behind.
    let (status, stopped) = relay.authed("DELETE", &format!("/v1/agents/{agent_id}"), None);
    assert_eq!(status, 200, "could not clean up test agent: {stopped}");
}

#[test]
fn binary_routing_and_framing_errors() {
    let relay = TestRelay::start();

    // Unknown route.
    let (status, _) = relay.authed("GET", "/v1/nope", None);
    assert_eq!(status, 404);

    // Wrong method on a known route.
    let (status, _) = relay.authed("DELETE", "/v1/health", None);
    assert_eq!(status, 404);

    // Oversized header block: 120 headers exceeds the 100-header DoS cap.
    let mut request = String::from("GET /v1/health HTTP/1.1\r\nHost: x\r\n");
    request.push_str(&format!("Authorization: Bearer {}\r\n", relay.secret));
    for i in 0..120 {
        request.push_str(&format!("X-Pad-{i}: abcdefgh\r\n"));
    }
    request.push_str("\r\n");
    let mut stream = TcpStream::connect(("127.0.0.1", relay.port)).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    stream.write_all(request.as_bytes()).unwrap();
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).unwrap();
    let text = String::from_utf8_lossy(&raw).into_owned();
    assert!(
        text.starts_with("HTTP/1.1 431"),
        "expected 431 for oversized headers, got: {}",
        text.lines().next().unwrap_or("")
    );
}

#[test]
fn binary_exec_and_spawn_fail_closed_without_gui_session() {
    // CI runners (and this test) have no macOS GUI login session and no
    // keychain-backed exec policy, so both endpoints must fail closed.
    let relay = TestRelay::start();

    let (status, body) = relay.authed(
        "POST",
        "/v1/exec",
        Some(r#"{"id":"e2e-exec","bin":"sh","args":["-c","echo hi"]}"#),
    );
    assert_eq!(status, 500, "exec did not fail closed: {body}");
    assert!(
        body.contains("could_not_read_execution_policy"),
        "unexpected exec body: {body}"
    );

    let (status, body) = relay.authed(
        "POST",
        "/v1/spawn",
        Some(r#"{"id":"e2e-spawn","bin":"sh","args":["-c","echo hi"]}"#),
    );
    assert_eq!(status, 500, "spawn did not fail closed: {body}");
    assert!(
        body.contains("could_not_read_execution_policy"),
        "unexpected spawn body: {body}"
    );
}

#[test]
fn binary_proc_kill_requires_control_secret() {
    // This relay was started without --control-secret-file, so the kill
    // route must 404 rather than accept the main secret.
    let relay = TestRelay::start();

    let (status, body) = relay.authed(
        "POST",
        "/v1/proc/0123456789abcdef0123456789abcdef/kill",
        None,
    );
    assert_eq!(
        status, 404,
        "kill without control secret did not 404: {body}"
    );
}

#[test]
fn binary_worktree_requests_are_validated() {
    let relay = TestRelay::start();

    // Not an object / missing fields.
    let (status, _) = relay.authed("POST", "/v1/worktrees", Some("{}"));
    assert_eq!(status, 400);
    let (status, _) = relay.authed("POST", "/v1/worktrees", Some("[]"));
    assert_eq!(status, 400);

    // A path outside the allowed roots is rejected (on Linux CI the roots
    // don't exist at all, so every path is rejected fail-closed).
    let (status, body) = relay.authed(
        "POST",
        "/v1/worktrees",
        Some(&format!(
            r#"{{"path":"{}/wt","branch":"b","repo":"{}/repo"}}"#,
            relay.dir.display(),
            relay.dir.display()
        )),
    );
    assert_eq!(status, 400, "outside-roots path not rejected: {body}");
    assert!(
        body.contains(r#""error""#),
        "expected JSON error body: {body}"
    );

    // DELETE with a missing path field.
    let (status, _) = relay.authed("DELETE", "/v1/worktrees", Some("{}"));
    assert_eq!(status, 400);
}
