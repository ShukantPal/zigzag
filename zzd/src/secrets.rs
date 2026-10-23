//! Daemon-only storage and use of narrowly scoped third-party credentials.
//!
//! BuildBuddy credentials live in the user's login Keychain. The relay only
//! exposes a curated invocation summary; upstream errors and response bodies
//! are intentionally not logged or returned to API callers.

use crate::http::{error, reply};
use keyring::Entry;
use serde_json::{Map, Value, json};
use std::net::TcpStream;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use zz::{Json, parse_json};

const SERVICE: &str = "zigzag";
const ACCOUNT: &str = "secret:buildbuddy:invocations:read:v1";
const PURPOSE: &str = "buildbuddy.invocations.read";
const MAX_TTL_DAYS: u64 = 90;

struct StoredSecret {
    token: String,
}

pub(crate) trait SecretStore {
    fn get(&mut self) -> Result<Option<String>, ()>;
    fn set(&mut self, value: &str) -> Result<(), ()>;
    fn delete(&mut self) -> Result<(), ()>;
}

pub(crate) struct KeychainSecretStore;

impl SecretStore for KeychainSecretStore {
    fn get(&mut self) -> Result<Option<String>, ()> {
        let entry = Entry::new(SERVICE, ACCOUNT).map_err(|_| ())?;
        match entry.get_password() {
            Ok(value) => Ok(Some(value)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(_) => Err(()),
        }
    }

    fn set(&mut self, value: &str) -> Result<(), ()> {
        Entry::new(SERVICE, ACCOUNT)
            .and_then(|entry| entry.set_password(value))
            .map_err(|_| ())
    }

    fn delete(&mut self) -> Result<(), ()> {
        let entry = Entry::new(SERVICE, ACCOUNT).map_err(|_| ())?;
        match entry.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(_) => Err(()),
        }
    }
}

pub(crate) fn buildbuddy_store(stream: &mut TcpStream, body: &[u8]) -> Result<(), String> {
    let mut store = KeychainSecretStore;
    buildbuddy_store_with(stream, body, &mut store, now_unix())
}

pub(crate) fn buildbuddy_store_with(
    stream: &mut TcpStream,
    body: &[u8],
    store: &mut impl SecretStore,
    now: u64,
) -> Result<(), String> {
    match store_buildbuddy_secret(body, store, now) {
        Ok(expires_at) => reply(
            stream,
            200,
            Json::Object(vec![
                ("service".to_owned(), Json::String("buildbuddy".to_owned())),
                ("purpose".to_owned(), Json::String(PURPOSE.to_owned())),
                ("expires_at".to_owned(), Json::number(expires_at)),
            ]),
        ),
        Err(StoreError::Invalid) => reply(stream, 400, error("invalid_secret_request")),
        Err(StoreError::Unavailable) => reply(stream, 500, error("secret_storage_unavailable")),
    }
}

#[derive(Debug, PartialEq, Eq)]
enum StoreError {
    Invalid,
    Unavailable,
}

fn store_buildbuddy_secret(
    body: &[u8],
    store: &mut impl SecretStore,
    now: u64,
) -> Result<u64, StoreError> {
    let (token, days) = parse_store_request(body).map_err(|()| StoreError::Invalid)?;
    let expires_at = now
        .checked_add(days * 24 * 60 * 60)
        .ok_or(StoreError::Invalid)?;
    let record = json!({
        "version": 1,
        "purpose": PURPOSE,
        "token": token,
        "expires_at": expires_at,
    });
    store
        .set(&record.to_string())
        .map_err(|()| StoreError::Unavailable)?;
    Ok(expires_at)
}

fn parse_store_request(body: &[u8]) -> Result<(String, u64), ()> {
    let parsed = parse_json(std::str::from_utf8(body).map_err(|_| ())?).map_err(|_| ())?;
    let Json::Object(fields) = &parsed else {
        return Err(());
    };
    if fields.iter().enumerate().any(|(index, (key, _))| {
        !matches!(key.as_str(), "token" | "expires_in_days")
            || fields[..index].iter().any(|(previous, _)| previous == key)
    }) {
        return Err(());
    }
    let token = parsed
        .object("token")
        .and_then(Json::as_str)
        .filter(|value| {
            (8..=4096).contains(&value.len()) && !value.chars().any(char::is_whitespace)
        });
    let days = match parsed.object("expires_in_days") {
        None => 30,
        Some(value) => match value.as_u64() {
            Some(days) => days,
            None => return Err(()),
        },
    };
    let token = token
        .filter(|_| (1..=MAX_TTL_DAYS).contains(&days))
        .ok_or(())?;
    Ok((token.to_owned(), days))
}

