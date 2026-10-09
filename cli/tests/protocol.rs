use std::io::{Read, Write};
use std::net::TcpListener;
use std::process::Command;
use std::thread;

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
