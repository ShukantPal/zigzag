use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, utf8_percent_encode};
use relay_core::{Json, parse_json, read_secret_file};
use std::env;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::Duration;
use url::Url;

struct Config {
    zigzag_url: String,
    secret_file: PathBuf,
    state_file: PathBuf,
    proxy: Option<String>,
    timeout: u64,
    once: bool,
}
struct Cursor {
    epoch: String,
    after: u64,
}
struct ZigzagUrl {
    base: Url,
}

fn main() {
    // Timestamps on every line, like the relay daemon. stdout stays reserved
    // for the JSON event stream.
    let _ = env_logger::Builder::from_default_env()
        .filter_level(log::LevelFilter::Info)
        .format_timestamp_secs()
        .try_init();
    if let Err(error) = run() {
        log::error!("{error}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), String> {
    let config = config(env::args().skip(1).collect())?;
    let secret = read_secret_file(&config.secret_file)?;
    let zigzag = parse_url(&config.zigzag_url)?;
    let mut cursor = load_cursor(&config.state_file)?;
    loop {
        match poll(
            &zigzag,
            config.proxy.as_deref(),
            &secret,
            &cursor,
            config.timeout,
        ) {
            Ok(message) => {
                let epoch = required_string(&message, "epoch")?.to_owned();
                let reset = required_bool(&message, "reset")?;
                let lost = required_bool(&message, "lost")?;
                let next = required_number(&message, "next")?;
                if reset {
                    log::info!("Zigzag epoch changed; resetting cursor");
                }
                if lost {
                    log::warn!("Zigzag retention was exceeded; some events were lost");
                }
                let events = match message.object("events") {
                    Some(Json::Array(events)) => events,
                    _ => return Err("Zigzag response missing events array".to_owned()),
                };
                for event in events {
                    let Json::Object(mut fields) = event.clone() else {
                        return Err("Zigzag response contains a non-object event".to_owned());
                    };
                    fields.push(("zigzag_epoch".to_owned(), Json::String(epoch.clone())));
                    println!("{}", Json::Object(fields).to_json());
                }
                std::io::stdout()
                    .flush()
                    .map_err(|error| format!("could not flush output: {error}"))?;
                cursor = Cursor { epoch, after: next };
                save_cursor(&config.state_file, &cursor)?;
                if config.once {
                    return Ok(());
                }
            }
            Err(error) => {
                log::warn!("poll failed: {error}; retrying in 2s");
                if config.once {
                    return Err(error);
                }
                thread::sleep(Duration::from_secs(2));
            }
        }
    }
}

fn config(arguments: Vec<String>) -> Result<Config, String> {
    let mut zigzag_url = None;
    let mut secret_file = env::var_os("ZIGZAG_SECRET_FILE").map(PathBuf::from);
    let mut state_file = None;
    let mut proxy = env::var("ZIGZAG_PROXY")
        .ok()
        .filter(|value| !value.is_empty());
    let mut timeout = 50;
    let mut once = false;
    let mut values = arguments.into_iter();
    while let Some(argument) = values.next() {
        let value = |values: &mut std::vec::IntoIter<String>, name: &str| {
            values
                .next()
                .ok_or_else(|| format!("{name} requires a value"))
        };
        match argument.as_str() {
            "--zigzag-url" => zigzag_url = Some(value(&mut values, "--zigzag-url")?), "--secret-file" => secret_file = Some(PathBuf::from(value(&mut values, "--secret-file")?)),
            "--state-file" => state_file = Some(PathBuf::from(value(&mut values, "--state-file")?)), "--proxy" => proxy = Some(value(&mut values, "--proxy")?),
            "--timeout" => timeout = value(&mut values, "--timeout")?.parse().map_err(|_| "--timeout must be an integer".to_owned())?, "--once" => once = true,
            "--help" | "-h" => return Err("usage: poller --zigzag-url http://HOST:PORT --secret-file PATH --state-file PATH [--proxy URL] [--timeout 50] [--once]".to_owned()), _ => return Err(format!("unknown argument: {argument}")),
        }
    }
    if !(1..=55).contains(&timeout) {
        return Err("--timeout must be between 1 and 55".to_owned());
    }
    Ok(Config {
        zigzag_url: zigzag_url.ok_or_else(|| "--zigzag-url is required".to_owned())?,
        secret_file: secret_file
            .ok_or_else(|| "--secret-file or ZIGZAG_SECRET_FILE is required".to_owned())?,
        state_file: state_file.ok_or_else(|| "--state-file is required".to_owned())?,
        proxy,
        timeout,
        once,
    })
}

fn parse_url(input: &str) -> Result<ZigzagUrl, String> {
    let mut url = Url::parse(input).map_err(|_| "invalid Zigzag URL".to_owned())?;
    if url.scheme() != "http" {
        return Err(
            "Zigzag URL must use http:// (the tailnet is the transport boundary)".to_owned(),
        );
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err("invalid Zigzag URL authority".to_owned());
    }
    if url.host_str().is_none_or(str::is_empty) {
        return Err("invalid Zigzag URL host".to_owned());
    }
    // Normalize the base path to directory form so joining "v1/events" is stable.
    let path = url.path().trim_matches('/').to_owned();
    let normalized = if path.is_empty() {
        "/".to_owned()
    } else {
        format!("/{path}/")
    };
    url.set_path(&normalized);
    Ok(ZigzagUrl { base: url })
}

fn poll(
    zigzag: &ZigzagUrl,
    proxy: Option<&str>,
    secret: &str,
    cursor: &Cursor,
    timeout: u64,
) -> Result<Json, String> {
    let events = zigzag
        .base
        .join("v1/events")
        .map_err(|error| format!("invalid Zigzag URL: {error}"))?;
    let target = format!(
        "{events}?after={}&epoch={}&timeout={timeout}",
        cursor.after,
        encode(&cursor.epoch)
    );
    let mut builder = ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(timeout + 15))
        // The old raw-socket client never followed redirects; a 3xx is an error.
        .redirects(0);
    if let Some(proxy) = proxy {
        let proxy =
            ureq::Proxy::new(proxy).map_err(|error| format!("invalid proxy URL: {error}"))?;
        builder = builder.proxy(proxy);
    }
    let authorization = format!("Bearer {secret}");
    match builder
        .build()
        .get(&target)
        .set("Accept", "application/json")
        .set("Authorization", &authorization)
        .call()
    {
        Ok(response) => {
            let body = response
                .into_string()
                .map_err(|error| format!("could not read Zigzag response: {error}"))?;
            parse_json(&body)
        }
        Err(ureq::Error::Status(code, _)) => Err(format!("Zigzag returned HTTP {code}")),
        Err(error) => Err(format!("poll failed: {error}")),
    }
}

fn load_cursor(path: &Path) -> Result<Cursor, String> {
    match fs::read_to_string(path) {
        Ok(contents) => {
            let value =
                parse_json(&contents).map_err(|error| format!("invalid cursor file: {error}"))?;
            Ok(Cursor {
                epoch: required_string(&value, "epoch")?.to_owned(),
                after: required_number(&value, "next")?,
            })
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Cursor {
            epoch: String::new(),
            after: 0,
        }),
        Err(error) => Err(format!(
            "could not read cursor file {}: {error}",
            path.display()
        )),
    }
}

fn save_cursor(path: &Path, cursor: &Cursor) -> Result<(), String> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)
        .map_err(|error| format!("could not create cursor directory: {error}"))?;
    let temporary = parent.join(format!(
        ".poller-{}-{}.tmp",
        std::process::id(),
        cursor.after
    ));
    let result = (|| -> Result<(), String> {
        let mut output = private_file(&temporary)
            .map_err(|error| format!("could not create temporary cursor: {error}"))?;
        output
            .write_all(
                Json::Object(vec![
                    ("epoch".to_owned(), Json::String(cursor.epoch.clone())),
                    ("next".to_owned(), Json::number(cursor.after)),
                ])
                .to_json()
                .as_bytes(),
            )
            .map_err(|error| format!("could not write cursor: {error}"))?;
        output.write_all(b"\n").map_err(|error| error.to_string())?;
        output.sync_all().map_err(|error| error.to_string())?;
        fs::rename(&temporary, path)
            .map_err(|error| format!("could not replace cursor: {error}"))?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(temporary);
    }
    result
}

