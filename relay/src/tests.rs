#[cfg(test)]
pub(crate) fn test_updater() -> Arc<update::Manager> {
    Arc::new(update::Manager::new(update::Config {
        directory: std::env::temp_dir().join("zigzag-test-updates"),
        interval: Duration::ZERO,
        policy: update::Policy::Enabled,
        ready_file: None,
    }))
}

/// A minimal exec policy for e2e tests: only `/bin/sh -c "<cmd>"` is allowed.
pub(crate) fn test_policy() -> exec::Policy {
    exec::Policy::parse(r#"{"bins":{"sh":{"path":"/bin/sh","commands":[["-c"]]}}}"#)
        .expect("test policy must parse")
}

use crate::auth::authorized;
use crate::config::{allowlist_file, is_get_allowlist, server_config, valid_github_repo};
use crate::events::{relay_event_at, replay_recovered_lifecycle};
use crate::exec;
use crate::github::{
    parse_github_open_pull_requests, review_gate_parameters, should_start_legacy_watch,
};
use crate::http::{
    MAX_BODY, MAX_HEADER_BLOCK_BYTES, MAX_HEADER_COUNT, ReadRequestError, denial_json,
    denial_response, error, get_query, percent_decode, query, read_request, reply,
};
use crate::proc::{
    AgentSpawnDetails, COMPAT_OUTPUT_CAP, FINISHED_PROC_RETENTION, MAX_FINISHED_PROCS, ProcEntry,
    process_group_running, process_identity, prune_procs, recovered_agent_identity_matches,
    spawn_proc, unique_handle,
};
use crate::provider::DEFAULT_CODEX_MODEL;
use crate::review_loop;
use crate::routes::agents::{
    AgentRoute, AgentWorktreeFailure, agent_create_worktree, agent_route, default_agent_worktree,
    parse_agent_create_request, persisted_agent_config, restart_argv, restart_config,
    valid_agent_model,
};
use crate::routes::events::{phase_events, same_clock_duration, timeline_output};
use crate::routes::exec::{parse_exec_request, parse_spawn_request};
use crate::routes::worktrees::{
    resolve_existing_worktree_path, resolve_new_worktree_path, resolve_worktree_repo,
    valid_worktree_branch, worktree_create_plan, worktree_delete_plan,
};
use crate::server::{
    ConnectionLimiter, ConnectionPermit, MAX_CONNECTIONS, Server, Supervisor, handle_with_services,
};
use crate::session::{
    CappedOutput, SESSION_HAS_GRAPHIC_ACCESS, SESSION_IS_REMOTE, is_local_gui_session,
    is_tailscale_ipv4,
};
use crate::update;
use relay_core::{AgentRecord, AgentRegistry, Json, Store, parse_json};
use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

#[test]
fn malformed_get_query_is_a_bad_request() {
    assert_eq!(
        get_query("/v1/events?after=not-a-number").unwrap_err(),
        "after must be a non-negative integer"
    );
    assert_eq!(
        get_query("/v1/events?epoch=%ZZ").unwrap_err(),
        "invalid URL encoding"
    );
}

#[test]
fn percent_decode_handles_form_encoding_edge_cases() {
    for (input, expected) in [
        ("", ""),
        ("abc", "abc"),
        ("hello+world", "hello world"),
        ("+", " "),
        ("%2B", "+"),
        ("%2b", "+"),
        ("%41%42%63", "ABc"),
        ("%E2%82%AC", "\u{20ac}"),
        ("a%20b%09c", "a b\tc"),
        ("%25", "%"),
        ("100%25", "100%"),
    ] {
        assert_eq!(percent_decode(input).unwrap(), expected, "{input:?}");
    }
    // Malformed escapes and invalid UTF-8 stay hard errors: query values
    // reach authenticated lookups, so bad input must never decode to
    // something surprising.
    for input in [
        "%", "%2", "a%", "%zz", "%2G", "G%2", "%%41", "%FF", "a%FFb", "%E2%82", "%C3%28",
    ] {
        assert_eq!(
            percent_decode(input).unwrap_err(),
            "invalid URL encoding",
            "{input:?}"
        );
    }
}

#[test]
fn query_decoding_matches_url_crate_form_decoding() {
    // Cross-check our strict decoder against the `url` crate's form
    // decoder on well-formed inputs; ours additionally rejects malformed
    // escapes instead of passing them through.
    for raw in [
        "a=1&b=2",
        "a=hello+world",
        "a=%2B%25%3D%26",
        "a=%E2%82%AC",
        "empty=&flag",
        "k=%20+%09",
        "a=1&b=%41%42",
    ] {
        let expected: HashMap<String, String> = url::form_urlencoded::parse(raw.as_bytes())
            .map(|(key, value)| (key.into_owned(), value.into_owned()))
            .collect();
        assert_eq!(
            query(&format!("/v1/events?{raw}")).unwrap(),
            expected,
            "{raw:?}"
        );
    }
    assert_eq!(
        query("/v1/events?a=%ZZ").unwrap_err(),
        "invalid URL encoding"
    );
    assert_eq!(
        query("/v1/events?a=1&a=2").unwrap_err(),
        "duplicate query parameter"
    );
}

#[test]
fn review_gate_query_requires_one_repository_and_positive_pr() {
    assert_eq!(
        review_gate_parameters("/v1/review-gate?repository=ShukantPal%2Fzigzag&pull_request=22"),
        Ok(("ShukantPal/zigzag".to_owned(), 22))
    );
    for target in [
        "/v1/review-gate",
        "/v1/review-gate?repository=ShukantPal%2Fzigzag&pull_request=0",
        "/v1/review-gate?repository=&pull_request=22",
        "/v1/review-gate?repository=ShukantPal%2Fzigzag&pull_request=22&extra=1",
        "/v1/review-gate?repository=one%2Frepo&repository=two%2Frepo&pull_request=22",
        "/v1/review-gate?repository=one%2Frepo&pull_request=22&pull_request=23",
        "/v1/review-gate?repository=%ZZ&pull_request=22",
    ] {
        assert_eq!(review_gate_parameters(target), Err(()));
    }
}

#[test]
fn authenticated_review_gate_route_forwards_mode_and_state_path() {
    let (state, state_path) = test_server();
    let policy =
        exec::Policy::parse(r#"{"bins":{"sh":{"path":"/bin/sh","commands":[["-c"]]}}}"#).unwrap();
    let head = "a".repeat(40);
    let base = "b".repeat(40);
    let key = format!("owner/repo#22@{head}");
    let durable_state = serde_json::json!({
        "schema_version": 1,
        "rounds": {
            (key): {
                "repository": "owner/repo",
                "pull_request": 22,
                "head": head,
                "base": base,
                "generation": 1,
                "verification": false,
                "phase": "reviewing",
                "reviewers": {},
                "verdicts": {},
                "excluded_comment_ids": [],
                "pending_comment_deletions": [],
                "owner": null,
                "owner_agent_id": null,
                "gate_reasons": [],
            }
        }
    });
    std::fs::write(
        &state.review_state_file,
        serde_json::to_vec(&durable_state).unwrap(),
    )
    .unwrap();
    let expected_state_path = state.review_state_file.clone();

    let response = request_once_with_gate(
        Arc::clone(&state),
        &policy,
        "GET",
        "/v1/review-gate?repository=owner%2Frepo&pull_request=22",
        "",
        |repository, pull_request, review_state_path, shadow, _config| {
            assert_eq!(repository, "owner/repo");
            assert_eq!(pull_request, 22);
            assert_eq!(review_state_path, expected_state_path);
            assert!(!shadow);
            let persisted: serde_json::Value =
                serde_json::from_slice(&std::fs::read(review_state_path).unwrap()).unwrap();
            assert!(persisted["rounds"].is_object());
            Ok(serde_json::json!({
                "pass": true,
                "head": "a".repeat(40),
            }))
        },
    );

    assert!(response.starts_with("HTTP/1.1 200 OK"));
    let report = response_json(response);
    assert_eq!(report.object("pass"), Some(&Json::Bool(true)));
    assert_eq!(report.object("head"), Some(&Json::String("a".repeat(40))));

    let unauthorized = request_once_with_gate_token(
        Arc::clone(&state),
        &policy,
        "GET",
        "/v1/review-gate?repository=owner%2Frepo&pull_request=22",
        "",
        "wrong-token",
        |_, _, _, _, _| panic!("unauthorized gate request reached the backend"),
    );
    assert!(unauthorized.starts_with("HTTP/1.1 401 Unauthorized"));

    drop(state);
    let _ = std::fs::remove_file(state_path);
    let _ = std::fs::remove_file(expected_state_path);
}

#[test]
fn bearer_comparison_requires_the_full_token() {
    assert!(authorized(
        &format!("Bearer {}", "x".repeat(32)),
        &"x".repeat(32)
    ));
    assert!(!authorized("Bearer x", &"x".repeat(32)));
    assert!(!authorized(
        &format!("Bearer {}suffix", "x".repeat(32)),
        &"x".repeat(32)
    ));
}

#[test]
fn explicit_bind_address_must_be_tailscale_ipv4() {
    assert!(is_tailscale_ipv4("100.101.237.83".parse().unwrap()));
    assert!(!is_tailscale_ipv4("0.0.0.0".parse().unwrap()));
    assert!(!is_tailscale_ipv4("127.0.0.1".parse().unwrap()));
}

#[test]
fn get_allowlist_matches_only_its_exact_arguments() {
    assert!(is_get_allowlist(&["get-allowlist".to_owned()]));
    for invalid in [
        Vec::new(),
        vec!["get-allowlist".to_owned(), "--file".to_owned()],
        vec!["set-allowlist".to_owned()],
    ] {
        assert!(!is_get_allowlist(&invalid));
    }
}

#[test]
fn allowlist_updater_accepts_only_its_exact_arguments() {
    let valid = vec![
        "set-allowlist".to_owned(),
        "--file".to_owned(),
        "/secure/policy.json".to_owned(),
    ];
    assert_eq!(allowlist_file(&valid).unwrap(), "/secure/policy.json");
    for invalid in [
        vec![],
        vec!["set-allowlist".to_owned()],
        vec!["set-allowlist".to_owned(), "--file".to_owned()],
        vec![
            "set-allowlist".to_owned(),
            "--other".to_owned(),
            "/secure/policy.json".to_owned(),
        ],
        vec![
            "set-allowlist".to_owned(),
            "--file".to_owned(),
            "".to_owned(),
        ],
    ] {
        assert!(allowlist_file(&invalid).is_err());
    }
}

#[test]
fn gui_session_check_rejects_remote_or_non_graphical_sessions() {
    assert!(is_local_gui_session(0, SESSION_HAS_GRAPHIC_ACCESS));
    assert!(!is_local_gui_session(
        0,
        SESSION_HAS_GRAPHIC_ACCESS | SESSION_IS_REMOTE
    ));
    assert!(!is_local_gui_session(0, 0));
    assert!(!is_local_gui_session(-1, SESSION_HAS_GRAPHIC_ACCESS));
}

#[test]
fn exec_denials_are_opaque_and_never_include_policy_data() {
    let expected = r#"{"id":"request-1","error":"denied"}"#;
    let invalid_json =
        match parse_exec_request(br#"{"id":"request-1","bin":"jules","args":["new"]"#) {
            Err(denial) => denial,
            Ok(_) => panic!("malformed request was accepted"),
        };
    assert_eq!(invalid_json.to_json(), r#"{"id":"","error":"denied"}"#);
    let malformed_schema =
        match parse_exec_request(br#"{"id":"request-1","bin":"jules","args":"new"}"#) {
            Err(denial) => denial,
            Ok(_) => panic!("malformed request was accepted"),
        };
    assert_eq!(malformed_schema.to_json(), expected);

    let policy = exec::Policy::parse(
        r#"{"bins":{"jules":{"path":"/private/configured-binary","commands":[["new"]]}}}"#,
    )
    .unwrap();
    for body in [
        br#"{"id":"request-1","bin":"unknown","args":["new"]}"#.as_slice(),
        br#"{"id":"request-1","bin":"jules","args":["login"]}"#.as_slice(),
    ] {
        let request = parse_exec_request(body).unwrap();
        let denial = match policy.verified_path(&request.bin, &request.args) {
            Err(exec::VerifyError::Denied) => denial_json(&request.id),
            other => panic!("expected an opaque denial, got {other:?}"),
        };
        assert_eq!(denial.to_json(), expected);
        assert!(!denial.to_json().contains("configured-binary"));
        let (status, response) = denial_response(&request.id);
        assert_eq!(status, 200);
        assert_eq!(response.to_json(), expected);
    }
}

#[test]
fn oversized_exec_request_is_an_opaque_denial_before_body_allocation() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let client = thread::spawn(move || {
        let mut stream = TcpStream::connect(address).unwrap();
        write!(
            stream,
            "POST /v1/exec HTTP/1.1\r\nContent-Length: {}\r\n\r\n",
            MAX_BODY + 1
        )
        .unwrap();
    });
    let (mut server, _) = listener.accept().unwrap();
    client.join().unwrap();
    assert!(matches!(
        read_request(&mut server),
        Err(ReadRequestError::ExecutionDenied)
    ));
}

#[test]
fn truncated_exec_request_is_an_opaque_denial() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let client = thread::spawn(move || {
        let mut stream = TcpStream::connect(address).unwrap();
        write!(
            stream,
            "POST /v1/exec HTTP/1.1\r\nContent-Length: 1\r\n\r\n"
        )
        .unwrap();
    });
    let (mut server, _) = listener.accept().unwrap();
    client.join().unwrap();
    assert!(matches!(
        read_request(&mut server),
        Err(ReadRequestError::ExecutionDenied)
    ));
}

#[test]
fn connection_limiter_admits_up_to_the_cap_and_releases_on_drop() {
    let limiter = Arc::new(ConnectionLimiter::new(2));
    let first = limiter.try_acquire();
    assert!(first.is_some());
    let second = limiter.try_acquire();
    assert!(second.is_some());
    assert!(limiter.try_acquire().is_none());
    drop(first);
    assert!(limiter.try_acquire().is_some());
}

#[test]
fn connection_limiter_never_exceeds_the_cap_concurrently() {
    let limiter = Arc::new(ConnectionLimiter::new(MAX_CONNECTIONS));
    let (done, results) = std::sync::mpsc::channel();
    // Every thread races for the same bounded pool. Admitted permits
    // travel back through the channel, so they stay held until the main
    // thread has collected them all.
    for _ in 0..MAX_CONNECTIONS * 2 {
        let limiter = Arc::clone(&limiter);
        let done = done.clone();
        thread::spawn(move || {
            done.send(limiter.try_acquire()).unwrap();
        });
    }
    drop(done);
    let permits: Vec<Option<ConnectionPermit>> = results.iter().collect();
    assert_eq!(
        permits.iter().filter(|permit| permit.is_some()).count(),
        MAX_CONNECTIONS
    );
    drop(permits);
    assert_eq!(*limiter.active.lock().unwrap(), 0);
}

#[test]
fn more_than_one_hundred_headers_are_rejected() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let client = thread::spawn(move || {
        let mut stream = TcpStream::connect(address).unwrap();
        write!(stream, "GET /v1/health HTTP/1.1\r\n").unwrap();
        for index in 0..=MAX_HEADER_COUNT {
            write!(stream, "X-Flood-{index}: value\r\n").unwrap();
        }
        write!(stream, "\r\n").unwrap();
    });
    let (mut server, _) = listener.accept().unwrap();
    client.join().unwrap();
    assert!(matches!(
        read_request(&mut server),
        Err(ReadRequestError::HeadersTooLarge)
    ));
}