pub(crate) fn buildbuddy_revoke(stream: &mut TcpStream) -> Result<(), String> {
    let mut store = KeychainSecretStore;
    buildbuddy_revoke_with(stream, &mut store)
}

pub(crate) fn buildbuddy_revoke_with(
    stream: &mut TcpStream,
    store: &mut impl SecretStore,
) -> Result<(), String> {
    if revoke_buildbuddy_secret(store).is_err() {
        return reply(stream, 500, error("secret_storage_unavailable"));
    }
    reply(
        stream,
        200,
        Json::Object(vec![
            ("service".to_owned(), Json::String("buildbuddy".to_owned())),
            ("revoked".to_owned(), Json::Bool(true)),
        ]),
    )
}

fn revoke_buildbuddy_secret(store: &mut impl SecretStore) -> Result<(), ()> {
    store.delete()
}

pub(crate) fn buildbuddy_get_invocation(stream: &mut TcpStream, body: &[u8]) -> Result<(), String> {
    let mut store = KeychainSecretStore;
    buildbuddy_get_invocation_with(stream, body, &mut store, now_unix())
}

pub(crate) fn buildbuddy_get_invocation_with(
    stream: &mut TcpStream,
    body: &[u8],
    store: &mut impl SecretStore,
    now: u64,
) -> Result<(), String> {
    let invocation_id = match invocation_id(body) {
        Ok(id) => id,
        Err(()) => return reply(stream, 400, error("invalid_invocation_id")),
    };
    let secret = match load_secret(store, now) {
        Ok(Some(secret)) => secret,
        Ok(None) => return reply(stream, 404, error("buildbuddy_secret_unavailable")),
        Err(()) => return reply(stream, 500, error("secret_storage_unavailable")),
    };
    let result = fetch_active_invocation(Some(secret), &invocation_id, send_buildbuddy_request);
    match result {
        Ok(summary) => {
            let payload = serde_json::to_string(&summary).unwrap_or_else(|_| "{}".to_owned());
            let value = parse_json(&payload).unwrap_or_else(|_| Json::Object(vec![]));
            reply(stream, 200, value)
        }
        Err(FetchError::NotFound) => reply(stream, 404, error("invocation_not_found")),
        Err(FetchError::NoCredential) => reply(stream, 404, error("buildbuddy_secret_unavailable")),
        Err(FetchError::Unavailable) => reply(stream, 502, error("buildbuddy_unavailable")),
    }
}

fn invocation_id(body: &[u8]) -> Result<String, ()> {
    let value = parse_json(std::str::from_utf8(body).map_err(|_| ())?).map_err(|_| ())?;
    let Json::Object(fields) = &value else {
        return Err(());
    };
    if fields.len() != 1 || fields[0].0 != "invocation_id" {
        return Err(());
    }
    let id = value
        .object("invocation_id")
        .and_then(Json::as_str)
        .ok_or(())?;
    if id.is_empty()
        || id.len() > 128
        || !id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
    {
        return Err(());
    }
    Ok(id.to_owned())
}

fn load_secret(store: &mut impl SecretStore, now: u64) -> Result<Option<StoredSecret>, ()> {
    let Some(value) = store.get()? else {
        return Ok(None);
    };
    let record: Value = serde_json::from_str(&value).map_err(|_| ())?;
    if record.get("version").and_then(Value::as_u64) != Some(1)
        || record.get("purpose").and_then(Value::as_str) != Some(PURPOSE)
    {
        return Err(());
    }
    let token = record
        .get("token")
        .and_then(Value::as_str)
        .filter(|token| !token.is_empty())
        .ok_or(())?;
    let expires_at = record.get("expires_at").and_then(Value::as_u64).ok_or(())?;
    if expires_at <= now {
        // Expired entries are treated as revoked even if Keychain cleanup fails.
        let _ = store.delete();
        return Ok(None);
    }
    Ok(Some(StoredSecret {
        token: token.to_owned(),
    }))
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_secs()
}

#[derive(Debug, PartialEq, Eq)]
enum FetchError {
    NoCredential,
    NotFound,
    Unavailable,
}