#[cfg(unix)]
fn private_file(path: &Path) -> std::io::Result<File> {
    use std::os::unix::fs::OpenOptionsExt;
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
}
#[cfg(not(unix))]
fn private_file(path: &Path) -> std::io::Result<File> {
    OpenOptions::new().write(true).create_new(true).open(path)
}

fn required_string<'a>(value: &'a Json, field: &str) -> Result<&'a str, String> {
    value
        .object(field)
        .and_then(Json::as_str)
        .ok_or_else(|| format!("Zigzag response missing string {field}"))
}
fn required_number(value: &Json, field: &str) -> Result<u64, String> {
    value
        .object(field)
        .and_then(Json::as_u64)
        .ok_or_else(|| format!("Zigzag response missing integer {field}"))
}
fn required_bool(value: &Json, field: &str) -> Result<bool, String> {
    value
        .object(field)
        .and_then(Json::as_bool)
        .ok_or_else(|| format!("Zigzag response missing boolean {field}"))
}

/// Percent-encode set matching exactly the RFC 3986 unreserved characters,
/// the only characters [`encode`] leaves unescaped.
const ENCODE_SET: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'_')
    .remove(b'.')
    .remove(b'~');

fn encode(input: &str) -> String {
    utf8_percent_encode(input, ENCODE_SET).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader};
    use std::net::TcpListener;
    use std::sync::{Arc, Mutex};
    use std::time::{SystemTime, UNIX_EPOCH};

    fn path(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "zigzag-poller-{name}-{}-{}.json",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    #[test]
    fn cursor_round_trip_is_private_and_restart_safe() {
        let file = path("cursor");
        let expected = Cursor {
            epoch: "epoch-1".to_owned(),
            after: 42,
        };
        save_cursor(&file, &expected).unwrap();
        let actual = load_cursor(&file).unwrap();
        assert_eq!(actual.epoch, expected.epoch);
        assert_eq!(actual.after, expected.after);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(fs::metadata(&file).unwrap().permissions().mode() & 0o077, 0);
        }
        let _ = fs::remove_file(file);
    }

    #[test]
    fn missing_cursor_starts_at_the_beginning() {
        let file = path("missing-cursor");
        let cursor = load_cursor(&file).unwrap();
        assert!(cursor.epoch.is_empty());
        assert_eq!(cursor.after, 0);
    }

    #[test]
    fn zigzag_url_and_timeout_validation_reject_unsafe_inputs() {
        assert!(parse_url("https://zigzag.example").is_err());
        assert!(parse_url("http://user@zigzag.example").is_err());
        assert!(
            config(vec![
                "--zigzag-url".to_owned(),
                "http://100.101.237.83:8765".to_owned(),
                "--secret-file".to_owned(),
                "/token".to_owned(),
                "--state-file".to_owned(),
                "/cursor".to_owned(),
                "--timeout".to_owned(),
                "56".to_owned(),
            ])
            .is_err()
        );
    }

    #[test]
    fn parse_url_accepts_http_and_normalizes_the_base_path() {
        let bare = parse_url("http://127.0.0.1:8765").unwrap();
        assert_eq!(bare.base.as_str(), "http://127.0.0.1:8765/");
        let nested = parse_url("http://relay.example/zigzag").unwrap();
        assert_eq!(nested.base.as_str(), "http://relay.example/zigzag/");
        assert_eq!(
            nested.base.join("v1/events").unwrap().as_str(),
            "http://relay.example/zigzag/v1/events"
        );
        let trailing = parse_url("http://relay.example/zigzag/").unwrap();
        assert_eq!(trailing.base.as_str(), "http://relay.example/zigzag/");
    }

    #[test]
    fn parse_url_rejects_non_http_authorities() {
        assert!(parse_url("https://relay.example").is_err());
        assert!(parse_url("http://user@relay.example").is_err());
        assert!(parse_url("http://user:pass@relay.example").is_err());
        assert!(parse_url("http://relay.example:notaport").is_err());
        assert!(parse_url("not a url").is_err());
    }

    #[test]
    fn encode_escapes_everything_outside_the_unreserved_set() {
        assert_eq!(encode("abcXYZ019-_.~"), "abcXYZ019-_.~");
        assert_eq!(encode(""), "");
        assert_eq!(encode("a b"), "a%20b");
        assert_eq!(encode("ep+och/1="), "ep%2Boch%2F1%3D");
        assert_eq!(encode("caf\u{e9}"), "caf%C3%A9");
    }

    /// Serve a single canned HTTP response on 127.0.0.1, capturing the raw
    /// request text. Returns the capture, the bound port, and the server thread.
    fn serve_once(status: u16, body: &str) -> (Arc<Mutex<String>>, u16, thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let seen = Arc::new(Mutex::new(String::new()));
        let captured = Arc::clone(&seen);
        let body = body.to_owned();
        let handle = thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(stream);
            let mut request = String::new();
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                request.push_str(&line);
                if line == "\r\n" || line == "\n" {
                    break;
                }
            }
            *captured.lock().unwrap() = request;
            let reason = if status == 200 { "OK" } else { "Error" };
            let response = format!(
                "HTTP/1.1 {status} {reason}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let mut stream = reader.into_inner();
            stream.write_all(response.as_bytes()).unwrap();
            stream.flush().unwrap();
        });
        (seen, port, handle)
    }

    fn test_zigzag(port: u16) -> ZigzagUrl {
        parse_url(&format!("http://127.0.0.1:{port}")).unwrap()
    }

    #[test]
    fn poll_sends_an_authorized_request_and_parses_the_response() {
        let body = r#"{"epoch":"epoch-9","reset":false,"lost":false,"next":7,"events":[]}"#;
        let (seen, port, handle) = serve_once(200, body);
        let cursor = Cursor {
            epoch: "ep+och/1".to_owned(),
            after: 3,
        };
        let message = poll(&test_zigzag(port), None, "test-secret", &cursor, 5).unwrap();
        handle.join().unwrap();
        assert_eq!(required_string(&message, "epoch").unwrap(), "epoch-9");
        assert_eq!(required_number(&message, "next").unwrap(), 7);
        let request = seen.lock().unwrap();
        assert!(
            request.starts_with("GET /v1/events?after=3&epoch=ep%2Boch%2F1&timeout=5 HTTP/1.1\r\n"),
            "unexpected request: {request}"
        );
        assert!(
            request.contains("\r\nAuthorization: Bearer test-secret\r\n"),
            "missing authorization header: {request}"
        );
        assert!(
            request.contains("\r\nAccept: application/json\r\n"),
            "missing accept header: {request}"
        );
    }

    #[test]
    fn poll_reports_non_200_statuses() {
        let (seen, port, handle) = serve_once(503, "try again later");
        let cursor = Cursor {
            epoch: String::new(),
            after: 0,
        };
        let error = poll(&test_zigzag(port), None, "test-secret", &cursor, 5).unwrap_err();
        handle.join().unwrap();
        assert_eq!(error, "Zigzag returned HTTP 503");
        assert!(
            seen.lock()
                .unwrap()
                .starts_with("GET /v1/events?after=0&epoch=&timeout=5 "),
            "unexpected request target"
        );
    }

    #[test]
    fn poll_sends_absolute_form_targets_through_a_proxy() {
        let body = r#"{"epoch":"e","reset":false,"lost":false,"next":1,"events":[]}"#;
        let (seen, proxy_port, handle) = serve_once(200, body);
        // With a proxy configured the client never resolves the origin host.
        let zigzag = parse_url("http://relay.internal:9999").unwrap();
        let cursor = Cursor {
            epoch: "e1".to_owned(),
            after: 0,
        };
        let proxy = format!("http://127.0.0.1:{proxy_port}");
        let message = poll(&zigzag, Some(&proxy), "proxy-secret", &cursor, 5).unwrap();
        handle.join().unwrap();
        assert_eq!(required_string(&message, "epoch").unwrap(), "e");
        let request = seen.lock().unwrap();
        assert!(
            request.starts_with(
                "GET http://relay.internal:9999/v1/events?after=0&epoch=e1&timeout=5 HTTP/1.1\r\n"
            ),
            "unexpected proxied request: {request}"
        );
    }
}