#[test]
fn exactly_one_hundred_headers_still_parse() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let client = thread::spawn(move || {
        let mut stream = TcpStream::connect(address).unwrap();
        write!(stream, "GET /v1/health HTTP/1.1\r\n").unwrap();
        for index in 0..MAX_HEADER_COUNT {
            write!(stream, "X-Ok-{index}: value\r\n").unwrap();
        }
        write!(stream, "\r\n").unwrap();
    });
    let (mut server, _) = listener.accept().unwrap();
    client.join().unwrap();
    let request = read_request(&mut server).unwrap();
    assert_eq!(request.headers.len(), MAX_HEADER_COUNT);
}

#[test]
fn header_block_over_eight_kib_is_rejected() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let client = thread::spawn(move || {
        let mut stream = TcpStream::connect(address).unwrap();
        write!(stream, "GET /v1/health HTTP/1.1\r\nX-Big: ").unwrap();
        stream
            .write_all(&vec![b'a'; MAX_HEADER_BLOCK_BYTES])
            .unwrap();
        write!(stream, "\r\n\r\n").unwrap();
    });
    let (mut server, _) = listener.accept().unwrap();
    client.join().unwrap();
    assert!(matches!(
        read_request(&mut server),
        Err(ReadRequestError::HeadersTooLarge)
    ));
}