fn fetch_active_invocation(
    secret: Option<StoredSecret>,
    invocation_id: &str,
    request: impl FnMut(&str, &str) -> Result<Value, u16>,
) -> Result<Value, FetchError> {
    let secret = secret.ok_or(FetchError::NoCredential)?;
    fetch_invocation(&secret.token, invocation_id, request)
}

fn fetch_invocation(
    token: &str,
    invocation_id: &str,
    mut request: impl FnMut(&str, &str) -> Result<Value, u16>,
) -> Result<Value, FetchError> {
    match request(token, invocation_id) {
        Ok(response) => sanitize_invocation(response, token).ok_or(FetchError::NotFound),
        Err(404) => Err(FetchError::NotFound),
        Err(_) => Err(FetchError::Unavailable),
    }
}

fn send_buildbuddy_request(token: &str, invocation_id: &str) -> Result<Value, u16> {
    send_buildbuddy_request_at(
        "https://app.buildbuddy.io/api/v1/GetInvocation",
        token,
        invocation_id,
    )
}

fn send_buildbuddy_request_at(url: &str, token: &str, invocation_id: &str) -> Result<Value, u16> {
    // The API key is carried in a custom header. ureq preserves custom headers
    // across redirects, so disable redirect handling to prevent credential
    // forwarding to a different host.
    let agent = ureq::AgentBuilder::new().redirects(0).build();
    let response = agent
        .post(url)
        .set("x-buildbuddy-api-key", token)
        .set("content-type", "application/json")
        .send_json(json!({ "selector": { "invocation_id": invocation_id } }));
    match response {
        Ok(response) if (300..400).contains(&response.status()) => Err(response.status()),
        Ok(response) => response.into_json::<Value>().map_err(|_| 502),
        Err(ureq::Error::Status(status, _)) => Err(status),
        Err(ureq::Error::Transport(_)) => Err(502),
    }
}

