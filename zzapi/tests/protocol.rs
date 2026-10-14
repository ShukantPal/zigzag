use std::io::{Read, Write};
use std::net::TcpListener;
use std::process::Command;
use std::thread;

fn transcript_server(
    responses: impl IntoIterator<Item = &'static str>,
) -> (u16, thread::JoinHandle<Vec<String>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let responses: Vec<_> = responses.into_iter().map(str::to_owned).collect();
    let server = thread::spawn(move || {
        let mut requests = Vec::new();
        for response in responses {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = Vec::new();
            let mut buffer = [0; 1024];
            let mut expected_len = None;
            loop {
                let read = stream.read(&mut buffer).unwrap();
                if read == 0 {
                    break;
                }
                request.extend_from_slice(&buffer[..read]);
                if let Some(header_end) =
                    request.windows(4).position(|window| window == b"\r\n\r\n")
                {
                    let header_end = header_end + 4;
                    if expected_len.is_none() {
                        let headers = std::str::from_utf8(&request[..header_end]).unwrap();
                        expected_len = headers.lines().find_map(|line| {
                            line.strip_prefix("Content-Length: ")?.parse::<usize>().ok()
                        });
                    }
                    if expected_len.is_some_and(|length| request.len() >= header_end + length)
                        || expected_len.is_none()
                    {
                        break;
                    }
                }
            }
            stream
                .write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response}",
                        response.len()
                    )
                    .as_bytes(),
                )
                .unwrap();
            requests.push(String::from_utf8(request).unwrap());
        }
        requests
    });
    (port, server)
}

#[test]
fn update_is_listed_in_help_and_needs_no_agent_flags() {
    let output = Command::new(env!("CARGO_BIN_EXE_zzapi"))
        .arg("--help")
        .output()
        .unwrap();
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(output.status.success());
    assert!(stdout.contains("update"));

    let output = Command::new(env!("CARGO_BIN_EXE_zzapi"))
        .args(["update", "--help"])
        .output()
        .unwrap();
    assert!(output.status.success());
    assert!(
        String::from_utf8(output.stdout)
            .unwrap()
            .contains("Download and install")
    );
}

#[test]
fn agents_create_reads_prompt_files_client_side_and_keeps_inline_text() {
    let (port, server) = transcript_server([
        r#"{"id":"agent-file","working_dir":"/tmp"}"#,
        r#"{"id":"agent-prompt-path","working_dir":"/tmp"}"#,
        r#"{"id":"agent-inline","working_dir":"/tmp"}"#,
    ]);
    let path = std::env::temp_dir().join(format!(
        "zzapi-prompt-protocol-{}-{}.md",
        std::process::id(),
        std::thread::current().name().unwrap_or("test")
    ));
    std::fs::write(&path, "read this prompt").unwrap();
    let path = path.to_string_lossy().into_owned();

    for prompt_args in [
        vec!["--prompt-file", path.as_str()],
        vec!["--prompt", path.as_str()],
        vec!["--prompt", "do the thing"],
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_zzapi"))
            .args([
                "--hostname",
                &format!("127.0.0.1:{port}"),
                "agents",
                "create",
            ])
            .args(prompt_args)
            .args(["--project-dir", "/tmp", "--no-branch"])
            .env("ZIGZAG_TOKEN", "test-token")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    let requests = server.join().unwrap();
    std::fs::remove_file(path).unwrap();
    let prompts: Vec<_> = requests
        .iter()
        .map(|request| {
            let body = request.split_once("\r\n\r\n").unwrap().1;
            serde_json::from_str::<serde_json::Value>(body).unwrap()["prompt"]
                .as_str()
                .unwrap()
                .to_owned()
        })
        .collect();
    assert_eq!(
        prompts,
        ["read this prompt", "read this prompt", "do the thing"]
    );
}

#[test]
fn agents_delete_calls_the_agent_delete_endpoint() {
    let (port, server) = transcript_server([
        r#"{"id":"agent-123","state":"orphaned"}"#,
        r#"{"id":"agent-123","state":"orphaned"}"#,
        r#"{"id":"agent-123","stopped":true,"deleted":true,"worktree_removed":true,"transcript_removed":false,"worktree":null}"#,
    ]);
    let output = Command::new(env!("CARGO_BIN_EXE_zzapi"))
        .args([
            "--hostname",
            &format!("127.0.0.1:{port}"),
            "agents",
            "delete",
            "agent-123",
        ])
        .env("ZIGZAG_TOKEN", "test-token")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&output.stdout)
            .contains("deleted agent-123 (worktree_removed=true, transcript_removed=false)"),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
    let requests = server.join().unwrap();
    assert_eq!(requests.len(), 3);
    assert!(requests[0].starts_with("GET /v1/agents/agent-123 "));
    assert!(requests[1].starts_with("GET /v1/agents/agent-123 "));
    assert!(requests[2].starts_with("DELETE /v1/agents/agent-123 "));
}