#[test]
fn unterminated_header_line_cannot_outgrow_the_budget() {
    // The client never sends a line terminator; the server must stop at
    // the budget instead of buffering the line without bound.
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let client = thread::spawn(move || {
        let mut stream = TcpStream::connect(address).unwrap();
        write!(stream, "GET /v1/health HTTP/1.1\r\nX-Big: ").unwrap();
        stream
            .write_all(&vec![b'a'; 2 * MAX_HEADER_BLOCK_BYTES])
            .unwrap();
    });
    let (mut server, _) = listener.accept().unwrap();
    let started = Instant::now();
    let result = read_request(&mut server);
    assert!(matches!(result, Err(ReadRequestError::HeadersTooLarge)));
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "server waited for a terminator instead of enforcing the budget"
    );
    client.join().unwrap();
}

#[test]
fn reply_reports_431_as_request_header_fields_too_large() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let client = thread::spawn(move || {
        let mut stream = TcpStream::connect(address).unwrap();
        let mut response = Vec::new();
        stream.read_to_end(&mut response).unwrap();
        let response = String::from_utf8(response).unwrap();
        assert!(
            response.starts_with("HTTP/1.1 431 Request Header Fields Too Large\r\n"),
            "unexpected status line: {response}"
        );
    });
    let (mut server, _) = listener.accept().unwrap();
    reply(&mut server, 431, error("request header fields too large")).unwrap();
    // Close so the client's read_to_end sees EOF.
    drop(server);
    client.join().unwrap();
}

#[test]
fn github_watch_repo_validation_rejects_unscoped_or_malformed_names() {
    assert!(valid_github_repo("leveled-inc/leveled"));
    assert!(valid_github_repo("owner.name/repo_name-2"));
    assert!(!valid_github_repo("leveled"));
    assert!(!valid_github_repo("owner/repo/extra"));
    assert!(!valid_github_repo("owner/repo space"));
}

#[test]
fn review_state_and_startup_mode_keep_shadow_observational_until_cutover() {
    let base_arguments = || {
        vec![
            "--secret-file".to_owned(),
            "/token".to_owned(),
            "--state-file".to_owned(),
            "/state".to_owned(),
        ]
    };
    let config = server_config(base_arguments()).unwrap();
    assert_eq!(
        config.review_state_file,
        PathBuf::from("/state").with_extension("reviews.json")
    );

    let mut arguments = base_arguments();
    arguments.extend([
        "--watch-repo".to_owned(),
        "leveled-inc/leveled".to_owned(),
        "--watch-interval".to_owned(),
        "60".to_owned(),
    ]);
    let config = server_config(arguments).unwrap();
    assert_eq!(config.github_watch_repos, ["leveled-inc/leveled"]);
    assert_eq!(config.github_watch_interval, Duration::from_secs(60));
    let shadow_authoritative = review_loop::authoritative_mode(true);
    assert!(!shadow_authoritative);
    assert!(should_start_legacy_watch(
        shadow_authoritative,
        &config.github_watch_repos
    ));
    let cutover_authoritative = review_loop::authoritative_mode(false);
    assert!(cutover_authoritative);
    assert!(!should_start_legacy_watch(
        cutover_authoritative,
        &config.github_watch_repos
    ));
}