/// Return a fixed metadata allowlist. Build metadata, workspace status,
/// artifacts, logs, and all unknown fields are omitted by construction.
fn sanitize_invocation(response: Value, token: &str) -> Option<Value> {
    let invocations = response.get("invocation")?.as_array()?;
    let invocation = invocations.first()?.as_object()?;
    let mut summary = Map::new();
    let invocation_resource_id = invocation
        .get("id")
        .and_then(Value::as_object)
        .and_then(|id| id.get("invocationId").or_else(|| id.get("invocation_id")))
        .and_then(Value::as_str)
        .map(|id| id.replace(token, "[REDACTED]"));
    if let Some(id) = invocation_resource_id {
        summary.insert("invocation_id".to_owned(), Value::String(id));
    }
    for (output, camel, snake) in [
        ("success", "success", "success"),
        ("duration_usec", "durationUsec", "duration_usec"),
        ("command", "command", "command"),
        ("action_count", "actionCount", "action_count"),
        ("created_at_usec", "createdAtUsec", "created_at_usec"),
        ("updated_at_usec", "updatedAtUsec", "updated_at_usec"),
        ("bazel_exit_code", "bazelExitCode", "bazel_exit_code"),
        ("invocation_status", "invocationStatus", "invocation_status"),
    ] {
        let value = invocation.get(camel).or_else(|| invocation.get(snake));
        if let Some(value) = value.filter(|v| v.is_string() || v.is_number() || v.is_boolean()) {
            let value = match value {
                Value::String(text) => Value::String(text.replace(token, "[REDACTED]")),
                other => other.clone(),
            };
            summary.insert(output.to_owned(), value);
        }
    }
    Some(json!({ "purpose": PURPOSE, "invocation": summary }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::{Shutdown, TcpListener};
    use std::thread;

    #[derive(Default)]
    struct MemoryStore {
        value: Option<String>,
        deletes: usize,
        fail_get: bool,
        fail_set: bool,
        fail_delete: bool,
    }

    impl SecretStore for MemoryStore {
        fn get(&mut self) -> Result<Option<String>, ()> {
            if self.fail_get {
                Err(())
            } else {
                Ok(self.value.clone())
            }
        }

        fn set(&mut self, value: &str) -> Result<(), ()> {
            if self.fail_set {
                Err(())
            } else {
                self.value = Some(value.to_owned());
                Ok(())
            }
        }

        fn delete(&mut self) -> Result<(), ()> {
            self.deletes += 1;
            if self.fail_delete {
                Err(())
            } else {
                self.value = None;
                Ok(())
            }
        }
    }

    fn route_reply(handler: impl FnOnce(&mut TcpStream) -> Result<(), String>) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let client = thread::spawn(move || {
            let mut stream = TcpStream::connect(address).unwrap();
            let mut response = String::new();
            stream.read_to_string(&mut response).unwrap();
            response
        });
        let (mut stream, _) = listener.accept().unwrap();
        handler(&mut stream).unwrap();
        stream.shutdown(Shutdown::Write).unwrap();
        client.join().unwrap()
    }

    #[test]
    fn invocation_proxy_sends_token_only_to_upstream_and_filters_response() {
        let token = "never-return-this-api-token";
        let response = fetch_invocation(token, "inv-123", |sent_token, id| {
            assert_eq!(sent_token, token);
            assert_eq!(id, "inv-123");
            Ok(json!({
                "invocation": [{
                    "id": { "invocationId": "inv-123" },
                    "success": false,
                    "durationUsec": "1234",
                    "command": "build",
                    "actionCount": "7",
                    "bazelExitCode": "TESTS_FAILED",
                    "buildMetadata": [{"key":"credential", "value": token}],
                    "artifacts": [{"uri": token}],
                    "apiKey": token
                }],
                "nextPageToken": token
            }))
        })
        .unwrap();
        let text = response.to_string();
        assert!(!text.contains(token));
        assert!(text.contains("inv-123"));
        assert!(text.contains("TESTS_FAILED"));
        assert!(!text.contains("buildMetadata"));
        assert!(!text.contains("artifacts"));
        assert!(!text.contains("nextPageToken"));
        assert_eq!(
            response["invocation"]["invocation_id"],
            Value::String("inv-123".to_owned())
        );
    }

    #[test]
    fn invocation_proxy_never_surfaces_upstream_error_details() {
        assert_eq!(
            fetch_invocation("token", "id", |_, _| Err(500)),
            Err(FetchError::Unavailable)
        );
        assert_eq!(
            fetch_invocation("token", "id", |_, _| Err(404)),
            Err(FetchError::NotFound)
        );
    }

    #[test]
    fn expired_credentials_are_revoked_at_the_boundary_and_never_reach_upstream() {
        let issued_at = 1_000_000;
        let mut store = MemoryStore::default();
        let expiry = store_buildbuddy_secret(
            br#"{"token":"secret-value","expires_in_days":1}"#,
            &mut store,
            issued_at,
        )
        .unwrap();
        assert!(load_secret(&mut store, expiry - 1).unwrap().is_some());
        let expired = load_secret(&mut store, expiry).unwrap();
        assert!(expired.is_none());
        assert_eq!(store.deletes, 1);

        let mut upstream_called = false;
        let result = fetch_active_invocation(expired, "inv-1", |_, _| {
            upstream_called = true;
            Ok(json!({"invocation": []}))
        });
        assert_eq!(result, Err(FetchError::NoCredential));
        assert!(!upstream_called);
    }

    #[test]
    fn buildbuddy_revocation_route_is_idempotent_and_never_returns_the_secret() {
        let mut store = MemoryStore {
            value: Some("stored credential".to_owned()),
            ..MemoryStore::default()
        };
        for expected_deletes in [1, 2] {
            let response = route_reply(|stream| buildbuddy_revoke_with(stream, &mut store));
            assert!(response.starts_with("HTTP/1.1 200"));
            assert!(!response.contains("stored credential"));
            assert_eq!(store.deletes, expected_deletes);
        }
        assert!(store.value.is_none());
    }

    #[test]
    fn buildbuddy_store_route_does_not_return_the_submitted_secret() {
        let mut store = MemoryStore::default();
        let response = route_reply(|stream| {
            buildbuddy_store_with(
                stream,
                br#"{"token":"sensitive-token-123"}"#,
                &mut store,
                1_000,
            )
        });
        assert!(response.starts_with("HTTP/1.1 200"));
        assert!(!response.contains("sensitive-token-123"));
        assert!(
            store
                .value
                .as_deref()
                .unwrap()
                .contains("sensitive-token-123")
        );
    }

    #[test]
    fn keychain_store_failures_map_to_generic_secret_errors() {
        let mut store = MemoryStore {
            fail_set: true,
            ..MemoryStore::default()
        };
        let response = route_reply(|stream| {
            buildbuddy_store_with(
                stream,
                br#"{"token":"submitted-sensitive-token"}"#,
                &mut store,
                1_000,
            )
        });
        assert!(response.starts_with("HTTP/1.1 500"));
        assert!(response.contains("secret_storage_unavailable"));
        assert!(!response.contains("submitted-sensitive-token"));

        let mut store = MemoryStore {
            fail_delete: true,
            value: Some("stored-sensitive-token".to_owned()),
            ..MemoryStore::default()
        };
        let response = route_reply(|stream| buildbuddy_revoke_with(stream, &mut store));
        assert!(response.starts_with("HTTP/1.1 500"));
        assert!(response.contains("secret_storage_unavailable"));
        assert!(!response.contains("stored-sensitive-token"));

        let mut store = MemoryStore {
            fail_get: true,
            value: Some("stored-sensitive-token".to_owned()),
            ..MemoryStore::default()
        };
        let response = route_reply(|stream| {
            buildbuddy_get_invocation_with(
                stream,
                br#"{"invocation_id":"inv-1"}"#,
                &mut store,
                1_000,
            )
        });
        assert!(response.starts_with("HTTP/1.1 500"));
        assert!(response.contains("secret_storage_unavailable"));
        assert!(!response.contains("stored-sensitive-token"));
    }

    #[test]
    fn malformed_stored_records_return_generic_errors_without_disclosure() {
        let records = [
            "malformed-stored-token-not-json",
            r#"{"version":2,"purpose":"buildbuddy.invocations.read","token":"malformed-stored-token","expires_at":2000}"#,
            r#"{"version":1,"purpose":"buildbuddy.invocations.read","token":"malformed-stored-token","expires_at":"not-a-timestamp"}"#,
        ];
        for record in records {
            let mut store = MemoryStore {
                value: Some(record.to_owned()),
                ..MemoryStore::default()
            };
            let response = route_reply(|stream| {
                buildbuddy_get_invocation_with(
                    stream,
                    br#"{"invocation_id":"inv-1"}"#,
                    &mut store,
                    1_000,
                )
            });
            assert!(response.starts_with("HTTP/1.1 500"));
            assert!(response.contains("secret_storage_unavailable"));
            assert!(!response.contains("malformed-stored-token"));
        }
    }

    #[test]
    fn buildbuddy_api_key_is_not_forwarded_across_redirects() {
        let redirect_listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let redirect_address = redirect_listener.local_addr().unwrap();
        let destination_listener = TcpListener::bind("127.0.0.1:0").unwrap();
        destination_listener.set_nonblocking(true).unwrap();
        let destination = destination_listener.local_addr().unwrap();
        let redirect_server = thread::spawn(move || {
            let (mut stream, _) = redirect_listener.accept().unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut line = String::new();
            loop {
                line.clear();
                reader.read_line(&mut line).unwrap();
                if line == "\r\n" || line.is_empty() {
                    break;
                }
            }
            write!(
                stream,
                "HTTP/1.1 302 Found\r\nLocation: http://{destination}/capture\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            )
            .unwrap();
        });

        let result = send_buildbuddy_request_at(
            &format!("http://{redirect_address}/GetInvocation"),
            "sensitive-api-key",
            "inv-1",
        );
        assert!(result.is_err());
        redirect_server.join().unwrap();
        assert!(destination_listener.accept().is_err());
    }

    #[test]
    fn invocation_request_requires_one_bounded_identifier_field() {
        assert_eq!(
            invocation_id(br#"{"invocation_id":"abc-123"}"#).unwrap(),
            "abc-123"
        );
        assert!(invocation_id(br#"{"invocation_id":"../secret"}"#).is_err());
        assert!(invocation_id(br#"{"invocation_id":"id","token":"leak"}"#).is_err());
    }

    #[test]
    fn secret_record_has_a_bounded_expiry_and_rejects_ambiguous_fields() {
        assert_eq!(
            parse_store_request(br#"{"token":"secret-value"}"#)
                .unwrap()
                .1,
            30
        );
        assert_eq!(
            parse_store_request(br#"{"token":"secret-value","expires_in_days":90}"#)
                .unwrap()
                .1,
            90
        );
        for body in [
            br#"{"token":"secret-value","expires_in_days":0}"#.as_slice(),
            br#"{"token":"secret-value","expires_in_days":91}"#.as_slice(),
            br#"{"token":"one","token":"two"}"#.as_slice(),
            br#"{"token":"secret-value","unknown":true}"#.as_slice(),
        ] {
            assert!(parse_store_request(body).is_err());
        }
    }
}
