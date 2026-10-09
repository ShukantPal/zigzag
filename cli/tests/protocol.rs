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
            loop {
                let read = stream.read(&mut buffer).unwrap();
                if read == 0 {
                    break;
                }
                request.extend_from_slice(&buffer[..read]);
                if request.windows(4).any(|window| window == b"\r\n\r\n") {
                    break;
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
    assert!(requests[1].starts_with("GET /v1/agents/agent-1/transcript?tail=128 HTTP/1.1\r\n"));
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
