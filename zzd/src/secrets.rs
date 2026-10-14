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

pub(crate) fn buildbuddy_store(stream: &mut TcpStream, body: &[u8]) -> Result<(), String> {
    let (token, days) = match parse_store_request(body) {
        Ok(value) => value,
        Err(()) => return reply(stream, 400, error("invalid_secret_request")),
    };
    let Some(expires_at) = now_unix().checked_add(days * 24 * 60 * 60) else {
        return reply(stream, 400, error("invalid_secret_request"));
    };
    let record = json!({
        "version": 1,
        "purpose": PURPOSE,
        "token": token,
        "expires_at": expires_at,
    });
    let result =
        Entry::new(SERVICE, ACCOUNT).and_then(|entry| entry.set_password(&record.to_string()));
    if result.is_err() {
        return reply(stream, 500, error("secret_storage_unavailable"));
    }
    reply(
        stream,
        200,
        Json::Object(vec![
            ("service".to_owned(), Json::String("buildbuddy".to_owned())),
            ("purpose".to_owned(), Json::String(PURPOSE.to_owned())),
            ("expires_at".to_owned(), Json::number(expires_at)),
        ]),
    )
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
    let result = Entry::new(SERVICE, ACCOUNT).and_then(|entry| match entry.delete_credential() {
        Ok(()) => Ok(()),
        // Revocation is idempotent: an absent item is already revoked.
        Err(keyring::Error::NoEntry) => Ok(()),
        Err(error) => Err(error),
    });
    if result.is_err() {
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

pub(crate) fn buildbuddy_get_invocation(stream: &mut TcpStream, body: &[u8]) -> Result<(), String> {
    let invocation_id = match invocation_id(body) {
        Ok(id) => id,
        Err(()) => return reply(stream, 400, error("invalid_invocation_id")),
    };
    let secret = match load_secret() {
        Ok(Some(secret)) => secret,
        Ok(None) => return reply(stream, 404, error("buildbuddy_secret_unavailable")),
        Err(()) => return reply(stream, 500, error("secret_storage_unavailable")),
    };
    let result = fetch_invocation(&secret.token, &invocation_id, send_buildbuddy_request);
    match result {
        Ok(summary) => {
            let payload = serde_json::to_string(&summary).unwrap_or_else(|_| "{}".to_owned());
            let value = parse_json(&payload).unwrap_or_else(|_| Json::Object(vec![]));
            reply(stream, 200, value)
        }
        Err(FetchError::NotFound) => reply(stream, 404, error("invocation_not_found")),
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

fn load_secret() -> Result<Option<StoredSecret>, ()> {
    let entry = Entry::new(SERVICE, ACCOUNT).map_err(|_| ())?;
    let value = match entry.get_password() {
        Ok(value) => value,
        Err(keyring::Error::NoEntry) => return Ok(None),
        Err(_) => return Err(()),
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
    if expires_at <= now_unix() {
        // Expired entries are treated as revoked even if Keychain cleanup fails.
        let _ = entry.delete_credential();
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
    NotFound,
    Unavailable,
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
    let response = ureq::post("https://app.buildbuddy.io/api/v1/GetInvocation")
        .set("x-buildbuddy-api-key", token)
        .set("content-type", "application/json")
        .send_json(json!({ "selector": { "invocation_id": invocation_id } }));
    match response {
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
    for (output, camel, snake) in [
        ("invocation_id", "invocationId", "invocation_id"),
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

    #[test]
    fn invocation_proxy_sends_token_only_to_upstream_and_filters_response() {
        let token = "never-return-this-api-token";
        let response = fetch_invocation(token, "inv-123", |sent_token, id| {
            assert_eq!(sent_token, token);
            assert_eq!(id, "inv-123");
            Ok(json!({
                "invocation": [{
                    "invocationId": "inv-123",
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