#[test]
fn agents_delete_refuses_a_running_agent_and_suggests_stop() {
    let (port, server) = transcript_server([
        r#"{"id":"agent-running","state":"running"}"#,
        r#"{"id":"agent-running","state":"running"}"#,
    ]);
    let output = Command::new(env!("CARGO_BIN_EXE_zzapi"))
        .args([
            "--hostname",
            &format!("127.0.0.1:{port}"),
            "agents",
            "delete",
            "agent-running",
        ])
        .env("ZIGZAG_TOKEN", "test-token")
        .output()
        .unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("refusing to delete running agent agent-running"));
    assert!(stderr.contains("zzapi agents stop agent-running"));
    let requests = server.join().unwrap();
    assert_eq!(requests.len(), 2);
    assert!(
        requests
            .iter()
            .all(|request| !request.starts_with("DELETE "))
    );
}

#[test]
fn agents_delete_dry_run_describes_effects_without_deleting() {
    let (port, server) = transcript_server([
        r#"{"id":"agent-dry","state":"orphaned"}"#,
        r#"{"id":"agent-dry","state":"orphaned"}"#,
    ]);
    let output = Command::new(env!("CARGO_BIN_EXE_zzapi"))
        .args([
            "--hostname",
            &format!("127.0.0.1:{port}"),
            "agents",
            "delete",
            "agent-dry",
            "--dry-run",
        ])
        .env("ZIGZAG_TOKEN", "test-token")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("retains its agent record"));
    assert!(stdout.contains("watched pull request"));
    assert!(stdout.contains("attempts to delete its branch"));
    assert!(stdout.contains("No DELETE request was sent"));
    let requests = server.join().unwrap();
    assert_eq!(requests.len(), 2);
    assert!(
        requests
            .iter()
            .all(|request| !request.starts_with("DELETE "))
    );
}