#[test]
fn github_pr_scan_requires_positive_numbers() {
    assert_eq!(
        parse_github_open_pull_requests(
            r#"[[{"number":42,"html_url":"https://github.com/leveled-inc/leveled/pull/42"}]]"#,
        )
        .unwrap(),
        vec![42]
    );
    assert!(parse_github_open_pull_requests(r#"{"number":42}"#).is_err());
    assert!(
        parse_github_open_pull_requests(r#"[{"number":0,"url":"https://example.test"}]"#).is_err()
    );
}

#[test]
fn process_handles_are_unique_128_bit_hex_values() {
    let mut entries = HashMap::new();
    entries.insert("0".repeat(32), completed_entry());
    let mut handles = std::collections::HashSet::new();
    for _ in 0..256 {
        let handle = unique_handle(&entries).unwrap();
        assert_eq!(handle.len(), 32);
        assert!(handle.bytes().all(|byte| byte.is_ascii_hexdigit()));
        assert!(!entries.contains_key(&handle));
        assert!(handles.insert(handle));
    }
}

#[test]
fn spawn_is_rejected_while_update_drain_is_active() {
    let (state, state_path) = test_server();
    state.updater.begin_drain().unwrap();
    let policy =
        exec::Policy::parse(r#"{"bins":{"sh":{"path":"/bin/sh","commands":[["-c"]]}}}"#).unwrap();
    let response = request_once(
        Arc::clone(&state),
        &policy,
        "POST",
        "/v1/spawn",
        r#"{"id":"during-drain","bin":"sh","args":["-c","true"]}"#,
    );
    assert!(response.starts_with("HTTP/1.1 503 Service Unavailable"));
    assert!(response.ends_with(r#"{"error":"updates_draining"}"#));
    drop(state);
    let _ = std::fs::remove_file(state_path);
}

#[test]
fn captured_output_stops_at_the_exec_output_cap() {
    let mut output = CappedOutput::default();
    output.append(&vec![b'x'; COMPAT_OUTPUT_CAP]);
    output.append(b"extra");
    let (captured, truncated) = output.snapshot();
    assert_eq!(captured.len(), COMPAT_OUTPUT_CAP);
    assert!(truncated);
}

#[test]
fn timeline_durations_never_cross_clock_boundaries() {
    let left = relay_event_at(
        "process_spawned",
        "task",
        "execution",
        "2026-01-02T03:04:05.006Z".to_owned(),
        Json::Object(vec![]),
    );
    let right = relay_event_at(
        "first_output",
        "task",
        "execution",
        "2026-01-02T03:04:07.009Z".to_owned(),
        Json::Object(vec![]),
    );
    assert_eq!(same_clock_duration(&left, &right), Some(2_003));
    let mut cross_clock = right.clone();
    if let Json::Object(fields) = &mut cross_clock {
        fields.retain(|(name, _)| name != "clock");
        fields.push(("clock".to_owned(), Json::String("vm:boot".to_owned())));
    }
    assert_eq!(same_clock_duration(&left, &cross_clock), None);
    let unrelated_completion = relay_event_at(
        "process_completed",
        "task",
        "other-execution",
        "2026-01-02T03:04:08.000Z".to_owned(),
        Json::Object(vec![]),
    );
    assert!(
        phase_events(
            &[left.clone(), unrelated_completion],
            "process_spawned",
            "process_completed"
        )
        .is_none()
    );
    let rendered = timeline_output("task", &[left, cross_clock]);
    assert!(rendered.contains("Timeline for task"));
    assert!(rendered.contains("cross-clock/unknown"));
    assert_eq!(
        timeline_output("missing", &[]),
        "No durable audit events for task missing.\n"
    );
}

#[test]
fn recovery_replays_a_persisted_first_output_fact() {
    let path = std::env::temp_dir().join(format!(
        "zigzag-recovery-audit-{}",
        unique_handle(&HashMap::new()).unwrap()
    ));
    let store = Store::open(&path, 10).unwrap();
    let agent = AgentRecord {
        id: "agent".to_owned(),
        task_id: "task".to_owned(),
        execution_id: "execution".to_owned(),
        leader_pid: 1,
        process_group: 1,
        process_identity: Some("test:1".to_owned()),
        worktree_path: None,
        started_at: "1".to_owned(),
        deadline_at: None,
        command: "sh -c".to_owned(),
        state: "orphaned".to_owned(),
        paused_at: None,
        exit_code: None,
        log_degraded: false,
        audit_degraded: true,
        redacted: false,
        stdout_next: 3,
        stderr_next: 0,
        stdout_dropped_before: 0,
        stderr_dropped_before: 0,
        log_next: 3,
        log_dropped_before: 0,
        first_output_at: Some("2026-01-02T03:04:05.006Z".to_owned()),
        first_output_stream: Some("stdout".to_owned()),
        first_output_bytes: Some(3),
        restarted_from: None,
        agent_config: None,
    };
    replay_recovered_lifecycle(&store, &agent).unwrap();
    let events = store.timeline("task").unwrap();
    let kinds: Vec<_> = events
        .iter()
        .filter_map(|event| event.object("kind").and_then(Json::as_str))
        .collect();
    assert_eq!(kinds, ["process_spawned", "first_output"]);
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_dir_all(path.with_extension("audit"));
}

#[test]
fn spawn_request_accepts_only_safe_execution_correlation_ids() {
    let request = parse_spawn_request(
        br#"{"id":"task","execution_id":"attempt-01_A","bin":"sh","args":["-c","true"]}"#,
    )
    .unwrap();
    assert_eq!(request.execution_id.as_deref(), Some("attempt-01_A"));
    for value in ["", "contains space", "slash/value", &"x".repeat(129)] {
        let body =
            format!(r#"{{"id":"task","execution_id":"{value}","bin":"sh","args":["-c","true"]}}"#);
        assert!(parse_spawn_request(body.as_bytes()).is_err(), "{value:?}");
    }
    // The synchronous command parser deliberately retains its exact old
    // request shape; spawn-only metadata never reaches argv execution.
    assert!(
        parse_exec_request(
            br#"{"id":"task","execution_id":"attempt","bin":"sh","args":["-c","true"]}"#
        )
        .is_err()
    );
}

#[test]
fn prune_drops_finished_entries_past_the_retention_window() {
    let registry_path = std::env::temp_dir().join(format!(
        "zigzag-test-agents-{}",
        unique_handle(&HashMap::new()).unwrap()
    ));
    let supervisor = Supervisor {
        registry: Arc::new(AgentRegistry::open(&registry_path).unwrap()),
        procs: Mutex::new(HashMap::new()),
    };
    let request = exec::ExecRequest {
        id: "old".to_owned(),
        bin: "sh".to_owned(),
        args: vec!["-c".to_owned(), "exit 0".to_owned()],
    };
    let event_path = registry_path.with_extension("events");
    let handle = spawn_proc(
        &supervisor,
        Arc::new(Store::open(&event_path, 10).unwrap()),
        Path::new("/bin/sh"),
        request,
        "execution-old".to_owned(),
        AgentSpawnDetails::default(),
    )
    .unwrap()
    .handle;
    let mut entries = supervisor.procs.lock().unwrap();
    let entry = entries.get_mut(&handle).unwrap();
    let _ = entry.child.wait();
    entry.finished_at = Some(Instant::now() - FINISHED_PROC_RETENTION - Duration::from_secs(1));
    prune_procs(&mut entries, Instant::now());
    assert!(entries.is_empty());
    let _ = std::fs::remove_file(registry_path);
    let _ = std::fs::remove_file(event_path);
}

#[test]
fn prune_keeps_at_most_128_finished_processes() {
    let mut entries = HashMap::new();
    for index in 0..=MAX_FINISHED_PROCS {
        entries.insert(format!("{index:032x}"), completed_entry());
    }
    prune_procs(&mut entries, Instant::now());
    assert_eq!(entries.len(), MAX_FINISHED_PROCS);
}

#[test]
fn detached_process_endpoints_authenticate_and_manage_process_trees() {
    let policy =
        exec::Policy::parse(r#"{"bins":{"sh":{"path":"/bin/sh","commands":[["-c"]]}}}"#).unwrap();
    let (state, state_path) = test_server();
    let denied = request_once(
        Arc::clone(&state),
        &policy,
        "POST",
        "/v1/spawn",
        r#"{"id":"bad","bin":"sh","args":["-c","true"],"extra":true}"#,
    );
    assert!(denied.starts_with("HTTP/1.1 200 OK"));
    assert!(denied.ends_with(r#"{"id":"bad","error":"denied"}"#));
    let policy_denied = request_once(
        Arc::clone(&state),
        &policy,
        "POST",
        "/v1/spawn",
        r#"{"id":"policy-denied","bin":"sh","args":["not-allowed"]}"#,
    );
    assert!(policy_denied.ends_with(r#"{"id":"policy-denied","error":"denied"}"#));
    let denied_events = state.store.timeline("policy-denied").unwrap();
    assert_eq!(denied_events.len(), 1);
    assert_eq!(
        denied_events[0].object("kind").and_then(Json::as_str),
        Some("relay_request_started")
    );

    let handle = spawn_for_test(&state, &policy, "echo", "printf hello");
    let completed = poll_until_complete(&state, &policy, &handle);
    assert_eq!(
        completed.object("exit_code"),
        Some(&Json::Number("0".to_owned()))
    );
    assert_eq!(
        completed.object("stdout"),
        Some(&Json::String("hello".to_owned()))
    );
    // New diagnostics are read-only and use the same authenticated relay
    // path; the legacy proc response above remains unchanged.
    let agent = response_json(request_once(
        Arc::clone(&state),
        &policy,
        "GET",
        &format!("/v1/agents/{handle}"),
        "",
    ));
    assert_eq!(agent.object("id"), Some(&Json::String(handle.clone())));
    assert_eq!(
        agent.object("state"),
        Some(&Json::String("succeeded".to_owned()))
    );
    let logs = response_json(request_once(
        Arc::clone(&state),
        &policy,
        "GET",
        &format!("/v1/agents/{handle}/logs?stream=stdout&after=0"),
        "",
    ));
    assert!(logs.to_json().contains("hello"));
    let audit = state.store.timeline("echo").unwrap();
    let kinds: Vec<_> = audit
        .iter()
        .filter_map(|event| event.object("kind").and_then(Json::as_str))
        .collect();
    for required in [
        "relay_request_started",
        "relay_accepted",
        "process_spawned",
        "first_output",
        "process_completed",
    ] {
        assert!(kinds.contains(&required), "missing audit event {required}");
    }
    let execution_ids: std::collections::HashSet<_> = audit
        .iter()
        .filter_map(|event| event.object("execution_id").and_then(Json::as_str))
        .collect();
    assert_eq!(execution_ids.len(), 1);
    assert!(execution_ids.iter().next().is_some_and(|id| !id.is_empty()));
    let missing_bin_policy = exec::Policy::parse(
        r#"{"bins":{"missing":{"path":"/definitely/not/installed","commands":[["-c"]]}}}"#,
    )
    .unwrap();
    let failed_spawn = request_once(
        Arc::clone(&state),
        &missing_bin_policy,
        "POST",
        "/v1/spawn",
        r#"{"id":"spawn-failure","bin":"missing","args":["-c","true"]}"#,
    );
    assert!(failed_spawn.starts_with("HTTP/1.1 500 Internal Server Error"));
    assert!(
        state
            .store
            .timeline("spawn-failure")
            .unwrap()
            .iter()
            .any(|event| { event.object("kind").and_then(Json::as_str) == Some("process_failed") })
    );
    let explicit = response_json(request_once(
        Arc::clone(&state),
        &policy,
        "POST",
        "/v1/spawn",
        r#"{"id":"explicit","execution_id":"vm-attempt-7","bin":"sh","args":["-c","printf err >&2; exit 7"]}"#,
    ));
    let explicit_handle = explicit.object("proc").and_then(Json::as_str).unwrap();
    let failed = poll_until_complete(&state, &policy, explicit_handle);
    assert_eq!(
        failed.object("exit_code"),
        Some(&Json::Number("7".to_owned()))
    );
    let explicit_events = state.store.timeline("explicit").unwrap();
    assert!(explicit_events.iter().any(|event| {
        event.object("execution_id").and_then(Json::as_str) == Some("vm-attempt-7")
            && event.object("kind").and_then(Json::as_str) == Some("process_failed")
    }));
    for event in &explicit_events {
        for field in [
            "schema_version",
            "id",
            "task_id",
            "execution_id",
            "kind",
            "source",
            "occurred_at",
            "clock",
            "payload",
        ] {
            assert!(event.object(field).is_some(), "missing {field}");
        }
    }
    assert_eq!(
        explicit_events
            .iter()
            .filter(|event| event.object("kind").and_then(Json::as_str) == Some("first_output"))
            .count(),
        1
    );

    // The shell and its background child remain in the daemon-created
    // process group on both Linux and macOS. The endpoint cannot report
    // completion until the group signal terminates both processes and
    // closes the inherited output pipes.
    let handle = spawn_for_test(&state, &policy, "sleep", "sleep 60 & wait");
    let running = response_json(request_once(
        Arc::clone(&state),
        &policy,
        "GET",
        &format!("/v1/proc/{handle}"),
        "",
    ));
    assert_eq!(running.object("running"), Some(&Json::Bool(true)));
    let recovered = state.supervisor.registry.get(&handle).unwrap();
    assert!(recovered.process_identity.is_some());
    assert!(recovered_agent_identity_matches(&recovered));
    let mut reused_pid = recovered.clone();
    reused_pid.process_identity = Some("different-process-birth".to_owned());
    assert!(!recovered_agent_identity_matches(&reused_pid));
    let killed = response_json(request_once(
        Arc::clone(&state),
        &policy,
        "POST",
        &format!("/v1/proc/{handle}/kill"),
        "",
    ));
    assert_eq!(killed.object("id"), Some(&Json::String("sleep".to_owned())));
    assert_eq!(killed.object("killed"), Some(&Json::Bool(true)));
    let completed = poll_until_complete(&state, &policy, &handle);
    assert_eq!(completed.object("running"), Some(&Json::Bool(false)));

    for (method, target) in [
        ("GET", "/v1/proc/0123456789abcdef0123456789abcdef"),
        ("POST", "/v1/proc/0123456789abcdef0123456789abcdef/kill"),
    ] {
        let response = request_once(Arc::clone(&state), &policy, method, target, "");
        assert!(response.starts_with("HTTP/1.1 404 Not Found"));
        assert!(response.ends_with(r#"{"error":"unknown_proc"}"#));
    }
    drop(state);
    let _ = std::fs::remove_file(state_path);
}

pub(crate) fn test_server() -> (Arc<Server>, PathBuf) {
    let path = std::env::temp_dir().join(format!(
        "zigzag-proc-test-{}",
        unique_handle(&HashMap::new()).unwrap()
    ));
    (
        Arc::new(Server {
            secret: "x".repeat(32),
            control_secret: Some("x".repeat(32)),
            store: Arc::new(Store::open(&path, 1).unwrap()),
            supervisor: Supervisor {
                registry: Arc::new(AgentRegistry::open(path.with_extension("agents")).unwrap()),
                procs: Mutex::new(HashMap::new()),
            },
            updater: Arc::new(update::Manager::new(update::Config {
                directory: path.with_extension("updates"),
                interval: Duration::ZERO,
                policy: update::Policy::Enabled,
                ready_file: None,
            })),
            review_state_file: path.with_extension("review-state"),
            review_loop_shadow: false,
            review_config: Mutex::new(Some(Arc::new(test_review_config()))),
        }),
        path,
    )
}

fn test_review_config() -> review_loop::ReviewLoopConfig {
    review_loop::ReviewLoopConfig {
        enabled: true,
        intervals: review_loop::Intervals {
            discovery_seconds: 1,
            review_seconds: 1,
            merge_seconds: 1,
        },
        repositories: Vec::new(),
    }
}

fn completed_entry() -> ProcEntry {
    let mut child = Command::new("/bin/sh")
        .args(["-c", "exit 0"])
        .spawn()
        .unwrap();
    let process_group = child.id() as i32;
    let _ = child.wait();
    ProcEntry {
        child,
        process_group,
        id: "finished".to_owned(),
        bin: "sh".to_owned(),
        subcommand: "-c".to_owned(),
        spawned_at: Instant::now(),
        finished_at: Some(Instant::now()),
        termination_requested_at: None,
        termination_escalated: false,
        leader_reaped: true,
        exit_code: Some(0),
        stdout: Arc::new(Mutex::new(CappedOutput {
            complete: true,
            ..CappedOutput::default()
        })),
        stderr: Arc::new(Mutex::new(CappedOutput {
            complete: true,
            ..CappedOutput::default()
        })),
    }
}

pub(crate) fn request_once(
    state: Arc<Server>,
    policy: &exec::Policy,
    method: &str,
    target: &str,
    body: &str,
) -> String {
    request_once_with_gate(
        state,
        policy,
        method,
        target,
        body,
        review_loop::gate_report,
    )
}

fn request_once_with_gate<G>(
    state: Arc<Server>,
    policy: &exec::Policy,
    method: &str,
    target: &str,
    body: &str,
    gate_report: G,
) -> String
where
    G: Fn(
        &str,
        u64,
        &std::path::Path,
        bool,
        &review_loop::ReviewLoopConfig,
    ) -> Result<serde_json::Value, String>,
{
    request_once_with_gate_token(
        state,
        policy,
        method,
        target,
        body,
        &"x".repeat(32),
        gate_report,
    )
}

pub(crate) fn request_once_with_gate_token<G>(
    state: Arc<Server>,
    policy: &exec::Policy,
    method: &str,
    target: &str,
    body: &str,
    token: &str,
    gate_report: G,
) -> String
where
    G: Fn(
        &str,
        u64,
        &std::path::Path,
        bool,
        &review_loop::ReviewLoopConfig,
    ) -> Result<serde_json::Value, String>,
{
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let request = format!(
        "{method} {target} HTTP/1.1\r\nAuthorization: Bearer {}\r\nContent-Length: {}\r\n\r\n{body}",
        token,
        body.len()
    );
    let client = thread::spawn(move || {
        let mut stream = TcpStream::connect(address).unwrap();
        stream.write_all(request.as_bytes()).unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).unwrap();
        response
    });
    let (server, _) = listener.accept().unwrap();
    handle_with_services(server, state, || Ok(policy.clone()), gate_report).unwrap();
    client.join().unwrap()
}

pub(crate) fn response_json(response: String) -> Json {
    parse_json(response.split_once("\r\n\r\n").unwrap().1).unwrap()
}

pub(crate) fn spawn_for_test(
    state: &Arc<Server>,
    policy: &exec::Policy,
    id: &str,
    command: &str,
) -> String {
    let response = response_json(request_once(
        Arc::clone(state),
        policy,
        "POST",
        "/v1/spawn",
        &format!(r#"{{"id":"{id}","bin":"sh","args":["-c","{command}"]}}"#),
    ));
    response
        .object("proc")
        .and_then(Json::as_str)
        .unwrap()
        .to_owned()
}

pub(crate) fn poll_until_complete(
    state: &Arc<Server>,
    policy: &exec::Policy,
    handle: &str,
) -> Json {
    let deadline = Instant::now() + Duration::from_secs(15);
    while Instant::now() < deadline {
        let response = response_json(request_once(
            Arc::clone(state),
            policy,
            "GET",
            &format!("/v1/proc/{handle}"),
            "",
        ));
        if response.object("running") == Some(&Json::Bool(false)) {
            return response;
        }
        thread::sleep(Duration::from_millis(20));
    }
    panic!("process did not finish in time");
}
pub(crate) fn worktree_test_base(prefix: &str) -> PathBuf {
    let base = std::env::temp_dir().join(format!(
        "zigzag-{prefix}-{}-{}",
        std::process::id(),
        unique_handle(&HashMap::new()).unwrap()
    ));
    let _ = std::fs::remove_dir_all(&base);
    base
}

pub(crate) fn worktree_test_roots(base: &Path) -> Vec<PathBuf> {
    let allowed = base.join("allowed");
    std::fs::create_dir_all(&allowed).unwrap();
    std::fs::create_dir_all(base.join("other")).unwrap();
    vec![allowed.canonicalize().unwrap()]
}

pub(crate) fn worktree_test_repo(base: &Path) -> PathBuf {
    let repo = base.join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    let git = |args: &[&str]| {
        let output = Command::new("git")
            .args(args)
            .current_dir(&repo)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    };
    git(&["init", "-b", "main"]);
    git(&["config", "user.email", "zigzag-test@example.com"]);
    git(&["config", "user.name", "zigzag-test"]);
    git(&["commit", "--allow-empty", "-m", "init"]);
    repo
}

#[test]
fn worktree_new_path_rejects_traversal_and_outside_roots() {
    let base = worktree_test_base("wt-roots");
    let roots = worktree_test_roots(&base);
    let joined = |parts: &[&str]| {
        let mut path = base.clone();
        for part in parts {
            path.push(part);
        }
        path.to_string_lossy().into_owned()
    };
    // A plain path under the root resolves to the canonical root + leaf.
    assert_eq!(
        resolve_new_worktree_path(&joined(&["allowed", "wt1"]), &roots).unwrap(),
        roots[0].join("wt1")
    );
    // `..` traversal that escapes the root is rejected.
    assert!(resolve_new_worktree_path(&joined(&["allowed", "..", "other", "wt"]), &roots).is_err());
    // A sibling directory outside the root is rejected.
    assert!(resolve_new_worktree_path(&joined(&["other", "wt"]), &roots).is_err());
    // Relative paths are rejected outright.
    assert!(resolve_new_worktree_path("allowed/wt", &roots).is_err());
    assert!(resolve_new_worktree_path("", &roots).is_err());
    // A symlink inside the root pointing outside cannot smuggle a path out:
    // the parent canonicalizes outside the root.
    std::os::unix::fs::symlink(base.join("other"), base.join("allowed").join("evil")).unwrap();
    assert!(resolve_new_worktree_path(&joined(&["allowed", "evil", "wt"]), &roots).is_err());
    let _ = std::fs::remove_dir_all(&base);
}

#[test]
fn worktree_existing_path_requires_existence_under_roots() {
    let base = worktree_test_base("wt-existing");
    let roots = worktree_test_roots(&base);
    let inside = base.join("allowed").join("wt");
    std::fs::create_dir_all(&inside).unwrap();
    assert_eq!(
        resolve_existing_worktree_path(inside.to_str().unwrap(), &roots).unwrap(),
        roots[0].join("wt")
    );
    // Missing path.
    assert!(
        resolve_existing_worktree_path(
            &base.join("allowed").join("nope").to_string_lossy(),
            &roots
        )
        .is_err()
    );
    // Existing but outside the roots.
    assert!(resolve_existing_worktree_path(&base.join("other").to_string_lossy(), &roots).is_err());
    let _ = std::fs::remove_dir_all(&base);
}

#[test]
fn worktree_branch_validation_rejects_hostile_names() {
    assert!(valid_worktree_branch("feature/my-branch"));
    assert!(valid_worktree_branch("codex/agent-endpoints-01-worktrees"));
    assert!(!valid_worktree_branch(""));
    assert!(!valid_worktree_branch("--help"));
    assert!(!valid_worktree_branch("-b"));
    assert!(!valid_worktree_branch("../escape"));
    assert!(!valid_worktree_branch("a b"));
    assert!(!valid_worktree_branch("a:b"));
    assert!(!valid_worktree_branch("branch.lock"));
    assert!(!valid_worktree_branch("branch@{1}"));
    assert!(!valid_worktree_branch("/leading"));
    assert!(!valid_worktree_branch("trailing/"));
}

#[test]
fn worktree_repo_must_live_under_workspace_root() {
    let base = worktree_test_base("wt-repo");
    let roots = worktree_test_roots(&base);
    let _ = roots;
    let repo_root = base.canonicalize().unwrap();
    let inside = base.join("allowed").join("repo");
    std::fs::create_dir_all(&inside).unwrap();
    assert!(resolve_worktree_repo(inside.to_str().unwrap(), &repo_root).is_ok());
    // Existing but outside the repo root.
    let outside_base = worktree_test_base("wt-repo-outside");
    std::fs::create_dir_all(&outside_base).unwrap();
    assert!(resolve_worktree_repo(outside_base.to_str().unwrap(), &repo_root).is_err());
    let _ = std::fs::remove_dir_all(&outside_base);
    // Missing entirely.
    assert!(
        resolve_worktree_repo(
            &base.join("allowed").join("nope").to_string_lossy(),
            &repo_root
        )
        .is_err()
    );
    let _ = std::fs::remove_dir_all(&base);
}

#[test]
fn worktree_create_and_delete_roundtrip() {
    let base = worktree_test_base("wt-e2e");
    let wt_root = base.join("worktrees");
    std::fs::create_dir_all(&wt_root).unwrap();
    let repo = worktree_test_repo(&base);
    let roots = vec![wt_root.canonicalize().unwrap()];
    let repo_root = base.canonicalize().unwrap();
    let plan = |path: &str, branch: &str| {
        worktree_create_plan(path, branch, repo.to_str().unwrap(), &roots, &repo_root)
    };

    // Create on a new branch.
    let wt = wt_root.join("feature-a");
    let wt_canonical = wt_root.canonicalize().unwrap().join("feature-a");
    let response = plan(wt.to_str().unwrap(), "feature-a").unwrap();
    assert_eq!(
        response.object("branch"),
        Some(&Json::String("feature-a".to_owned()))
    );
    assert_eq!(
        response.object("path").and_then(Json::as_str),
        wt_canonical.to_str()
    );
    assert!(wt.join(".git").exists());

    // The same branch cannot back a second worktree.
    let err = plan(wt_root.join("feature-a2").to_str().unwrap(), "feature-a").unwrap_err();
    assert_eq!(
        (err.code, err.message),
        (400, "worktree_branch_already_checked_out")
    );

    // A live agent attached to the worktree blocks deletion.
    let agent_command = format!("codex exec --cd {}", wt.display());
    let agents: Vec<(&str, &str)> = vec![("running", agent_command.as_str())];
    let err = worktree_delete_plan(wt.to_str().unwrap(), &roots, &agents).unwrap_err();
    assert_eq!((err.code, err.message), (409, "worktree_in_use"));
    assert!(wt.exists(), "refused delete must leave the worktree alone");

    // A finished agent does not block.
    let agents: Vec<(&str, &str)> = vec![("succeeded", agent_command.as_str())];
    let response = worktree_delete_plan(wt.to_str().unwrap(), &roots, &agents).unwrap();
    assert_eq!(response.object("removed"), Some(&Json::Bool(true)));
    assert!(!wt.exists());

    // Creating on an existing branch checks it out instead of creating it.
    let output = Command::new("git")
        .args(["branch", "existing"])
        .current_dir(&repo)
        .output()
        .unwrap();
    assert!(output.status.success());
    let wt2 = wt_root.join("feature-b");
    let response = plan(wt2.to_str().unwrap(), "existing").unwrap();
    assert_eq!(
        response.object("branch"),
        Some(&Json::String("existing".to_owned()))
    );
    assert!(wt2.join(".git").exists());

    // Deleting a path that is gone fails instead of claiming success.
    worktree_delete_plan(wt2.to_str().unwrap(), &roots, &[]).unwrap();
    assert!(worktree_delete_plan(wt2.to_str().unwrap(), &roots, &[]).is_err());
    let _ = std::fs::remove_dir_all(&base);
}

#[test]
fn worktree_endpoints_reject_outside_roots_over_http() {
    let (state, state_path) = test_server();
    let policy =
        exec::Policy::parse(r#"{"bins":{"sh":{"path":"/bin/sh","commands":[["-c"]]}}}"#).unwrap();
    // A path no canonical root can contain is rejected with 400, through
    // the full authenticated routing path.
    let response = request_once(
        Arc::clone(&state),
        &policy,
        "POST",
        "/v1/worktrees",
        r#"{"path":"/definitely-not-a-worktree-root/wt","branch":"b","repo":"/definitely-not-a-worktree-root/repo"}"#,
    );
    assert!(
        response.starts_with("HTTP/1.1 400 Bad Request"),
        "{response}"
    );
    assert!(
        response.ends_with(r#"{"error":"worktree_path_not_found"}"#)
            || response.ends_with(r#"{"error":"worktree_path_outside_allowed_roots"}"#),
        "{response}"
    );
    let response = request_once(
        Arc::clone(&state),
        &policy,
        "DELETE",
        "/v1/worktrees",
        r#"{"path":"/definitely-not-a-worktree-root/wt"}"#,
    );
    assert!(
        response.starts_with("HTTP/1.1 400 Bad Request"),
        "{response}"
    );
    // Malformed bodies are rejected before any git runs.
    let response = request_once(
        Arc::clone(&state),
        &policy,
        "POST",
        "/v1/worktrees",
        r#"{"path":"/x"}"#,
    );
    assert!(
        response.ends_with(r#"{"error":"invalid_worktree_request"}"#),
        "{response}"
    );
    // The bearer check still guards the new routes.
    let unauthorized = request_once_with_gate_token(
        Arc::clone(&state),
        &policy,
        "POST",
        "/v1/worktrees",
        r#"{"path":"/x","branch":"b","repo":"/y"}"#,
        "wrong-token",
        |_, _, _, _, _| panic!("unauthorized worktree request reached the backend"),
    );
    assert!(
        unauthorized.starts_with("HTTP/1.1 401 Unauthorized"),
        "{unauthorized}"
    );
    drop(state);
    let _ = std::fs::remove_file(state_path);
}

#[test]
fn agent_create_request_parsing_and_helpers() {
    let request = parse_agent_create_request(
        br#"{"prompt":"do it","project_dir":"/Users/shukant/Workspace/repo","branch":"codex/x","model":"gpt-6-luna","approval_mode":"full-auto","timeout_secs":3600}"#,
    )
    .unwrap();
    assert_eq!(request.prompt, "do it");
    assert_eq!(request.project_dir, "/Users/shukant/Workspace/repo");
    assert_eq!(request.branch, "codex/x");
    assert_eq!(request.model.as_deref(), Some(DEFAULT_CODEX_MODEL));
    assert_eq!(request.approval_mode.as_deref(), Some("full-auto"));
    assert_eq!(request.timeout_secs, Some(3600));
    assert!(request.worktree.is_none());
    assert!(
        parse_agent_create_request(br#"{"prompt":"x","project_dir":"y","branch":"z","nope":1}"#)
            .is_err()
    );
    assert!(parse_agent_create_request(br#"{"project_dir":"y","branch":"z"}"#).is_err());
    assert!(
        parse_agent_create_request(
            br#"{"prompt":"x","project_dir":"y","branch":"z","timeout_secs":"3600"}"#
        )
        .is_err()
    );
    assert_eq!(
        default_agent_worktree("codex/a/b"),
        "/private/tmp/codex-a-b/"
    );
    assert!(valid_agent_model("org/model:1.0"));
    assert!(!valid_agent_model("model name"));
}

#[test]
fn restart_config_replays_prompt_contents_timeout_and_argv() {
    let request = parse_agent_create_request(
        br#"{"prompt":"/temporary/prompt.md","project_dir":"/repo","branch":"codex/restart","model":"gpt-6","approval_mode":"auto-edit","timeout_secs":42}"#,
    )
    .unwrap();
    let persisted = persisted_agent_config(&request, "/tmp/worktree", "original prompt contents");
    let config = restart_config(&persisted).unwrap();
    assert_eq!(config.prompt, "original prompt contents");
    assert_eq!(config.timeout_secs, Some(42));

    let fresh = restart_argv(
        &config,
        super::routes::agents::RestartMode::Fresh,
        &config.prompt,
        None,
    )
    .unwrap();
    assert_eq!(
        fresh.last().map(String::as_str),
        Some("original prompt contents")
    );
    assert!(fresh.windows(2).any(|args| args == ["-m", "gpt-6"]));
    assert!(
        fresh
            .windows(2)
            .any(|args| args == ["--auto-edit", "--skip-git-repo-check"])
    );

    let resumed = restart_argv(
        &config,
        super::routes::agents::RestartMode::Resume,
        "continue from override",
        Some("thread-123"),
    )
    .unwrap();
    assert!(
        resumed
            .windows(2)
            .any(|args| args == ["resume", "thread-123"])
    );
    assert!(resumed.iter().any(|arg| arg == "continue from override"));
}

#[test]
fn restart_argv_uses_luna_when_the_original_request_omitted_a_model() {
    let request = parse_agent_create_request(
        br#"{"prompt":"continue","project_dir":"/repo","branch":"codex/restart"}"#,
    )
    .unwrap();
    let persisted = persisted_agent_config(&request, "/tmp/worktree", "continue");
    let config = restart_config(&persisted).unwrap();

    let argv = restart_argv(
        &config,
        super::routes::agents::RestartMode::Fresh,
        &config.prompt,
        None,
    )
    .unwrap();
    assert!(
        argv.windows(2)
            .any(|args| args == ["-m", DEFAULT_CODEX_MODEL])
    );
}

#[test]
fn agent_route_selects_create_only_for_post_collection() {
    assert!(matches!(
        agent_route("GET", "/v1/agents"),
        Some(AgentRoute::List)
    ));
    assert!(matches!(
        agent_route("POST", "/v1/agents"),
        Some(AgentRoute::Create)
    ));
    assert!(agent_route("DELETE", "/v1/agents").is_none());
}

#[test]
fn agent_create_route_passes_the_body_to_request_validation() {
    let (state, state_path) = test_server();
    let policy = test_policy();
    let response = request_once(Arc::clone(&state), &policy, "POST", "/v1/agents", r#"{}"#);
    assert!(
        response.starts_with("HTTP/1.1 400 Bad Request"),
        "{response}"
    );
    assert!(
        response.ends_with(r#"{"error":"invalid_agent_create_request"}"#),
        "{response}"
    );
    drop(state);
    let _ = std::fs::remove_file(state_path);
}

#[test]
fn agent_create_worktree_roundtrip() {
    let base = worktree_test_base("agent-wt");
    let root = base.join("worktrees");
    std::fs::create_dir_all(&root).unwrap();
    let repo = worktree_test_repo(&base);
    let worktree = root.join("codex-feature");
    agent_create_worktree(&repo, &worktree, "codex/feature").unwrap();
    assert!(worktree.join(".git").exists());
    let error = agent_create_worktree(&repo, &root.join("second"), "codex/feature").unwrap_err();
    assert!(matches!(
        error,
        AgentWorktreeFailure::Validation(failure)
            if failure.message == "worktree_branch_already_checked_out"
    ));
    let _ = std::fs::remove_dir_all(base);
}

fn agent_record(id: &str) -> AgentRecord {
    AgentRecord {
        id: id.to_owned(),
        task_id: "task-1".to_owned(),
        execution_id: "exec-1".to_owned(),
        leader_pid: 0,
        process_group: 0,
        process_identity: None,
        worktree_path: None,
        started_at: "2026-10-09T00:00:00Z".to_owned(),
        deadline_at: None,
        command: "sleep 300".to_owned(),
        state: "running".to_owned(),
        exit_code: None,
        log_degraded: false,
        audit_degraded: false,
        redacted: false,
        stdout_next: 0,
        stderr_next: 0,
        stdout_dropped_before: 0,
        stderr_dropped_before: 0,
        log_next: 0,
        log_dropped_before: 0,
        first_output_at: None,
        first_output_stream: None,
        first_output_bytes: None,
        paused_at: None,
        restarted_from: None,
        agent_config: None,
    }
}

#[test]
fn agent_delete_unknown_agent_is_404() {
    let (state, _state_path) = test_server();
    let policy =
        exec::Policy::parse(r#"{"bins":{"sh":{"path":"/bin/sh","commands":[["-c"]]}}}"#).unwrap();
    let response = request_once(
        Arc::clone(&state),
        &policy,
        "DELETE",
        "/v1/agents/no-such-agent",
        "",
    );
    assert!(response.starts_with("HTTP/1.1 404"), "{response}");
    let body = response_json(response);
    assert_eq!(
        body.object("error").and_then(Json::as_str),
        Some("unknown_agent")
    );
}

#[test]
fn agent_delete_terminal_agent_is_409() {
    let (state, _state_path) = test_server();
    let policy =
        exec::Policy::parse(r#"{"bins":{"sh":{"path":"/bin/sh","commands":[["-c"]]}}}"#).unwrap();
    let mut record = agent_record("done-agent");
    record.state = "succeeded".to_owned();
    state.supervisor.registry.register(record).unwrap();
    let response = request_once(
        Arc::clone(&state),
        &policy,
        "DELETE",
        "/v1/agents/done-agent",
        "",
    );
    assert!(response.starts_with("HTTP/1.1 409"), "{response}");
    let body = response_json(response);
    assert_eq!(
        body.object("error").and_then(Json::as_str),
        Some("agent_not_running")
    );
    // The terminal record is untouched.
    assert_eq!(
        state.supervisor.registry.get("done-agent").unwrap().state,
        "succeeded"
    );
}

#[test]
fn agent_delete_dead_process_deregisters_without_signalling() {
    let (state, _state_path) = test_server();
    let policy =
        exec::Policy::parse(r#"{"bins":{"sh":{"path":"/bin/sh","commands":[["-c"]]}}}"#).unwrap();
    // A stale record: the PID no longer exists, so the identity check
    // fails. The endpoint must deregister without signalling anything.
    let mut record = agent_record("stale-agent");
    record.leader_pid = 2_000_000_000;
    record.process_group = 2_000_000_000;
    state.supervisor.registry.register(record).unwrap();
    let response = request_once(
        Arc::clone(&state),
        &policy,
        "DELETE",
        "/v1/agents/stale-agent",
        "",
    );
    assert!(response.starts_with("HTTP/1.1 200"), "{response}");
    let body = response_json(response);
    assert_eq!(body.object("stopped").and_then(Json::as_bool), Some(true));
    assert_eq!(
        state.supervisor.registry.get("stale-agent").unwrap().state,
        "stopped"
    );
}

#[test]
fn agent_delete_terminates_process_group_and_keeps_worktree() {
    let (state, _state_path) = test_server();
    let policy =
        exec::Policy::parse(r#"{"bins":{"sh":{"path":"/bin/sh","commands":[["-c"]]}}}"#).unwrap();
    // A stand-in worktree: the endpoint must leave it in place.
    let worktree = std::env::temp_dir().join(format!(
        "zigzag-agent-delete-wt-{}",
        unique_handle(&HashMap::new()).unwrap()
    ));
    std::fs::create_dir_all(&worktree).unwrap();
    std::fs::write(worktree.join("sentinel.txt"), b"keep me").unwrap();

    // A real process in its own process group.
    let mut child = Command::new("/bin/sleep")
        .arg("300")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()
        .expect("spawn sleep");
    let pgid = child.id() as i32;
    let identity = process_identity(pgid).expect("process identity");
    // The test is the parent: reap the leader when it exits, otherwise the
    // zombie keeps kill(-pgid, 0) succeeding and the endpoint would
    // escalate to SIGKILL.
    thread::spawn(move || {
        let _ = child.wait();
    });

    let mut record = agent_record("delete-me");
    record.leader_pid = pgid;
    record.process_group = pgid;
    record.process_identity = Some(identity);
    record.worktree_path = Some(worktree.to_string_lossy().into_owned());
    state.supervisor.registry.register(record).unwrap();

    let response = request_once(
        Arc::clone(&state),
        &policy,
        "DELETE",
        "/v1/agents/delete-me",
        "",
    );
    assert!(response.starts_with("HTTP/1.1 200"), "{response}");
    let body = response_json(response);
    assert_eq!(body.object("id").and_then(Json::as_str), Some("delete-me"));
    assert_eq!(body.object("stopped").and_then(Json::as_bool), Some(true));
    assert_eq!(
        body.object("worktree").and_then(Json::as_str),
        Some(worktree.to_str().unwrap())
    );

    let deadline = Instant::now() + Duration::from_secs(5);
    while process_group_running(pgid) && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(50));
    }
    assert!(
        !process_group_running(pgid),
        "process group {pgid} survived DELETE"
    );

    assert_eq!(
        state.supervisor.registry.get("delete-me").unwrap().state,
        "stopped"
    );
    // The worktree is left in place.
    assert!(worktree.join("sentinel.txt").exists());
    let _ = std::fs::remove_dir_all(&worktree);

    // A second DELETE of the now-terminal agent is a 409.
    let response = request_once(
        Arc::clone(&state),
        &policy,
        "DELETE",
        "/v1/agents/delete-me",
        "",
    );
    assert!(response.starts_with("HTTP/1.1 409"), "{response}");
}

#[test]
fn agent_restart_endpoint_covers_success_and_guards() {
    let (state, state_path) = test_server();
    let policy = test_policy();
    let worktree = std::env::temp_dir().to_string_lossy().into_owned();
    let config = format!(
        r#"{{"prompt":"continue","project_dir":"/repo","branch":"codex/restart","worktree":"{worktree}","model":null,"approval_mode":null,"timeout_secs":60}}"#
    );
    let mut stopped = agent_record("restartable");
    stopped.state = "succeeded".to_owned();
    state.supervisor.registry.register(stopped).unwrap();
    state
        .supervisor
        .registry
        .set_agent_config("restartable", &config)
        .unwrap();

    let resumed = request_once(
        Arc::clone(&state),
        &policy,
        "POST",
        "/v1/agents/restartable/restart",
        r#"{"mode":"resume"}"#,
    );
    assert!(
        resumed.ends_with(r#"{"error":"no_resumable_session"}"#),
        "{resumed}"
    );
    state
        .supervisor
        .registry
        .append_log(
            "restartable",
            "stdout",
            b"{\"payload\":{\"thread\":{\"id\":\"thread-from-jsonl\"}}}\n",
        )
        .unwrap();
    let resumed = request_once(
        Arc::clone(&state),
        &policy,
        "POST",
        "/v1/agents/restartable/restart",
        r#"{"mode":"resume","prompt":"continue from override"}"#,
    );
    assert!(resumed.starts_with("HTTP/1.1 200"), "{resumed}");
    let resumed_id = response_json(resumed)
        .object("id")
        .and_then(Json::as_str)
        .unwrap()
        .to_owned();
    let resumed_agent = state.supervisor.registry.get(&resumed_id).unwrap();
    assert_eq!(resumed_agent.restarted_from.as_deref(), Some("restartable"));
    assert_eq!(resumed_agent.agent_config.as_deref(), Some(config.as_str()));
    assert!(resumed_agent.deadline_at.is_some());

    let response = request_once(
        Arc::clone(&state),
        &policy,
        "POST",
        "/v1/agents/restartable/restart",
        "",
    );
    assert!(response.starts_with("HTTP/1.1 200"), "{response}");
    let body = response_json(response);
    let restarted = body.object("id").and_then(Json::as_str).unwrap().to_owned();
    assert_eq!(
        body.object("restarted_from").and_then(Json::as_str),
        Some("restartable")
    );
    assert_eq!(
        state
            .supervisor
            .registry
            .get(&restarted)
            .unwrap()
            .restarted_from
            .as_deref(),
        Some("restartable")
    );

    let mut paused = agent_record("paused-restart");
    paused.state = "paused".to_owned();
    paused.agent_config = Some(config.clone());
    state.supervisor.registry.register(paused).unwrap();
    let paused_response = request_once(
        Arc::clone(&state),
        &policy,
        "POST",
        "/v1/agents/paused-restart/restart",
        "",
    );
    assert!(paused_response.ends_with(r#"{"error":"agent_not_stopped"}"#));

    // A running agent must never get a second process in the same worktree.
    let running = agent_record("running-restart");
    state.supervisor.registry.register(running).unwrap();
    let running_response = request_once(
        Arc::clone(&state),
        &policy,
        "POST",
        "/v1/agents/running-restart/restart",
        "",
    );
    assert!(running_response.ends_with(r#"{"error":"agent_not_stopped"}"#));

    // Recovery marks live processes orphaned.  They remain non-restartable
    // until the original process is no longer alive.
    let orphan = spawn_proc(
        &state.supervisor,
        Arc::clone(&state.store),
        Path::new("/bin/sh"),
        exec::ExecRequest {
            id: "orphan-task".to_owned(),
            bin: "sh".to_owned(),
            args: vec!["-c".to_owned(), "sleep 30".to_owned()],
        },
        "orphan-execution".to_owned(),
        AgentSpawnDetails::default(),
    )
    .unwrap();
    state
        .supervisor
        .registry
        .transition(&orphan.handle, "orphaned", None)
        .unwrap();
    let orphan_response = request_once(
        Arc::clone(&state),
        &policy,
        "POST",
        &format!("/v1/agents/{}/restart", orphan.handle),
        "",
    );
    assert!(orphan_response.ends_with(r#"{"error":"agent_not_stopped"}"#));
    let _ = request_once(
        Arc::clone(&state),
        &policy,
        "DELETE",
        &format!("/v1/agents/{}", orphan.handle),
        "",
    );

    let mut missing = agent_record("missing-worktree");
    missing.state = "succeeded".to_owned();
    missing.agent_config = Some(config.replace(&worktree, "/definitely/missing-worktree"));
    state.supervisor.registry.register(missing).unwrap();
    let missing_response = request_once(
        Arc::clone(&state),
        &policy,
        "POST",
        "/v1/agents/missing-worktree/restart",
        "",
    );
    assert!(missing_response.ends_with(r#"{"error":"worktree_missing"}"#));

    let mut malformed = agent_record("bad-config");
    malformed.state = "succeeded".to_owned();
    malformed.agent_config = Some("not json".to_owned());
    state.supervisor.registry.register(malformed).unwrap();
    let malformed_response = request_once(
        Arc::clone(&state),
        &policy,
        "POST",
        "/v1/agents/bad-config/restart",
        "",
    );
    assert!(malformed_response.ends_with(r#"{"error":"invalid_agent_config"}"#));

    let _ = request_once(
        Arc::clone(&state),
        &policy,
        "DELETE",
        &format!("/v1/agents/{restarted}"),
        "",
    );
    let _ = request_once(
        Arc::clone(&state),
        &policy,
        "DELETE",
        &format!("/v1/agents/{resumed_id}"),
        "",
    );
    drop(state);
    let _ = std::fs::remove_file(state_path);
}
