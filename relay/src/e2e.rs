//! End-to-end lifecycle tests for the relay.
//!
//! These tests drive multi-step flows through the real HTTP request handling
//! (`handle_with_services` over a loopback TCP socket) using the harness in
//! [`crate::tests`]: spawn -> poll -> agent status -> logs -> events, exec
//! allowlist enforcement, auth, proc kill, and worktree round-trips.
//!
//! They are hermetic: the only subprocesses executed are `/bin/sh` snippets
//! injected through a test-only policy, worktrees are created inside a temp
//! git repo, and no network is touched.

use crate::exec;
use crate::proc::{agent_stderr_path, agent_transcript_path};
use crate::review_loop;
use crate::routes::worktrees::{worktree_create_plan, worktree_delete_plan};
use crate::tests::{
    poll_until_complete, request_once, request_once_with_gate_token, response_json, spawn_for_test,
    test_policy, test_server, worktree_test_base, worktree_test_repo, worktree_test_roots,
};
use relay_core::Json;
use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

static TASK_COUNTER: AtomicU64 = AtomicU64::new(0);

fn unique_id(prefix: &str) -> String {
    let n = TASK_COUNTER.fetch_add(1, Ordering::SeqCst);
    format!("e2e-{prefix}-{n}")
}

fn json_array(value: &Json) -> &[Json] {
    match value {
        Json::Array(items) => items,
        other => panic!("expected JSON array, got {other:?}"),
    }
}