#[test]
fn agents_continue_posts_authenticated_prompt_and_returns_turn_result() {
    let (port, server) = transcript_server([
        r#"{"id":"agent-1"}"#,
        r#"{"agent_id":"agent-1","turn":{"status":"completed","id":"turn-2"}}"#,
    ]);
    let output = Command::new(env!("CARGO_BIN_EXE_zzapi"))
        .args([
            "--hostname",
            &format!("127.0.0.1:{port}"),
            "--json",
            "agents",
            "continue",
            "agent-1",
            "--prompt",
            "address review feedback",
        ])
        .env("ZIGZAG_TOKEN", "test-token")
        .output()
        .unwrap();
    let requests = server.join().unwrap();
    assert!(
        output.status.success(),
        "{}\nrequests={requests:?}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(requests[0].starts_with("GET /v1/agents/agent-1 HTTP/1.1\r\n"));
    assert!(requests[0].contains("Authorization: Bearer test-token\r\n"));
    assert!(requests[1].starts_with("POST /v1/agents/agent-1/continue HTTP/1.1\r\n"));
    assert!(requests[1].contains("Authorization: Bearer test-token\r\n"));
    assert!(requests[1].contains(r#"{"prompt":"address review feedback"}"#));
    assert!(String::from_utf8_lossy(&output.stdout).contains("\"turn\""));
}

#[test]
fn built_binary_continues_the_live_agent_selected_by_branch() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = thread::spawn(move || {
        let responses = [
            (404, r#"{"error":"unknown_agent"}"#),
            (200, r#"{"agents":[]}"#),
            (
                200,
                r#"{"agents":[{"id":"agent-1","branch":"feature/x","state":"running"}]}"#,
            ),
            (
                200,
                r#"{"agent_id":"agent-1","turn":{"status":"completed"}}"#,
            ),
        ];
        let mut requests = Vec::new();
        for (status, body) in responses {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = Vec::new();
            let mut buffer = [0; 2048];
            loop {
                let read = stream.read(&mut buffer).unwrap();
                if read == 0 {
                    break;
                }
                request.extend_from_slice(&buffer[..read]);
                if request.windows(4).any(|window| window == b"\r\n\r\n") {
                    let headers = String::from_utf8_lossy(&request);
                    let expected = headers
                        .lines()
                        .find_map(|line| {
                            line.strip_prefix("Content-Length: ")?.parse::<usize>().ok()
                        })
                        .unwrap_or(0);
                    let header_end = request
                        .windows(4)
                        .position(|window| window == b"\r\n\r\n")
                        .unwrap()
                        + 4;
                    if request.len() >= header_end + expected {
                        break;
                    }
                }
            }
            stream.write_all(format!("HTTP/1.1 {status} Test\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).unwrap();
            requests.push(String::from_utf8(request).unwrap());
        }
        requests
    });
    let output = Command::new(env!("CARGO_BIN_EXE_zzapi"))
        .args([
            "--hostname",
            &format!("127.0.0.1:{port}"),
            "agents",
            "continue",
            "--branch",
            "feature/x",
            "--prompt",
            "resume work",
        ])
        .env("ZIGZAG_TOKEN", "test-token")
        .output()
        .unwrap();
    let requests = server.join().unwrap();
    assert!(
        output.status.success(),
        "{}\nrequests={requests:?}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(requests[0].starts_with("GET /v1/agents/feature/x "));
    assert!(requests[1].starts_with("GET /v1/agents "));
    assert!(requests[2].starts_with("GET /v1/agents "));
    assert!(requests[3].starts_with("POST /v1/agents/agent-1/continue "));
    assert!(requests[3].contains("\"prompt\":\"resume work\""));
}

#[test]
fn built_binary_continues_the_live_agent_selected_by_pr() {
    let (port, server) = transcript_server([
        r#"{"repository":"owner/repo","agents":[{"id":"stale","state":"succeeded"},{"id":"agent-pr","state":"running"}]}"#,
        r#"{"agent_id":"agent-pr","turn":{"status":"completed"}}"#,
    ]);
    let output = Command::new(env!("CARGO_BIN_EXE_zzapi"))
        .args([
            "--hostname",
            &format!("127.0.0.1:{port}"),
            "agents",
            "continue",
            "--pr",
            "135",
            "--prompt",
            "continue review fixes",
        ])
        .env("ZIGZAG_TOKEN", "test-token")
        .output()
        .unwrap();
    let requests = server.join().unwrap();
    assert!(
        output.status.success(),
        "{}\nrequests={requests:?}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        requests[0].starts_with("GET /v1/agents?pr=135 "),
        "{requests:?}"
    );
    assert!(requests[1].starts_with("POST /v1/agents/agent-pr/continue "));
    assert!(requests[1].contains(r#"{"prompt":"continue review fixes"}"#));
}

#[test]
fn list_branch_filter_is_sent_to_the_relay() {
    let (port, server) = transcript_server([r#"{"agents":[]}"#]);
    let output = Command::new(env!("CARGO_BIN_EXE_zzapi"))
        .args([
            "--hostname",
            &format!("127.0.0.1:{port}"),
            "agents",
            "list",
            "--state",
            "running",
            "--branch",
            "feature/x",
        ])
        .env("ZIGZAG_TOKEN", "test-token")
        .output()
        .unwrap();
    assert!(output.status.success());
    let request = server.join().unwrap().remove(0);
    assert!(
        request.starts_with("GET /v1/agents?state=running&branch=feature%2Fx "),
        "{request}"
    );
}

#[test]
fn bulk_delete_uses_one_relay_request_with_selection_policy() {
    let (port, server) = transcript_server([
        r#"{"branch":"feature/x","dry_run":true,"deleted_count":2,"skipped_count":1,"deleted":["a","b"],"failures":[]}"#,
    ]);
    let output = Command::new(env!("CARGO_BIN_EXE_zzapi"))
        .args([
            "--hostname",
            &format!("127.0.0.1:{port}"),
            "agents",
            "delete",
            "--branch",
            "feature/x",
            "--keep-last",
            "1",
            "--older-than",
            "7d",
            "--dry-run",
        ])
        .env("ZIGZAG_TOKEN", "test-token")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let request = server.join().unwrap().remove(0);
    assert!(
        request.starts_with(
            "DELETE /v1/agents?branch=feature%2Fx&keep_last=1&older_than=7d&dry_run=1 "
        ),
        "{request}"
    );
}

#[test]
fn events_since_last_reads_and_advances_the_cli_cursor_file() {
    let (port, server) =
        transcript_server([r#"{"epoch":"epoch-1","next":9,"events":[],"lost":false}"#]);
    let cursor = std::env::temp_dir().join(format!("zzapi-cursor-{}", std::process::id()));
    std::fs::write(&cursor, "7").unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_zzapi"))
        .args([
            "--hostname",
            &format!("127.0.0.1:{port}"),
            "events",
            "--since-last",
        ])
        .env("ZIGZAG_TOKEN", "test-token")
        .env("ZIGZAG_EVENTS_CURSOR", &cursor)
        .output()
        .unwrap();
    let request = server.join().unwrap().remove(0);
    let saved = std::fs::read_to_string(&cursor).unwrap();
    std::fs::remove_file(cursor).unwrap();
    assert!(output.status.success());
    assert!(request.starts_with("GET /v1/events?after=7&"), "{request}");
    assert_eq!(saved, "9");
}

#[test]
fn pr_status_combines_agent_transcript_and_repository_scoped_github_status() {
    let (port, server) = transcript_server([
        r#"{"repository":"owner/repo","agents":[{"id":"agent-1","state":"running"}]}"#,
        r#"{"id":"agent-1","branch":"feature/x","state":"running","turn_in_flight":true,"current_turn_id":"turn-9","last_activity_at":"2026-10-14T12:00:00Z"}"#,
        r#"{"stdout":"{\"type\":\"item.completed\",\"item\":{\"type\":\"agent_message\",\"text\":\"answer\"}}\n","stderr":""}"#,
        r#"{"exit_code":0,"stdout":"{\"headRefOid\":\"abc123\",\"statusCheckRollup\":[],\"url\":\"https://github.com/owner/repo/pull/5\",\"state\":\"OPEN\",\"commits\":[{\"oid\":\"abc123\",\"messageHeadline\":\"last commit\"}]}"}"#,
    ]);
    let output = Command::new(env!("CARGO_BIN_EXE_zzapi"))
        .args([
            "--hostname",
            &format!("127.0.0.1:{port}"),
            "--json",
            "agents",
            "status",
            "--pr",
            "5",
        ])
        .env("ZIGZAG_TOKEN", "test-token")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let requests = server.join().unwrap();
    assert!(requests[0].starts_with("GET /v1/agents?pr=5 "));
    assert!(requests[1].starts_with("GET /v1/agents/agent-1 "));
    assert!(requests[2].starts_with("GET /v1/agents/agent-1/transcript?tail_items=200 "));
    assert!(requests[3].contains("\"pr\",\"view\",\"5\",\"--repo\",\"owner/repo\""));
    let result: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["agent"]["turn_in_flight"], true);
    assert_eq!(result["last_final_answer"], "answer");
    assert_eq!(result["pr"]["head_sha"], "abc123");
}

#[test]
fn admin_check_update_posts_authenticated_request_and_displays_update_result() {
    let (port, server) = transcript_server([
        r#"{"current_version":"1.2.3","latest_available_version":"1.2.4","update_applied":true}"#,
    ]);

    let output = Command::new(env!("CARGO_BIN_EXE_zzapi"))
        .args([
            "--hostname",
            &format!("127.0.0.1:{port}"),
            "admin",
            "check-update",
        ])
        .env("ZIGZAG_TOKEN", "test-token")
        .output()
        .unwrap();

    let requests = server.join().unwrap();
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(output.status.success());
    assert!(requests[0].starts_with("POST /v1/update/check HTTP/1.1\r\n"));
    assert!(requests[0].contains("Authorization: Bearer test-token\r\n"));
    assert!(requests[0].ends_with("{}"));
    assert!(stdout.contains("current version: 1.2.3"));
    assert!(stdout.contains("latest version:  1.2.4"));
    assert!(stdout.contains("update applied:  yes"));
}

#[test]
fn admin_check_update_displays_no_available_release_and_no_update_applied() {
    let (port, server) = transcript_server([
        r#"{"current_version":"1.2.3","latest_available_version":null,"update_applied":false}"#,
    ]);

    let output = Command::new(env!("CARGO_BIN_EXE_zzapi"))
        .args([
            "--hostname",
            &format!("127.0.0.1:{port}"),
            "admin",
            "check-update",
        ])
        .env("ZIGZAG_TOKEN", "test-token")
        .output()
        .unwrap();

    server.join().unwrap();
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(output.status.success());
    assert!(stdout.contains("current version: 1.2.3"));
    assert!(stdout.contains("latest version:  none"));
    assert!(stdout.contains("update applied:  no"));
}

#[test]
fn admin_check_update_is_listed_in_help() {
    let output = Command::new(env!("CARGO_BIN_EXE_zzapi"))
        .args(["admin", "--help"])
        .output()
        .unwrap();

    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(output.status.success());
    assert!(stdout.contains("check-update"));
    assert!(stdout.contains("Check for and apply an available relay update"));
}

fn read_socket_frame(stream: &mut std::net::TcpStream) -> serde_json::Value {
    let mut prefix = [0; 4];
    stream.read_exact(&mut prefix).unwrap();
    let mut body = vec![0; u32::from_be_bytes(prefix) as usize];
    stream.read_exact(&mut body).unwrap();
    serde_json::from_slice(&body).unwrap()
}

fn write_socket_frame(stream: &mut std::net::TcpStream, value: &serde_json::Value) {
    let body = serde_json::to_vec(value).unwrap();
    stream
        .write_all(&(body.len() as u32).to_be_bytes())
        .unwrap();
    stream.write_all(&body).unwrap();
}

#[test]
fn exec_null_exit_code_returns_one_after_posting_the_authenticated_protocol_request() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut request = Vec::new();
        let mut buffer = [0; 1024];
        let mut expected_len = None;
        loop {
            let read = stream.read(&mut buffer).unwrap();
            if read == 0 {
                break;
            }
            request.extend_from_slice(&buffer[..read]);
            if let Some(header_end) = request.windows(4).position(|window| window == b"\r\n\r\n") {
                let header_end = header_end + 4;
                if expected_len.is_none() {
                    let headers = std::str::from_utf8(&request[..header_end]).unwrap();
                    expected_len = headers.lines().find_map(|line| {
                        line.strip_prefix("Content-Length: ")?.parse::<usize>().ok()
                    });
                }
                if expected_len.is_some_and(|length| request.len() >= header_end + length) {
                    break;
                }
            }
        }
        stream
            .write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 95\r\nConnection: close\r\n\r\n{\"id\":\"request-1\",\"exit_code\":null,\"stdout\":\"\",\"stderr\":\"\",\"timed_out\":false,\"truncated\":false}"
            )
            .unwrap();
        String::from_utf8(request).unwrap()
    });

    let output = Command::new(env!("CARGO_BIN_EXE_zzapi"))
        .args([
            "--hostname",
            &format!("127.0.0.1:{port}"),
            "exec",
            "--bin",
            "gh",
            "--id",
            "request-1",
        ])
        .env("ZIGZAG_TOKEN", "test-token")
        .output()
        .unwrap();

    let request = server.join().unwrap();
    assert_eq!(output.status.code(), Some(1));
    assert!(request.starts_with("POST /v1/exec HTTP/1.1\r\n"));
    assert!(request.contains("Authorization: Bearer test-token\r\n"));
    assert!(request.contains("\"id\":\"request-1\""));
}

#[test]
fn transcript_uses_the_endpoint_tail_query_and_renders_sections() {
    let (port, server) = transcript_server([
        "{\"id\":\"agent-1\"}",
        "{\"id\":\"agent-1\",\"task_id\":\"task-1\",\"execution_id\":\"run-1\",\"command\":\"codex exec\",\"state\":\"exited\",\"started_at\":\"2026-10-09T10:00:00Z\",\"exit_code\":0,\"prompt\":\"Fix it\\n\",\"last_message\":\"Done\",\"stdout\":\"hello\\n\",\"stderr\":\"warning\\n\",\"next_cursor\":14,\"log_degraded\":false}",
    ]);

    let output = Command::new(env!("CARGO_BIN_EXE_zzapi"))
        .args([
            "--hostname",
            &format!("127.0.0.1:{port}"),
            "agents",
            "transcript",
            "agent-1",
            "--tail",
            "128",
        ])
        .env("ZIGZAG_TOKEN", "test-token")
        .output()
        .unwrap();

    let requests = server.join().unwrap();
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(output.status.success());
    assert!(requests[0].starts_with("GET /v1/agents/agent-1 HTTP/1.1\r\n"));
    assert!(
        requests[1].starts_with("GET /v1/agents/agent-1/transcript?tail_items=128 HTTP/1.1\r\n")
    );
    assert!(requests[1].contains("Authorization: Bearer test-token\r\n"));
    assert!(stdout.contains("--- prompt ---\nFix it\n"));
    assert!(stdout.contains("--- last message ---\nDone\n"));
    assert!(stdout.contains("--- stdout ---\nhello\n"));
    assert!(stdout.contains("--- stderr ---\nwarning\n"));
}

#[test]
fn transcript_follow_polls_without_requiring_server_follow_support() {
    let (port, server) = transcript_server([
        "{\"id\":\"agent-1\"}",
        "{\"id\":\"agent-1\",\"state\":\"running\",\"stdout\":\"first\\n\",\"stderr\":\"\",\"next_cursor\":6,\"log_degraded\":false}",
        "{\"id\":\"agent-1\",\"state\":\"exited\",\"stdout\":\"second\\n\",\"stderr\":\"\",\"next_cursor\":13,\"log_degraded\":true}",
    ]);

    let output = Command::new(env!("CARGO_BIN_EXE_zzapi"))
        .args([
            "--hostname",
            &format!("127.0.0.1:{port}"),
            "agents",
            "transcript",
            "agent-1",
            "--follow",
        ])
        .env("ZIGZAG_TOKEN", "test-token")
        .output()
        .unwrap();

    let requests = server.join().unwrap();
    let stdout = String::from_utf8(output.stdout).unwrap();
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(output.status.success());
    assert_eq!(requests.len(), 3);
    assert!(requests[1].starts_with("GET /v1/agents/agent-1/transcript HTTP/1.1\r\n"));
    assert!(requests[2].starts_with("GET /v1/agents/agent-1/transcript?after=6 HTTP/1.1\r\n"));
    assert!(stdout.contains("--- stdout ---\nfirst\n"));
    assert!(stdout.contains("--- stdout ---\nsecond\n"));
    assert!(
        stderr.contains("warning: agent transcript is incomplete because log capture degraded")
    );
}

#[test]
fn transcript_json_is_pretty_once_and_json_lines_when_following() {
    let (port, server) = transcript_server([
        "{\"id\":\"agent-1\"}",
        "{\"id\":\"agent-1\",\"state\":\"running\",\"stdout\":\"first\\n\",\"stderr\":\"\",\"next_cursor\":6,\"log_degraded\":false}",
        "{\"id\":\"agent-1\",\"state\":\"exited\",\"stdout\":\"second\\n\",\"stderr\":\"\",\"next_cursor\":13,\"log_degraded\":false}",
    ]);

    let output = Command::new(env!("CARGO_BIN_EXE_zzapi"))
        .args([
            "--hostname",
            &format!("127.0.0.1:{port}"),
            "--json",
            "agents",
            "transcript",
            "agent-1",
            "--follow",
        ])
        .env("ZIGZAG_TOKEN", "test-token")
        .output()
        .unwrap();

    let requests = server.join().unwrap();
    let stdout = String::from_utf8(output.stdout).unwrap();
    let lines: Vec<_> = stdout.lines().collect();
    assert!(output.status.success());
    assert_eq!(lines.len(), 2);
    assert!(
        lines
            .iter()
            .all(|line| serde_json::from_str::<serde_json::Value>(line).is_ok())
    );
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(lines[0]).unwrap()["stdout"],
        "first\n"
    );
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(lines[1]).unwrap()["stdout"],
        "second\n"
    );
    assert!(requests[2].starts_with("GET /v1/agents/agent-1/transcript?after=6 HTTP/1.1\r\n"));
}

#[test]
fn transcript_json_once_is_pretty_json() {
    let (port, server) = transcript_server([
        "{\"id\":\"agent-1\"}",
        "{\"id\":\"agent-1\",\"state\":\"exited\",\"stdout\":\"done\\n\",\"stderr\":\"\",\"next_cursor\":5,\"log_degraded\":false}",
    ]);

    let output = Command::new(env!("CARGO_BIN_EXE_zzapi"))
        .args([
            "--hostname",
            &format!("127.0.0.1:{port}"),
            "--json",
            "agents",
            "transcript",
            "agent-1",
        ])
        .env("ZIGZAG_TOKEN", "test-token")
        .output()
        .unwrap();

    let requests = server.join().unwrap();
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(output.status.success());
    assert_eq!(requests.len(), 2);
    assert!(requests[1].starts_with("GET /v1/agents/agent-1/transcript HTTP/1.1\r\n"));
    assert!(stdout.contains("\n  \"id\": \"agent-1\","));
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&stdout).unwrap()["stdout"],
        "done\n"
    );
}

#[test]
fn events_stream_authenticates_and_subscribes_to_the_bidi_socket() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        assert_eq!(
            read_socket_frame(&mut stream),
            serde_json::json!({"type":"auth","authorization":"Bearer test-token"})
        );
        write_socket_frame(&mut stream, &serde_json::json!({"type":"authenticated"}));
        assert_eq!(
            read_socket_frame(&mut stream),
            serde_json::json!({"type":"subscribe","topic":"events"})
        );
        write_socket_frame(
            &mut stream,
            &serde_json::json!({
                "topic":"events",
                "event":{"id":"pushed-event","kind":"test","task_id":"task-1"}
            }),
        );
    });

    let output = Command::new(env!("CARGO_BIN_EXE_zzapi"))
        .args([
            "--hostname",
            "127.0.0.1:8765",
            "--json",
            "events",
            "stream",
            "--socket-port",
            &port.to_string(),
        ])
        .env("ZIGZAG_TOKEN", "test-token")
        .output()
        .unwrap();

    server.join().unwrap();
    // The test server closes after one frame, while production streams remain
    // open. The client should print the frame before reporting the closure.
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&output.stdout).unwrap(),
        serde_json::json!({
            "topic":"events",
            "event":{"id":"pushed-event","kind":"test","task_id":"task-1"}
        })
    );
}