fn agent_create_test_repo(id: &str) -> PathBuf {
    let repo = std::env::temp_dir().join(format!("zigzag-agent-e2e-{id}"));
    std::fs::create_dir_all(&repo).expect("could not create agent test repository");
    for args in [
        vec!["init", "-b", "main"],
        vec!["config", "user.email", "zigzag-test@example.com"],
        vec!["config", "user.name", "Zigzag test"],
        vec!["commit", "--allow-empty", "-m", "init"],
    ] {
        let output = Command::new("git")
            .args(args)
            .current_dir(&repo)
            .output()
            .expect("could not run git for agent test repository");
        assert!(
            output.status.success(),
            "git setup failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let origin = repo.join(".test-origin.git");
    let output = Command::new("git")
        .args(["init", "--bare", "--initial-branch=main", "-q"])
        .arg(&origin)
        .current_dir(&repo)
        .output()
        .expect("could not initialize agent test origin");
    assert!(
        output.status.success(),
        "agent test origin setup failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    for args in [
        vec!["remote", "add", "origin", origin.to_str().unwrap()],
        vec!["push", "--set-upstream", "origin", "main"],
    ] {
        let output = Command::new("git")
            .args(args)
            .current_dir(&repo)
            .output()
            .expect("could not configure agent test origin");
        assert!(
            output.status.success(),
            "agent test origin setup failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    repo
}

/// Full agent lifecycle: spawn -> poll to completion -> agent record ->
/// logs -> event stream. This is the closest thing to "create a Codex agent
/// and watch it finish" without burning real Codex credits.
#[test]
fn e2e_spawn_to_completion_records_logs_and_events() {
    let (state, _state_path) = test_server();
    let policy = test_policy();
    let id = unique_id("lifecycle");

    let raw = request_once(
        Arc::clone(&state),
        &policy,
        "POST",
        "/v1/spawn",
        &format!(r#"{{"id":"{id}","bin":"sh","args":["-c","echo hello-e2e; echo err-e2e >&2"]}}"#),
    );
    assert!(raw.starts_with("HTTP/1.1 200"), "spawn failed: {raw}");
    let spawned = response_json(raw);
    assert_eq!(
        spawned.object("id").and_then(Json::as_str),
        Some(id.as_str())
    );
    let handle = spawned
        .object("proc")
        .and_then(Json::as_str)
        .expect("spawn response missing proc handle")
        .to_owned();

    let completed = poll_until_complete(&state, &policy, &handle);
    assert_eq!(
        completed.object("running"),
        Some(&Json::Bool(false)),
        "proc never finished"
    );

    // The agent record exists and is no longer running. The registry is
    // keyed by proc handle (not task id).
    let raw = request_once(
        Arc::clone(&state),
        &policy,
        "GET",
        &format!("/v1/agents/{handle}"),
        "",
    );
    assert!(
        raw.starts_with("HTTP/1.1 200"),
        "agent status failed: {raw}"
    );
    let agent = response_json(raw);
    assert_eq!(
        agent.object("task_id").and_then(Json::as_str),
        Some(id.as_str())
    );
    assert_ne!(
        agent.object("state").and_then(Json::as_str),
        Some("running"),
        "agent still running after proc completed"
    );

    // Stdout made it into the log spool.
    let raw = request_once(
        Arc::clone(&state),
        &policy,
        "GET",
        &format!("/v1/agents/{handle}/logs?stream=stdout"),
        "",
    );
    assert!(raw.starts_with("HTTP/1.1 200"), "logs failed: {raw}");
    let logs = response_json(raw);
    let stdout: String = json_array(logs.object("records").expect("logs missing records"))
        .iter()
        .filter_map(|record| record.object("data"))
        .filter_map(Json::as_str)
        .collect::<Vec<_>>()
        .join("");
    assert!(
        stdout.contains("hello-e2e"),
        "stdout missing from logs: {stdout}"
    );

    // Stderr is kept separate from stdout.
    let raw = request_once(
        Arc::clone(&state),
        &policy,
        "GET",
        &format!("/v1/agents/{handle}/logs?stream=stderr"),
        "",
    );
    let logs = response_json(raw);
    let stderr: String = json_array(logs.object("records").expect("logs missing records"))
        .iter()
        .filter_map(|record| record.object("data"))
        .filter_map(Json::as_str)
        .collect::<Vec<_>>()
        .join("");
    assert!(
        stderr.contains("err-e2e"),
        "stderr missing from logs: {stderr}"
    );
    assert!(
        !stderr.contains("hello-e2e"),
        "stdout leaked into stderr stream"
    );

    // The event stream recorded the completion (timeout=0: return immediately).
    let raw = request_once(
        Arc::clone(&state),
        &policy,
        "GET",
        "/v1/events?after=0&timeout=0",
        "",
    );
    assert!(raw.starts_with("HTTP/1.1 200"), "events failed: {raw}");
    let events = response_json(raw);
    let saw_completed = json_array(events.object("events").expect("events missing list"))
        .iter()
        .any(|event| {
            event.object("kind").and_then(Json::as_str) == Some("process_completed")
                && event.object("task_id").and_then(Json::as_str) == Some(id.as_str())
        });
    assert!(saw_completed, "no process_completed event for {id}");
}

/// Agent creation bypasses the arbitrary-command allowlist because the relay,
/// not the caller, owns the fixed `codex exec` invocation. Exercise the
/// complete agent API lifecycle with a policy that deliberately excludes
/// `codex`: create -> list -> pause -> resume -> delete.
#[test]
fn e2e_agent_create_list_pause_resume_delete_without_exec_policy() {
    let (state, _state_path) = test_server();
    let policy = test_policy(); // Only /bin/sh is allowed; no `codex` entry.
    let id = unique_id("agent-api");
    let repo = agent_create_test_repo(&id);
    let branch = format!("codex/{id}");
    let worktree = std::env::temp_dir().join(format!("zigzag-{id}"));
    let worktree_text = worktree.to_string_lossy();
    let body = format!(
        r#"{{"prompt":"keep running","project_dir":"{}","branch":"{branch}","worktree":"{worktree_text}"}}"#,
        repo.display()
    );

    let created = response_json(request_once(
        Arc::clone(&state),
        &policy,
        "POST",
        "/v1/agents",
        &body,
    ));
    let handle = created
        .object("id")
        .and_then(Json::as_str)
        .expect("agent create response missing id")
        .to_owned();
    assert!(worktree.exists(), "agent worktree was not created");

    let listed = response_json(request_once(
        Arc::clone(&state),
        &policy,
        "GET",
        "/v1/agents",
        "",
    ));
    let agent = json_array(listed.object("agents").expect("agents list missing agents"))
        .iter()
        .find(|agent| agent.object("id").and_then(Json::as_str) == Some(handle.as_str()))
        .expect("created agent missing from list");
    assert_eq!(
        agent.object("state").and_then(Json::as_str),
        Some("running")
    );

    let paused = response_json(request_once(
        Arc::clone(&state),
        &policy,
        "POST",
        &format!("/v1/agents/{handle}/pause"),
        "{}",
    ));
    assert_eq!(paused.object("paused"), Some(&Json::Bool(true)));

    let resumed = response_json(request_once(
        Arc::clone(&state),
        &policy,
        "POST",
        &format!("/v1/agents/{handle}/resume"),
        "{}",
    ));
    assert_eq!(resumed.object("paused"), Some(&Json::Bool(false)));

    let deleted = response_json(request_once(
        Arc::clone(&state),
        &policy,
        "DELETE",
        &format!("/v1/agents/{handle}"),
        "",
    ));
    assert_eq!(deleted.object("stopped"), Some(&Json::Bool(true)));

    let stopped = response_json(request_once(
        Arc::clone(&state),
        &policy,
        "GET",
        &format!("/v1/agents/{handle}"),
        "",
    ));
    assert!(
        matches!(
            stopped.object("state").and_then(Json::as_str),
            Some("stopped") | Some("killed")
        ),
        "agent was not stopped after delete: {stopped:?}"
    );

    assert!(
        !worktree.exists(),
        "agent delete did not clean up its worktree"
    );
    std::fs::remove_dir_all(repo).expect("could not clean up agent test repository");
}

#[test]
fn e2e_agent_no_branch_runs_in_project_without_worktree_or_checkout() {
    let (state, _state_path) = test_server();
    let policy = test_policy();
    let id = unique_id("agent-no-branch");
    let repo = agent_create_test_repo(&id).canonicalize().unwrap();
    let repo_text = repo.to_string_lossy();
    let body =
        format!(r#"{{"prompt":"inspect only","project_dir":"{repo_text}","no_branch":true}}"#);

    let created = response_json(request_once(
        Arc::clone(&state),
        &policy,
        "POST",
        "/v1/agents",
        &body,
    ));
    let handle = created
        .object("id")
        .and_then(Json::as_str)
        .expect("agent create response missing id")
        .to_owned();
    assert_eq!(created.object("worktree"), Some(&Json::Null));
    assert_eq!(
        created.object("working_dir").and_then(Json::as_str),
        Some(repo_text.as_ref())
    );

    let agent = state
        .supervisor
        .registry
        .get(&handle)
        .expect("no-branch agent missing from registry");
    assert!(agent.worktree_path.is_none());
    assert!(!repo.join(".codex-prompt.md").exists());
    assert!(!repo.join("last-message.txt").exists());
    let branch = Command::new("git")
        .args(["branch", "--show-current"])
        .current_dir(&repo)
        .output()
        .expect("could not inspect current branch");
    assert!(branch.status.success());
    assert_eq!(String::from_utf8_lossy(&branch.stdout).trim(), "main");

    let stopped = request_once(
        Arc::clone(&state),
        &policy,
        "DELETE",
        &format!("/v1/agents/{handle}"),
        "",
    );
    assert!(stopped.starts_with("HTTP/1.1 200"), "{stopped}");
    drop(state);
    let _ = std::fs::remove_dir_all(repo);
}

/// API-created Codex agents save their JSON event stream outside the bounded
/// diagnostics spool, so it remains available through the transcript API.
#[test]
fn e2e_agent_create_persists_transcript_and_serves_it() {
    let (state, _state_path) = test_server();
    let policy = test_policy();
    let id = unique_id("agent-transcript");
    let repo = agent_create_test_repo(&id);
    let branch = format!("codex/{id}");
    let worktree = std::env::temp_dir().join(format!("zigzag-{id}"));
    let worktree_text = worktree.to_string_lossy();
    let body = format!(
        r#"{{"prompt":"persist-transcript","project_dir":"{}","branch":"{branch}","worktree":"{worktree_text}"}}"#,
        repo.display()
    );

    let created = response_json(request_once(
        Arc::clone(&state),
        &policy,
        "POST",
        "/v1/agents",
        &body,
    ));
    let handle = created
        .object("id")
        .and_then(Json::as_str)
        .expect("agent create response missing id")
        .to_owned();
    let completed = poll_until_complete(&state, &policy, &handle);
    assert_eq!(
        completed.object("exit_code").and_then(Json::as_u64),
        Some(0)
    );

    let transcript_path = agent_transcript_path(&handle).expect("valid agent transcript path");
    let transcript = std::fs::read_to_string(&transcript_path)
        .expect("API-created agent transcript was not persisted");
    assert!(
        transcript.contains("persisted-agent-output"),
        "persisted transcript missing Codex output: {transcript}"
    );
    let stderr_path = agent_stderr_path(&handle).expect("valid agent stderr path");
    let stderr =
        std::fs::read_to_string(&stderr_path).expect("API-created agent stderr was not persisted");
    assert!(
        stderr.contains("persisted-agent-stderr"),
        "persisted stderr missing Codex diagnostics: {stderr}"
    );

    let served = response_json(request_once(
        Arc::clone(&state),
        &policy,
        "GET",
        &format!("/v1/agents/{handle}/transcript"),
        "",
    ));
    assert!(
        served
            .object("stdout")
            .and_then(Json::as_str)
            .is_some_and(|stdout| stdout.contains("persisted-agent-output")),
        "transcript endpoint did not return persisted output: {served:?}"
    );
    assert!(
        served
            .object("stderr")
            .and_then(Json::as_str)
            .is_some_and(|stderr| stderr.contains("persisted-agent-stderr")),
        "transcript endpoint did not return persisted stderr: {served:?}"
    );

    let output = Command::new("git")
        .args(["worktree", "remove", "--force", worktree_text.as_ref()])
        .current_dir(&repo)
        .output()
        .expect("could not clean up agent test worktree");
    assert!(output.status.success());
    std::fs::remove_dir_all(repo).expect("could not clean up agent test repository");
    std::fs::remove_file(transcript_path).expect("could not clean up agent transcript");
    std::fs::remove_file(stderr_path).expect("could not clean up agent stderr");
}

/// A failing command records its exit code and stderr instead of vanishing.
#[test]
fn e2e_failed_spawn_records_exit_code() {
    let (state, _state_path) = test_server();
    let policy = test_policy();
    let id = unique_id("failure");

    let handle = spawn_for_test(&state, &policy, &id, "echo boom >&2; exit 3");
    let completed = poll_until_complete(&state, &policy, &handle);
    assert_eq!(
        completed.object("running"),
        Some(&Json::Bool(false)),
        "proc never finished"
    );
    assert_eq!(
        completed.object("exit_code").and_then(Json::as_u64),
        Some(3),
        "exit code not recorded"
    );

    let raw = request_once(
        Arc::clone(&state),
        &policy,
        "GET",
        &format!("/v1/agents/{handle}/logs?stream=stderr"),
        "",
    );
    let logs = response_json(raw);
    let stderr: String = json_array(logs.object("records").expect("logs missing records"))
        .iter()
        .filter_map(|record| record.object("data"))
        .filter_map(Json::as_str)
        .collect::<Vec<_>>()
        .join("");
    assert!(stderr.contains("boom"), "stderr missing: {stderr}");
}

/// The exec allowlist is enforced: allowed bins run, everything else is denied
/// without revealing policy internals.
#[test]
fn e2e_exec_honors_allowlist() {
    let (state, _state_path) = test_server();
    let policy = test_policy();

    // Allowed: sh -c "<anything>".
    let raw = request_once(
        Arc::clone(&state),
        &policy,
        "POST",
        "/v1/exec",
        r#"{"id":"exec-ok","bin":"sh","args":["-c","echo exec-works"]}"#,
    );
    assert!(raw.starts_with("HTTP/1.1 200"), "exec failed: {raw}");
    let result = response_json(raw);
    let stdout = result.object("stdout").and_then(Json::as_str).unwrap_or("");
    assert!(
        stdout.contains("exec-works"),
        "unexpected exec stdout: {stdout}"
    );

    // Denied: unknown bin.
    let raw = request_once(
        Arc::clone(&state),
        &policy,
        "POST",
        "/v1/exec",
        r#"{"id":"exec-denied-bin","bin":"nope","args":[]}"#,
    );
    let denied = response_json(raw);
    assert_eq!(
        denied.object("error").and_then(Json::as_str),
        Some("denied"),
        "unknown bin was not denied"
    );

    // Denied: known bin, disallowed args (policy only allows ["-c", ...]).
    let raw = request_once(
        Arc::clone(&state),
        &policy,
        "POST",
        "/v1/exec",
        r#"{"id":"exec-denied-args","bin":"sh","args":["--evil"]}"#,
    );
    let denied = response_json(raw);
    assert_eq!(
        denied.object("error").and_then(Json::as_str),
        Some("denied"),
        "disallowed args were not denied"
    );

    // Denied: spawn path enforces the same policy.
    let raw = request_once(
        Arc::clone(&state),
        &policy,
        "POST",
        "/v1/spawn",
        r#"{"id":"spawn-denied","bin":"nope","args":[]}"#,
    );
    let denied = response_json(raw);
    assert_eq!(
        denied.object("error").and_then(Json::as_str),
        Some("denied"),
        "spawn with unknown bin was not denied"
    );
}

/// Every route requires the bearer token: missing and wrong tokens get 401.
#[test]
fn e2e_auth_rejects_bad_tokens() {
    let (state, _state_path) = test_server();
    let policy = test_policy();

    for target in ["/v1/health", "/v1/agents", "/v1/events?after=0&timeout=0"] {
        // No token at all.
        let raw = request_once_with_gate_token(
            Arc::clone(&state),
            &policy,
            "GET",
            target,
            "",
            "",
            review_loop::gate_report,
        );
        assert!(
            raw.starts_with("HTTP/1.1 401"),
            "missing token not rejected for {target}: {raw}"
        );
        // Wrong token.
        let raw = request_once_with_gate_token(
            Arc::clone(&state),
            &policy,
            "GET",
            target,
            "",
            "Bearer wrong-token",
            review_loop::gate_report,
        );
        assert!(
            raw.starts_with("HTTP/1.1 401"),
            "wrong token not rejected for {target}: {raw}"
        );
    }

    // The right token works.
    let raw = request_once(Arc::clone(&state), &policy, "GET", "/v1/health", "");
    assert!(raw.starts_with("HTTP/1.1 200"), "health failed: {raw}");
    assert_eq!(
        response_json(raw).object("status").and_then(Json::as_str),
        Some("ok")
    );
}

/// A spawn that cannot read its policy fails closed with a 500. This is the
/// same failure the daemon produces on Linux or outside the GUI login
/// session, where the keychain-backed policy is unreadable.
#[test]
fn e2e_spawn_without_policy_fails_closed() {
    use crate::server::handle_with_services;
    use std::io::{Read, Write};

    let (state, _state_path) = test_server();
    let id = unique_id("nopolicy");
    let body = format!(r#"{{"id":"{id}","bin":"sh","args":["-c","true"]}}"#);

    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let request = format!(
        "POST /v1/spawn HTTP/1.1\r\nAuthorization: Bearer {}\r\nContent-Length: {}\r\n\r\n{body}",
        "x".repeat(32),
        body.len()
    );
    let client = std::thread::spawn(move || {
        let mut stream = std::net::TcpStream::connect(address).unwrap();
        stream.write_all(request.as_bytes()).unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).unwrap();
        response
    });
    let (server_stream, _) = listener.accept().unwrap();
    handle_with_services(
        server_stream,
        Arc::clone(&state),
        || Err::<exec::Policy, String>("keychain unavailable".to_owned()),
        review_loop::gate_report,
    )
    .unwrap();
    let raw = client.join().unwrap();
    assert!(raw.starts_with("HTTP/1.1 500"), "expected 500, got: {raw}");
    assert!(
        raw.contains("could_not_read_execution_policy"),
        "unexpected body: {raw}"
    );

    // Sanity: with a working policy the same shape succeeds.
    let policy = test_policy();
    let proc = spawn_for_test(&state, &policy, &unique_id("ok"), "true");
    assert!(!proc.is_empty());
}

/// Proc kill flow: spawn a sleeper, observe it running, kill it via the
/// control route, watch it complete.
#[test]
fn e2e_proc_kill_lifecycle() {
    let (state, _state_path) = test_server();
    let policy = test_policy();
    let id = unique_id("kill");

    let handle = spawn_for_test(&state, &policy, &id, "sleep 60");
    let running = response_json(request_once(
        Arc::clone(&state),
        &policy,
        "GET",
        &format!("/v1/proc/{handle}"),
        "",
    ));
    assert_eq!(
        running.object("running"),
        Some(&Json::Bool(true)),
        "proc not running"
    );

    // The test server sets the control secret to the same value as the main
    // secret, so the default test token authorizes the kill route.
    let killed = response_json(request_once(
        Arc::clone(&state),
        &policy,
        "POST",
        &format!("/v1/proc/{handle}/kill"),
        "",
    ));
    assert_eq!(
        killed.object("killed"),
        Some(&Json::Bool(true)),
        "kill not acknowledged"
    );

    let completed = poll_until_complete(&state, &policy, &handle);
    assert_eq!(
        completed.object("running"),
        Some(&Json::Bool(false)),
        "killed proc never reaped"
    );

    // Unknown handles 404 on both poll and kill.
    let bogus = "0123456789abcdef0123456789abcdef";
    let raw = request_once(
        Arc::clone(&state),
        &policy,
        "GET",
        &format!("/v1/proc/{bogus}"),
        "",
    );
    assert!(
        raw.starts_with("HTTP/1.1 404"),
        "unknown proc poll did not 404: {raw}"
    );
}

/// Worktree create -> delete round-trip against a real temp git repo, using
/// explicit roots so the test is hermetic on any machine.
#[test]
fn e2e_worktree_create_delete_roundtrip() {
    let base = worktree_test_base("e2e-wt");
    let roots = worktree_test_roots(&base);
    let repo = worktree_test_repo(&base);
    // Canonicalize: on macOS the temp dir lives under /var -> /private/var,
    // and the plan compares canonical paths.
    let repo_root = base.canonicalize().unwrap();
    let repo_str = repo.to_str().unwrap();

    let wt_path = roots[0].join("e2e-wt1");
    let wt_str = wt_path.to_string_lossy().into_owned();
    let created = worktree_create_plan(&wt_str, "feature-e2e", repo_str, &roots, &repo_root)
        .expect("worktree create failed");
    assert_eq!(
        created.object("branch").and_then(Json::as_str),
        Some("feature-e2e")
    );
    assert!(wt_path.exists(), "worktree dir not created");

    // The branch is now checked out in the new worktree: creating another
    // worktree for it must be refused.
    let dup = worktree_create_plan(
        &roots[0].join("e2e-wt2").to_string_lossy(),
        "feature-e2e",
        repo_str,
        &roots,
        &repo_root,
    );
    assert!(dup.is_err(), "duplicate branch checkout was allowed");

    // Delete removes the worktree.
    let (state, _state_path) = test_server();
    let agents = state.supervisor.registry.list(None, None);
    let commands: Vec<(&str, &str)> = agents
        .iter()
        .map(|agent| (agent.state.as_str(), agent.command.as_str()))
        .collect();
    worktree_delete_plan(&wt_str, &roots, &commands).expect("worktree delete failed");
    assert!(!wt_path.exists(), "worktree dir not removed");

    // Deleting it again fails: the path is gone.
    let again = worktree_delete_plan(&wt_str, &roots, &commands);
    assert!(again.is_err(), "double delete was allowed");

    let _ = std::fs::remove_dir_all(&base);
}

/// The update policy parses, and `paused` is accepted: the binary-level test
/// boots the daemon with `ZIGZAG_UPDATE_POLICY=paused` so CI never hits the
/// network from the update checker.
#[test]
fn e2e_update_policy_parses() {
    use crate::update::Policy;
    assert!(matches!(Policy::parse("enabled"), Ok(Policy::Enabled)));
    assert!(matches!(Policy::parse("paused"), Ok(Policy::Paused)));
    assert!(matches!(
        Policy::parse("pin:1.2.3"),
        Ok(Policy::Pin(version)) if version == "1.2.3"
    ));
    assert!(Policy::parse("bogus").is_err());
}
